//! OneBot v11 全量消息段双向 CQ 码、JSON 数组编解码与可读摘要折叠器

use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

use super::face_table::face_id_to_name;
use super::segments::MessageSegment;
use crate::error::{AdapterError, Result};

/// 追加键值对参数并在构造 CQ 标签时统一处理可选参数跳过空值
macro_rules! cq_encode_tag {
    ($out:expr, $tag:expr, req => [$($rk:literal => $rv:expr),* $(,)?], opt => [$($ok:literal => $ov:expr),* $(,)?]) => {{
        $out.push_str("[CQ:");
        $out.push_str($tag);
        $(
            $out.push(',');
            $out.push_str($rk);
            $out.push('=');
            $out.push_str(&escape_cq_param($rv));
        )*
        $(
            if let Some(val) = $ov.as_deref().filter(|s| !s.is_empty()) {
                $out.push(',');
                $out.push_str($ok);
                $out.push('=');
                $out.push_str(&escape_cq_param(val));
            }
        )*
        $out.push(']');
    }};
}

/// 从 JSON 字段表中提取必填或可选标量并转为字符串
macro_rules! json_opt_scalar {
    ($data:expr, $key:literal) => {
        $data
            .and_then(|d| d.get($key))
            .and_then(value_as_flexible_scalar_string)
    };
    ($data:expr, $key:literal, $alias:literal) => {
        $data
            .and_then(|d| d.get($key).or_else(|| d.get($alias)))
            .and_then(value_as_flexible_scalar_string)
    };
    ($data:expr, $key:literal, $alias1:literal, $alias2:literal) => {
        $data
            .and_then(|d| {
                d.get($key)
                    .or_else(|| d.get($alias1))
                    .or_else(|| d.get($alias2))
            })
            .and_then(value_as_flexible_scalar_string)
    };
}

