//! Wave v6 新增 OneBot v11 动作处理器：覆盖单条消息原生转发、最近联系人聚合、审计流水检索与安全熔断重置

use std::cmp::Reverse;
use std::collections::HashMap;

use common_types::{
    canonical_group_names_match, strip_group_count_suffix, AdapterError, AuditEventRecord,
    ChatTargetKind, FriendInfo, GroupInfoSummary, GroupTargetSpec, MessageHistoryFilter,
    SendOutcome, SessionSummary, SingleForwardRequest, StoredMessageRecord,
};
use serde_json::{json, Value};

use crate::onebot::handlers::{
    ensure_within_outbound_deadline, now_utc_ms, resolve_send_target_group,
    resolve_send_target_private,
};
use crate::onebot::payloads::{
    parse_optional_i64, parse_optional_string, resolve_effective_deadline_ms, OneBotSendPayload,
};
use crate::AppState;

const RECENT_CONTACT_DEFAULT_COUNT: i64 = 20;
const RECENT_CONTACT_MAX_COUNT: i64 = 200;
const AUDIT_EVENTS_DEFAULT_LIMIT: i64 = 50;
const AUDIT_EVENTS_MAX_LIMIT: usize = 200;

/// 最近联系人会话聚合条目（用于跨 `list_sessions` 与离线历史消息合并去重与排序）
#[derive(Debug, Clone)]
struct RecentContactEntry {
    chat_type: ChatTargetKind,
    peer_name: String,
    group_id: Option<i64>,
    user_id: Option<i64>,
    last_msg_id: Option<i64>,
    last_msg_time: Option<i64>,
    last_msg_text: Option<String>,
    unread_count: u32,
    sort_captured_ms: u64,
    sort_msg_id: i64,
    session_rank: usize,
}

/// 校验群名称与已识别成员数是否满足目标群人数安全守卫（允许已配置放行的大监控群）
pub(crate) fn passes_group_guard(
    target_cfg: &GroupTargetSpec,
    group_name: &str,
    detected_members: Option<u32>,
) -> bool {
    let guard = target_cfg.member_guard_for_group(group_name);
    if let Some(limit) = guard.strict_below_members {
        match detected_members {
            Some(count) => count < limit,
            None => !guard.require_member_count_verified,
        }
    } else {
        true
    }
}

fn parse_trailing_member_count(raw: &str) -> Option<u32> {
    let trimmed = raw.trim();
    let (without_close, open_delim) = trimmed
        .strip_suffix(')')
        .map(|inner| (inner, '('))
        .or_else(|| trimmed.strip_suffix('）').map(|inner| (inner, '（')))?;
    let (prefix, suffix_body) = without_close.rsplit_once(open_delim)?;
    let count_digits = suffix_body.trim().trim_end_matches('人').trim();
    if prefix.trim().is_empty()
        || count_digits.is_empty()
        || count_digits.len() > 5
        || !count_digits.chars().all(|ch| ch.is_ascii_digit())
    {
        return None;
    }
    count_digits.parse::<u32>().ok()
}

fn canonical_contact_key(raw: &str) -> String {
    let canon = strip_group_count_suffix(raw);
    if canon.is_empty() {
        raw.trim().to_string()
    } else {
        canon
    }
}

fn resolve_group_member_count(
    group_name: &str,
    session_hint: Option<u32>,
    sessions: &[SessionSummary],
    cached_groups: &[GroupInfoSummary],
) -> Option<u32> {
    if let Some(cnt) = session_hint {
        return Some(cnt);
    }
    if let Some(session_match) = sessions.iter().find(|item| {
        item.member_count_hint.is_some() && canonical_group_names_match(&item.title, group_name)
    }) {
        return session_match.member_count_hint;
    }
    if let Some(cnt) = parse_trailing_member_count(group_name) {
        return Some(cnt);
    }
    cached_groups
        .iter()
        .find(|item| {
            item.member_count > 0 && canonical_group_names_match(&item.group_name, group_name)
        })
        .map(|item| item.member_count as u32)
}

