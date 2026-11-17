//! 出站富消息解析、图片源（`base64://`/`data:`/`file://`/本地路径/`http://`）预解码与发送执行计划构造

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use common_types::constants::IMAGE_PLACEHOLDER_TOKEN;
use common_types::{
    has_sendable_content, has_unparsed_cq_tag, parse_cq_string_to_segments,
    segments_to_plain_summary, AdapterError, ChatTargetKind, GroupMemberInfo, MessageSegment,
    Result, SendRequest, StoredMessageRecord,
};
use storage::Storage;
use uia_core::{decode_image_bytes_to_bgra, DecodedImageBgra};

use crate::constants::{
    AT_ALL_DISPLAY_NAME, HTTP_IMAGE_FETCH_TIMEOUT_SECS, IMAGE_SRC_BASE64_PREFIX,
    IMAGE_SRC_DATA_URL_PREFIX, IMAGE_SRC_HTTPS_PREFIX, IMAGE_SRC_HTTP_PREFIX,
    MEDIA_URL_FILE_SCHEME, ONEBOT_AT_ALL_QQ,
};
use crate::parser;

/// 预解码后的待粘贴发送图片位图封装，保证进入剪贴板临界区前已完成像素格式校验
pub(crate) struct DecodedSendImage {
    pub(crate) bitmap: DecodedImageBgra,
}

/// 经安全解析与多媒体预解码后可直接投递至桌面客户端的出站发送执行计划
pub(crate) struct OutboundSendPlan {
    pub(crate) combined_text: String,
    pub(crate) images: Vec<DecodedSendImage>,
    pub(crate) canonical_segments: Vec<MessageSegment>,
    pub(crate) summary_text: String,
    pub(crate) verify_norm: String,
    pub degraded_segments: Vec<String>,
}

fn push_degraded_label(out: &mut Vec<String>, label: &str) {
    if !out.iter().any(|existing| existing == label) {
        out.push(label.to_string());
    }
}

fn degraded_segment_label(segment: &MessageSegment) -> Option<&'static str> {
    match segment {
        MessageSegment::Text { .. } => None,
        MessageSegment::Anonymous { .. } => Some("anonymous"),
        MessageSegment::Image { image_type, .. } => {
            if image_type
                .as_deref()
                .is_some_and(|kind| kind.trim().eq_ignore_ascii_case("flash"))
            {
                Some("image(flash)")
            } else {
                None
            }
        }
        MessageSegment::Face { .. } => Some("face"),
        MessageSegment::At { .. } => Some("at"),
        MessageSegment::Reply { .. } => Some("reply"),
        MessageSegment::Record { .. } => Some("record"),
        MessageSegment::Video { .. } => Some("video"),
        MessageSegment::Forward { .. } => Some("forward"),
        MessageSegment::Node { .. } => Some("node"),
        MessageSegment::Share { .. } => Some("share"),
        MessageSegment::Contact { .. } => Some("contact"),
        MessageSegment::Location { .. } => Some("location"),
        MessageSegment::Music { .. } => Some("music"),
        MessageSegment::Rps {} => Some("rps"),
        MessageSegment::Dice {} => Some("dice"),
        MessageSegment::Shake {} => Some("shake"),
        MessageSegment::Poke { .. } => Some("poke"),
        MessageSegment::Xml { .. } => Some("xml"),
        MessageSegment::Json { .. } => Some("json"),
        MessageSegment::File { .. } => Some("file"),
    }
}

/// 从 `SendRequest` 统一解析生效的消息段数组并执行非空发送内容校检
pub(crate) fn extract_effective_segments(req: &SendRequest) -> Result<Vec<MessageSegment>> {
    let segments = if !req.segments.is_empty() {
        req.segments.clone()
    } else if !req.text.trim().is_empty() {
        parse_cq_string_to_segments(&req.text, false)
    } else {
        Vec::new()
    };

    if !has_sendable_content(&segments) {
        return Err(AdapterError::InputRejected(
            "message cannot be empty".to_string(),
        ));
    }
    Ok(segments)
}

/// 在严格模式下拦截未识别或残缺 `[CQ:...]` 标签，随后统一提取有效消息段列表
pub(crate) fn extract_effective_segments_checked(
    req: &SendRequest,
    allow_degraded: bool,
) -> Result<Vec<MessageSegment>> {
    if !allow_degraded
        && req.segments.is_empty()
        && !req.text.trim().is_empty()
        && has_unparsed_cq_tag(&req.text)
    {
        let action = match req.target_kind {
            ChatTargetKind::Group => "send_group_msg",
            ChatTargetKind::Private => "send_private_msg",
        };
        return Err(AdapterError::UnsupportedAction {
            action: action.to_string(),
            reason: "unrecognized or malformed CQ tag cannot be typed as plain text in strict mode"
                .to_string(),
        });
    }
    extract_effective_segments(req)
}

