//! 入站聊天消息富媒体与结构化消息段丰富器（解析 `Reply`、`Image` 局部截切与 `@成员`）

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::SystemTime;

use common_types::constants::{DEFAULT_ONEBOT_ID_BOUND, IMAGE_PLACEHOLDER_TOKEN};
use common_types::{
    face_name_to_id, stable_onebot_id, BotLoginInfo, ChatTargetKind, GroupMemberInfo,
    MessageSegment,
};
use regex::Regex;
use serde_json::{json, Value};
use storage::Storage;
use tracing::debug;

static FORWARD_CARD_FOOTER_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^查看\s*\d+\s*条转发消息$").expect("valid forward card footer regex")
});

use crate::constants::{
    AT_ALL_DISPLAY_NAME, FILE_BUBBLE_SIZE_REGEX, FILE_BUBBLE_STATUS_KEYWORDS,
    MEDIA_CACHE_KEEP_LIMIT, MEDIA_CACHE_SUBDIR, MEDIA_PNG_EXTENSION, MEDIA_URL_FILE_SCHEME,
    ONEBOT_AT_ALL_QQ,
};
use crate::parser::{self, ParsedTreeMessage};

/// 将单条 UIA 解析消息丰富为 OneBot 富媒体消息段时使用的上下文对象（Parameter Object）
pub(crate) struct EnrichContext<'a> {
    pub(crate) storage: &'a Storage,
    pub(crate) session_key: &'a str,
    pub(crate) chat_kind: ChatTargetKind,
    pub(crate) hwnd: isize,
    pub(crate) members: &'a [GroupMemberInfo],
    pub(crate) bot_login: &'a BotLoginInfo,
}

/// 批量将一批解析消息（含可见图片矩形框）丰富为结构化 `MessageSegment` 并回填回复关系
pub(crate) fn enrich_parsed_messages(ctx: &EnrichContext<'_>, items: &mut [ParsedTreeMessage]) {
    for item in items.iter_mut() {
        enrich_single_message(ctx, item);
    }
}

/// 为单条入站消息解析回复引用、提取 `@` 提及并尝试截取可见图片气泡保存到本地缓存
pub(crate) fn enrich_single_message(ctx: &EnrichContext<'_>, item: &mut ParsedTreeMessage) {
    populate_sender_member_metadata(ctx.members, &mut item.message);
    if !item.message.segments.is_empty() {
        check_reply_targets_self(ctx, &mut item.message, None);
        item.message.target_kind = ctx.chat_kind;
        return;
    }

    let mut segments = Vec::new();
    let (reply_preview, after_reply) = parser::extract_reply_preview(&item.message.content);
    let mut reply_sender_hint: Option<String> = None;
    let remaining_body = if let Some((sender_hint, excerpt)) = reply_preview {
        reply_sender_hint = sender_hint.clone();
        match ctx.storage.find_recent_message_for_reply_in_session(
            ctx.session_key,
            ctx.chat_kind,
            sender_hint.as_deref(),
            &excerpt,
        ) {
            Ok(Some(reply_id)) => {
                item.message.reply_to_message_id = Some(reply_id);
                segments.push(MessageSegment::Reply {
                    id: reply_id.to_string(),
                });
                after_reply
            }
            _ => {
                // 未匹配到历史被引用消息时，仅将引文头作为纯文本保留，严禁解析引文内的 @ 触发误响应
                let prefix_len = item.message.content.len().saturating_sub(after_reply.len());
                if let Some(quote_prefix) = item.message.content.get(..prefix_len) {
                    if !quote_prefix.is_empty() {
                        segments.push(MessageSegment::Text {
                            text: quote_prefix.to_string(),
                        });
                    }
                }
                after_reply
            }
        }
    } else {
        item.message.content.as_str()
    };

    if let Some(file_seg) = try_parse_file_bubble_segment(remaining_body) {
        segments.push(file_seg);
        check_reply_targets_self(ctx, &mut item.message, reply_sender_hint.as_deref());
        item.message.target_kind = ctx.chat_kind;
        item.message.segments = segments;
        return;
    }

    if let Some(forward_seg) =
        try_parse_forward_card_segment(ctx, &item.message.message_id, remaining_body)
    {
        segments.push(forward_seg);
        check_reply_targets_self(ctx, &mut item.message, reply_sender_hint.as_deref());
        item.message.target_kind = ctx.chat_kind;
        item.message.segments = segments;
        return;
    }

    let mut captured_images =
        capture_image_segments(ctx, &item.message.message_id, &item.image_rects);
    let mut img_iter = captured_images.drain(..);
    let mentions = build_mention_candidates(ctx.members, ctx.bot_login);

    if remaining_body.contains(IMAGE_PLACEHOLDER_TOKEN) {
        let mut parts = remaining_body.split(IMAGE_PLACEHOLDER_TOKEN).peekable();
        while let Some(part) = parts.next() {
            append_text_and_mentions(part, &mentions, &mut segments);
            if parts.peek().is_some() {
                if let Some(img_seg) = img_iter.next() {
                    segments.push(img_seg);
                } else {
                    segments.push(MessageSegment::Image {
                        file: String::new(),
                        url: None,
                        summary: Some(IMAGE_PLACEHOLDER_TOKEN.to_string()),
                        image_type: None,
                    });
                }
            }
        }
    } else {
        append_text_and_mentions(remaining_body, &mentions, &mut segments);
    }

    for extra_img in img_iter {
        segments.push(extra_img);
    }

    if segments.is_empty() && !item.message.content.is_empty() {
        segments.push(MessageSegment::Text {
            text: item.message.content.clone(),
        });
    }

    check_reply_targets_self(ctx, &mut item.message, reply_sender_hint.as_deref());
    item.message.target_kind = ctx.chat_kind;
    item.message.segments = segments;
}