fn resolve_session_chat_kind(
    session: &SessionSummary,
    target_cfg: &GroupTargetSpec,
    cached_groups: &[GroupInfoSummary],
    recent_messages: &[StoredMessageRecord],
) -> ChatTargetKind {
    if session.member_count_hint.is_some() || session.is_group_hint == Some(true) {
        return ChatTargetKind::Group;
    }
    if session.is_group_hint == Some(false) {
        return ChatTargetKind::Private;
    }
    if cached_groups
        .iter()
        .any(|item| canonical_group_names_match(&item.group_name, &session.title))
    {
        return ChatTargetKind::Group;
    }
    if let Some(msg) = recent_messages
        .iter()
        .find(|rec| canonical_group_names_match(&rec.group_name, &session.title))
    {
        return msg.chat_type;
    }
    if target_cfg.is_monitored_group(&session.title) {
        return ChatTargetKind::Group;
    }
    // 未命中任何群特征或群缓存时按私聊处理，交由私聊白名单严格拦截防止陌生人会话泄漏
    ChatTargetKind::Private
}

fn lookup_group_id(
    group_name: &str,
    record_group_id: Option<i64>,
    cached_groups: &[GroupInfoSummary],
) -> Option<i64> {
    if let Some(gid) = record_group_id.filter(|&id| id > 0) {
        return Some(gid);
    }
    cached_groups
        .iter()
        .find(|item| item.group_id > 0 && canonical_group_names_match(&item.group_name, group_name))
        .map(|item| item.group_id)
}

fn lookup_friend_id(
    peer_name: &str,
    record_user_id: Option<i64>,
    cached_friends: &[FriendInfo],
) -> Option<i64> {
    if let Some(uid) = record_user_id.filter(|&id| id > 0) {
        return Some(uid);
    }
    let trimmed = peer_name.trim();
    cached_friends
        .iter()
        .find(|item| {
            item.user_id > 0 && (item.nickname.trim() == trimmed || item.remark.trim() == trimmed)
        })
        .map(|item| item.user_id)
}

/// 校验待转发源消息编号及权限，严防非白名单私聊消息被转发出站
pub(crate) async fn check_source_forward_allowed(
    state: &AppState,
    message_id: i64,
) -> std::result::Result<(), AdapterError> {
    if let Some(source_rec) = state.driver.get_message(message_id).await? {
        if source_rec.chat_type == ChatTargetKind::Private
            && !state
                .config
                .target
                .is_private_friend_allowed(&source_rec.group_name)
        {
            return Err(AdapterError::PrivateTargetNotAllowed {
                target: source_rec.group_name,
            });
        }
    }
    Ok(())
}

/// 在发送门控与响应时间窗保护下执行单条历史消息原生转发，并确认返回持久化的 OneBot 消息编号
pub(crate) async fn execute_single_forward(
    state: &AppState,
    req: SingleForwardRequest,
    window_ms_hint: Option<u64>,
) -> std::result::Result<SendOutcome, AdapterError> {
    if req.message_id <= 0 {
        return Err(AdapterError::InputRejected(
            "message_id must be a positive integer".to_string(),
        ));
    }
    if req.target_kind == ChatTargetKind::Private
        && !state.config.target.is_private_friend_allowed(&req.target)
    {
        return Err(AdapterError::PrivateTargetNotAllowed {
            target: req.target.clone(),
        });
    }
    check_source_forward_allowed(state, req.message_id).await?;

    let started_ms = now_utc_ms();
    ensure_within_outbound_deadline(
        &req.target,
        req.deadline_unix_ms,
        window_ms_hint,
        started_ms,
        state,
    )?;
    let _send_guard = state.send_gate.lock().await;
    ensure_within_outbound_deadline(
        &req.target,
        req.deadline_unix_ms,
        window_ms_hint,
        started_ms,
        state,
    )?;

    let outcome = state.driver.forward_single_message(req).await?;
    if !outcome.ok {
        return Err(AdapterError::UiaFailure(format!(
            "forward_single_message not confirmed in chat UI: {}",
            outcome.detail
        )));
    }
    if outcome.onebot_message_id.is_none() {
        return Err(AdapterError::UiaFailure(
            "confirmed forward_single_message missing persisted onebot_message_id".to_string(),
        ));
    }
    state.record_message_activity();
    Ok(outcome)
}

