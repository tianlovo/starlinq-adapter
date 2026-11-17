//! OneBot v11 业务动作实现与声明式路由表定义模块

use common_types::{
    segments_to_plain_summary, AdapterError, AdapterStatus, BotLoginInfo, ChatTargetKind,
    MessageSegment, SchedulerTelemetrySnapshot, SendRequest,
};
use serde_json::{json, Value};

use crate::constants::{
    ACTION_CAN_SEND_IMAGE, ACTION_CAN_SEND_RECORD, ACTION_CAN_SEND_VIDEO, ACTION_CLEAN_CACHE,
    ACTION_DELETE_MSG, ACTION_DOT_OCR_IMAGE, ACTION_FORWARD_FRIEND_SINGLE_MSG,
    ACTION_FORWARD_GROUP_SINGLE_MSG, ACTION_GET_AUDIT_EVENTS, ACTION_GET_FILE,
    ACTION_GET_FORWARD_MSG, ACTION_GET_FRIEND_LIST, ACTION_GET_FRIEND_MSG_HISTORY,
    ACTION_GET_GROUP_INFO, ACTION_GET_GROUP_LIST, ACTION_GET_GROUP_MEMBER_INFO,
    ACTION_GET_GROUP_MEMBER_LIST, ACTION_GET_GROUP_MSG_HISTORY, ACTION_GET_IMAGE,
    ACTION_GET_LOGIN_INFO, ACTION_GET_MSG, ACTION_GET_RECENT_CONTACT, ACTION_GET_SCHEDULER_STATUS,
    ACTION_GET_STATUS, ACTION_GET_STRANGER_INFO, ACTION_GET_VERSION_INFO, ACTION_GET_WORD_SLICES,
    ACTION_HANDLE_QUICK_OPERATION, ACTION_MARK_GROUP_MSG_AS_READ, ACTION_MARK_MSG_AS_READ,
    ACTION_MARK_PRIVATE_MSG_AS_READ, ACTION_OCR_IMAGE, ACTION_RESET_SECURITY_BREAKER,
    ACTION_SEND_FORWARD_MSG, ACTION_SEND_GROUP_FORWARD_MSG, ACTION_SEND_GROUP_MSG, ACTION_SEND_MSG,
    ACTION_SEND_PRIVATE_FORWARD_MSG, ACTION_SEND_PRIVATE_MSG, ACTION_SET_RESTART,
    ACTION_UPLOAD_GROUP_FILE, ACTION_UPLOAD_PRIVATE_FILE, ADAPTER_APP_NAME, DEFAULT_BOT_SELF_NAME,
    ONEBOT_MESSAGE_TYPE_GROUP, ONEBOT_MESSAGE_TYPE_PRIVATE, ONEBOT_PROTOCOL_VERSION,
};
use crate::onebot::payloads::{
    extract_onebot_segments, group_name_matches, parse_optional_bool, parse_optional_i64,
    parse_optional_string, parse_value_as_i64, resolve_effective_deadline_ms,
    stored_record_to_onebot_msg, OneBotSendPayload,
};
use crate::AppState;

/// 获取当前 Unix 毫秒级时间戳（非负安全转换）
pub(crate) fn now_utc_ms() -> u64 {
    chrono::Utc::now().timestamp_millis().max(0) as u64
}

/// 校验出站消息是否仍在允许的响应窗口内；超时则登记熔断指标并返回 `ResponseWindowExpired`
pub(crate) fn ensure_within_outbound_deadline(
    target: &str,
    deadline_unix_ms: Option<u64>,
    window_ms_hint: Option<u64>,
    started_ms: u64,
    state: &AppState,
) -> std::result::Result<(), AdapterError> {
    let current_ms = now_utc_ms();
    if window_ms_hint == Some(0) {
        state.driver.record_outbound_deadline_abort();
        return Err(AdapterError::ResponseWindowExpired {
            target: target.to_string(),
            elapsed_ms: current_ms.saturating_sub(started_ms),
            window_ms: 0,
        });
    }
    if let Some(deadline) = deadline_unix_ms {
        if current_ms > deadline {
            let elapsed_ms = current_ms
                .saturating_sub(started_ms)
                .max(current_ms.saturating_sub(deadline));
            let window_ms = window_ms_hint.unwrap_or_else(|| deadline.saturating_sub(started_ms));
            state.driver.record_outbound_deadline_abort();
            return Err(AdapterError::ResponseWindowExpired {
                target: target.to_string(),
                elapsed_ms,
                window_ms,
            });
        }
    }
    Ok(())
}

