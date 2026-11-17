//! OneBot v11 协议载荷解析、消息段提取与事件序列化转换模块

use std::collections::HashMap;

use axum::body::Bytes;
use axum::Json;
use chrono::Utc;
use common_types::{
    canonical_group_names_match, has_sendable_content, parse_onebot_message_value,
    segments_to_cq_string, segments_to_plain_summary, stable_onebot_id, AdapterError, AdapterEvent,
    ChatMessage, ChatTargetKind, InputMode, MessageSegment, StoredMessageRecord,
};
use serde::{Deserialize, Deserializer};
use serde_json::{json, Map, Value};

use crate::constants::{
    CQ_SEGMENT_TEXT, DEFAULT_ONEBOT_ID_BOUND, ONEBOT_MESSAGE_TYPE_GROUP,
    ONEBOT_MESSAGE_TYPE_PRIVATE, ONEBOT_POST_TYPE_MESSAGE, ONEBOT_POST_TYPE_MESSAGE_SENT,
    ONEBOT_STATUS_OK, ONEBOT_SUB_TYPE_FRIEND, ONEBOT_SUB_TYPE_NORMAL,
};
use crate::onebot::handlers::resolve_login_profile;
use crate::AppState;

fn deserialize_lenient_opt_bool<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<bool>, D::Error>
where
    D: Deserializer<'de>,
{
    let opt: Option<Value> = Option::deserialize(deserializer)?;
    Ok(match opt {
        None | Some(Value::Null) => None,
        Some(Value::Bool(flag)) => Some(flag),
        Some(Value::Number(num)) => Some(num.as_i64().is_some_and(|val| val != 0)),
        Some(Value::String(raw)) => {
            let trimmed = raw.trim();
            if trimmed.eq_ignore_ascii_case("true") || trimmed == "1" {
                Some(true)
            } else if trimmed.eq_ignore_ascii_case("false") || trimmed == "0" {
                Some(false)
            } else {
                return Err(serde::de::Error::custom(format!(
                    "invalid boolean value '{raw}'"
                )));
            }
        }
        Some(other) => {
            return Err(serde::de::Error::custom(format!(
                "invalid boolean value: {other}"
            )));
        }
    })
}

/// 宽松解析可选无符号 64 位整数参数（兼容 JSON 数字、`null` 及数字字符串）
pub(crate) fn deserialize_lenient_opt_u64<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<u64>, D::Error>
where
    D: Deserializer<'de>,
{
    let opt: Option<Value> = Option::deserialize(deserializer)?;
    match opt {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(num)) => num
            .as_u64()
            .map(Some)
            .ok_or_else(|| serde::de::Error::custom("invalid unsigned integer value")),
        Some(Value::String(raw)) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                return Ok(None);
            }
            trimmed
                .parse::<u64>()
                .map(Some)
                .map_err(|err| serde::de::Error::custom(format!("invalid u64 '{raw}': {err}")))
        }
        Some(other) => Err(serde::de::Error::custom(format!(
            "invalid unsigned integer value: {other}"
        ))),
    }
}

/// `/onebot/v11/send_group_msg`、`/onebot/v11/send_private_msg` 与 `/onebot/v11/send_msg` 共用发送载荷
#[derive(Debug, Default, Deserialize)]
pub struct OneBotSendPayload {
    #[serde(default)]
    pub group_name: Option<String>,
    #[serde(default)]
    pub group_id: Option<Value>,
    #[serde(default)]
    pub user_id: Option<Value>,
    #[serde(default)]
    pub nickname: Option<String>,
    #[serde(default)]
    pub message: Value,
    #[serde(default)]
    pub mode: Option<InputMode>,
    #[serde(default)]
    pub message_type: Option<String>,
    #[serde(default, deserialize_with = "deserialize_lenient_opt_bool")]
    pub auto_escape: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_lenient_opt_u64")]
    pub deadline_unix_ms: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_lenient_opt_u64")]
    pub response_window_ms: Option<u64>,
}