fn normalize_thin_spaces(raw: &str) -> String {
    raw.replace(['\u{2009}', '\u{00A0}', '\u{202F}', '\u{3000}'], " ")
}

fn parse_qqnt_file_size_bytes(meta: &str) -> Option<u64> {
    let caps = FILE_BUBBLE_SIZE_REGEX.captures(meta)?;
    let num: f64 = caps.name("num")?.as_str().parse().ok()?;
    let unit = caps.name("unit")?.as_str().to_ascii_uppercase();
    let mult = match unit.as_str() {
        "TB" => 1024.0_f64 * 1024.0 * 1024.0 * 1024.0,
        "GB" => 1024.0_f64 * 1024.0 * 1024.0,
        "MB" => 1024.0_f64 * 1024.0,
        "KB" => 1024.0_f64,
        "B" => 1.0_f64,
        _ => return None,
    };
    let scaled = (num * mult).round();
    if scaled.is_finite() && scaled >= 0.0 {
        Some(scaled as u64)
    } else {
        None
    }
}

fn is_video_bubble_filename(filename: &str) -> bool {
    let lower = filename.trim().to_ascii_lowercase();
    [".mp4", ".mov", ".mkv", ".webm", ".avi", ".flv", ".m4v"]
        .iter()
        .any(|ext| lower.ends_with(ext))
}

fn has_percent_progress_token(meta: &str) -> bool {
    meta.split_whitespace().any(|tok| {
        tok.strip_suffix('%')
            .and_then(|digits| digits.parse::<u16>().ok())
            .is_some_and(|pct| pct <= 100)
    })
}

fn try_parse_file_bubble_segment(body: &str) -> Option<MessageSegment> {
    let normalized = normalize_thin_spaces(body);
    let lines: Vec<&str> = normalized
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && *line != IMAGE_PLACEHOLDER_TOKEN)
        .collect();
    if lines.len() < 2 || lines.len() > 6 {
        return None;
    }
    let size_idx = lines
        .iter()
        .position(|line| parse_qqnt_file_size_bytes(line).is_some())?;
    if size_idx < 1 {
        return None;
    }
    if lines[..size_idx].iter().any(|part| {
        part.is_empty()
            || part.starts_with('[')
            || FILE_BUBBLE_STATUS_KEYWORDS
                .iter()
                .any(|kw| part.contains(kw))
    }) {
        return None;
    }
    let filename = lines[..size_idx].concat();
    if filename.is_empty() || filename.len() > 255 {
        return None;
    }

    let meta = lines[size_idx..].join(" ");
    let size_bytes = parse_qqnt_file_size_bytes(&meta)?;
    let has_status_or_progress = FILE_BUBBLE_STATUS_KEYWORDS
        .iter()
        .any(|kw| meta.contains(kw))
        || has_percent_progress_token(&meta);
    if !has_status_or_progress {
        return None;
    }
    let without_size = FILE_BUBBLE_SIZE_REGEX.replace_all(&meta, " ");
    let mut residual = without_size.into_owned();
    for kw in FILE_BUBBLE_STATUS_KEYWORDS {
        residual = residual.replace(kw, " ");
    }
    for action in ["下载", "打开", "电脑", "手机"] {
        residual = residual.replace(action, " ");
    }
    let has_extra_prose = residual.chars().any(|ch| {
        !ch.is_ascii_digit()
            && !ch.is_whitespace()
            && !matches!(ch, '|' | '/' | '·' | '-' | '.' | '%')
    });
    if has_extra_prose {
        return None;
    }

    if is_video_bubble_filename(&filename) {
        return Some(MessageSegment::Video {
            file: filename,
            url: None,
        });
    }

    Some(MessageSegment::File {
        file: filename.clone(),
        name: Some(filename),
        size: Some(size_bytes),
    })
}

