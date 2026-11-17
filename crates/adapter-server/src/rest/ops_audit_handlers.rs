//! 原生运维、单条消息转发、审计检索与缓存清理 REST 处理器模块

use axum::extract::{Query, State};
use axum::Json;
use common_types::{
    AdapterError, AdapterStatus, AuditEventRecord, ChatTargetKind, SendOutcome,
    SingleForwardRequest,
};
use serde::Deserialize;
use serde_json::Value;

use crate::error::ApiResult;
use crate::onebot::extended_handlers::clean_runtime_caches;
use crate::onebot::handlers::{now_utc_ms, resolve_send_target_group, resolve_send_target_private};
use crate::onebot::payloads::{
    deserialize_lenient_opt_u64, resolve_deadline_bounds_ms, OneBotSendPayload,
};
use crate::onebot::wave6_handlers::{execute_single_forward, query_filtered_audit_events};
use crate::AppState;

const REST_AUDIT_DEFAULT_LIMIT: usize = 50;

/// `POST /api/forward-msg` 请求载荷（支持群号、群名、好友昵称、用户号及可选超时窗口）
#[derive(Debug, Default, Deserialize)]
pub struct RestForwardMsgRequest {
    pub message_id: i64,
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub group: Option<String>,
    #[serde(default)]
    pub group_name: Option<String>,
    #[serde(default)]
    pub group_id: Option<Value>,
    #[serde(default)]
    pub user_id: Option<Value>,
    #[serde(default)]
    pub nickname: Option<String>,
    #[serde(default)]
    pub target_kind: Option<ChatTargetKind>,
    #[serde(default, deserialize_with = "deserialize_lenient_opt_u64")]
    pub deadline_unix_ms: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_lenient_opt_u64")]
    pub response_window_ms: Option<u64>,
}

/// `GET /api/audit-events` 查询参数载荷
#[derive(Debug, Default, Deserialize)]
pub struct AuditEventsQuery {
    #[serde(default)]
    pub event_kind: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
}

fn non_empty_trimmed(raw: Option<String>) -> Option<String> {
    raw.and_then(|val| {
        let trimmed = val.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    })
}

/// `POST /api/forward-msg` -> 校验权限与发送窗后将单条历史消息原生转发至群聊或白名单好友
pub async fn forward_msg(
    State(state): State<AppState>,
    Json(payload): Json<RestForwardMsgRequest>,
) -> ApiResult<Json<SendOutcome>> {
    if payload.message_id <= 0 {
        return Err(AdapterError::InputRejected(
            "message_id must be a positive integer".to_string(),
        )
        .into());
    }
    let target_opt = non_empty_trimmed(payload.target);
    let group_opt = non_empty_trimmed(payload.group);
    let group_name_opt = non_empty_trimmed(payload.group_name);
    let nickname_opt = non_empty_trimmed(payload.nickname);

    let target_kind = match payload.target_kind {
        Some(kind) => kind,
        None => {
            if payload.user_id.as_ref().is_some_and(|v| !v.is_null()) || nickname_opt.is_some() {
                ChatTargetKind::Private
            } else {
                ChatTargetKind::Group
            }
        }
    };

    let target = match target_kind {
        ChatTargetKind::Private => {
            let ob_payload = OneBotSendPayload {
                user_id: payload.user_id,
                nickname: nickname_opt.or(target_opt),
                group_name: group_name_opt.or(group_opt),
                ..Default::default()
            };
            resolve_send_target_private(&state, &ob_payload).await?
        }
        ChatTargetKind::Group => {
            let ob_payload = OneBotSendPayload {
                group_name: group_name_opt.or(group_opt).or(target_opt),
                group_id: payload.group_id,
                ..Default::default()
            };
            resolve_send_target_group(&state, &ob_payload).await?
        }
    };

    let deadline_unix_ms = resolve_deadline_bounds_ms(
        payload.deadline_unix_ms,
        payload.response_window_ms,
        now_utc_ms(),
    );
    let outcome = execute_single_forward(
        &state,
        SingleForwardRequest {
            message_id: payload.message_id,
            target,
            target_kind,
            deadline_unix_ms,
        },
        payload.response_window_ms,
    )
    .await?;
    Ok(Json(outcome))
}

/// `GET /api/audit-events` -> 分页检索已脱敏的持久化安全审计流水记录
pub async fn audit_events(
    State(state): State<AppState>,
    Query(query): Query<AuditEventsQuery>,
) -> ApiResult<Json<Vec<AuditEventRecord>>> {
    let event_kind = non_empty_trimmed(query.event_kind).or_else(|| non_empty_trimmed(query.kind));
    let limit = query.limit.unwrap_or(REST_AUDIT_DEFAULT_LIMIT);
    let events = query_filtered_audit_events(&state, event_kind.as_deref(), limit).await?;
    Ok(Json(events))
}

/// `POST /api/security/reset` -> 在发送锁保护下核验风控弹窗已关闭并重置安全熔断器
pub async fn reset_security(State(state): State<AppState>) -> ApiResult<Json<AdapterStatus>> {
    let _send_guard = state.send_gate.lock().await;
    let status = state.driver.reset_security_breaker().await?;
    Ok(Json(status))
}

/// `POST /api/cache/clean` -> 串行清理 `.runtime/media` 与 `.runtime/staging_uploads` 运行缓存
pub async fn clean_cache(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let summary = clean_runtime_caches(&state).await?;
    Ok(Json(summary))
}