/// 对纯文本节点执行 OneBot CQ 码转义（转义 `&`、`[`、`]`）
pub fn escape_cq_text(raw: &str) -> String {
    let mut escaped = String::with_capacity(raw.len());
    for ch in raw.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '[' => escaped.push_str("&#91;"),
            ']' => escaped.push_str("&#93;"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

/// 对 CQ 码参数值执行 OneBot 转义（额外转义逗号 `,`）
pub fn escape_cq_param(raw: &str) -> String {
    let mut escaped = String::with_capacity(raw.len());
    for ch in raw.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '[' => escaped.push_str("&#91;"),
            ']' => escaped.push_str("&#93;"),
            ',' => escaped.push_str("&#44;"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

/// 将 CQ 码中常见的 XML/HTML 实体转义序列还原为原始字符
pub fn unescape_cq(raw: &str) -> String {
    if !raw.contains('&') {
        return raw.to_string();
    }
    raw.replace("&#91;", "[")
        .replace("&#93;", "]")
        .replace("&#44;", ",")
        .replace("&amp;", "&")
}

/// 将单个结构化消息段编码为标准 CQ 码或转义纯文本并追加到缓冲
fn append_segment_cq(output: &mut String, segment: &MessageSegment) {
    match segment {
        MessageSegment::Text { text } => output.push_str(&escape_cq_text(text)),
        MessageSegment::Face { id, .. } => {
            cq_encode_tag!(output, "face", req => ["id" => id], opt => [])
        }
        MessageSegment::Image {
            file,
            url,
            summary: _,
            image_type,
        } => cq_encode_tag!(
            output,
            "image",
            req => ["file" => file],
            opt => ["type" => image_type, "url" => url]
        ),
        MessageSegment::Record { file, url, magic } => cq_encode_tag!(
            output,
            "record",
            req => ["file" => file],
            opt => ["magic" => magic, "url" => url]
        ),
        MessageSegment::Video { file, url } => {
            cq_encode_tag!(output, "video", req => ["file" => file], opt => ["url" => url])
        }
        MessageSegment::At { qq, name } => {
            cq_encode_tag!(output, "at", req => ["qq" => qq], opt => ["name" => name])
        }
        MessageSegment::Rps {} => cq_encode_tag!(output, "rps", req => [], opt => []),
        MessageSegment::Dice {} => cq_encode_tag!(output, "dice", req => [], opt => []),
        MessageSegment::Shake {} => cq_encode_tag!(output, "shake", req => [], opt => []),
        MessageSegment::Poke {
            poke_type,
            id,
            name,
        } => cq_encode_tag!(
            output,
            "poke",
            req => ["type" => poke_type, "id" => id],
            opt => ["name" => name]
        ),
        MessageSegment::Anonymous { ignore } => {
            cq_encode_tag!(output, "anonymous", req => [], opt => ["ignore" => ignore])
        }
        MessageSegment::Share {
            url,
            title,
            content,
            image,
        } => cq_encode_tag!(
            output,
            "share",
            req => ["url" => url, "title" => title],
            opt => ["content" => content, "image" => image]
        ),
        MessageSegment::Contact { contact_type, id } => cq_encode_tag!(
            output,
            "contact",
            req => ["type" => contact_type, "id" => id],
            opt => []
        ),
        MessageSegment::Location {
            lat,
            lon,
            title,
            content,
        } => cq_encode_tag!(
            output,
            "location",
            req => ["lat" => lat, "lon" => lon],
            opt => ["title" => title, "content" => content]
        ),
        MessageSegment::Music {
            music_type,
            id,
            url,
            audio,
            title,
            content,
            image,
        } => cq_encode_tag!(
            output,
            "music",
            req => ["type" => music_type],
            opt => [
                "id" => id,
                "url" => url,
                "audio" => audio,
                "title" => title,
                "content" => content,
                "image" => image,
            ]
        ),
        MessageSegment::Reply { id } => {
            cq_encode_tag!(output, "reply", req => ["id" => id], opt => [])
        }
        MessageSegment::Forward { id } => {
            cq_encode_tag!(output, "forward", req => ["id" => id], opt => [])
        }
        MessageSegment::Node {
            id,
            user_id,
            nickname,
            content,
        } => {
            let content_raw = content.as_ref().map(|val| match val {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            });
            cq_encode_tag!(
                output,
                "node",
                req => [],
                opt => [
                    "id" => id,
                    "user_id" => user_id,
                    "nickname" => nickname,
                    "content" => content_raw.as_ref(),
                ]
            );
        }
        MessageSegment::Xml { data } => {
            cq_encode_tag!(output, "xml", req => ["data" => data], opt => [])
        }
        MessageSegment::Json { data } => {
            cq_encode_tag!(output, "json", req => ["data" => data], opt => [])
        }
        MessageSegment::File { file, name, size } => {
            let size_raw = size.map(|val| val.to_string());
            cq_encode_tag!(
                output,
                "file",
                req => ["file" => file],
                opt => ["name" => name, "size" => size_raw.as_ref()]
            );
        }
    }
}

/// 将消息段序列编码为标准 OneBot v11 `raw_message`（含全量 21 种 `[CQ:...]` 标记）
pub fn segments_to_cq_string(segments: &[MessageSegment]) -> String {
    let mut output = String::new();
    for segment in segments {
        append_segment_cq(&mut output, segment);
    }
    output
}

fn push_spaced_token(output: &mut String, token: &str) {
    if token.is_empty() {
        return;
    }
    if !output.is_empty() && !output.ends_with('\n') && !output.ends_with(' ') {
        output.push(' ');
    }
    output.push_str(token);
}

fn summarize_media_or_card(segment: &MessageSegment) -> Option<String> {
    let token = match segment {
        MessageSegment::Text { .. } | MessageSegment::At { .. } => return None,
        MessageSegment::Reply { .. } | MessageSegment::Anonymous { .. } => return None,
        MessageSegment::Face { id, text } => {
            let label = text
                .as_deref()
                .map(str::trim)
                .filter(|val| !val.is_empty())
                .or_else(|| face_id_to_name(id))
                .unwrap_or(id.trim());
            format!("[表情:{label}]")
        }
        MessageSegment::Image {
            summary,
            image_type,
            ..
        } => {
            if image_type
                .as_deref()
                .is_some_and(|val| val.trim().eq_ignore_ascii_case("flash"))
            {
                "[闪照]".to_string()
            } else {
                summary
                    .as_deref()
                    .filter(|val| !val.trim().is_empty())
                    .unwrap_or("[图片]")
                    .to_string()
            }
        }
        MessageSegment::Record { .. } => "[语音]".to_string(),
        MessageSegment::Video { .. } => "[视频]".to_string(),
        MessageSegment::Rps {} => "[猜拳]".to_string(),
        MessageSegment::Dice {} => "[掷骰子]".to_string(),
        MessageSegment::Shake {} => "[窗口抖动]".to_string(),
        MessageSegment::Poke {
            poke_type,
            id,
            name,
        } => {
            let label = name
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| {
                    if poke_type.trim().is_empty() {
                        id.trim()
                    } else {
                        poke_type.trim()
                    }
                });
            format!("[戳一戳:{label}]")
        }
        MessageSegment::Share { title, url, .. } => format!("[链接分享] {title} {url}"),
        MessageSegment::Contact { contact_type, id } => {
            format!("[推荐名片:{contact_type}:{id}]")
        }
        MessageSegment::Location {
            lat, lon, title, ..
        } => {
            let label = title.as_deref().map(str::trim).filter(|s| !s.is_empty());
            match label {
                Some(t) => format!("[位置] {t} ({lat},{lon})"),
                None => format!("[位置] ({lat},{lon})"),
            }
        }
        MessageSegment::Music {
            music_type,
            id,
            url,
            title,
            ..
        } => {
            let hint = title
                .as_deref()
                .or(url.as_deref())
                .or(id.as_deref())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("");
            if hint.is_empty() {
                format!("[音乐分享:{music_type}]")
            } else {
                format!("[音乐分享:{music_type}] {hint}")
            }
        }
        MessageSegment::Forward { id } => {
            if id.trim().is_empty() {
                "[合并转发]".to_string()
            } else {
                format!("[合并转发:{id}]")
            }
        }
        MessageSegment::Node { nickname, id, .. } => {
            let label = nickname
                .as_deref()
                .or(id.as_deref())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("节点");
            format!("[转发节点:{label}]")
        }
        MessageSegment::Xml { .. } => "[XML卡片]".to_string(),
        MessageSegment::Json { .. } => "[JSON卡片]".to_string(),
        MessageSegment::File { file, name, .. } => {
            let label = name
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(file.trim());
            format!("[文件:{label}]")
        }
    };
    Some(token)
}

/// 将消息段序列折叠为便于日志、UI 回显校验与纯文本发送降级展示的可读摘要
pub fn segments_to_plain_summary(segments: &[MessageSegment]) -> String {
    let mut output = String::new();
    for segment in segments {
        match segment {
            MessageSegment::Text { text } => output.push_str(text),
            MessageSegment::At { qq, name } => {
                if qq.eq_ignore_ascii_case("all") {
                    output.push_str("@全体成员 ");
                } else if let Some(display_name) =
                    name.as_deref().filter(|val| !val.trim().is_empty())
                {
                    output.push('@');
                    output.push_str(display_name.trim());
                    output.push(' ');
                } else {
                    output.push('@');
                    output.push_str(qq.trim());
                    output.push(' ');
                }
            }
            other => {
                if let Some(token) = summarize_media_or_card(other) {
                    push_spaced_token(&mut output, &token);
                }
            }
        }
    }
    output.trim().to_string()
}

fn cq_tag_regex() -> &'static Regex {
    // 使用单一非嵌套重复组 `(,[^\]]*)?` 替代嵌套重复，避免未闭合逗号炸弹产生冗余状态回溯
    static RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\[CQ:([a-zA-Z0-9_]+)(,[^\]]*)?\]").expect("valid CQ code regex")
    });
    &RE
}