fn split_forward_preview_line(line: &str) -> (Option<&str>, &str) {
    let sep = line
        .find(": ")
        .map(|idx| (idx, 2))
        .or_else(|| line.find('：').map(|idx| (idx, '：'.len_utf8())))
        .or_else(|| line.find(':').map(|idx| (idx, 1)));
    let Some((idx, sep_len)) = sep else {
        return (None, line.trim());
    };
    let sender = line[..idx].trim();
    if sender.is_empty()
        || sender.chars().count() > 40
        || sender.eq_ignore_ascii_case("http")
        || sender.eq_ignore_ascii_case("https")
    {
        return (None, line.trim());
    }
    let body = line[idx + sep_len..].trim();
    (Some(sender), body)
}

fn resolve_forward_preview_user_id(ctx: &EnrichContext<'_>, nickname: &str) -> String {
    let nick = nickname.trim();
    if nick.is_empty() {
        return "0".to_string();
    }
    let self_nick = ctx.bot_login.nickname.trim();
    if !self_nick.is_empty() && nick == self_nick {
        return ctx
            .bot_login
            .real_qq_id
            .filter(|id| *id > 0)
            .unwrap_or(ctx.bot_login.user_id)
            .to_string();
    }
    if let Some(member) = ctx
        .members
        .iter()
        .find(|m| m.nickname == nick || (!m.card.is_empty() && m.card == nick))
    {
        return member.real_user_id.unwrap_or(member.user_id).to_string();
    }
    stable_onebot_id(&format!("forward:{nick}"), DEFAULT_ONEBOT_ID_BOUND).to_string()
}

/// 识别原生合并转发气泡并将预览行幂等持久化为 `fwd-*` 节点包，返回 `MessageSegment::Forward`
fn try_parse_forward_card_segment(
    ctx: &EnrichContext<'_>,
    message_uid: &str,
    remaining_body: &str,
) -> Option<MessageSegment> {
    let lines: Vec<&str> = remaining_body
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    if lines.len() < 3 {
        return None;
    }
    let header = lines[0];
    let valid_header = header == "群聊的聊天记录"
        || header == "聊天记录"
        || (header.ends_with("的聊天记录")
            && header.chars().count() > "的聊天记录".chars().count());
    if !valid_header {
        return None;
    }
    let footer = lines[lines.len() - 1];
    if !FORWARD_CARD_FOOTER_REGEX.is_match(footer) {
        return None;
    }

    let mut nodes: Vec<Value> = Vec::with_capacity(lines.len() - 2);
    for preview_line in &lines[1..lines.len() - 1] {
        let (sender_opt, body) = split_forward_preview_line(preview_line);
        let nickname = sender_opt.unwrap_or("成员");
        let user_id = sender_opt
            .map(|name| resolve_forward_preview_user_id(ctx, name))
            .unwrap_or_else(|| "0".to_string());
        let content_text = if body.is_empty() { *preview_line } else { body };
        nodes.push(json!({
            "type": "node",
            "data": {
                "user_id": user_id,
                "nickname": nickname,
                "content": [
                    {
                        "type": "text",
                        "data": { "text": content_text }
                    }
                ]
            }
        }));
    }

    let nodes_json = serde_json::to_string(&nodes).ok()?;
    let forward_id = ctx
        .storage
        .get_or_allocate_forward_for_uid(message_uid, &nodes_json)
        .ok()?;
    Some(MessageSegment::Forward { id: forward_id })
}