fn parse_positive_message_id(params: &Value) -> std::result::Result<i64, AdapterError> {
    let message_id = parse_optional_i64(params, "message_id")?
        .ok_or_else(|| AdapterError::InputRejected("message_id is required".to_string()))?;
    if message_id <= 0 {
        return Err(AdapterError::InputRejected(
            "message_id must be a positive integer".to_string(),
        ));
    }
    Ok(message_id)
}

/// `forward_group_single_msg` -> 校验正整数 `message_id` 与群目标后原生单条转发至目标群聊
pub(crate) async fn exec_forward_group_single_msg(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, AdapterError> {
    let message_id = parse_positive_message_id(&params)?;
    let mut payload: OneBotSendPayload = serde_json::from_value(params.clone()).map_err(|err| {
        AdapterError::InputRejected(format!("invalid forward_group_single_msg params: {err}"))
    })?;
    if payload
        .group_name
        .as_deref()
        .is_none_or(|name| name.trim().is_empty())
    {
        payload.group_name = parse_optional_string(&params, "group");
    }

    let target = resolve_send_target_group(state, &payload).await?;
    let deadline_unix_ms = resolve_effective_deadline_ms(&payload, now_utc_ms());
    let outcome = execute_single_forward(
        state,
        SingleForwardRequest {
            message_id,
            target,
            target_kind: ChatTargetKind::Group,
            deadline_unix_ms,
        },
        payload.response_window_ms,
    )
    .await?;
    Ok(json!({
        "message_id": outcome.onebot_message_id.unwrap_or_default(),
    }))
}

/// `forward_friend_single_msg` -> 强制通过私聊白名单校验后原生单条转发至目标好友私聊
pub(crate) async fn exec_forward_friend_single_msg(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, AdapterError> {
    let message_id = parse_positive_message_id(&params)?;
    let mut payload: OneBotSendPayload = serde_json::from_value(params.clone()).map_err(|err| {
        AdapterError::InputRejected(format!("invalid forward_friend_single_msg params: {err}"))
    })?;
    if payload
        .nickname
        .as_deref()
        .is_none_or(|name| name.trim().is_empty())
    {
        payload.nickname = parse_optional_string(&params, "friend")
            .or_else(|| parse_optional_string(&params, "target"));
    }

    let target = resolve_send_target_private(state, &payload).await?;
    let deadline_unix_ms = resolve_effective_deadline_ms(&payload, now_utc_ms());
    let outcome = execute_single_forward(
        state,
        SingleForwardRequest {
            message_id,
            target,
            target_kind: ChatTargetKind::Private,
            deadline_unix_ms,
        },
        payload.response_window_ms,
    )
    .await?;
    Ok(json!({
        "message_id": outcome.onebot_message_id.unwrap_or_default(),
    }))
}

/// `get_recent_contact` -> 汇总左侧会话与跨会话离线消息，过滤越权会话后按最近活跃时间倒序输出
pub(crate) async fn exec_get_recent_contact(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, AdapterError> {
    let count = parse_optional_i64(&params, "count")?
        .unwrap_or(RECENT_CONTACT_DEFAULT_COUNT)
        .clamp(1, RECENT_CONTACT_MAX_COUNT) as usize;

    let sessions = match state.driver.list_sessions().await {
        Ok(list) => list,
        Err(AdapterError::WindowNotFound(_)) => Vec::new(),
        Err(err) => return Err(err),
    };
    let recent_messages = state
        .driver
        .query_messages_history(MessageHistoryFilter {
            session_name: String::new(),
            limit: RECENT_CONTACT_MAX_COUNT as usize,
            reverse_order: true,
            ..Default::default()
        })
        .await?;
    let cached_groups = state.driver.get_group_list(false).await.unwrap_or_default();
    let cached_friends = state
        .driver
        .get_friend_list(false)
        .await
        .unwrap_or_default();

    let mut merged: HashMap<(&'static str, String), RecentContactEntry> = HashMap::new();
    for rec in &recent_messages {
        let canon_key = canonical_contact_key(&rec.group_name);
        if canon_key.is_empty() {
            continue;
        }
        let allowed = match rec.chat_type {
            ChatTargetKind::Private => state
                .config
                .target
                .is_private_friend_allowed(&rec.group_name),
            ChatTargetKind::Group => {
                let members =
                    resolve_group_member_count(&rec.group_name, None, &sessions, &cached_groups);
                passes_group_guard(&state.config.target, &rec.group_name, members)
            }
        };
        if !allowed {
            continue;
        }
        let key = (rec.chat_type.as_str(), canon_key.clone());
        if merged.contains_key(&key) {
            continue;
        }
        let (group_id, user_id) = match rec.chat_type {
            ChatTargetKind::Group => (
                lookup_group_id(&rec.group_name, Some(rec.group_id), &cached_groups),
                None,
            ),
            ChatTargetKind::Private => (
                None,
                lookup_friend_id(&rec.group_name, Some(rec.user_id), &cached_friends),
            ),
        };
        let last_msg_time = (rec.captured_at_ms > 0).then_some((rec.captured_at_ms / 1000) as i64);
        merged.insert(
            key,
            RecentContactEntry {
                chat_type: rec.chat_type,
                peer_name: rec.group_name.clone(),
                group_id,
                user_id,
                last_msg_id: Some(rec.onebot_message_id),
                last_msg_time,
                last_msg_text: Some(rec.raw_message.clone()),
                unread_count: 0,
                sort_captured_ms: rec.captured_at_ms,
                sort_msg_id: rec.onebot_message_id,
                session_rank: usize::MAX,
            },
        );
    }

    for (rank, session) in sessions.iter().enumerate() {
        let canon_key = canonical_contact_key(&session.title);
        if canon_key.is_empty() {
            continue;
        }
        let chat_type = resolve_session_chat_kind(
            session,
            &state.config.target,
            &cached_groups,
            &recent_messages,
        );
        let allowed = match chat_type {
            ChatTargetKind::Private => state
                .config
                .target
                .is_private_friend_allowed(&session.title),
            ChatTargetKind::Group => {
                let members = resolve_group_member_count(
                    &session.title,
                    session.member_count_hint,
                    &sessions,
                    &cached_groups,
                );
                passes_group_guard(&state.config.target, &session.title, members)
            }
        };
        if !allowed {
            continue;
        }

        let key = (chat_type.as_str(), canon_key);
        if let Some(existing) = merged.get_mut(&key) {
            existing.unread_count = existing.unread_count.max(session.unread_count.unwrap_or(0));
            existing.session_rank = existing.session_rank.min(rank);
            if existing.last_msg_text.is_none() {
                existing.last_msg_text = session.preview.clone();
            }
            continue;
        }
        let (group_id, user_id) = match chat_type {
            ChatTargetKind::Group => (lookup_group_id(&session.title, None, &cached_groups), None),
            ChatTargetKind::Private => (
                None,
                lookup_friend_id(&session.title, None, &cached_friends),
            ),
        };
        merged.insert(
            key,
            RecentContactEntry {
                chat_type,
                peer_name: session.title.clone(),
                group_id,
                user_id,
                last_msg_id: None,
                last_msg_time: None,
                last_msg_text: session.preview.clone(),
                unread_count: session.unread_count.unwrap_or(0),
                sort_captured_ms: 0,
                sort_msg_id: 0,
                session_rank: rank,
            },
        );
    }

    let mut entries: Vec<RecentContactEntry> = merged.into_values().collect();
    entries.sort_by_key(|item| {
        (
            Reverse(item.sort_captured_ms),
            Reverse(item.sort_msg_id),
            item.session_rank,
        )
    });
    entries.truncate(count);

    let result: Vec<Value> = entries
        .into_iter()
        .map(|item| {
            json!({
                "chat_type": item.chat_type.as_str(),
                "peer_name": item.peer_name,
                "group_id": item.group_id,
                "user_id": item.user_id,
                "last_msg_id": item.last_msg_id,
                "last_msg_time": item.last_msg_time,
                "last_msg_text": item.last_msg_text,
                "unread_count": item.unread_count,
            })
        })
        .collect();
    Ok(Value::Array(result))
}

fn extract_obj_str<'a>(obj: &'a serde_json::Map<String, Value>, key: &str) -> Option<&'a str> {
    obj.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|val| !val.is_empty())
}

/// 判定单条审计事件是否允许对外暴露（剔除含非白名单私聊目标的记录，防止私聊用户名泄漏）
pub fn is_audit_record_allowed(target_cfg: &GroupTargetSpec, record: &AuditEventRecord) -> bool {
    let Ok(parsed) = serde_json::from_str::<Value>(&record.detail_json) else {
        return !record.event_kind.contains("private");
    };
    let Some(obj) = parsed.as_object() else {
        return !record.event_kind.contains("private");
    };

    if let Some(err_text) = extract_obj_str(obj, "error") {
        if let Some(after) = err_text.split("private chat target '").nth(1) {
            if let Some((target_name, _)) = after.split_once('\'') {
                if !target_cfg.is_private_friend_allowed(target_name) {
                    return false;
                }
            }
        }
    }

    let explicit_kind = extract_obj_str(obj, "chat_type")
        .or_else(|| extract_obj_str(obj, "target_kind"))
        .or_else(|| extract_obj_str(obj, "message_type"));
    let is_private_record = explicit_kind.is_some_and(|val| val.eq_ignore_ascii_case("private"))
        || record.event_kind == "private_whitelist_abort"
        || record.event_kind.contains("private")
        || obj.contains_key("expected_friend")
        || obj.contains_key("friend");

    if !is_private_record {
        return true;
    }

    let candidate_target = extract_obj_str(obj, "target")
        .or_else(|| extract_obj_str(obj, "peer_name"))
        .or_else(|| extract_obj_str(obj, "expected_friend"))
        .or_else(|| extract_obj_str(obj, "friend"))
        .or_else(|| extract_obj_str(obj, "nickname"));
    match candidate_target {
        Some(name) => target_cfg.is_private_friend_allowed(name),
        None => false,
    }
}

/// 查询并过滤非白名单私聊目标审计事件（供 OneBot `.get_audit_events` 与 REST `/api/audit-events` 共用）
pub(crate) async fn query_filtered_audit_events(
    state: &AppState,
    event_kind: Option<&str>,
    limit: usize,
) -> std::result::Result<Vec<AuditEventRecord>, AdapterError> {
    let clamped_limit = limit.clamp(1, AUDIT_EVENTS_MAX_LIMIT);
    let clean_kind = event_kind.map(str::trim).filter(|val| !val.is_empty());
    let mut events = state
        .driver
        .list_audit_events(clean_kind, AUDIT_EVENTS_MAX_LIMIT)
        .await?;
    events.retain(|rec| is_audit_record_allowed(&state.config.target, rec));
    events.truncate(clamped_limit);
    Ok(events)
}

/// `.get_audit_events` -> 按可选事件类型与条数上限分页检索脱敏后的持久化审计事件流水
pub(crate) async fn exec_get_audit_events(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, AdapterError> {
    let event_kind = parse_optional_string(&params, "event_kind")
        .or_else(|| parse_optional_string(&params, "kind"));
    let limit = parse_optional_i64(&params, "limit")?
        .unwrap_or(AUDIT_EVENTS_DEFAULT_LIMIT)
        .clamp(1, AUDIT_EVENTS_MAX_LIMIT as i64) as usize;
    let events = query_filtered_audit_events(state, event_kind.as_deref(), limit).await?;
    Ok(json!(events))
}

/// `.reset_security_breaker` -> 在发送锁保护下校验风控弹窗关闭状态并重置安全熔断器
pub(crate) async fn exec_reset_security_breaker(
    state: &AppState,
    _params: Value,
) -> std::result::Result<Value, AdapterError> {
    let _send_guard = state.send_gate.lock().await;
    let status = state.driver.reset_security_breaker().await?;
    Ok(json!(status))
}