/// 结合绝对截止时间戳与相对响应窗口计算实际生效的出站发送截止毫秒时间戳
pub fn resolve_deadline_bounds_ms(
    deadline_unix_ms: Option<u64>,
    response_window_ms: Option<u64>,
    default_now_ms: u64,
) -> Option<u64> {
    let relative_deadline = response_window_ms.map(|window| default_now_ms.saturating_add(window));
    match (deadline_unix_ms, relative_deadline) {
        (Some(abs), Some(rel)) => Some(abs.min(rel)),
        (Some(abs), None) => Some(abs),
        (None, Some(rel)) => Some(rel),
        (None, None) => None,
    }
}

/// 从 OneBot 发送载荷解析实际生效的出站截止时间戳（同时给出绝对与相对窗口时取更严格者）
pub fn resolve_effective_deadline_ms(
    payload: &OneBotSendPayload,
    default_now_ms: u64,
) -> Option<u64> {
    resolve_deadline_bounds_ms(
        payload.deadline_unix_ms,
        payload.response_window_ms,
        default_now_ms,
    )
}

/// WebSocket 双向通信通道接收的 OneBot v11 动作调用帧
#[derive(Debug, Deserialize)]
pub struct OneBotWsAction {
    pub action: String,
    #[serde(default)]
    pub params: Value,
    #[serde(default)]
    pub echo: Option<Value>,
}

/// 构造标准 OneBot v11 成功应答 JSON 外壳
pub fn ok_envelope(data: Value) -> Json<Value> {
    Json(json!({
        "status": ONEBOT_STATUS_OK,
        "retcode": 0,
        "data": data,
    }))
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn decode_percent_bytes(raw: &str, plus_as_space: bool) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut idx = 0;
    while idx < bytes.len() {
        match bytes[idx] {
            b'+' if plus_as_space => {
                out.push(b' ');
                idx += 1;
            }
            // 按原始字节读取十六进制数字，避免在多字节 UTF-8 字符边界做字符串切片引发 panic
            b'%' if idx + 2 < bytes.len() => {
                if let (Some(hi), Some(lo)) =
                    (hex_nibble(bytes[idx + 1]), hex_nibble(bytes[idx + 2]))
                {
                    out.push((hi << 4) | lo);
                    idx += 3;
                    continue;
                }
                out.push(bytes[idx]);
                idx += 1;
            }
            byte => {
                out.push(byte);
                idx += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 解码 RFC 3986 URL 查询片段的 `%XX` 百分号序列（保留原始 `+` 号字面值）
pub(crate) fn decode_url_query_component(raw: &str) -> String {
    decode_percent_bytes(raw, false)
}

fn decode_form_component(raw: &str) -> String {
    decode_percent_bytes(raw, true)
}

fn parse_form_urlencoded_map(raw: &str) -> Option<Map<String, Value>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.starts_with('{') || trimmed.starts_with('[') {
        return None;
    }
    let mut map = Map::new();
    for pair in trimmed.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (raw_k, raw_v) = pair.split_once('=').unwrap_or((pair, ""));
        let key = decode_form_component(raw_k);
        if key.is_empty() {
            continue;
        }
        let value = decode_form_component(raw_v);
        map.insert(key, Value::String(value));
    }
    Some(map)
}

fn merge_query_into_map(obj: &mut Map<String, Value>, query: HashMap<String, String>) {
    for (key, value) in query {
        if key == "access_token" {
            continue;
        }
        obj.entry(key).or_insert(Value::String(value));
    }
}

/// 将 HTTP GET 查询参数与可选 POST JSON/Form 请求体合并为统一参数对象（自动过滤 `access_token`）
pub fn parse_onebot_http_params(
    query: HashMap<String, String>,
    body: Bytes,
) -> std::result::Result<Value, AdapterError> {
    let has_body = !body.iter().all(u8::is_ascii_whitespace);
    if !has_body {
        let mut map = Map::new();
        merge_query_into_map(&mut map, query);
        return Ok(Value::Object(map));
    }

    match serde_json::from_slice::<Value>(&body) {
        Ok(Value::Object(mut obj)) => {
            merge_query_into_map(&mut obj, query);
            Ok(Value::Object(obj))
        }
        Ok(_) => Err(AdapterError::InputRejected(
            "OneBot HTTP payload must be a JSON object".to_string(),
        )),
        Err(json_err) => {
            let text = std::str::from_utf8(&body).map_err(|_| {
                AdapterError::InputRejected(format!("invalid JSON payload: {json_err}"))
            })?;
            if let Some(mut form_map) = parse_form_urlencoded_map(text) {
                merge_query_into_map(&mut form_map, query);
                Ok(Value::Object(form_map))
            } else {
                Err(AdapterError::InputRejected(format!(
                    "invalid JSON payload: {json_err}"
                )))
            }
        }
    }
}

/// 从 JSON 值（支持数字与字符串两种形式）中宽松解析整数编号
pub fn parse_value_as_i64(
    value: &Value,
    field_name: &str,
) -> std::result::Result<Option<i64>, AdapterError> {
    match value {
        Value::Null => Ok(None),
        Value::Number(num) => num.as_i64().map(Some).ok_or_else(|| {
            AdapterError::InputRejected(format!("invalid integer for '{field_name}'"))
        }),
        Value::String(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                return Ok(None);
            }
            trimmed.parse::<i64>().map(Some).map_err(|err| {
                AdapterError::InputRejected(format!("invalid '{field_name}': {err}"))
            })
        }
        _ => Err(AdapterError::InputRejected(format!(
            "'{field_name}' must be an integer or numeric string"
        ))),
    }
}

/// 从参数对象中读取可选整数参数
pub fn parse_optional_i64(
    params: &Value,
    field_name: &str,
) -> std::result::Result<Option<i64>, AdapterError> {
    match params.get(field_name) {
        Some(value) => parse_value_as_i64(value, field_name),
        None => Ok(None),
    }
}

/// 从 JSON 值（支持数字与数字字符串）中宽松解析无符号 64 位整数
pub fn parse_value_as_u64(
    value: &Value,
    field_name: &str,
) -> std::result::Result<Option<u64>, AdapterError> {
    match value {
        Value::Null => Ok(None),
        Value::Number(num) => num.as_u64().map(Some).ok_or_else(|| {
            AdapterError::InputRejected(format!("invalid unsigned integer for '{field_name}'"))
        }),
        Value::String(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                return Ok(None);
            }
            trimmed.parse::<u64>().map(Some).map_err(|err| {
                AdapterError::InputRejected(format!("invalid '{field_name}': {err}"))
            })
        }
        _ => Err(AdapterError::InputRejected(format!(
            "'{field_name}' must be an unsigned integer or numeric string"
        ))),
    }
}