/// 判断给定消息段列表中是否包含 `At`、`Reply` 或 `Image` 等非纯文本富媒体段
pub(crate) fn has_rich_segments(segments: &[MessageSegment]) -> bool {
    segments
        .iter()
        .any(|seg| !matches!(seg, MessageSegment::Text { .. }))
}

/// 从已缓存群成员列表中回填发送者的群角色与群名片
fn populate_sender_member_metadata(
    members: &[GroupMemberInfo],
    msg: &mut common_types::ChatMessage,
) {
    let sender = msg.sender.trim();
    if sender.is_empty() || members.is_empty() {
        return;
    }
    if let Some(member) = members
        .iter()
        .find(|m| m.nickname == sender || (!m.card.is_empty() && m.card == sender))
    {
        let role = member.role.trim();
        if !role.is_empty() {
            msg.sender_role = Some(role.to_string());
        }
        let card = member.card.trim();
        if !card.is_empty() {
            msg.sender_card = Some(card.to_string());
        }
    }
}

/// 结合数据库记录与引述作者昵称判定入站引用回复是否指向机器人自身
fn check_reply_targets_self(
    ctx: &EnrichContext<'_>,
    msg: &mut common_types::ChatMessage,
    reply_sender_hint: Option<&str>,
) {
    if msg.reply_targets_self {
        return;
    }
    if let Some(reply_id) = msg.reply_to_message_id {
        if let Ok(Some(rec)) = ctx.storage.get_message_by_id(reply_id) {
            if rec.is_self {
                msg.reply_targets_self = true;
                return;
            }
        }
    }
    if let Some(hint) = reply_sender_hint.map(str::trim).filter(|s| !s.is_empty()) {
        let self_nick = ctx.bot_login.nickname.trim();
        if !self_nick.is_empty() && hint.eq_ignore_ascii_case(self_nick) {
            msg.reply_targets_self = true;
        }
    }
}

fn capture_image_segments(
    ctx: &EnrichContext<'_>,
    message_id: &str,
    image_rects: &[[i32; 4]],
) -> Vec<MessageSegment> {
    if image_rects.is_empty() {
        return Vec::new();
    }
    let can_capture_disk = !ctx.storage.is_memory() && ctx.hwnd != 0;
    let mut out = Vec::with_capacity(image_rects.len());

    for (idx, rect) in image_rects.iter().copied().enumerate() {
        if !can_capture_disk {
            out.push(MessageSegment::Image {
                file: String::new(),
                url: None,
                summary: Some(IMAGE_PLACEHOLDER_TOKEN.to_string()),
                image_type: None,
            });
            continue;
        }
        match load_or_capture_media_file(ctx.hwnd, message_id, idx, rect) {
            Some(file_url) => out.push(MessageSegment::Image {
                file: file_url.clone(),
                url: Some(file_url),
                summary: Some(IMAGE_PLACEHOLDER_TOKEN.to_string()),
                image_type: None,
            }),
            None => out.push(MessageSegment::Image {
                file: String::new(),
                url: None,
                summary: Some(IMAGE_PLACEHOLDER_TOKEN.to_string()),
                image_type: None,
            }),
        }
    }
    out
}

fn load_or_capture_media_file(
    hwnd: isize,
    message_id: &str,
    idx: usize,
    rect: [i32; 4],
) -> Option<String> {
    let dir = Path::new(MEDIA_CACHE_SUBDIR);
    if fs::create_dir_all(dir).is_err() {
        return None;
    }
    let safe_id: String = message_id
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || *ch == '_' || *ch == '-')
        .take(32)
        .collect();
    let filename = format!("{safe_id}_{idx}.{MEDIA_PNG_EXTENSION}");
    let file_path = dir.join(filename);

    // 对已落盘缓存的图片切片直接复用路径，避免后台高频轮询反复重刷 PrintWindow
    if let Ok(meta) = fs::metadata(&file_path) {
        if meta.len() > 0 {
            return path_to_file_url(&file_path);
        }
    }

    let png_bytes = match uia_core::capture_window_crop_png(hwnd, rect) {
        Ok(bytes) => bytes,
        Err(err) => {
            debug!(error = %err, message_id, idx, "局部截切入站图片失败，保留占位摘要");
            return None;
        }
    };
    if fs::write(&file_path, &png_bytes).is_err() {
        return None;
    }
    prune_media_cache_if_oversized(dir);
    path_to_file_url(&file_path)
}