/// 构建对齐 OneBot v11 标准规范与调度器统计指标的状态 `stat` 字段映射
pub(crate) fn build_onebot_stat_payload(
    status: Option<&AdapterStatus>,
    telemetry: &SchedulerTelemetrySnapshot,
    state: &AppState,
) -> Value {
    let messages_observed = status.map(|st| st.messages_observed).unwrap_or(0);
    let messages_sent = status.map(|st| st.messages_sent).unwrap_or(0);
    json!({
        "packet_received": messages_observed,
        "packet_sent": messages_sent,
        "packet_lost": telemetry.expired_window_total.saturating_add(telemetry.outbound_deadline_aborted_total),
        "message_received": messages_observed,
        "message_sent": messages_sent,
        "disconnect_times": state.ws_disconnect_times(),
        "lost_times": telemetry.outbound_deadline_aborted_total,
        "last_message_time": state.last_message_time_secs(),
        "active_group": status.and_then(|st| st.active_group.as_deref()),
        "active_group_id": status.and_then(|st| st.active_group_id),
        "active_real_group_id": status.and_then(|st| st.active_real_group_id),
        "detected_member_count": status.and_then(|st| st.detected_member_count),
        "messages_observed": messages_observed,
        "messages_sent": messages_sent,
    })
}

/// 解析并返回当前生效的 OneBot 机器人登录态信息，确保动作返回与 WS 事件口径一致
pub(crate) async fn resolve_login_profile(
    state: &AppState,
) -> std::result::Result<BotLoginInfo, AdapterError> {
    let mut info = state.driver.get_login_info().await?;
    if info.user_id <= 0 {
        info.user_id = state.config.bot.self_id;
    }
    if info.nickname.trim().is_empty() {
        info.nickname = state
            .config
            .bot
            .self_nicknames
            .first()
            .cloned()
            .unwrap_or_else(|| DEFAULT_BOT_SELF_NAME.to_string());
    }
    Ok(info)
}

/// 统一计算用于 WS 生命周期、心跳、消息事件与反向握手 `X-Self-ID` 的生效自身账号标识
pub(crate) async fn resolve_effective_self_id(state: &AppState) -> i64 {
    resolve_login_profile(state)
        .await
        .map(|info| info.user_id)
        .unwrap_or(state.config.bot.self_id)
}

async fn exec_get_status(
    state: &AppState,
    _params: Value,
) -> std::result::Result<Value, AdapterError> {
    let qq_status = state.driver.status().await?;
    let storage = state.driver.storage_stats().await?;
    let telemetry = state.driver.scheduler_telemetry().await?;
    Ok(json!({
        "online": true,
        "good": !qq_status.security_breaker_tripped,
        "stat": build_onebot_stat_payload(Some(&qq_status), &telemetry, state),
        "scheduler": &telemetry,
        "qq_status": qq_status,
        "storage": storage,
        "reverse_ws_urls": state.config.onebot.reverse_ws_urls,
    }))
}

async fn exec_get_login_info(
    state: &AppState,
    _params: Value,
) -> std::result::Result<Value, AdapterError> {
    let info = resolve_login_profile(state).await?;
    Ok(json!(info))
}

async fn exec_get_version_info(
    _state: &AppState,
    _params: Value,
) -> std::result::Result<Value, AdapterError> {
    Ok(json!({
        "app_name": ADAPTER_APP_NAME,
        "app_version": env!("CARGO_PKG_VERSION"),
        "protocol_version": ONEBOT_PROTOCOL_VERSION,
    }))
}

/// 解析发送群聊动作的目标群名称（支持直接群名、数字群号及配置默认群）
pub(crate) async fn resolve_send_target_group(
    state: &AppState,
    payload: &OneBotSendPayload,
) -> std::result::Result<String, AdapterError> {
    if let Some(explicit_name) = payload
        .group_name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        return Ok(explicit_name.to_string());
    }

    if let Some(raw_group_id) = payload.group_id.as_ref() {
        if let Some(group_id) = parse_value_as_i64(raw_group_id, "group_id")? {
            if let Some(resolved_name) = state.driver.resolve_group_name(group_id).await? {
                return Ok(resolved_name);
            }
            // 本地缓存未命中时主动刷新会话列表重试一次，仍未知则拒绝发送以严防误路由至默认群
            let _ = state.driver.get_group_list(true).await;
            if let Some(resolved_name) = state.driver.resolve_group_name(group_id).await? {
                return Ok(resolved_name);
            }
            return Err(AdapterError::GroupNotFound {
                group: group_id.to_string(),
            });
        }
    }

    Ok(state.config.target.group_name.clone())
}

