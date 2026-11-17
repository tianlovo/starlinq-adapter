//! OneBot v11 事件快速操作（`.handle_quick_operation`）与 HTTP Event Webhook 响应快速回复执行器

use common_types::{AdapterError, ChatTargetKind, MessageSegment};
use serde_json::{json, Value};

use crate::constants::{
    ACTION_HANDLE_QUICK_OPERATION, ONEBOT_MESSAGE_TYPE_GROUP, ONEBOT_MESSAGE_TYPE_PRIVATE,
};
use crate::onebot::handlers::{
    ensure_within_outbound_deadline, exec_send_group_with_payload, exec_send_private_with_payload,
    is_standalone_onebot_segment, now_utc_ms,
};
use crate::onebot::payloads::{
    extract_onebot_segments, group_name_matches, parse_optional_bool, parse_optional_i64,
    parse_optional_string, parse_optional_u64, parse_value_as_i64, OneBotSendPayload,
};
use crate::AppState;

fn has_non_empty_reply(reply: Option<&Value>) -> bool {
    match reply {
        None | Some(Value::Null) => false,
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Array(items)) => !items.is_empty(),
        Some(Value::Object(_)) => true,
        Some(_) => false,
    }
}

fn has_unsupported_quick_flag(op_map: &serde_json::Map<String, Value>) -> bool {
    let has_admin_flag = ["kick", "ban", "approve", "set_card", "reject_add_request"]
        .into_iter()
        .any(|key| match op_map.get(key) {
            Some(Value::Bool(flag)) => *flag,
            Some(Value::String(val)) => !val.trim().is_empty(),
            _ => false,
        });
    if has_admin_flag {
        return true;
    }
    match op_map.get("ban_duration") {
        Some(Value::Number(num)) => num.as_u64().is_some_and(|secs| secs > 0),
        Some(Value::String(raw)) => raw.trim().parse::<u64>().is_ok_and(|secs| secs > 0),
        _ => false,
    }
}

async fn is_same_group_message_record(state: &AppState, context: &Value, msg_id: i64) -> bool {
    let Ok(Some(rec)) = state.driver.get_message(msg_id).await else {
        return false;
    };
    if rec.chat_type != ChatTargetKind::Group {
        return false;
    }
    let ctx_group_name = parse_optional_string(context, "group_name");
    let ctx_group_id = parse_optional_i64(context, "group_id").ok().flatten();
    if let Some(ref expected_name) = ctx_group_name {
        if !group_name_matches(&rec.group_name, expected_name) {
            return false;
        }
    }
    if let Some(expected_gid) = ctx_group_id {
        if rec.group_id != expected_gid {
            let resolved_matches = matches!(
                state.driver.resolve_group_name(expected_gid).await,
                Ok(Some(resolved)) if group_name_matches(&rec.group_name, &resolved)
            );
            if !resolved_matches {
                return false;
            }
        }
    }
    if ctx_group_name.is_none() && ctx_group_id.is_none() {
        return group_name_matches(&rec.group_name, &state.config.target.group_name);
    }
    true
}

fn resolve_quick_at_target(
    context: &Value,
    operation: &Value,
    reply_segments: &[MessageSegment],
) -> Option<(String, Option<String>)> {
    let at_sender = operation
        .get("at_sender")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    if !at_sender {
        return None;
    }
    let raw_uid = context.get("user_id")?;
    let Ok(Some(uid)) = parse_value_as_i64(raw_uid, "user_id") else {
        return None;
    };
    if uid <= 0 {
        return None;
    }
    let uid_str = uid.to_string();
    let already_has_at = reply_segments
        .iter()
        .any(|seg| matches!(seg, MessageSegment::At { qq, .. } if qq.trim() == uid_str));
    if already_has_at {
        return None;
    }
    let sender_name = context
        .get("sender")
        .and_then(|s| s.get("card").or_else(|| s.get("nickname")))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(ToOwned::to_owned);
    Some((uid_str, sender_name))
}

async fn build_quick_reply_segments(
    state: &AppState,
    context: &Value,
    operation: &Value,
    reply_val: &Value,
    is_group: bool,
) -> std::result::Result<Vec<MessageSegment>, AdapterError> {
    let auto_escape = parse_optional_bool(operation, "auto_escape");
    let reply_segments = extract_onebot_segments(reply_val, auto_escape)?;
    // 独占原生段（文件/视频/魔法表情/抖动）禁止混插引用或 @ 段，直接原样发送
    if reply_segments.iter().any(is_standalone_onebot_segment) {
        return Ok(reply_segments);
    }
    if !is_group {
        return Ok(reply_segments);
    }

    let has_reply_seg = reply_segments
        .iter()
        .any(|seg| matches!(seg, MessageSegment::Reply { .. }));
    let prepend_reply_id = if !has_reply_seg {
        match context
            .get("message_id")
            .and_then(|raw| parse_value_as_i64(raw, "message_id").ok())
            .flatten()
            .filter(|&id| id > 0)
        {
            Some(msg_id) if is_same_group_message_record(state, context, msg_id).await => {
                Some(msg_id.to_string())
            }
            _ => None,
        }
    } else {
        None
    };
    let prepend_at = resolve_quick_at_target(context, operation, &reply_segments);

    let mut final_segments = Vec::with_capacity(reply_segments.len() + 3);
    if let Some(reply_id) = prepend_reply_id {
        final_segments.push(MessageSegment::Reply { id: reply_id });
    }
    if let Some((qq, name)) = prepend_at {
        if has_reply_seg && matches!(reply_segments.first(), Some(MessageSegment::Reply { .. })) {
            final_segments.push(reply_segments[0].clone());
            final_segments.push(MessageSegment::At { qq, name });
            final_segments.push(MessageSegment::Text {
                text: " ".to_string(),
            });
            final_segments.extend_from_slice(&reply_segments[1..]);
            return Ok(final_segments);
        }
        final_segments.push(MessageSegment::At { qq, name });
        final_segments.push(MessageSegment::Text {
            text: " ".to_string(),
        });
    }

    final_segments.extend(reply_segments);
    Ok(final_segments)
}