fn path_to_file_url(path: &Path) -> Option<String> {
    let abs = fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path));
    let raw = abs.to_string_lossy();
    let trimmed = raw.strip_prefix(r"\\?\").unwrap_or(&raw);
    let forward = trimmed.replace('\\', "/");
    if forward.starts_with('/') {
        Some(format!("{MEDIA_URL_FILE_SCHEME}{forward}"))
    } else {
        Some(format!("{MEDIA_URL_FILE_SCHEME}/{forward}"))
    }
}

fn prune_media_cache_if_oversized(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(SystemTime, PathBuf)> = entries
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let path = entry.path();
            let is_png = path
                .extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| ext.eq_ignore_ascii_case(MEDIA_PNG_EXTENSION))
                .unwrap_or(false);
            if !is_png {
                return None;
            }
            let modified = entry
                .metadata()
                .ok()
                .and_then(|meta| meta.modified().ok())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            Some((modified, path))
        })
        .collect();

    if files.len() <= MEDIA_CACHE_KEEP_LIMIT {
        return;
    }
    files.sort_by_key(|(modified, _)| *modified);
    let excess = files.len() - MEDIA_CACHE_KEEP_LIMIT;
    for (_, path) in files.into_iter().take(excess) {
        let _ = fs::remove_file(path);
    }
}

fn build_mention_candidates(
    members: &[GroupMemberInfo],
    bot_login: &BotLoginInfo,
) -> Vec<(String, String)> {
    let mut pairs: Vec<(String, String)> = Vec::new();
    pairs.push((
        AT_ALL_DISPLAY_NAME.to_string(),
        ONEBOT_AT_ALL_QQ.to_string(),
    ));

    let bot_nick = bot_login.nickname.trim();
    if !bot_nick.is_empty() {
        let bot_qq = bot_login
            .real_qq_id
            .filter(|id| *id > 0)
            .unwrap_or(bot_login.user_id)
            .to_string();
        pairs.push((bot_nick.to_string(), bot_qq));
    }

    for member in members {
        let effective_qq = member.real_user_id.unwrap_or(member.user_id).to_string();
        for alias in [member.card.trim(), member.nickname.trim()] {
            if !alias.is_empty() && !pairs.iter().any(|(name, _)| name == alias) {
                pairs.push((alias.to_string(), effective_qq.clone()));
            }
        }
    }

    // 较长昵称优先匹配，避免带公共前缀的成员昵称被短昵称提前截断
    pairs.sort_by_key(|left| std::cmp::Reverse(left.0.len()));
    pairs
}

fn append_text_and_mentions(
    input: &str,
    candidates: &[(String, String)],
    out: &mut Vec<MessageSegment>,
) {
    if input.is_empty() {
        return;
    }

    let mut cursor = 0usize;
    let mut text_start = 0usize;
    while cursor < input.len() {
        let ch = match input[cursor..].chars().next() {
            Some(c) => c,
            None => break,
        };
        if ch != '@' && ch != '＠' {
            cursor += ch.len_utf8();
            continue;
        }

        let after_at_idx = cursor + ch.len_utf8();
        let rest = &input[after_at_idx..];

        // 跳过形如 user@example.com 的电子邮件地址中的 `@`
        let preceded_by_ascii_ident = input[..cursor]
            .chars()
            .next_back()
            .map(|prev| prev.is_ascii_alphanumeric() || matches!(prev, '.' | '_' | '-'))
            .unwrap_or(false);

        if let Some((matched_name, matched_qq, byte_len)) = match_known_candidate(rest, candidates)
        {
            push_plain_text(&input[text_start..cursor], out);
            let display = if matched_qq == ONEBOT_AT_ALL_QQ {
                Some(AT_ALL_DISPLAY_NAME.to_string())
            } else {
                Some(matched_name.to_string())
            };
            out.push(MessageSegment::At {
                qq: matched_qq.to_string(),
                name: display,
            });
            cursor = after_at_idx + byte_len;
            text_start = cursor;
            continue;
        }

        if !preceded_by_ascii_ident {
            if let Some((token, byte_len)) = extract_fallback_mention_token(rest) {
                let is_numeric_uin =
                    (5..=12).contains(&token.len()) && token.chars().all(|c| c.is_ascii_digit());
                let qq = if is_numeric_uin {
                    token.to_string()
                } else {
                    stable_onebot_id(&format!("at:{token}"), DEFAULT_ONEBOT_ID_BOUND).to_string()
                };
                push_plain_text(&input[text_start..cursor], out);
                out.push(MessageSegment::At {
                    qq,
                    name: Some(token.to_string()),
                });
                cursor = after_at_idx + byte_len;
                text_start = cursor;
                continue;
            }
        }

        cursor = after_at_idx;
    }

    push_plain_text(&input[text_start..], out);
}