fn push_merged_text_segment(segments: &mut Vec<MessageSegment>, chunk: String) {
    if chunk.is_empty() {
        return;
    }
    if let Some(MessageSegment::Text { text }) = segments.last_mut() {
        text.push_str(&chunk);
    } else {
        segments.push(MessageSegment::Text { text: chunk });
    }
}

fn parse_cq_params(param_str: &str) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    for part in param_str.split(',') {
        if part.is_empty() {
            continue;
        }
        if let Some((key, val)) = part.split_once('=') {
            let clean_key = key.trim().to_ascii_lowercase();
            // 仅裁剪参数名空白，完整保留参数值首尾空格与换行符以保证往返编解码无损
            if !clean_key.is_empty() {
                pairs.push((clean_key, unescape_cq(val)));
            }
        }
    }
    pairs
}

fn find_param(pairs: &[(String, String)], target_key: &str) -> Option<String> {
    pairs
        .iter()
        .find(|(k, _)| k == target_key)
        .map(|(_, v)| v.clone())
        .filter(|val| !val.is_empty())
}

fn parse_cq_tag_to_segment(cq_type: &str, pairs: &[(String, String)]) -> Option<MessageSegment> {
    let seg = match cq_type.to_ascii_lowercase().as_str() {
        "face" => MessageSegment::Face {
            id: find_param(pairs, "id")?,
            text: find_param(pairs, "text"),
        },
        "image" => {
            let file = find_param(pairs, "file");
            let url = find_param(pairs, "url");
            if file.is_none() && url.is_none() {
                return None;
            }
            MessageSegment::Image {
                file: file.or_else(|| url.clone()).unwrap_or_default(),
                url,
                summary: find_param(pairs, "summary"),
                image_type: find_param(pairs, "type").or_else(|| find_param(pairs, "sub_type")),
            }
        }
        "record" => {
            let file = find_param(pairs, "file");
            let url = find_param(pairs, "url");
            if file.is_none() && url.is_none() {
                return None;
            }
            MessageSegment::Record {
                file: file.or_else(|| url.clone()).unwrap_or_default(),
                url,
                magic: find_param(pairs, "magic"),
            }
        }
        "video" => {
            let file = find_param(pairs, "file");
            let url = find_param(pairs, "url");
            if file.is_none() && url.is_none() {
                return None;
            }
            MessageSegment::Video {
                file: file.or_else(|| url.clone()).unwrap_or_default(),
                url,
            }
        }
        "at" => MessageSegment::At {
            qq: find_param(pairs, "qq")?,
            name: find_param(pairs, "name"),
        },
        "rps" => MessageSegment::Rps {},
        "dice" => MessageSegment::Dice {},
        "shake" => MessageSegment::Shake {},
        "poke" => {
            let explicit_type = find_param(pairs, "type");
            let peer_qq = find_param(pairs, "qq")
                .or_else(|| find_param(pairs, "user_id"))
                .or_else(|| find_param(pairs, "uin"));
            let id = peer_qq.clone().or_else(|| find_param(pairs, "id"))?;
            let poke_type = explicit_type.or_else(|| peer_qq.map(|_| "poke".to_string()))?;
            MessageSegment::Poke {
                poke_type,
                id,
                name: find_param(pairs, "name"),
            }
        }
        "anonymous" => MessageSegment::Anonymous {
            ignore: find_param(pairs, "ignore"),
        },
        "share" => MessageSegment::Share {
            url: find_param(pairs, "url")?,
            title: find_param(pairs, "title")?,
            content: find_param(pairs, "content"),
            image: find_param(pairs, "image"),
        },
        "contact" => MessageSegment::Contact {
            contact_type: find_param(pairs, "type")?,
            id: find_param(pairs, "id")?,
        },
        "location" => MessageSegment::Location {
            lat: find_param(pairs, "lat")?,
            lon: find_param(pairs, "lon")?,
            title: find_param(pairs, "title"),
            content: find_param(pairs, "content"),
        },
        "music" => MessageSegment::Music {
            music_type: find_param(pairs, "type")?,
            id: find_param(pairs, "id"),
            url: find_param(pairs, "url"),
            audio: find_param(pairs, "audio"),
            title: find_param(pairs, "title"),
            content: find_param(pairs, "content"),
            image: find_param(pairs, "image"),
        },
        "reply" => MessageSegment::Reply {
            id: find_param(pairs, "id")?,
        },
        "forward" => MessageSegment::Forward {
            id: find_param(pairs, "id")?,
        },
        "node" => {
            let id = find_param(pairs, "id");
            let user_id = find_param(pairs, "user_id").or_else(|| find_param(pairs, "uin"));
            let nickname = find_param(pairs, "nickname").or_else(|| find_param(pairs, "name"));
            let content = find_param(pairs, "content").map(Value::String);
            if id.is_none() && user_id.is_none() && content.is_none() {
                return None;
            }
            MessageSegment::Node {
                id,
                user_id,
                nickname,
                content,
            }
        }
        "xml" => MessageSegment::Xml {
            data: find_param(pairs, "data")?,
        },
        "json" => MessageSegment::Json {
            data: find_param(pairs, "data")?,
        },
        "file" => MessageSegment::File {
            file: find_param(pairs, "file")?,
            name: find_param(pairs, "name"),
            size: find_param(pairs, "size").and_then(|raw| raw.trim().parse::<u64>().ok()),
        },
        _ => return None,
    };
    Some(seg)
}

