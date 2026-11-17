//! OneBot v11 扩展标准动作实现模块：覆盖历史消息、陌生人、图片缓存、合并转发、清缓存与热重启

use std::path::{Path, PathBuf};

use common_types::{
    segments_to_plain_summary, AdapterError, ChatTargetKind, ForwardSendRequest,
    MessageHistoryFilter, MessageSegment, StoredMessageRecord,
};
use serde_json::{json, Value};
use tokio::fs;

use crate::constants::{
    ACTION_SEND_GROUP_FORWARD_MSG, ACTION_SEND_PRIVATE_FORWARD_MSG, MAX_FORWARD_NODES,
    MEDIA_CACHE_SUBDIR, ONEBOT_MESSAGE_TYPE_GROUP, ONEBOT_MESSAGE_TYPE_PRIVATE,
    RESTART_SPAWN_DELAY_MS, STAGING_UPLOADS_SUBDIR,
};
use crate::onebot::handlers::{
    exec_send_group_with_payload, exec_send_private_with_payload, resolve_login_profile,
    resolve_send_target_group, resolve_send_target_private,
};
use crate::onebot::payloads::{
    parse_optional_bool, parse_optional_i64, parse_optional_string, stored_record_to_onebot_msg,
    OneBotSendPayload,
};
use crate::rest::restart_ops::schedule_daemon_restart;
use crate::AppState;

const FORWARD_PREVIEW_MAX_LINES: usize = 3;
const FORWARD_PREVIEW_MAX_CHARS: usize = 160;
const HISTORY_DEFAULT_COUNT: i64 = 20;
const HISTORY_MAX_COUNT: i64 = 200;

async fn query_onebot_msg_history(
    state: &AppState,
    session_name: String,
    chat_type: ChatTargetKind,
    params: &Value,
) -> std::result::Result<Value, AdapterError> {
    let count = parse_optional_i64(params, "count")?
        .unwrap_or(HISTORY_DEFAULT_COUNT)
        .clamp(1, HISTORY_MAX_COUNT) as usize;
    // SDK 默认常传入 message_seq: 0，所有非正数游标均视作未指定以拉取最新一页
    let before_or_at_id = parse_optional_i64(params, "message_seq")?
        .filter(|&seq| seq > 0)
        .or(parse_optional_i64(params, "message_id")?.filter(|&id| id > 0));
    let reverse_order = if params.get("reverseOrder").is_some_and(|v| !v.is_null()) {
        parse_optional_bool(params, "reverseOrder")
    } else {
        parse_optional_bool(params, "reverse_order")
    };

    let records = state
        .driver
        .query_messages_history(MessageHistoryFilter {
            session_name,
            chat_type: Some(chat_type),
            before_or_at_id,
            after_id: None,
            sender: None,
            keyword: None,
            limit: count,
            reverse_order,
        })
        .await?;
    let messages: Vec<Value> = records.iter().map(stored_record_to_onebot_msg).collect();
    Ok(json!({ "messages": messages }))
}

pub(crate) fn format_display_path(canon: &Path) -> String {
    let raw = canon.to_string_lossy();
    raw.strip_prefix(r"\\?\").unwrap_or(&raw).replace('\\', "/")
}

pub(crate) fn strip_file_url_prefix(raw: &str) -> &str {
    // 使用不区分大小写的安全切片兼容 FILE:/// 与 file:// 前缀
    if let Some(prefix) = raw.get(..8) {
        if prefix.eq_ignore_ascii_case("file:///") {
            let rest = &raw[8..];
            let bytes = rest.as_bytes();
            if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
                return rest;
            }
            return raw.get(7..).unwrap_or(rest);
        }
    }
    if let Some(prefix) = raw.get(..7) {
        if prefix.eq_ignore_ascii_case("file://") {
            return &raw[7..];
        }
    }
    raw
}