/// 从参数对象中读取可选无符号 64 位整数参数
pub fn parse_optional_u64(
    params: &Value,
    field_name: &str,
) -> std::result::Result<Option<u64>, AdapterError> {
    match params.get(field_name) {
        Some(value) => parse_value_as_u64(value, field_name),
        None => Ok(None),
    }
}

/// 从参数对象中读取可选布尔标志（兼容布尔值及 `"true"`/`"1"` 查询字符串）
pub fn parse_optional_bool(params: &Value, field_name: &str) -> bool {
    match params.get(field_name) {
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Number(num)) => num.as_i64().is_some_and(|val| val != 0),
        Some(Value::String(raw)) => {
            let trimmed = raw.trim();
            trimmed.eq_ignore_ascii_case("true") || trimmed == "1"
        }
        _ => false,
    }
}

/// 从参数对象中读取非空字符串参数
pub fn parse_optional_string(params: &Value, field_name: &str) -> Option<String> {
    params
        .get(field_name)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

/// 将单条领域消息段经由 `serde` 自动编码为标准 OneBot v11 JSON 段对象
fn segment_to_onebot_json(segment: &MessageSegment) -> Value {
    serde_json::to_value(segment).unwrap_or_else(|_| {
        json!({
            "type": CQ_SEGMENT_TEXT,
            "data": { "text": "" }
        })
    })
}

/// 将领域消息段列表转换为 OneBot v11 消息段数组，无结构化段时退化为单个纯文本段
pub fn segments_to_onebot_json(segments: &[MessageSegment], fallback_text: &str) -> Value {
    if segments.is_empty() {
        return json!([
            {
                "type": CQ_SEGMENT_TEXT,
                "data": { "text": fallback_text },
            }
        ]);
    }
    Value::Array(segments.iter().map(segment_to_onebot_json).collect())
}

/// 解析 OneBot 字符串 CQ 码或段数组，并校验存在可投递内容（文本、@ 或图片）
pub fn extract_onebot_segments(
    message: &Value,
    auto_escape: bool,
) -> std::result::Result<Vec<MessageSegment>, AdapterError> {
    let segments = parse_onebot_message_value(message, auto_escape)?;
    if !has_sendable_content(&segments) {
        return Err(AdapterError::InputRejected(
            "OneBot message content is empty".to_string(),
        ));
    }
    Ok(segments)
}

/// 将纯文本或 OneBot 消息段数组规范化提取为可发送的纯文本摘要
pub fn extract_onebot_message_text(message: &Value) -> std::result::Result<String, AdapterError> {
    let segments = extract_onebot_segments(message, false)?;
    let summary = segments_to_plain_summary(&segments);
    if summary.trim().is_empty() {
        return Err(AdapterError::InputRejected(
            "OneBot message text is empty".to_string(),
        ));
    }
    Ok(summary)
}

/// 剥离末尾成员计数后缀后精确比较当前激活群名与请求群名是否指向同一群聊
pub fn group_name_matches(active: &str, requested: &str) -> bool {
    canonical_group_names_match(active, requested)
}

/// 构造标准 OneBot v11 私聊发送者详情对象
fn build_private_sender_json(user_id: i64, nickname: &str) -> Value {
    json!({
        "user_id": user_id,
        "nickname": nickname,
        "sex": "unknown",
        "age": 0,
    })
}

/// 构造标准 OneBot v11 群聊发送者详情对象（含名片与群角色兜底）
fn build_group_sender_json(
    user_id: i64,
    nickname: &str,
    sender_card: Option<&str>,
    sender_role: Option<&str>,
) -> Value {
    let card = sender_card
        .map(str::trim)
        .filter(|val| !val.is_empty())
        .unwrap_or(nickname);
    let role = sender_role
        .map(str::trim)
        .filter(|val| !val.is_empty())
        .unwrap_or("member");
    json!({
        "user_id": user_id,
        "nickname": nickname,
        "card": card,
        "role": role,
        "sex": "unknown",
        "age": 0,
        "area": "",
        "level": "",
        "title": "",
    })
}

/// 将领域消息 `ChatMessage` 转换为标准 OneBot v11 群聊或好友私聊消息事件对象
pub fn chat_message_to_onebot_event(self_id: i64, message: &ChatMessage) -> Value {
    let event_time = if message.captured_at_ms > 0 {
        (message.captured_at_ms / 1000) as i64
    } else {
        Utc::now().timestamp()
    };
    // 优先使用持久化分配的真实自增或映射标识，缺失时按 FNV-1a 回退生成确定性正整数
    let message_id = message
        .onebot_message_id
        .unwrap_or_else(|| stable_onebot_id(&message.message_id, i64::from(i32::MAX)));
    let user_id = message
        .user_id
        .unwrap_or_else(|| stable_onebot_id(&message.sender, DEFAULT_ONEBOT_ID_BOUND));
    let raw_message = if message.segments.is_empty() {
        message.content.clone()
    } else {
        segments_to_cq_string(&message.segments)
    };
    let message_array = segments_to_onebot_json(&message.segments, &message.content);

    let mut payload = match message.target_kind {
        ChatTargetKind::Private => json!({
            "time": event_time,
            "self_id": self_id,
            "post_type": ONEBOT_POST_TYPE_MESSAGE,
            "message_type": ONEBOT_MESSAGE_TYPE_PRIVATE,
            "sub_type": ONEBOT_SUB_TYPE_FRIEND,
            "message_id": message_id,
            "message_seq": message_id,
            "real_id": message_id,
            "font": 0,
            "user_id": user_id,
            "sender": build_private_sender_json(user_id, &message.sender),
            "raw_message": raw_message,
            "message": message_array,
        }),
        ChatTargetKind::Group => {
            let group_id = message
                .group_id
                .unwrap_or_else(|| stable_onebot_id(&message.group_name, DEFAULT_ONEBOT_ID_BOUND));
            json!({
                "time": event_time,
                "self_id": self_id,
                "post_type": ONEBOT_POST_TYPE_MESSAGE,
                "message_type": ONEBOT_MESSAGE_TYPE_GROUP,
                "sub_type": ONEBOT_SUB_TYPE_NORMAL,
                "message_id": message_id,
                "message_seq": message_id,
                "real_id": message_id,
                "font": 0,
                "anonymous": Value::Null,
                "group_id": group_id,
                "group_name": message.group_name,
                "user_id": user_id,
                "sender": build_group_sender_json(
                    user_id,
                    &message.sender,
                    message.sender_card.as_deref(),
                    message.sender_role.as_deref(),
                ),
                "raw_message": raw_message,
                "message": message_array,
            })
        }
    };
    if let Some(obj) = payload.as_object_mut() {
        if message.captured_at_ms > 0 {
            obj.insert("captured_at_ms".to_string(), json!(message.captured_at_ms));
        }
        if let Some(deadline_ms) = message.response_deadline_ms {
            obj.insert("response_deadline_ms".to_string(), json!(deadline_ms));
        }
        if let Some(tier) = message.priority_tier {
            obj.insert("priority_tier".to_string(), json!(tier));
        }
    }
    payload
}

/// 将 SQLite 持久化消息记录统一序列化为标准 OneBot 消息响应结构（供 `get_msg` 与历史消息共用）
pub fn stored_record_to_onebot_msg(record: &StoredMessageRecord) -> Value {
    let time_secs = if record.captured_at_ms > 0 {
        (record.captured_at_ms / 1000) as i64
    } else {
        Utc::now().timestamp()
    };
    let message_array = segments_to_onebot_json(&record.segments, &record.raw_message);
    let mut response = json!({
        "time": time_secs,
        "message_type": record.chat_type.as_str(),
        "message_id": record.onebot_message_id,
        "message_seq": record.onebot_message_id,
        "real_id": record.onebot_message_id,
        "font": 0,
        "message": message_array,
        "raw_message": record.raw_message,
    });
    if let Some(obj) = response.as_object_mut() {
        if record.captured_at_ms > 0 {
            obj.insert("captured_at_ms".to_string(), json!(record.captured_at_ms));
        }
        match record.chat_type {
            ChatTargetKind::Private => {
                obj.insert(
                    "sub_type".to_string(),
                    Value::String(ONEBOT_SUB_TYPE_FRIEND.to_string()),
                );
                obj.insert("user_id".to_string(), json!(record.user_id));
                obj.insert(
                    "sender".to_string(),
                    build_private_sender_json(record.user_id, &record.sender_name),
                );
            }
            ChatTargetKind::Group => {
                obj.insert(
                    "sub_type".to_string(),
                    Value::String(ONEBOT_SUB_TYPE_NORMAL.to_string()),
                );
                obj.insert("anonymous".to_string(), Value::Null);
                obj.insert("group_id".to_string(), json!(record.group_id));
                obj.insert("group_name".to_string(), json!(record.group_name));
                obj.insert("user_id".to_string(), json!(record.user_id));
                obj.insert(
                    "sender".to_string(),
                    build_group_sender_json(record.user_id, &record.sender_name, None, None),
                );
            }
        }
    }
    response
}

/// 将内部 `AdapterEvent` 转换为对外 OneBot v11 事件 JSON（支持可选 `message_sent`、`notice`、`request`）
pub fn translate_adapter_event_to_onebot(
    self_id: i64,
    post_message_sent: bool,
    event: &AdapterEvent,
) -> Option<Value> {
    match event {
        AdapterEvent::GroupMessage(message) | AdapterEvent::PrivateMessage(message) => {
            if message.is_self {
                return None;
            }
            Some(chat_message_to_onebot_event(self_id, message))
        }
        AdapterEvent::MessageSent(outcome) => {
            if !post_message_sent || !outcome.ok || outcome.onebot_message_id.is_none() {
                return None;
            }
            let message_id = outcome.onebot_message_id?;
            let msg_arr = segments_to_onebot_json(&[], &outcome.detail);
            let now = Utc::now().timestamp();
            let mut obj = json!({
                "time": now,
                "self_id": self_id,
                "post_type": ONEBOT_POST_TYPE_MESSAGE_SENT,
                "message_type": outcome.target_kind.as_str(),
                "message_id": message_id,
                "message_seq": message_id,
                "real_id": message_id,
                "font": 0,
                "user_id": outcome.user_id.unwrap_or(self_id),
                "raw_message": outcome.detail,
                "message": msg_arr,
            });
            if let Some(map) = obj.as_object_mut() {
                match outcome.target_kind {
                    ChatTargetKind::Group => {
                        map.insert(
                            "sub_type".to_string(),
                            Value::String(ONEBOT_SUB_TYPE_NORMAL.to_string()),
                        );
                        map.insert("anonymous".to_string(), Value::Null);
                        if let Some(gid) = outcome.group_id {
                            map.insert("group_id".to_string(), json!(gid));
                        }
                        map.insert("group_name".to_string(), json!(outcome.group));
                        map.insert(
                            "sender".to_string(),
                            build_group_sender_json(self_id, "", None, None),
                        );
                    }
                    ChatTargetKind::Private => {
                        map.insert(
                            "sub_type".to_string(),
                            Value::String(ONEBOT_SUB_TYPE_FRIEND.to_string()),
                        );
                        map.insert("sender".to_string(), build_private_sender_json(self_id, ""));
                    }
                }
            }
            Some(obj)
        }
        AdapterEvent::Notice(raw) | AdapterEvent::Request(raw) => {
            let mut cloned = raw.clone();
            if let Some(map) = cloned.as_object_mut() {
                map.entry("time".to_string())
                    .or_insert_with(|| json!(Utc::now().timestamp()));
                let should_fill_self = map
                    .get("self_id")
                    .and_then(Value::as_i64)
                    .is_none_or(|id| id <= 0);
                if should_fill_self {
                    map.insert("self_id".to_string(), json!(self_id));
                }
            }
            Some(cloned)
        }
        AdapterEvent::SecurityAlert { .. } | AdapterEvent::StatusChanged(_) => None,
    }
}

/// 异步补全 OneBot v11 事件载荷：在 `message_sent` 场景下从存储回填真实正文与段数组并写入机器人昵称
pub(crate) async fn translate_adapter_event_enriched(
    state: &AppState,
    self_id: i64,
    event: &AdapterEvent,
) -> Option<Value> {
    let mut payload =
        translate_adapter_event_to_onebot(self_id, state.config.onebot.post_message_sent, event)?;
    let AdapterEvent::MessageSent(outcome) = event else {
        return Some(payload);
    };
    let Some(msg_id) = outcome.onebot_message_id else {
        return Some(payload);
    };

    let record_opt = state.driver.get_message(msg_id).await.ok().flatten();
    let bot_nickname = resolve_login_profile(state)
        .await
        .ok()
        .map(|info| info.nickname.trim().to_string())
        .filter(|name| !name.is_empty());

    if let Some(map) = payload.as_object_mut() {
        if let Some(ref record) = record_opt {
            map.insert(
                "raw_message".to_string(),
                Value::String(record.raw_message.clone()),
            );
            map.insert(
                "message".to_string(),
                segments_to_onebot_json(&record.segments, &record.raw_message),
            );
        }
        let effective_nick = bot_nickname.or_else(|| {
            record_opt
                .as_ref()
                .map(|rec| rec.sender_name.trim().to_string())
                .filter(|name| !name.is_empty())
        });
        if let Some(nick) = effective_nick {
            if let Some(sender) = map.get_mut("sender").and_then(Value::as_object_mut) {
                sender.insert("nickname".to_string(), Value::String(nick.clone()));
                if outcome.target_kind == ChatTargetKind::Group {
                    sender.insert("card".to_string(), Value::String(nick));
                }
            }
        }
    }
    Some(payload)
}