/// 解析发送私聊动作的目标好友名称并强制校验 `[target].allowed_private_friends` 白名单
pub(crate) async fn resolve_send_target_private(
    state: &AppState,
    payload: &OneBotSendPayload,
) -> std::result::Result<String, AdapterError> {
    // 显式给出好友昵称或群名回退字段时，先经过白名单校验再决定是否唤醒界面
    let explicit_name = payload
        .nickname
        .as_deref()
        .or(payload.group_name.as_deref())
        .map(str::trim)
        .filter(|name| !name.is_empty());
    if let Some(name) = explicit_name {
        if !state.config.target.is_private_friend_allowed(name) {
            return Err(AdapterError::PrivateTargetNotAllowed {
                target: name.to_string(),
            });
        }
        return Ok(name.to_string());
    }

    if let Some(raw_user_id) = payload.user_id.as_ref() {
        if let Some(user_id) = parse_value_as_i64(raw_user_id, "user_id")? {
            if state.config.target.allowed_private_friends.is_empty() {
                return Err(AdapterError::PrivateTargetNotAllowed {
                    target: user_id.to_string(),
                });
            }
            if let Some(resolved_name) = state.driver.resolve_private_target(user_id).await? {
                if !state
                    .config
                    .target
                    .is_private_friend_allowed(&resolved_name)
                {
                    return Err(AdapterError::PrivateTargetNotAllowed {
                        target: resolved_name,
                    });
                }
                return Ok(resolved_name);
            }
            // 本地好友缓存未命中时刷新一次会话与好友目录再查，禁止退回默认会话以免误发
            let _ = state.driver.get_friend_list(true).await;
            if let Some(resolved_name) = state.driver.resolve_private_target(user_id).await? {
                if !state
                    .config
                    .target
                    .is_private_friend_allowed(&resolved_name)
                {
                    return Err(AdapterError::PrivateTargetNotAllowed {
                        target: resolved_name,
                    });
                }
                return Ok(resolved_name);
            }
            return Err(AdapterError::InputRejected(format!(
                "private friend not found for user_id={user_id}"
            )));
        }
    }

    if state.config.target.allowed_private_friends.is_empty() {
        return Err(AdapterError::PrivateTargetNotAllowed {
            target: "<unspecified>".to_string(),
        });
    }
    Err(AdapterError::InputRejected(
        "private message requires user_id or nickname".to_string(),
    ))
}

/// 出站发送模板方法所选用的目标会话路由策略（区分群聊与私聊通道）
#[derive(Clone, Copy, PartialEq, Eq)]
enum SendRouteKind {
    Group,
    Private,
}

/// 判断媒体或文件引用是否使用 `http://` / `https://` 远程地址
pub(crate) fn is_http_or_https_source(raw: &str) -> bool {
    let trimmed = raw.trim();
    trimmed
        .get(..7)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("http://"))
        || trimmed
            .get(..8)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("https://"))
}

pub(crate) fn is_standalone_onebot_segment(segment: &MessageSegment) -> bool {
    matches!(
        segment,
        MessageSegment::File { .. }
            | MessageSegment::Video { .. }
            | MessageSegment::Rps {}
            | MessageSegment::Dice {}
            | MessageSegment::Shake {}
    )
}