/// 在异步调度入口提前拉取所有 `http://` 图片源并就地转为 `base64://`，避免阻塞单线程 COM 工作者
pub(crate) async fn prefetch_http_image_segments(segments: &mut [MessageSegment]) -> Result<()> {
    let mut client_opt: Option<reqwest::Client> = None;

    for segment in segments.iter_mut() {
        let MessageSegment::Image { file, url, .. } = segment else {
            continue;
        };
        let raw_source = select_image_source(file, url.as_deref())?;
        let lower = raw_source.to_ascii_lowercase();

        if lower.starts_with(IMAGE_SRC_HTTPS_PREFIX) {
            return Err(AdapterError::InputRejected(
                "https:// image URLs are not supported in offline build; use base64://, file://, local path, or http://".to_string(),
            ));
        }
        if !lower.starts_with(IMAGE_SRC_HTTP_PREFIX) {
            continue;
        }

        let client = match client_opt.as_ref() {
            Some(existing) => existing,
            None => {
                let built = reqwest::Client::builder()
                    .timeout(Duration::from_secs(HTTP_IMAGE_FETCH_TIMEOUT_SECS))
                    .build()
                    .map_err(|err| {
                        AdapterError::Io(std::io::Error::other(format!(
                            "failed to initialize HTTP image client: {err}"
                        )))
                    })?;
                client_opt = Some(built);
                client_opt.as_ref().expect("just inserted client")
            }
        };

        let response = client.get(&raw_source).send().await.map_err(|err| {
            AdapterError::InputRejected(format!(
                "failed to download image from '{raw_source}': {err}"
            ))
        })?;
        if !response.status().is_success() {
            return Err(AdapterError::InputRejected(format!(
                "image HTTP download returned status {} for '{raw_source}'",
                response.status()
            )));
        }
        let bytes = response.bytes().await.map_err(|err| {
            AdapterError::InputRejected(format!(
                "failed to read image body from '{raw_source}': {err}"
            ))
        })?;
        if bytes.is_empty() {
            return Err(AdapterError::InputRejected(format!(
                "downloaded empty image body from '{raw_source}'"
            )));
        }

        *file = format!("{IMAGE_SRC_BASE64_PREFIX}{}", STANDARD.encode(&bytes));
    }

    Ok(())
}

/// 按 ID 读取被引用消息记录并校验其归属于当前目标会话（防止跨会话引用泄密或错点）
pub(crate) fn load_verified_reply_record(
    storage: &Storage,
    raw_id: &str,
    target_session: (&str, ChatTargetKind),
) -> Result<(i64, StoredMessageRecord)> {
    let parsed_id = raw_id.trim().parse::<i64>().map_err(|_| {
        AdapterError::InputRejected(format!("invalid reply target message_id='{raw_id}'"))
    })?;
    let record = storage.get_message_by_id(parsed_id)?.ok_or_else(|| {
        AdapterError::InputRejected(format!("reply target message_id={parsed_id} not found"))
    })?;
    if !target_session.0.trim().is_empty()
        && (record.chat_type != target_session.1
            || !common_types::canonical_group_names_match(&record.group_name, target_session.0))
    {
        return Err(AdapterError::InputRejected(format!(
            "reply target message_id={parsed_id} belongs to another session"
        )));
    }
    Ok((parsed_id, record))
}