/// `get_stranger_info` -> 查询好友/群成员缓存或返回兜底陌生人资料
pub(crate) async fn exec_get_stranger_info(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, AdapterError> {
    let user_id = parse_optional_i64(&params, "user_id")?
        .ok_or_else(|| AdapterError::InputRejected("user_id is required".to_string()))?;

    if let Ok(login) = resolve_login_profile(state).await {
        if login.user_id == user_id {
            return Ok(json!({
                "user_id": login.user_id,
                "nickname": login.nickname,
                "sex": "unknown",
                "age": 0,
                "remark": "",
            }));
        }
    }

    if let Some(info) = state.driver.get_stranger_info(user_id).await? {
        return Ok(json!({
            "user_id": info.user_id,
            "real_user_id": info.real_user_id,
            "nickname": info.nickname,
            "remark": info.remark,
            "sex": "unknown",
            "age": 0,
        }));
    }

    Ok(json!({
        "user_id": user_id,
        "nickname": "",
        "sex": "unknown",
        "age": 0,
        "remark": "",
    }))
}

/// `get_image` -> 仅在 `.runtime/media` 受限缓存目录内解析并返回图片元数据与访问路径
pub(crate) async fn exec_get_image(
    _state: &AppState,
    params: Value,
) -> std::result::Result<Value, AdapterError> {
    let raw_file = parse_optional_string(&params, "file")
        .ok_or_else(|| AdapterError::InputRejected("file is required".to_string()))?;
    let stripped = strip_file_url_prefix(&raw_file).trim();
    if stripped.is_empty() || stripped.contains("..") {
        return Err(AdapterError::InputRejected(format!(
            "invalid media file reference '{raw_file}'"
        )));
    }

    let media_root = fs::canonicalize(Path::new(MEDIA_CACHE_SUBDIR))
        .await
        .map_err(|_| {
            AdapterError::InputRejected(format!("cached image not found for file='{raw_file}'"))
        })?;

    let candidate: PathBuf = if !stripped.contains('/') && !stripped.contains('\\') {
        media_root.join(stripped)
    } else {
        PathBuf::from(stripped)
    };
    let canon_file = fs::canonicalize(&candidate).await.map_err(|_| {
        AdapterError::InputRejected(format!("cached image not found for file='{raw_file}'"))
    })?;
    if !canon_file.starts_with(&media_root) {
        return Err(AdapterError::InputRejected(
            "access outside media cache directory is forbidden".to_string(),
        ));
    }

    let meta = fs::metadata(&canon_file).await.map_err(|err| {
        AdapterError::InputRejected(format!("failed to read cached image metadata: {err}"))
    })?;
    if !meta.is_file() {
        return Err(AdapterError::InputRejected(
            "target media entry is not a regular file".to_string(),
        ));
    }

    let display = format_display_path(&canon_file);
    let url = if display.starts_with('/') {
        format!("file://{display}")
    } else {
        format!("file:///{display}")
    };
    let filename = canon_file
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();

    Ok(json!({
        "size": meta.len(),
        "filename": filename,
        "url": url,
        "file": display,
    }))
}

/// `get_group_msg_history` -> 按群名或群号查询 SQLite 持久化历史消息（支持 `message_seq`、`count` 与 `reverseOrder`）
pub(crate) async fn exec_get_group_msg_history(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, AdapterError> {
    let payload = OneBotSendPayload {
        group_name: parse_optional_string(&params, "group_name"),
        group_id: params.get("group_id").cloned(),
        ..Default::default()
    };
    let target_group = resolve_send_target_group(state, &payload).await?;
    query_onebot_msg_history(state, target_group, ChatTargetKind::Group, &params).await
}

/// `get_friend_msg_history` -> 严格校验私聊白名单后返回指定好友历史消息记录
pub(crate) async fn exec_get_friend_msg_history(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, AdapterError> {
    let payload = OneBotSendPayload {
        user_id: params.get("user_id").cloned(),
        nickname: parse_optional_string(&params, "nickname"),
        group_name: parse_optional_string(&params, "group_name"),
        ..Default::default()
    };
    let target_friend = resolve_send_target_private(state, &payload).await?;
    query_onebot_msg_history(state, target_friend, ChatTargetKind::Private, &params).await
}

fn summarize_forward_node_line(node: &Value) -> String {
    let data = node.get("data").unwrap_or(node);
    let sender = data
        .get("nickname")
        .or_else(|| data.get("name"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or("成员");

    let body_raw = data.get("content").unwrap_or(&Value::Null);
    let body_text = match body_raw {
        Value::String(s) => s.trim().to_string(),
        Value::Array(_) => common_types::parse_onebot_message_value(body_raw, false)
            .map(|segs| segments_to_plain_summary(&segs))
            .unwrap_or_default(),
        Value::Object(_) => common_types::parse_onebot_message_value(body_raw, false)
            .map(|segs| segments_to_plain_summary(&segs))
            .unwrap_or_default(),
        _ => String::new(),
    };

    if body_text.is_empty() {
        format!("{sender}: [转发节点]")
    } else {
        format!("{sender}: {body_text}")
    }
}

fn build_forward_summary_segments(forward_id: &str, nodes: &[Value]) -> Value {
    let mut lines = Vec::new();
    for node in nodes.iter().take(FORWARD_PREVIEW_MAX_LINES) {
        lines.push(summarize_forward_node_line(node));
    }
    if nodes.len() > FORWARD_PREVIEW_MAX_LINES {
        lines.push(format!("...共{}条消息", nodes.len()));
    }
    let mut summary = format!("[聊天记录]\n{}", lines.join("\n"));
    if summary.chars().count() > FORWARD_PREVIEW_MAX_CHARS {
        summary = summary
            .chars()
            .take(FORWARD_PREVIEW_MAX_CHARS)
            .collect::<String>()
            + "...";
    }
    let segs = vec![
        MessageSegment::Text { text: summary },
        MessageSegment::Forward {
            id: forward_id.to_string(),
        },
    ];
    serde_json::to_value(segs).unwrap_or_else(|_| json!([]))
}

fn validate_forward_nodes_array(params: &Value) -> std::result::Result<&Vec<Value>, AdapterError> {
    let raw_nodes = params
        .get("messages")
        .or_else(|| params.get("message"))
        .ok_or_else(|| {
            AdapterError::InputRejected("forward messages array is required".to_string())
        })?;
    let nodes = raw_nodes.as_array().ok_or_else(|| {
        AdapterError::InputRejected("'messages' must be a non-empty JSON array".to_string())
    })?;
    if nodes.is_empty() {
        return Err(AdapterError::InputRejected(
            "forward 'messages' array cannot be empty".to_string(),
        ));
    }
    if nodes.len() > MAX_FORWARD_NODES {
        return Err(AdapterError::InputRejected(format!(
            "forward 'messages' count {} exceeds safe limit {MAX_FORWARD_NODES}",
            nodes.len()
        )));
    }
    Ok(nodes)
}

fn extract_node_referenced_id(node: &Value) -> Option<i64> {
    let data = node.get("data").unwrap_or(node);
    if data.get("content").is_some_and(|v| !v.is_null()) {
        return None;
    }
    let id_val = data.get("id")?;
    let parsed = match id_val {
        Value::Number(num) => num.as_i64(),
        Value::String(raw) => raw.trim().parse::<i64>().ok(),
        _ => None,
    }?;
    (parsed > 0).then_some(parsed)
}

fn extract_referenced_message_ids(nodes: &[Value]) -> Option<Vec<i64>> {
    nodes.iter().map(extract_node_referenced_id).collect()
}

async fn prepare_forward_payload(
    state: &AppState,
    nodes: &[Value],
) -> std::result::Result<(String, Value), AdapterError> {
    let serialized = serde_json::to_string(nodes).map_err(|err| {
        AdapterError::InputRejected(format!("failed to serialize forward nodes: {err}"))
    })?;
    let forward_id = state.driver.allocate_forward_nodes(&serialized).await?;
    let message_value = build_forward_summary_segments(&forward_id, nodes);
    Ok((forward_id, message_value))
}

fn attach_forward_id(mut result: Value, forward_id: String) -> Value {
    if let Some(obj) = result.as_object_mut() {
        obj.insert("forward_id".to_string(), Value::String(forward_id));
    }
    result
}

async fn exec_templated_forward_msg(
    state: &AppState,
    params: Value,
    target_kind: ChatTargetKind,
) -> std::result::Result<Value, AdapterError> {
    let is_private = target_kind == ChatTargetKind::Private;
    let target_payload = if is_private {
        OneBotSendPayload {
            user_id: params.get("user_id").cloned(),
            nickname: parse_optional_string(&params, "nickname"),
            group_name: parse_optional_string(&params, "group_name"),
            mode: serde_json::from_value(params.get("mode").cloned().unwrap_or(Value::Null))
                .unwrap_or(None),
            ..Default::default()
        }
    } else {
        OneBotSendPayload {
            group_name: parse_optional_string(&params, "group_name"),
            group_id: params.get("group_id").cloned(),
            mode: serde_json::from_value(params.get("mode").cloned().unwrap_or(Value::Null))
                .unwrap_or(None),
            ..Default::default()
        }
    };
    let target = if is_private {
        resolve_send_target_private(state, &target_payload).await?
    } else {
        resolve_send_target_group(state, &target_payload).await?
    };

    let nodes = validate_forward_nodes_array(&params)?;
    if !state.config.onebot.allow_degraded_media_summary {
        let Some(message_ids) = extract_referenced_message_ids(nodes) else {
            let action = if is_private {
                ACTION_SEND_PRIVATE_FORWARD_MSG
            } else {
                ACTION_SEND_GROUP_FORWARD_MSG
            };
            return Err(AdapterError::UnsupportedAction {
                action: action.to_string(),
                reason: "QQNT 9.9.31 desktop UIA cannot forge custom fake forward nodes with arbitrary sender nicknames; supply real referenced message IDs in strict mode or set [onebot].allow_degraded_media_summary=true for degraded plain-text summary".to_string(),
            });
        };
        let _send_guard = state.send_gate.lock().await;
        let outcome = state
            .driver
            .send_forward_messages(ForwardSendRequest {
                target,
                target_kind,
                message_ids,
            })
            .await?;
        return Ok(if is_private {
            json!({
                "message_id": outcome.message_id,
                "forward_id": outcome.forward_id,
                "user_id": outcome.user_id,
            })
        } else {
            json!({
                "message_id": outcome.message_id,
                "forward_id": outcome.forward_id,
                "group_id": outcome.group_id,
            })
        });
    }

    let (forward_id, message) = prepare_forward_payload(state, nodes).await?;
    let payload = OneBotSendPayload {
        message,
        ..target_payload
    };
    let result = if is_private {
        exec_send_private_with_payload(state, payload).await?
    } else {
        exec_send_group_with_payload(state, payload).await?
    };
    Ok(attach_forward_id(result, forward_id))
}

/// `send_group_forward_msg` -> 先完成群目标校验，再按引用节点走原生多选合并转发或降级持久化摘要
pub(crate) async fn exec_send_group_forward_msg(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, AdapterError> {
    exec_templated_forward_msg(state, params, ChatTargetKind::Group).await
}

/// `send_private_forward_msg` -> 先校验私聊白名单与好友存在性，再执行引用节点原生合并转发或降级摘要
pub(crate) async fn exec_send_private_forward_msg(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, AdapterError> {
    exec_templated_forward_msg(state, params, ChatTargetKind::Private).await
}

/// `send_forward_msg` -> 根据 `message_type` 或目标字段自动分流至群/私聊合并转发
pub(crate) async fn exec_send_forward_msg(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, AdapterError> {
    let msg_type = parse_optional_string(&params, "message_type");
    let is_private = match msg_type.as_deref() {
        Some(ONEBOT_MESSAGE_TYPE_PRIVATE) => true,
        Some(ONEBOT_MESSAGE_TYPE_GROUP) => false,
        Some(other) => {
            return Err(AdapterError::InputRejected(format!(
                "unsupported message_type '{other}'; expected '{ONEBOT_MESSAGE_TYPE_GROUP}' or '{ONEBOT_MESSAGE_TYPE_PRIVATE}'"
            )));
        }
        None => {
            params.get("group_id").is_none()
                && parse_optional_string(&params, "group_name").is_none()
                && (params.get("user_id").is_some()
                    || parse_optional_string(&params, "nickname").is_some())
        }
    };
    if is_private {
        exec_send_private_forward_msg(state, params).await
    } else {
        exec_send_group_forward_msg(state, params).await
    }
}

/// 尝试从消息记录的段结构或原始文本中提取合并转发节点标识（如 `fwd-1`）
pub fn extract_forward_id_from_record(record: &StoredMessageRecord) -> Option<String> {
    for segment in &record.segments {
        if let MessageSegment::Forward { id } = segment {
            let trimmed = id.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    if let Some(after) = record.raw_message.split("[合并转发:").nth(1) {
        if let Some((candidate, _)) = after.split_once(']') {
            let trimmed = candidate.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    if let Some(after) = record.raw_message.split("[CQ:forward,id=").nth(1) {
        let candidate = after.split([',', ']']).next().unwrap_or("").trim();
        if !candidate.is_empty() {
            return Some(candidate.to_string());
        }
    }
    None
}

/// `get_forward_msg` -> 按 `fwd-*` 字符串或数字 `message_id` 回查本地存储的合并转发节点树
pub(crate) async fn exec_get_forward_msg(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, AdapterError> {
    let raw_id = parse_optional_string(&params, "id")
        .or_else(|| {
            params
                .get("id")
                .and_then(Value::as_i64)
                .map(|id| id.to_string())
        })
        .or_else(|| parse_optional_string(&params, "message_id"))
        .or_else(|| {
            params
                .get("message_id")
                .and_then(Value::as_i64)
                .map(|id| id.to_string())
        })
        .ok_or_else(|| AdapterError::InputRejected("id is required".to_string()))?;

    let mut raw_json_opt = state.driver.get_forward_nodes(&raw_id).await?;
    if raw_json_opt.is_none() {
        if let Ok(msg_id) = raw_id.trim().parse::<i64>() {
            if let Some(record) = state.driver.get_message(msg_id).await? {
                if record.chat_type == ChatTargetKind::Private
                    && !state
                        .config
                        .target
                        .is_private_friend_allowed(&record.group_name)
                {
                    return Err(AdapterError::PrivateTargetNotAllowed {
                        target: record.group_name,
                    });
                }
                if let Some(fwd_id) = extract_forward_id_from_record(&record) {
                    raw_json_opt = state.driver.get_forward_nodes(&fwd_id).await?;
                }
            }
        }
    }

    let raw_json = raw_json_opt.ok_or_else(|| {
        AdapterError::InputRejected(format!("forward messages not found for id={raw_id}"))
    })?;
    let parsed: Value = serde_json::from_str(&raw_json).map_err(|err| {
        AdapterError::Io(std::io::Error::other(format!(
            "corrupted stored forward nodes: {err}"
        )))
    })?;
    Ok(json!({ "messages": parsed }))
}

async fn clean_directory_files(dir: &Path, recursive: bool) -> (u64, u64) {
    let mut removed: u64 = 0;
    let mut freed_bytes: u64 = 0;
    let mut dirs_to_visit: Vec<PathBuf> = vec![dir.to_path_buf()];
    let mut subdirs_to_remove: Vec<PathBuf> = Vec::new();

    while let Some(current_dir) = dirs_to_visit.pop() {
        let Ok(mut entries) = fs::read_dir(&current_dir).await else {
            continue;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let Ok(file_type) = entry.file_type().await else {
                continue;
            };
            let entry_path = entry.path();
            if file_type.is_file() {
                let size = entry.metadata().await.map(|m| m.len()).unwrap_or(0);
                if fs::remove_file(&entry_path).await.is_ok() {
                    removed += 1;
                    freed_bytes = freed_bytes.saturating_add(size);
                }
            } else if recursive && file_type.is_dir() {
                dirs_to_visit.push(entry_path.clone());
                subdirs_to_remove.push(entry_path);
            }
        }
    }

    // 自底向上移除空哈希子目录，保留顶层 `.runtime/staging_uploads` 根目录本身
    for subdir in subdirs_to_remove.into_iter().rev() {
        let _ = fs::remove_dir(&subdir).await;
    }
    (removed, freed_bytes)
}

/// 清理 `.runtime/media` 常规文件与 `.runtime/staging_uploads` 暂存文件及空子目录（供本地 CLI 复用）
pub async fn clean_runtime_cache_dirs() -> Value {
    let (media_removed, media_bytes) =
        clean_directory_files(Path::new(MEDIA_CACHE_SUBDIR), false).await;
    let (staging_removed, staging_bytes) =
        clean_directory_files(Path::new(STAGING_UPLOADS_SUBDIR), true).await;
    let removed_files = media_removed.saturating_add(staging_removed);
    let freed_bytes = media_bytes.saturating_add(staging_bytes);

    json!({
        "cleaned": true,
        "removed_files": removed_files,
        "freed_bytes": freed_bytes,
        "media_removed": media_removed,
        "staging_removed": staging_removed,
    })
}

/// 在 `send_gate` 串行保护下清理 `.runtime/media` 与 `.runtime/staging_uploads` 本地运行缓存
pub(crate) async fn clean_runtime_caches(
    state: &AppState,
) -> std::result::Result<Value, AdapterError> {
    let _send_guard = state.send_gate.lock().await;
    Ok(clean_runtime_cache_dirs().await)
}

/// `clean_cache` -> 安全清理 `.runtime/media` 与 `.runtime/staging_uploads` 下的本地缓存文件
pub(crate) async fn exec_clean_cache(
    state: &AppState,
    _params: Value,
) -> std::result::Result<Value, AdapterError> {
    clean_runtime_caches(state).await
}

/// `set_restart` -> 调度纯 Rust 热重启任务（尊重停机取消信号，防止测试误触发）
pub(crate) async fn exec_set_restart(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, AdapterError> {
    let delay_ms = parse_optional_i64(&params, "delay")?
        .unwrap_or(RESTART_SPAWN_DELAY_MS as i64)
        .clamp(50, 60_000) as u64;
    let scheduled = schedule_daemon_restart(state, delay_ms)?;
    Ok(json!({
        "ok": true,
        "restarting": true,
        "delay_ms": delay_ms,
        "already_scheduled": !scheduled,
    }))
}

/// `mark_msg_as_read` -> 根据消息 ID 定位所在会话并调用驱动执行会话已读定位
pub(crate) async fn exec_mark_msg_as_read(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, AdapterError> {
    let message_id = parse_optional_i64(&params, "message_id")?
        .ok_or_else(|| AdapterError::InputRejected("message_id is required".to_string()))?;
    if message_id <= 0 {
        return Err(AdapterError::InputRejected(
            "message_id must be a positive integer".to_string(),
        ));
    }
    let record = state.driver.get_message(message_id).await?.ok_or_else(|| {
        AdapterError::InputRejected(format!("message not found for message_id={message_id}"))
    })?;
    if record.chat_type == ChatTargetKind::Private
        && !state
            .config
            .target
            .is_private_friend_allowed(&record.group_name)
    {
        return Err(AdapterError::PrivateTargetNotAllowed {
            target: record.group_name,
        });
    }
    let _send_guard = state.send_gate.lock().await;
    state
        .driver
        .mark_session_read(&record.group_name, record.chat_type)
        .await?;
    Ok(json!({
        "marked": true,
        "message_id": message_id,
        "target": record.group_name,
        "chat_type": record.chat_type,
    }))
}

/// `mark_group_msg_as_read` -> 将指定群聊会话标记为已读并消除未读提示角标
pub(crate) async fn exec_mark_group_msg_as_read(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, AdapterError> {
    let payload = OneBotSendPayload {
        group_name: parse_optional_string(&params, "group_name")
            .or_else(|| parse_optional_string(&params, "group")),
        group_id: params.get("group_id").cloned(),
        ..Default::default()
    };
    let target_group = resolve_send_target_group(state, &payload).await?;
    let _send_guard = state.send_gate.lock().await;
    state
        .driver
        .mark_session_read(&target_group, ChatTargetKind::Group)
        .await?;
    Ok(json!({
        "marked": true,
        "target": target_group,
        "chat_type": ChatTargetKind::Group,
    }))
}

/// `mark_private_msg_as_read` -> 严格校验私聊白名单后将指定好友会话标记为已读
pub(crate) async fn exec_mark_private_msg_as_read(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, AdapterError> {
    let payload = OneBotSendPayload {
        user_id: params.get("user_id").cloned(),
        nickname: parse_optional_string(&params, "nickname"),
        group_name: parse_optional_string(&params, "group_name"),
        ..Default::default()
    };
    let target_friend = resolve_send_target_private(state, &payload).await?;
    let _send_guard = state.send_gate.lock().await;
    state
        .driver
        .mark_session_read(&target_friend, ChatTargetKind::Private)
        .await?;
    Ok(json!({
        "marked": true,
        "target": target_friend,
        "chat_type": ChatTargetKind::Private,
    }))
}

fn is_cjk_ideograph(ch: char) -> bool {
    matches!(
        ch as u32,
        0x3040..=0x30FF
            | 0x3400..=0x4DBF
            | 0x4E00..=0x9FFF
            | 0xF900..=0xFAFF
            | 0xAC00..=0xD7AF
            | 0x20000..=0x2A6DF
    )
}

fn tokenize_word_slices(content: &str) -> Vec<String> {
    let mut slices = Vec::new();
    let mut word_buf = String::new();
    for ch in content.chars() {
        if ch.is_whitespace() {
            if !word_buf.is_empty() {
                slices.push(std::mem::take(&mut word_buf));
            }
            continue;
        }
        if is_cjk_ideograph(ch) {
            if !word_buf.is_empty() {
                slices.push(std::mem::take(&mut word_buf));
            }
            slices.push(ch.to_string());
            continue;
        }
        if ch.is_alphanumeric() || ch == '_' {
            word_buf.push(ch);
            continue;
        }
        if !word_buf.is_empty() {
            slices.push(std::mem::take(&mut word_buf));
        }
        slices.push(ch.to_string());
    }
    if !word_buf.is_empty() {
        slices.push(word_buf);
    }
    slices
}

/// `.get_word_slices` -> 返回确定性的 CJK 字符、ASCII 单词与标点分词切片列表
pub(crate) async fn exec_get_word_slices(
    _state: &AppState,
    params: Value,
) -> std::result::Result<Value, AdapterError> {
    let content = params
        .get("content")
        .and_then(Value::as_str)
        .ok_or_else(|| AdapterError::InputRejected("content string is required".to_string()))?;
    Ok(json!({
        "slices": tokenize_word_slices(content),
    }))
}

/// `.get_scheduler_status` -> 查询当前突发风暴调度策略配置与实时遥测快照
pub(crate) async fn exec_get_scheduler_status(
    state: &AppState,
    _params: Value,
) -> std::result::Result<Value, AdapterError> {
    let config = state.driver.scheduler_config().await?;
    let telemetry = state.driver.scheduler_telemetry().await?;
    Ok(json!({
        "config": config,
        "telemetry": telemetry,
    }))
}