fn validate_strict_onebot_segments(
    action_name: &str,
    route: SendRouteKind,
    segments: &[MessageSegment],
) -> std::result::Result<(), AdapterError> {
    if segments.iter().any(is_standalone_onebot_segment) && segments.len() > 1 {
        return Err(AdapterError::UnsupportedAction {
            action: action_name.to_string(),
            reason: "standalone native actions (file/video/rps/dice/shake) cannot be mixed with other segments in strict mode".to_string(),
        });
    }

    let mut reply_count = 0usize;
    for segment in segments {
        match segment {
            MessageSegment::Text { .. } => {}
            MessageSegment::Image { image_type, .. } => {
                if image_type
                    .as_deref()
                    .map(str::trim)
                    .is_some_and(|kind| kind.eq_ignore_ascii_case("flash"))
                {
                    return Err(AdapterError::UnsupportedAction {
                        action: action_name.to_string(),
                        reason: "QQNT desktop UIA does not support native flash images".to_string(),
                    });
                }
            }
            MessageSegment::Face { id, .. } => {
                if common_types::face_id_to_name(id).is_none() {
                    return Err(AdapterError::UnsupportedAction {
                        action: action_name.to_string(),
                        reason: format!(
                            "unknown classic QQ face id '{id}'; cannot degrade unsupported face to plain text"
                        ),
                    });
                }
            }
            MessageSegment::At { .. } if route == SendRouteKind::Private => {
                return Err(AdapterError::UnsupportedAction {
                    action: action_name.to_string(),
                    reason: "private chat does not support native @mention".to_string(),
                });
            }
            MessageSegment::At { .. } => {}
            MessageSegment::Reply { .. } => {
                reply_count += 1;
                if reply_count > 1 {
                    return Err(AdapterError::InputRejected(
                        "at most one reply segment is supported per native message".to_string(),
                    ));
                }
            }
            MessageSegment::File { file, .. } => {
                if is_http_or_https_source(file) {
                    return Err(AdapterError::UnsupportedAction {
                        action: action_name.to_string(),
                        reason: "strict UIA mode rejects remote http(s):// file URLs; provide a local file path, file:// URI, or base64:// payload".to_string(),
                    });
                }
            }
            MessageSegment::Video { file, url } => {
                let remote_only = is_http_or_https_source(file)
                    || (file.trim().is_empty()
                        && url.as_deref().is_some_and(is_http_or_https_source));
                if remote_only {
                    return Err(AdapterError::UnsupportedAction {
                        action: action_name.to_string(),
                        reason: "strict UIA mode rejects remote http(s):// video URLs; provide a local file path, file:// URI, or base64:// payload".to_string(),
                    });
                }
            }
            MessageSegment::Rps {} | MessageSegment::Dice {} => {}
            MessageSegment::Poke { .. } => {
                return Err(AdapterError::UnsupportedAction {
                    action: action_name.to_string(),
                    reason: "QQNT 9.9.31 desktop UIA does not support native outbound poke; set [onebot].allow_degraded_media_summary=true only if plain text placeholder is explicitly desired".to_string(),
                });
            }
            MessageSegment::Shake {} => {
                if route == SendRouteKind::Group {
                    return Err(AdapterError::UnsupportedAction {
                        action: action_name.to_string(),
                        reason: "group chat does not support private window shake".to_string(),
                    });
                }
            }
            MessageSegment::Record { .. } => {
                return Err(AdapterError::UnsupportedAction {
                    action: action_name.to_string(),
                    reason: "desktop UIA does not support native record upload (can_send_record=false); set [onebot].allow_degraded_media_summary=true only if plain text '[语音]' placeholder is explicitly desired".to_string(),
                });
            }
            MessageSegment::Share { .. }
            | MessageSegment::Contact { .. }
            | MessageSegment::Location { .. }
            | MessageSegment::Music { .. }
            | MessageSegment::Xml { .. }
            | MessageSegment::Json { .. }
            | MessageSegment::Anonymous { .. }
            | MessageSegment::Forward { .. }
            | MessageSegment::Node { .. } => {
                return Err(AdapterError::UnsupportedAction {
                    action: action_name.to_string(),
                    reason: "desktop UIA cannot natively construct share/contact/location/music/xml/json/forward/anonymous segments in strict mode".to_string(),
                });
            }
        }
    }
    Ok(())
}

/// 模板方法模式：统一执行段解析、目标路由与白名单校验、发送锁窗口附着检验、实际投递及响应封包
async fn execute_templated_send(
    state: &AppState,
    payload: OneBotSendPayload,
    route: SendRouteKind,
) -> std::result::Result<Value, AdapterError> {
    let auto_escape = payload.auto_escape.unwrap_or(false);
    let strict_unparsed = !state.config.onebot.allow_degraded_media_summary
        && common_types::has_unparsed_onebot_value(&payload.message, auto_escape);
    let segments_res = extract_onebot_segments(&payload.message, auto_escape);
    if !strict_unparsed {
        if let Err(err) = segments_res {
            return Err(err);
        }
    }
    let target_name = match route {
        SendRouteKind::Group => resolve_send_target_group(state, &payload).await?,
        SendRouteKind::Private => resolve_send_target_private(state, &payload).await?,
    };
    if !state.config.onebot.allow_degraded_media_summary {
        let action_name = match route {
            SendRouteKind::Group => ACTION_SEND_GROUP_MSG,
            SendRouteKind::Private => ACTION_SEND_PRIVATE_MSG,
        };
        if strict_unparsed {
            return Err(AdapterError::UnsupportedAction {
                action: action_name.to_string(),
                reason: "unrecognized or malformed OneBot CQ/segment cannot be sent as literal text in strict mode".to_string(),
            });
        }
    }
    let segments = segments_res?;
    if !state.config.onebot.allow_degraded_media_summary {
        let action_name = match route {
            SendRouteKind::Group => ACTION_SEND_GROUP_MSG,
            SendRouteKind::Private => ACTION_SEND_PRIVATE_MSG,
        };
        validate_strict_onebot_segments(action_name, route, &segments)?;
    }
    send_segments_with_deadline(state, target_name, segments, &payload, route).await
}