/// 解析事件上下文与响应操作对象，若存在 `reply` 字段则通过现有聊天分发通道执行快速回复
pub(crate) async fn apply_quick_operation(
    state: &AppState,
    context: &Value,
    operation: &Value,
) -> std::result::Result<bool, AdapterError> {
    let Some(op_map) = operation.as_object() else {
        return Ok(false);
    };
    if op_map.is_empty() {
        return Ok(false);
    }
    // 组合管理标志必须在执行任何回复或撤回前优先拦截，禁止部分执行后静默吞掉指令
    if has_unsupported_quick_flag(op_map) {
        return Err(AdapterError::UnsupportedAction {
            action: ACTION_HANDLE_QUICK_OPERATION.to_string(),
            reason: "quick operation admin flags (kick/ban/approve/set_card/reject_add_request/ban_duration) are disabled in UIA mode".to_string(),
        });
    }

    let wants_delete = parse_optional_bool(operation, "delete");
    let delete_msg_id = if wants_delete {
        let parsed = context
            .get("message_id")
            .map(|raw| parse_value_as_i64(raw, "message_id"))
            .transpose()?
            .flatten()
            .filter(|&id| id > 0)
            .ok_or_else(|| {
                AdapterError::InputRejected(
                    "context.message_id is required when operation.delete is true".to_string(),
                )
            })?;
        Some(parsed)
    } else {
        None
    };

    let reply_opt = op_map.get("reply");
    let has_reply = has_non_empty_reply(reply_opt);
    if !has_reply && delete_msg_id.is_none() {
        return Ok(false);
    }

    let captured_at_ms = parse_optional_u64(context, "captured_at_ms")?.filter(|ts| *ts > 0);
    let response_deadline_ms = parse_optional_u64(context, "response_deadline_ms")?;
    let deadline_unix_ms = response_deadline_ms.or_else(|| {
        captured_at_ms
            .map(|ts| ts.saturating_add(state.config.scheduler.critical_response_window_ms))
    });

    if has_reply {
        let reply_val = reply_opt.expect("checked non-empty reply");
        let message_type = context
            .get("message_type")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        let is_group = if message_type == ONEBOT_MESSAGE_TYPE_PRIVATE {
            false
        } else {
            message_type == ONEBOT_MESSAGE_TYPE_GROUP
                || context.get("group_id").is_some_and(|v| !v.is_null())
                || parse_optional_string(context, "group_name").is_some()
        };

        let final_segments =
            build_quick_reply_segments(state, context, operation, reply_val, is_group).await?;
        let encoded_message =
            serde_json::to_value(&final_segments).unwrap_or_else(|_| reply_val.clone());

        if is_group {
            let payload = OneBotSendPayload {
                group_name: parse_optional_string(context, "group_name"),
                group_id: context.get("group_id").cloned(),
                message: encoded_message,
                deadline_unix_ms,
                ..Default::default()
            };
            let _ = exec_send_group_with_payload(state, payload).await?;
        } else {
            let nickname = parse_optional_string(context, "nickname").or_else(|| {
                context
                    .get("sender")
                    .and_then(|s| s.get("nickname"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(ToOwned::to_owned)
            });
            let payload = OneBotSendPayload {
                user_id: context.get("user_id").cloned(),
                nickname,
                group_name: parse_optional_string(context, "group_name"),
                message: encoded_message,
                deadline_unix_ms,
                ..Default::default()
            };
            let _ = exec_send_private_with_payload(state, payload).await?;
        }
    }

    if let Some(msg_id) = delete_msg_id {
        let delete_target = parse_optional_string(context, "group_name")
            .or_else(|| parse_optional_string(context, "nickname"))
            .unwrap_or_else(|| format!("message:{msg_id}"));
        let started_ms = captured_at_ms.unwrap_or_else(now_utc_ms);
        ensure_within_outbound_deadline(&delete_target, deadline_unix_ms, None, started_ms, state)?;
        let _send_guard = state.send_gate.lock().await;
        ensure_within_outbound_deadline(&delete_target, deadline_unix_ms, None, started_ms, state)?;
        state.driver.delete_message(msg_id).await?;
    }

    Ok(true)
}

/// `.handle_quick_operation` -> 执行客户端显式上送的事件快速回复或快速操作
pub(crate) async fn exec_handle_quick_operation(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, AdapterError> {
    let context = params
        .get("context")
        .filter(|val| val.is_object())
        .ok_or_else(|| AdapterError::InputRejected("'context' object is required".to_string()))?;
    let operation = params
        .get("operation")
        .filter(|val| val.is_object())
        .ok_or_else(|| AdapterError::InputRejected("'operation' object is required".to_string()))?;

    let applied = apply_quick_operation(state, context, operation).await?;
    Ok(json!({ "applied": applied }))
}