/// 解析内含 CQ 码或纯文本的字符串为结构化消息段列表
pub fn parse_cq_string_to_segments(raw: &str, auto_escape: bool) -> Vec<MessageSegment> {
    if auto_escape || !raw.contains("[CQ:") {
        if raw.is_empty() {
            return Vec::new();
        }
        return vec![MessageSegment::Text {
            text: if auto_escape {
                raw.to_string()
            } else {
                unescape_cq(raw)
            },
        }];
    }

    let mut segments = Vec::new();
    let mut cursor = 0usize;

    for caps in cq_tag_regex().captures_iter(raw) {
        let Some(full_match) = caps.get(0) else {
            continue;
        };
        if full_match.start() > cursor {
            push_merged_text_segment(&mut segments, unescape_cq(&raw[cursor..full_match.start()]));
        }

        let cq_type = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        let params_raw = caps.get(2).map(|m| m.as_str()).unwrap_or("");
        let pairs = parse_cq_params(params_raw);

        if let Some(parsed) = parse_cq_tag_to_segment(cq_type, &pairs) {
            segments.push(parsed);
        } else {
            push_merged_text_segment(&mut segments, full_match.as_str().to_string());
        }
        cursor = full_match.end();
    }

    if cursor < raw.len() {
        push_merged_text_segment(&mut segments, unescape_cq(&raw[cursor..]));
    }
    segments
}