/// 在进入剪贴板保护区前解析引用消息、成员 `@` 提及并同步解码所有出站图片位图
pub(crate) fn build_outbound_send_plan(
    storage: &Storage,
    members: &[GroupMemberInfo],
    target_session: (&str, ChatTargetKind),
    segments: &[MessageSegment],
) -> Result<OutboundSendPlan> {
    if !has_sendable_content(segments) {
        return Err(AdapterError::InputRejected(
            "message cannot be empty".to_string(),
        ));
    }

    let mut combined_text = String::new();
    let mut images = Vec::new();
    let mut canonical_segments = Vec::with_capacity(segments.len());
    let mut degraded_segments = Vec::new();
    let mut pending_reply_sep = false;

    for segment in segments {
        if let Some(label) = degraded_segment_label(segment) {
            push_degraded_label(&mut degraded_segments, label);
        }
        match segment {
            MessageSegment::Reply { id } => {
                let (parsed_id, record) = load_verified_reply_record(storage, id, target_session)?;
                let target_segments = if !record.segments.is_empty() {
                    record.segments.clone()
                } else {
                    parse_cq_string_to_segments(&record.raw_message, true)
                };
                let plain = segments_to_plain_summary(&target_segments);
                let excerpt = compact_reply_excerpt(&plain, 32);
                let sender = record.sender_name.trim();
                if !combined_text.is_empty() && !combined_text.ends_with([' ', '\n']) {
                    combined_text.push(' ');
                }
                let _ = write!(combined_text, "「回复 @{sender}: {excerpt}」");
                pending_reply_sep = true;
                canonical_segments.push(MessageSegment::Reply {
                    id: parsed_id.to_string(),
                });
            }
            MessageSegment::At { qq, name } => {
                if std::mem::take(&mut pending_reply_sep) {
                    combined_text.push(' ');
                }
                let display = resolve_at_display_name(qq, name.as_deref(), members);
                combined_text.push('@');
                combined_text.push_str(&display);
                combined_text.push(' ');
                canonical_segments.push(MessageSegment::At {
                    qq: qq.clone(),
                    name: Some(display),
                });
            }
            MessageSegment::Text { text } => {
                if !text.is_empty() {
                    if std::mem::take(&mut pending_reply_sep)
                        && !text.starts_with(char::is_whitespace)
                    {
                        combined_text.push(' ');
                    }
                    combined_text.push_str(text);
                }
                canonical_segments.push(MessageSegment::Text { text: text.clone() });
            }
            MessageSegment::Image {
                file,
                url,
                summary,
                image_type,
            } => {
                let raw_source = select_image_source(file, url.as_deref())?;
                let bytes = load_image_bytes_sync(&raw_source)?;
                let bitmap = decode_image_bytes_to_bgra(&bytes)?;
                images.push(DecodedSendImage { bitmap });
                canonical_segments.push(MessageSegment::Image {
                    file: file.clone(),
                    url: url.clone(),
                    summary: summary
                        .clone()
                        .or_else(|| Some(IMAGE_PLACEHOLDER_TOKEN.to_string())),
                    image_type: image_type.clone(),
                });
            }
            MessageSegment::Anonymous { ignore } => {
                canonical_segments.push(MessageSegment::Anonymous {
                    ignore: ignore.clone(),
                });
            }
            other => {
                let rendered = segments_to_plain_summary(std::slice::from_ref(other));
                if !rendered.is_empty() {
                    pending_reply_sep = false;
                    if !combined_text.is_empty()
                        && !combined_text.ends_with('\n')
                        && !combined_text.ends_with(' ')
                    {
                        combined_text.push(' ');
                    }
                    combined_text.push_str(&rendered);
                }
                canonical_segments.push(other.clone());
            }
        }
    }

    let summary_text = segments_to_plain_summary(&canonical_segments);
    let verify_norm = if images.is_empty() {
        parser::normalize_message_text(&combined_text)
    } else if combined_text.trim().is_empty() {
        IMAGE_PLACEHOLDER_TOKEN.to_string()
    } else {
        parser::normalize_message_text(&format!(
            "{}{IMAGE_PLACEHOLDER_TOKEN}",
            combined_text.trim()
        ))
    };

    Ok(OutboundSendPlan {
        combined_text,
        images,
        canonical_segments,
        summary_text,
        verify_norm,
        degraded_segments,
    })
}

pub(crate) fn select_image_source(file: &str, url: Option<&str>) -> Result<String> {
    let trimmed_file = file.trim();
    if !trimmed_file.is_empty() {
        return Ok(trimmed_file.to_string());
    }
    if let Some(trimmed_url) = url.map(str::trim).filter(|candidate| !candidate.is_empty()) {
        return Ok(trimmed_url.to_string());
    }
    Err(AdapterError::InputRejected(
        "image segment missing both 'file' and 'url'".to_string(),
    ))
}

pub(crate) fn load_image_bytes_sync(source: &str) -> Result<Vec<u8>> {
    let trimmed = source.trim();
    // 使用 UTF-8 字符边界安全的 get(..len) 切片，防止中文或 Emoji 图片路径触发下标越界 panic
    if trimmed
        .get(..IMAGE_SRC_BASE64_PREFIX.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(IMAGE_SRC_BASE64_PREFIX))
    {
        let payload = trimmed
            .get(IMAGE_SRC_BASE64_PREFIX.len()..)
            .unwrap_or_default();
        return decode_base64_payload(payload);
    }

    if trimmed
        .get(..IMAGE_SRC_DATA_URL_PREFIX.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(IMAGE_SRC_DATA_URL_PREFIX))
    {
        let Some(marker_pos) = trimmed.to_ascii_lowercase().find(";base64,") else {
            return Err(AdapterError::InputRejected(
                "data: image URI must use ';base64,' encoding".to_string(),
            ));
        };
        let payload = trimmed
            .get(marker_pos + ";base64,".len()..)
            .unwrap_or_default();
        return decode_base64_payload(payload);
    }

    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with(IMAGE_SRC_HTTPS_PREFIX) {
        return Err(AdapterError::InputRejected(
            "https:// image URLs are not supported in offline build; use base64://, file://, local path, or http://".to_string(),
        ));
    }
    if lower.starts_with(IMAGE_SRC_HTTP_PREFIX) {
        return Err(AdapterError::InputRejected(
            "http:// image source must be prefetched before synchronous WIC decoding".to_string(),
        ));
    }
    if crate::composer::file_staging::is_unc_or_network_path(trimmed) {
        return Err(AdapterError::InputRejected(
            "UNC or remote network share paths are forbidden in offline mode".to_string(),
        ));
    }

    let path = if lower.starts_with(MEDIA_URL_FILE_SCHEME) {
        file_url_to_local_path(trimmed)?
    } else {
        PathBuf::from(trimmed)
    };

    if !Path::new(&path).is_file() {
        return Err(AdapterError::InputRejected(format!(
            "local image file does not exist: {}",
            path.display()
        )));
    }
    fs::read(&path).map_err(AdapterError::Io)
}