fn match_known_candidate<'a>(
    rest: &str,
    candidates: &'a [(String, String)],
) -> Option<(&'a str, &'a str, usize)> {
    for (name, qq) in candidates {
        if let Some(after) = rest.strip_prefix(name.as_str()) {
            let clean_boundary = after
                .chars()
                .next()
                .map(|next| {
                    !(name
                        .chars()
                        .next_back()
                        .map(|last| last.is_ascii_alphanumeric())
                        .unwrap_or(false)
                        && next.is_ascii_alphanumeric())
                })
                .unwrap_or(true);
            if clean_boundary {
                return Some((name.as_str(), qq.as_str(), name.len()));
            }
        }
    }
    None
}

fn extract_fallback_mention_token(rest: &str) -> Option<(&str, usize)> {
    let mut end = 0usize;
    for ch in rest.chars() {
        if ch.is_whitespace()
            || matches!(
                ch,
                '@' | '＠' | ',' | '，' | '。' | '!' | '！' | '?' | '？' | '[' | ']' | '「' | '」'
            )
        {
            break;
        }
        end += ch.len_utf8();
        if end >= 48 {
            break;
        }
    }
    if end == 0 {
        return None;
    }
    let token = &rest[..end];
    Some((token, end))
}

/// 通过声明式宏生成入站方括号占位标记（语音、视频、合并转发及系统表情）的段映射分类器
macro_rules! define_bracket_placeholder_rules {
    (
        $(
            $($token:literal)|+ => $builder:expr
        ),* $(,)?
    ) => {
        fn classify_bracket_placeholder(tag: &str) -> Option<MessageSegment> {
            match tag.trim() {
                $(
                    $($token)|+ => Some($builder),
                )*
                other if !other.is_empty() => {
                    face_name_to_id(other).map(|face_id| MessageSegment::Face {
                        id: face_id.to_string(),
                        text: Some(other.to_string()),
                    })
                }
                _ => None,
            }
        }
    };
}

define_bracket_placeholder_rules! {
    "语音" => MessageSegment::Record {
        file: String::new(),
        url: None,
        magic: None,
    },
    "视频" | "短视频" => MessageSegment::Video {
        file: String::new(),
        url: None,
    },
    "合并转发" | "聊天记录" => MessageSegment::Forward { id: String::new() },
}

fn push_plain_text(slice: &str, out: &mut Vec<MessageSegment>) {
    if slice.is_empty() {
        return;
    }

    // 扫描文本中的 `[微笑]`、`[语音]`、`[视频]`、`[合并转发]` 等 QQNT UIA 占位标记并拆分为结构化段
    let mut cursor = 0usize;
    let mut text_start = 0usize;
    while cursor < slice.len() {
        let Some(open_rel) = slice[cursor..].find('[') else {
            break;
        };
        let open = cursor + open_rel;
        let Some(close_rel) = slice[open + 1..].find(']') else {
            break;
        };
        let close = open + 1 + close_rel;
        let inner = &slice[open + 1..close];
        if inner.contains('[') || inner.contains('/') || inner.contains('\n') || inner.len() > 24 {
            cursor = open + 1;
            continue;
        }
        if let Some(seg) = classify_bracket_placeholder(inner) {
            push_merged_raw_text(&slice[text_start..open], out);
            out.push(seg);
            cursor = close + 1;
            text_start = cursor;
        } else {
            cursor = open + 1;
        }
    }

    push_merged_raw_text(&slice[text_start..], out);
}