fn value_as_flexible_scalar_string(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => {
            let trimmed = s.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        }
        Value::Number(num) => Some(num.to_string()),
        Value::Bool(flag) => Some(flag.to_string()),
        _ => None,
    }
}

fn parse_json_segment_by_type(seg_type: &str, data: Option<&Value>) -> Option<MessageSegment> {
    let seg = match seg_type.to_ascii_lowercase().as_str() {
        "face" => MessageSegment::Face {
            id: json_opt_scalar!(data, "id")?,
            text: json_opt_scalar!(data, "text"),
        },
        "image" => {
            let file = json_opt_scalar!(data, "file");
            let url = json_opt_scalar!(data, "url");
            if file.is_none() && url.is_none() {
                return None;
            }
            MessageSegment::Image {
                file: file.or_else(|| url.clone()).unwrap_or_default(),
                url,
                summary: json_opt_scalar!(data, "summary"),
                image_type: json_opt_scalar!(data, "type", "sub_type"),
            }
        }
        "record" => {
            let file = json_opt_scalar!(data, "file");
            let url = json_opt_scalar!(data, "url");
            if file.is_none() && url.is_none() {
                return None;
            }
            MessageSegment::Record {
                file: file.or_else(|| url.clone()).unwrap_or_default(),
                url,
                magic: json_opt_scalar!(data, "magic"),
            }
        }
        "video" => {
            let file = json_opt_scalar!(data, "file");
            let url = json_opt_scalar!(data, "url");
            if file.is_none() && url.is_none() {
                return None;
            }
            MessageSegment::Video {
                file: file.or_else(|| url.clone()).unwrap_or_default(),
                url,
            }
        }
        "at" => MessageSegment::At {
            qq: json_opt_scalar!(data, "qq")?,
            name: json_opt_scalar!(data, "name"),
        },
        "rps" => MessageSegment::Rps {},
        "dice" => MessageSegment::Dice {},
        "shake" => MessageSegment::Shake {},
        "poke" => {
            let explicit_type = json_opt_scalar!(data, "type");
            let peer_qq = json_opt_scalar!(data, "qq", "user_id", "uin");
            let id = peer_qq.clone().or_else(|| json_opt_scalar!(data, "id"))?;
            let poke_type = explicit_type.or_else(|| peer_qq.map(|_| "poke".to_string()))?;
            MessageSegment::Poke {
                poke_type,
                id,
                name: json_opt_scalar!(data, "name"),
            }
        }
        "anonymous" => MessageSegment::Anonymous {
            ignore: json_opt_scalar!(data, "ignore"),
        },
        "share" => MessageSegment::Share {
            url: json_opt_scalar!(data, "url")?,
            title: json_opt_scalar!(data, "title")?,
            content: json_opt_scalar!(data, "content"),
            image: json_opt_scalar!(data, "image"),
        },
        "contact" => MessageSegment::Contact {
            contact_type: json_opt_scalar!(data, "type")?,
            id: json_opt_scalar!(data, "id")?,
        },
        "location" => MessageSegment::Location {
            lat: json_opt_scalar!(data, "lat")?,
            lon: json_opt_scalar!(data, "lon")?,
            title: json_opt_scalar!(data, "title"),
            content: json_opt_scalar!(data, "content"),
        },
        "music" => MessageSegment::Music {
            music_type: json_opt_scalar!(data, "type")?,
            id: json_opt_scalar!(data, "id"),
            url: json_opt_scalar!(data, "url"),
            audio: json_opt_scalar!(data, "audio"),
            title: json_opt_scalar!(data, "title"),
            content: json_opt_scalar!(data, "content"),
            image: json_opt_scalar!(data, "image"),
        },
        "reply" => MessageSegment::Reply {
            id: json_opt_scalar!(data, "id")?,
        },
        "forward" => MessageSegment::Forward {
            id: json_opt_scalar!(data, "id")?,
        },
        "node" => {
            let id = json_opt_scalar!(data, "id");
            let user_id = json_opt_scalar!(data, "user_id", "uin");
            let nickname = json_opt_scalar!(data, "nickname", "name");
            let content = data.and_then(|d| d.get("content")).cloned();
            if id.is_none() && user_id.is_none() && content.is_none() {
                return None;
            }
            MessageSegment::Node {
                id,
                user_id,
                nickname,
                content,
            }
        }
        "xml" => MessageSegment::Xml {
            data: json_opt_scalar!(data, "data")?,
        },
        "json" => MessageSegment::Json {
            data: json_opt_scalar!(data, "data")?,
        },
        "file" => MessageSegment::File {
            file: json_opt_scalar!(data, "file")?,
            name: json_opt_scalar!(data, "name"),
            size: json_opt_scalar!(data, "size").and_then(|raw| raw.trim().parse::<u64>().ok()),
        },
        _ => return None,
    };
    Some(seg)
}