/// 在完成目标解析与校验后，统一执行响应窗口检查、发送锁序列化、窗口附着与底层投递
async fn send_segments_with_deadline(
    state: &AppState,
    target_name: String,
    segments: Vec<MessageSegment>,
    payload: &OneBotSendPayload,
    route: SendRouteKind,
) -> std::result::Result<Value, AdapterError> {
    let text = segments_to_plain_summary(&segments);
    let expected_kind = match route {
        SendRouteKind::Group => ChatTargetKind::Group,
        SendRouteKind::Private => ChatTargetKind::Private,
    };
    let window_ms_hint = payload.response_window_ms;
    let started_ms = now_utc_ms();
    let deadline_unix_ms = resolve_effective_deadline_ms(payload, started_ms);
    // 排队获取发送锁前先行检验响应时间窗，避免已过期回复继续占位抢锁
    ensure_within_outbound_deadline(
        &target_name,
        deadline_unix_ms,
        window_ms_hint,
        started_ms,
        state,
    )?;
    let _send_guard = state.send_gate.lock().await;
    // 取得发送锁后立即复检截止时间，防止洪峰队列积压造成陈旧回复被发出
    ensure_within_outbound_deadline(
        &target_name,
        deadline_unix_ms,
        window_ms_hint,
        started_ms,
        state,
    )?;

    // 鉴权与目标解析完成后再检视当前已附着窗口，避免未放行请求触发冗余 UIA 会话切换
    let current_status = state.driver.status().await?;
    let already_open = current_status.attached_window.is_some()
        && current_status.active_chat_kind == Some(expected_kind)
        && current_status
            .active_group
            .as_deref()
            .is_some_and(|active| group_name_matches(active, &target_name));

    let group_guard = state.config.target.member_guard_for_group(&target_name);
    let guard_satisfied = match route {
        SendRouteKind::Group => match (
            group_guard.strict_below_members,
            current_status.detected_member_count,
        ) {
            (Some(limit), Some(count)) => count < limit,
            (Some(_), None) => !group_guard.require_member_count_verified,
            (None, _) => true,
        },
        SendRouteKind::Private => true,
    };

    if !already_open || !guard_satisfied {
        match route {
            SendRouteKind::Group => {
                state.driver.open_group(group_guard).await?;
            }
            SendRouteKind::Private => {
                state.driver.open_private(&target_name).await?;
            }
        }
    }

    let request = SendRequest {
        group: target_name,
        text,
        mode: payload.mode,
        target_kind: expected_kind,
        segments,
        deadline_unix_ms,
    };
    let outcome = match route {
        SendRouteKind::Group => state.driver.send_group_message(request).await?,
        SendRouteKind::Private => state.driver.send_private_message(request).await?,
    };
    if !outcome.ok {
        return Err(AdapterError::UiaFailure(format!(
            "send not confirmed in chat UI: {}",
            outcome.detail
        )));
    }
    state.record_message_activity();
    let message_id = outcome.onebot_message_id.ok_or_else(|| {
        AdapterError::UiaFailure("confirmed send missing persisted onebot_message_id".to_string())
    })?;
    Ok(match route {
        SendRouteKind::Group => json!({
            "message_id": message_id,
            "group_id": outcome.group_id,
            "outcome": outcome,
        }),
        SendRouteKind::Private => json!({
            "message_id": message_id,
            "user_id": outcome.user_id,
            "outcome": outcome,
        }),
    })
}

/// 供扩展动作模块复用群聊截止时间熔断与安全门禁投递已构造消息段
pub(crate) async fn send_group_segments_with_deadline(
    state: &AppState,
    target_name: String,
    segments: Vec<MessageSegment>,
    payload: &OneBotSendPayload,
) -> std::result::Result<Value, AdapterError> {
    send_segments_with_deadline(state, target_name, segments, payload, SendRouteKind::Group).await
}

/// 供扩展动作模块复用私聊截止时间熔断与锁序列化投递已构造消息段
pub(crate) async fn send_private_segments_with_deadline(
    state: &AppState,
    target_name: String,
    segments: Vec<MessageSegment>,
    payload: &OneBotSendPayload,
) -> std::result::Result<Value, AdapterError> {
    send_segments_with_deadline(
        state,
        target_name,
        segments,
        payload,
        SendRouteKind::Private,
    )
    .await
}

/// 解析通用发送群消息载荷并在发送锁内附着目标群且执行文本/富媒体发送
pub(crate) async fn exec_send_group_with_payload(
    state: &AppState,
    payload: OneBotSendPayload,
) -> std::result::Result<Value, AdapterError> {
    execute_templated_send(state, payload, SendRouteKind::Group).await
}

/// 解析通用发送私聊载荷，白名单校验通过后在发送锁内附着好友会话并发送
pub(crate) async fn exec_send_private_with_payload(
    state: &AppState,
    payload: OneBotSendPayload,
) -> std::result::Result<Value, AdapterError> {
    execute_templated_send(state, payload, SendRouteKind::Private).await
}