fn push_merged_raw_text(slice: &str, out: &mut Vec<MessageSegment>) {
    if slice.is_empty() {
        return;
    }
    if let Some(MessageSegment::Text { text }) = out.last_mut() {
        text.push_str(slice);
    } else {
        out.push(MessageSegment::Text {
            text: slice.to_string(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enriches_qqnt_file_bubble_variants_with_thin_and_nbsp_spaces() {
        let thin_seg =
            try_parse_file_bubble_segment("sample_live_verify.txt\n56.00\u{2009}B   已发送")
                .expect("parse file bubble with thin space");
        assert_eq!(
            thin_seg,
            MessageSegment::File {
                file: "sample_live_verify.txt".to_string(),
                name: Some("sample_live_verify.txt".to_string()),
                size: Some(56),
            }
        );

        let nbsp_seg =
            try_parse_file_bubble_segment("季度报表.pdf\n2.50\u{00A0}MB\n未下载 7天内有效")
                .expect("parse multi-line file bubble");
        assert_eq!(
            nbsp_seg,
            MessageSegment::File {
                file: "季度报表.pdf".to_string(),
                name: Some("季度报表.pdf".to_string()),
                size: Some(2_621_440),
            }
        );

        let video_seg = try_parse_file_bubble_segment(
            "[图片]\nproto_v4_live_video.mp4\n7.69\u{2009}KB 已发送\n[图片]",
        )
        .expect("parse mp4 card into Video segment");
        assert_eq!(
            video_seg,
            MessageSegment::Video {
                file: "proto_v4_live_video.mp4".to_string(),
                url: None,
            }
        );

        let video_0pct =
            try_parse_file_bubble_segment("proto_v4_live_video.mp4\n10.97\u{2009}KB\n0%")
                .expect("parse 0% video card into Video segment");
        assert_eq!(
            video_0pct,
            MessageSegment::Video {
                file: "proto_v4_live_video.mp4".to_string(),
                url: None,
            }
        );

        let file_0pct = try_parse_file_bubble_segment("report.txt\n10.97\u{2009}KB\n0%")
            .expect("parse 0% file card into File segment");
        assert_eq!(
            file_0pct,
            MessageSegment::File {
                file: "report.txt".to_string(),
                name: Some("report.txt".to_string()),
                size: Some(11233),
            }
        );

        let split_video_with_images = try_parse_file_bubble_segment(
            "[图片]\nproto_v4_live_vid\neo.mp4\n7.69\u{2009}KB 已发送\n[图片]",
        )
        .expect("parse vertically split video card into Video segment");
        assert_eq!(
            split_video_with_images,
            MessageSegment::Video {
                file: "proto_v4_live_video.mp4".to_string(),
                url: None,
            }
        );

        let split_file_seg =
            try_parse_file_bubble_segment("proto_v4_live_ve\nrify.txt\n70.00\u{2009}B   已发送")
                .expect("parse vertically split file card into File segment");
        assert_eq!(
            split_file_seg,
            MessageSegment::File {
                file: "proto_v4_live_verify.txt".to_string(),
                name: Some("proto_v4_live_verify.txt".to_string()),
                size: Some(70),
            }
        );

        let storage = Storage::open(":memory:").expect("memory storage");
        let bot_login = BotLoginInfo {
            user_id: 10001,
            nickname: "测试机器人".to_string(),
            real_qq_id: None,
        };
        let ctx = EnrichContext {
            storage: &storage,
            session_key: "Demo测试群",
            chat_kind: ChatTargetKind::Group,
            hwnd: 0,
            members: &[],
            bot_login: &bot_login,
        };
        let mut clipped_item = ParsedTreeMessage {
            message: common_types::ChatMessage {
                message_id: "uid-clipped-video".to_string(),
                group_name: "Demo测试群".to_string(),
                sender: "测试机器人".to_string(),
                content: "[图片]\nproto_v4_live_vid\neo.mp4\n7.69\u{2009}KB 已发送\n[图片]\n[图片]"
                    .to_string(),
                is_self: true,
                target_kind: ChatTargetKind::Group,
                ..Default::default()
            },
            image_rects: vec![[900, 100, 1100, 190], [900, 200, 1100, 250]],
            ..Default::default()
        };
        enrich_single_message(&ctx, &mut clipped_item);
        assert_eq!(
            clipped_item.message.segments,
            vec![MessageSegment::Video {
                file: "proto_v4_live_video.mp4".to_string(),
                url: None,
            }],
            "半裁切带 image_rects 的视频卡片不得退化为 Image 截切伪造"
        );

        assert!(
            try_parse_file_bubble_segment("电池状态\n50%").is_none(),
            "percentage without valid file size should not be treated as file/video bubble"
        );

        assert!(
            try_parse_file_bubble_segment("readme.txt\n12 KB 已发送给你请查收").is_none(),
            "prose containing status words should not be treated as file bubble"
        );
    }

    #[test]
    fn unmatched_reply_keeps_quote_as_plain_text_without_at_and_caches_forward_card_idempotently() {
        let storage = Storage::open(":memory:").expect("memory storage");
        let bot_login = BotLoginInfo {
            user_id: 200000003,
            nickname: "测试机器人".to_string(),
            real_qq_id: Some(200000003),
        };
        let members = vec![GroupMemberInfo {
            group_id: 100000001,
            user_id: 200000002,
            real_user_id: Some(200000002),
            nickname: "测试好友A".to_string(),
            card: String::new(),
            role: "owner".to_string(),
        }];
        let ctx = EnrichContext {
            storage: &storage,
            session_key: "Demo测试群",
            chat_kind: ChatTargetKind::Group,
            hwnd: 0,
            members: &members,
            bot_login: &bot_login,
        };

        // 1. 未命中历史 reply_id 时，引文内部的 @测试机器人 不得解析为 At 段，正文中的 @测试好友A 正常解析
        let mut unmatched_reply = ParsedTreeMessage {
            message: common_types::ChatMessage {
                message_id: "uid-unmatched-reply".to_string(),
                group_name: "Demo测试群".to_string(),
                sender: "群友A".to_string(),
                content: "「回复 @群友B: @测试机器人 历史引文内容」\n@测试好友A 正文响应"
                    .to_string(),
                target_kind: ChatTargetKind::Group,
                ..Default::default()
            },
            ..Default::default()
        };
        enrich_single_message(&ctx, &mut unmatched_reply);
        assert!(unmatched_reply.message.reply_to_message_id.is_none());
        assert!(
            !unmatched_reply.message.segments.iter().any(|seg| matches!(
                seg,
                MessageSegment::Reply { .. }
            ) || matches!(
                seg,
                MessageSegment::At { qq, .. } if qq == "200000003"
            )),
            "unmatched quote must not emit Reply or bot At segment: {:?}",
            unmatched_reply.message.segments
        );
        assert!(
            unmatched_reply.message.segments.iter().any(|seg| matches!(
                seg,
                MessageSegment::At { qq, .. } if qq == "200000002"
            )),
            "body @测试好友A must still be parsed: {:?}",
            unmatched_reply.message.segments
        );

        // 2. 原生合并转发卡片结构化提取与同 UID 幂等复用 fwd-* 编号
        let make_forward_item = || {
            ParsedTreeMessage {
            message: common_types::ChatMessage {
                message_id: "uid-live-forward-card".to_string(),
                group_name: "Demo测试群".to_string(),
                sender: "测试机器人".to_string(),
                content: "群聊的聊天记录\n测试机器人: [LIVE-06-ANCHOR-1]\n测试好友A: [LIVE-06-ANCHOR-2]\n查看2条转发消息".to_string(),
                is_self: true,
                target_kind: ChatTargetKind::Group,
                ..Default::default()
            },
            ..Default::default()
        }
        };
        let mut fwd1 = make_forward_item();
        enrich_single_message(&ctx, &mut fwd1);
        let fwd_id_1 = match fwd1.message.segments.as_slice() {
            [MessageSegment::Forward { id }] => id.clone(),
            other => panic!("expected single Forward segment, got {other:?}"),
        };
        assert!(fwd_id_1.starts_with("fwd-"));

        let mut fwd2 = make_forward_item();
        enrich_single_message(&ctx, &mut fwd2);
        assert_eq!(
            fwd2.message.segments,
            vec![MessageSegment::Forward {
                id: fwd_id_1.clone()
            }]
        );

        let stored_nodes = storage
            .get_meta(&format!("forward:{fwd_id_1}"))
            .expect("get_meta")
            .expect("forward nodes stored");
        let parsed: Value = serde_json::from_str(&stored_nodes).expect("valid nodes json");
        let arr = parsed.as_array().expect("nodes array");
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["data"]["nickname"], "测试机器人");
        assert_eq!(
            arr[0]["data"]["content"][0]["data"]["text"],
            "[LIVE-06-ANCHOR-1]"
        );
        assert_eq!(arr[1]["data"]["nickname"], "测试好友A");
        assert_eq!(arr[1]["data"]["user_id"], "200000002");
    }
}