fn parse_json_segment_object(segment: &Value, segments: &mut Vec<MessageSegment>) {
    match segment {
        Value::String(text) => push_merged_text_segment(segments, text.clone()),
        Value::Object(obj) => {
            let seg_type = obj.get("type").and_then(Value::as_str).unwrap_or("");
            let data = obj.get("data");
            if seg_type.eq_ignore_ascii_case("text") {
                if let Some(text) = data.and_then(|d| d.get("text")).and_then(Value::as_str) {
                    push_merged_text_segment(segments, text.to_string());
                }
                return;
            }
            if let Some(parsed) = parse_json_segment_by_type(seg_type, data) {
                segments.push(parsed);
            }
        }
        _ => {}
    }
}

/// 将 OneBot JSON `message` 字段（字符串、单段对象或数组）统一解析为消息段列表
pub fn parse_onebot_message_value(value: &Value, auto_escape: bool) -> Result<Vec<MessageSegment>> {
    let mut segments = Vec::new();
    match value {
        Value::String(raw) => {
            segments = parse_cq_string_to_segments(raw, auto_escape);
        }
        Value::Array(items) => {
            for item in items {
                parse_json_segment_object(item, &mut segments);
            }
        }
        Value::Object(_) => parse_json_segment_object(value, &mut segments),
        Value::Null => {}
        other => {
            return Err(AdapterError::InputRejected(format!(
                "unsupported OneBot message payload type: {other}"
            )));
        }
    }

    if !has_sendable_content(&segments) {
        return Err(AdapterError::InputRejected(
            "OneBot message payload contains no sendable text or media segment".to_string(),
        ));
    }
    Ok(segments)
}