async fn exec_send_group_msg(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, AdapterError> {
    let payload: OneBotSendPayload = serde_json::from_value(params).map_err(|err| {
        AdapterError::InputRejected(format!("invalid send_group_msg params: {err}"))
    })?;
    if let Some(ref msg_type) = payload.message_type {
        if msg_type.trim() != ONEBOT_MESSAGE_TYPE_GROUP {
            return Err(AdapterError::InputRejected(format!(
                "unsupported message_type '{msg_type}' for send_group_msg"
            )));
        }
    }
    exec_send_group_with_payload(state, payload).await
}

async fn exec_send_private_msg(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, AdapterError> {
    let payload: OneBotSendPayload = serde_json::from_value(params).map_err(|err| {
        AdapterError::InputRejected(format!("invalid send_private_msg params: {err}"))
    })?;
    if let Some(ref msg_type) = payload.message_type {
        if msg_type.trim() != ONEBOT_MESSAGE_TYPE_PRIVATE {
            return Err(AdapterError::InputRejected(format!(
                "unsupported message_type '{msg_type}' for send_private_msg"
            )));
        }
    }
    exec_send_private_with_payload(state, payload).await
}

async fn exec_send_msg(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, AdapterError> {
    let payload: OneBotSendPayload = serde_json::from_value(params)
        .map_err(|err| AdapterError::InputRejected(format!("invalid send_msg params: {err}")))?;

    let is_private = match payload
        .message_type
        .as_deref()
        .map(str::trim)
        .filter(|val| !val.is_empty())
    {
        Some(ONEBOT_MESSAGE_TYPE_PRIVATE) => true,
        Some(ONEBOT_MESSAGE_TYPE_GROUP) => false,
        Some(other) => {
            return Err(AdapterError::InputRejected(format!(
                "unsupported message_type '{other}'; expected '{ONEBOT_MESSAGE_TYPE_GROUP}' or '{ONEBOT_MESSAGE_TYPE_PRIVATE}'"
            )));
        }
        None => {
            payload.group_id.is_none()
                && payload
                    .group_name
                    .as_deref()
                    .is_none_or(|name| name.trim().is_empty())
                && (payload.user_id.is_some()
                    || payload
                        .nickname
                        .as_deref()
                        .is_some_and(|nick| !nick.trim().is_empty()))
        }
    };

    if is_private {
        exec_send_private_with_payload(state, payload).await
    } else {
        exec_send_group_with_payload(state, payload).await
    }
}

async fn exec_get_msg(state: &AppState, params: Value) -> std::result::Result<Value, AdapterError> {
    let message_id = parse_optional_i64(&params, "message_id")?
        .ok_or_else(|| AdapterError::InputRejected("message_id is required".to_string()))?;
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
    Ok(stored_record_to_onebot_msg(&record))
}

async fn exec_get_group_list(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, AdapterError> {
    let no_cache = parse_optional_bool(&params, "no_cache");
    let groups = state.driver.get_group_list(no_cache).await?;
    Ok(json!(groups))
}

async fn exec_get_group_info(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, AdapterError> {
    let group_id = parse_optional_i64(&params, "group_id")?;
    let group_name = parse_optional_string(&params, "group_name");
    let no_cache = parse_optional_bool(&params, "no_cache");
    let info = state
        .driver
        .get_group_info(group_id, group_name, no_cache)
        .await?;
    Ok(json!(info))
}

async fn exec_get_group_member_list(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, AdapterError> {
    let group_id = parse_optional_i64(&params, "group_id")?;
    let group_name = parse_optional_string(&params, "group_name");
    let no_cache = parse_optional_bool(&params, "no_cache");
    let members = state
        .driver
        .get_group_member_list(group_id, group_name, no_cache)
        .await?;
    Ok(json!(members))
}

async fn exec_get_group_member_info(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, AdapterError> {
    let group_id = parse_optional_i64(&params, "group_id")?;
    let group_name = parse_optional_string(&params, "group_name");
    let user_id = parse_optional_i64(&params, "user_id")?
        .ok_or_else(|| AdapterError::InputRejected("user_id is required".to_string()))?;
    let no_cache = parse_optional_bool(&params, "no_cache");

    let members = state
        .driver
        .get_group_member_list(group_id, group_name, no_cache)
        .await?;
    let member = members
        .into_iter()
        .find(|item| item.user_id == user_id || item.real_user_id == Some(user_id))
        .ok_or_else(|| {
            AdapterError::InputRejected(format!("group member not found for user_id={user_id}"))
        })?;
    Ok(json!(member))
}

async fn exec_get_friend_list(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, AdapterError> {
    let no_cache = parse_optional_bool(&params, "no_cache");
    let friends = state.driver.get_friend_list(no_cache).await?;
    Ok(json!(friends))
}

async fn exec_can_send_true(
    _state: &AppState,
    _params: Value,
) -> std::result::Result<Value, AdapterError> {
    Ok(json!({ "yes": true }))
}

async fn exec_can_send_false(
    _state: &AppState,
    _params: Value,
) -> std::result::Result<Value, AdapterError> {
    Ok(json!({ "yes": false }))
}

define_onebot_actions! {
    supported {
        ACTION_GET_STATUS => {
            method: get_post,
            target: exec_get_status,
        },
        ACTION_GET_LOGIN_INFO => {
            method: get_post,
            target: exec_get_login_info,
        },
        ACTION_GET_VERSION_INFO => {
            method: get_post,
            target: exec_get_version_info,
        },
        ACTION_SEND_GROUP_MSG => {
            method: post_only,
            target: exec_send_group_msg,
        },
        ACTION_SEND_PRIVATE_MSG => {
            method: post_only,
            target: exec_send_private_msg,
        },
        ACTION_SEND_MSG => {
            method: post_only,
            target: exec_send_msg,
        },
        ACTION_GET_MSG => {
            method: get_post,
            target: exec_get_msg,
        },
        ACTION_GET_FRIEND_LIST => {
            method: get_post,
            target: exec_get_friend_list,
        },
        ACTION_GET_GROUP_LIST => {
            method: get_post,
            target: exec_get_group_list,
        },
        ACTION_GET_GROUP_INFO => {
            method: get_post,
            target: exec_get_group_info,
        },
        ACTION_GET_GROUP_MEMBER_LIST => {
            method: get_post,
            target: exec_get_group_member_list,
        },
        ACTION_GET_GROUP_MEMBER_INFO => {
            method: get_post,
            target: exec_get_group_member_info,
        },
        ACTION_CAN_SEND_IMAGE => {
            method: get_post,
            target: exec_can_send_true,
        },
        ACTION_CAN_SEND_RECORD => {
            method: get_post,
            target: exec_can_send_false,
        },
        ACTION_GET_STRANGER_INFO => {
            method: get_post,
            target: crate::onebot::extended_handlers::exec_get_stranger_info,
        },
        ACTION_GET_IMAGE => {
            method: get_post,
            target: crate::onebot::extended_handlers::exec_get_image,
        },
        ACTION_GET_GROUP_MSG_HISTORY => {
            method: get_post,
            target: crate::onebot::extended_handlers::exec_get_group_msg_history,
        },
        ACTION_GET_FRIEND_MSG_HISTORY => {
            method: get_post,
            target: crate::onebot::extended_handlers::exec_get_friend_msg_history,
        },
        ACTION_SEND_GROUP_FORWARD_MSG => {
            method: post_only,
            target: crate::onebot::extended_handlers::exec_send_group_forward_msg,
        },
        ACTION_SEND_PRIVATE_FORWARD_MSG => {
            method: post_only,
            target: crate::onebot::extended_handlers::exec_send_private_forward_msg,
        },
        ACTION_SEND_FORWARD_MSG => {
            method: post_only,
            target: crate::onebot::extended_handlers::exec_send_forward_msg,
        },
        ACTION_GET_FORWARD_MSG => {
            method: get_post,
            target: crate::onebot::extended_handlers::exec_get_forward_msg,
        },
        ACTION_CLEAN_CACHE => {
            method: post_only,
            target: crate::onebot::extended_handlers::exec_clean_cache,
        },
        ACTION_SET_RESTART => {
            method: post_only,
            target: crate::onebot::extended_handlers::exec_set_restart,
        },
        ACTION_HANDLE_QUICK_OPERATION => {
            method: post_only,
            target: crate::onebot::quick_ops::exec_handle_quick_operation,
        },
        ACTION_DELETE_MSG => {
            method: post_only,
            target: crate::onebot::poke_recall_handlers::exec_delete_msg,
        },
        ACTION_MARK_MSG_AS_READ => {
            method: get_post,
            target: crate::onebot::extended_handlers::exec_mark_msg_as_read,
        },
        ACTION_MARK_GROUP_MSG_AS_READ => {
            method: get_post,
            target: crate::onebot::extended_handlers::exec_mark_group_msg_as_read,
        },
        ACTION_MARK_PRIVATE_MSG_AS_READ => {
            method: get_post,
            target: crate::onebot::extended_handlers::exec_mark_private_msg_as_read,
        },
        ACTION_GET_WORD_SLICES => {
            method: get_post,
            target: crate::onebot::extended_handlers::exec_get_word_slices,
        },
        ACTION_GET_SCHEDULER_STATUS => {
            method: get_post,
            target: crate::onebot::extended_handlers::exec_get_scheduler_status,
        },
        ACTION_CAN_SEND_VIDEO => {
            method: get_post,
            target: exec_can_send_true,
        },
        ACTION_OCR_IMAGE => {
            method: get_post,
            target: crate::onebot::file_ocr_handlers::exec_ocr_image,
        },
        ACTION_DOT_OCR_IMAGE => {
            method: get_post,
            target: crate::onebot::file_ocr_handlers::exec_ocr_image,
        },
        ACTION_UPLOAD_GROUP_FILE => {
            method: post_only,
            target: crate::onebot::file_ocr_handlers::exec_upload_group_file,
        },
        ACTION_UPLOAD_PRIVATE_FILE => {
            method: post_only,
            target: crate::onebot::file_ocr_handlers::exec_upload_private_file,
        },
        ACTION_GET_FILE => {
            method: get_post,
            target: crate::onebot::file_ocr_handlers::exec_get_file,
        },
        ACTION_FORWARD_GROUP_SINGLE_MSG => {
            method: post_only,
            target: crate::onebot::wave6_handlers::exec_forward_group_single_msg,
        },
        ACTION_FORWARD_FRIEND_SINGLE_MSG => {
            method: post_only,
            target: crate::onebot::wave6_handlers::exec_forward_friend_single_msg,
        },
        ACTION_GET_RECENT_CONTACT => {
            method: get_post,
            target: crate::onebot::wave6_handlers::exec_get_recent_contact,
        },
        ACTION_GET_AUDIT_EVENTS => {
            method: get_post,
            target: crate::onebot::wave6_handlers::exec_get_audit_events,
        },
        ACTION_RESET_SECURITY_BREAKER => {
            method: post_only,
            target: crate::onebot::wave6_handlers::exec_reset_security_breaker,
        },
    }
    unsupported {
        "send_poke" => "QQNT 9.9.31 desktop client does not support native outbound avatar poke in UIA mode",
        "group_poke" => "QQNT 9.9.31 desktop client does not support native outbound avatar poke in UIA mode",
        "friend_poke" => "QQNT 9.9.31 desktop client does not support native outbound avatar poke in UIA mode",
        "get_record" => "QQNT UIA mode cannot extract encrypted SILK or AMR voice streams",
        "send_like" => "profile like clicking is disabled in non-intrusive background UIA mode",
        "get_cookies" => "QQNT desktop client does not expose web session cookies",
        "get_csrf_token" => "QQNT desktop client does not expose CSRF tokens",
        "get_credentials" => "QQNT desktop client does not expose credential cookies or CSRF tokens",
        "get_group_honor_info" => "group honor list scraping is unsupported in non-intrusive desktop UIA mode",
        "get_group_system_msg" => "group join and invitation system notification lists are unsupported in UIA mode",
        "get_group_at_all_remain" => "at-all remaining quota query requires server protocol RPC unavailable in UIA mode",
        "get_essence_msg_list" => "group essence message list requires protocol API unavailable in UIA mode",
        "get_group_root_files" => "group shared filesystem enumeration is unsupported in UIA mode",
        "download_file" => "arbitrary remote URL download to local host via OneBot action is disabled for security",
        "set_qq_profile" => "bot account profile editing is blocked by non-intrusive UIA safety policy",
        "set_group_kick" => "group kick admin action is blocked by non-intrusive UIA safety policy",
        "set_group_ban" => "group mute admin action is blocked by non-intrusive UIA safety policy",
        "set_group_anonymous_ban" => "anonymous mute admin action is unsupported in UIA mode",
        "set_group_whole_ban" => "group whole-ban toggle is blocked by non-intrusive UIA safety policy",
        "set_group_admin" => "group admin role mutation is blocked by UIA safety policy",
        "set_group_anonymous" => "group anonymous toggle is obsolete and unsupported",
        "set_group_card" => "group member card modification via settings drawer is unsupported",
        "set_group_name" => "group name modification is blocked by UIA safety policy",
        "set_group_leave" => "leaving or dismissing groups via UIA is permanently forbidden",
        "set_group_special_title" => "group special title modification is unsupported in UIA mode",
        "set_group_portrait" => "group avatar upload is unsupported in UIA mode",
        "set_friend_add_request" => "auto-approving friend requests via UIA is disabled for safety",
        "set_group_add_request" => "auto-approving group join or invite requests via UIA is disabled for safety",
        "set_msg_emoji_like" => "message emoji reaction clicking is unsupported in non-intrusive desktop UIA mode",
        "fetch_emoji_like" => "message emoji reaction listing requires protocol RPC unavailable in UIA mode",
        "set_essence_msg" => "group essence message pinning is unsupported in non-intrusive UIA mode",
        "delete_essence_msg" => "group essence message removal is unsupported in non-intrusive UIA mode",
        "_send_group_notice" => "group announcement publishing is blocked by non-intrusive UIA safety policy",
        "_get_group_notice" => "group announcement scraping requires protocol RPC unavailable in UIA mode",
        "delete_friend" => "friend deletion is permanently blocked by non-intrusive UIA safety policy",
        "set_qq_avatar" => "bot avatar modification is blocked by non-intrusive UIA safety policy",
        "get_group_file_system_info" => "group shared filesystem metadata enumeration is unsupported in UIA mode",
        "get_group_files_by_folder" => "group shared folder enumeration is unsupported in UIA mode",
        "get_group_file_url" => "group shared file download URL resolution requires protocol RPC unavailable in UIA mode",
    }
}