fn decode_base64_payload(raw_b64: &str) -> Result<Vec<u8>> {
    let compact: String = raw_b64
        .chars()
        .filter(|ch| !ch.is_ascii_whitespace())
        .collect();
    if compact.is_empty() {
        return Err(AdapterError::InputRejected(
            "base64 image payload is empty".to_string(),
        ));
    }
    STANDARD
        .decode(&compact)
        .map_err(|err| AdapterError::InputRejected(format!("invalid base64 image payload: {err}")))
}

pub(crate) fn file_url_to_local_path(raw_url: &str) -> Result<PathBuf> {
    if crate::composer::file_staging::is_unc_or_network_path(raw_url) {
        return Err(AdapterError::InputRejected(
            "UNC or remote network share paths are forbidden in offline mode".to_string(),
        ));
    }
    let without_scheme = raw_url
        .get(MEDIA_URL_FILE_SCHEME.len()..)
        .unwrap_or_default();
    let stripped_host = if without_scheme
        .get(..9)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("localhost"))
        && without_scheme
            .as_bytes()
            .get(9)
            .is_some_and(|b| *b == b'/' || *b == b'\\')
    {
        without_scheme.get(9..).unwrap_or_default()
    } else {
        without_scheme
    };

    // Windows 下 `file:///F:/dir/a.png` 剥离前导 `/` 以还原盘符路径 `F:/dir/a.png`
    let unslashed = if stripped_host.starts_with('/')
        && stripped_host.as_bytes().get(2).is_some_and(|b| *b == b':')
        && stripped_host
            .as_bytes()
            .get(1)
            .is_some_and(u8::is_ascii_alphabetic)
    {
        stripped_host.get(1..).unwrap_or_default()
    } else {
        stripped_host
    };

    let decoded = decode_basic_file_percent(unslashed);
    if crate::composer::file_staging::is_unc_or_network_path(&decoded) {
        return Err(AdapterError::InputRejected(
            "UNC or remote network share paths are forbidden in offline mode".to_string(),
        ));
    }
    if decoded.trim().is_empty() {
        return Err(AdapterError::InputRejected(format!(
            "invalid file:// image path: {raw_url}"
        )));
    }
    Ok(PathBuf::from(decoded))
}

fn decode_basic_file_percent(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut idx = 0usize;
    while idx < bytes.len() {
        if bytes[idx] == b'%' && idx + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_nibble(bytes[idx + 1]), hex_nibble(bytes[idx + 2])) {
                out.push((hi << 4) | lo);
                idx += 3;
                continue;
            }
        }
        out.push(bytes[idx]);
        idx += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

pub(crate) fn resolve_at_display_name(
    qq: &str,
    explicit_name: Option<&str>,
    members: &[GroupMemberInfo],
) -> String {
    let trimmed_qq = qq.trim();
    if trimmed_qq.eq_ignore_ascii_case(ONEBOT_AT_ALL_QQ) {
        return AT_ALL_DISPLAY_NAME.to_string();
    }
    if let Some(name) = explicit_name.map(str::trim).filter(|val| !val.is_empty()) {
        return name.to_string();
    }
    if let Ok(qq_num) = trimmed_qq.parse::<i64>() {
        if let Some(member) = members
            .iter()
            .find(|cand| cand.user_id == qq_num || cand.real_user_id == Some(qq_num))
        {
            if !member.card.trim().is_empty() {
                return member.card.trim().to_string();
            }
            if !member.nickname.trim().is_empty() {
                return member.nickname.trim().to_string();
            }
        }
    }
    trimmed_qq.to_string()
}

pub(crate) fn compact_reply_excerpt(text: &str, max_chars: usize) -> String {
    let collapsed = parser::normalize_message_text(text);
    let mut chars = collapsed.chars();
    let taken: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{taken}...")
    } else if taken.is_empty() {
        IMAGE_PLACEHOLDER_TOKEN.to_string()
    } else {
        taken
    }
}