/// 检测未开启转义的原始 CQ 字符串中是否存在无法解析为结构化段的闭合标签
pub fn has_unparsed_cq_tag(raw: &str) -> bool {
    if !raw.contains("[CQ:") {
        return false;
    }
    for caps in cq_tag_regex().captures_iter(raw) {
        let cq_type = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        let params_raw = caps.get(2).map(|m| m.as_str()).unwrap_or("");
        let pairs = parse_cq_params(params_raw);
        if parse_cq_tag_to_segment(cq_type, &pairs).is_none() {
            return true;
        }
    }
    false
}

/// 检测 OneBot 消息载荷在未开启转义时是否包含未知或残缺的标签与对象段
pub fn has_unparsed_onebot_value(value: &Value, auto_escape: bool) -> bool {
    match value {
        Value::String(raw) => !auto_escape && has_unparsed_cq_tag(raw),
        Value::Object(obj) => {
            let seg_type = obj.get("type").and_then(Value::as_str).unwrap_or("").trim();
            if seg_type.eq_ignore_ascii_case("text") {
                return false;
            }
            parse_json_segment_by_type(seg_type, obj.get("data")).is_none()
        }
        Value::Array(items) => items.iter().any(|item| {
            matches!(item, Value::Object(_)) && has_unparsed_onebot_value(item, auto_escape)
        }),
        _ => false,
    }
}

fn non_empty_opt(opt: &Option<String>) -> bool {
    opt.as_deref().is_some_and(|val| !val.trim().is_empty())
}

/// 判定消息段集合是否至少包含一项非空文本、表情、媒体、卡片、动作或转发内容
pub fn has_sendable_content(segments: &[MessageSegment]) -> bool {
    segments.iter().any(|seg| match seg {
        MessageSegment::Text { text } => !text.trim().is_empty(),
        MessageSegment::Face { id, .. }
        | MessageSegment::Reply { id }
        | MessageSegment::Forward { id } => !id.trim().is_empty(),
        MessageSegment::Image { file, url, .. }
        | MessageSegment::Record { file, url, .. }
        | MessageSegment::Video { file, url } => !file.trim().is_empty() || non_empty_opt(url),
        MessageSegment::At { qq, .. } => !qq.trim().is_empty(),
        MessageSegment::Rps {} | MessageSegment::Dice {} | MessageSegment::Shake {} => true,
        MessageSegment::Poke { poke_type, id, .. } => {
            !poke_type.trim().is_empty() && !id.trim().is_empty()
        }
        MessageSegment::Anonymous { .. } => false,
        MessageSegment::Share { url, title, .. } => {
            !url.trim().is_empty() && !title.trim().is_empty()
        }
        MessageSegment::Contact { contact_type, id } => {
            !contact_type.trim().is_empty() && !id.trim().is_empty()
        }
        MessageSegment::Location { lat, lon, .. } => {
            !lat.trim().is_empty() && !lon.trim().is_empty()
        }
        MessageSegment::Music {
            music_type,
            id,
            url,
            audio,
            title,
            ..
        } => {
            !music_type.trim().is_empty()
                && (non_empty_opt(id)
                    || non_empty_opt(url)
                    || non_empty_opt(audio)
                    || non_empty_opt(title))
        }
        MessageSegment::Node {
            id,
            user_id,
            content,
            ..
        } => non_empty_opt(id) || non_empty_opt(user_id) || content.is_some(),
        MessageSegment::Xml { data } | MessageSegment::Json { data } => !data.trim().is_empty(),
        MessageSegment::File { file, .. } => !file.trim().is_empty(),
    })
}
