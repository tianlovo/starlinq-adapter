//! 原生调度器策略与会话已读操作处理器模块：暴露 `/api/scheduler`、`/api/scheduler/policy` 与 `/api/sessions/read`

use axum::extract::State;
use axum::Json;
use common_types::{AdapterError, ChatTargetKind, SchedulerConfig};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::ApiResult;
use crate::onebot::handlers::{resolve_send_target_group, resolve_send_target_private};
use crate::onebot::payloads::OneBotSendPayload;
use crate::AppState;

/// `POST /api/sessions/read` 请求载荷，支持按群聊或私聊白名单好友标识定位会话并清除未读角标
#[derive(Debug, Default, Deserialize)]
pub struct MarkSessionReadRequest {
    #[serde(default)]
    pub target: Option<String>,
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
}

fn validate_scheduler_config(cfg: &SchedulerConfig) -> std::result::Result<(), AdapterError> {
    for (field, value) in [
        ("burst_window_ms", cfg.burst_window_ms),
        (
            "critical_response_window_ms",
            cfg.critical_response_window_ms,
        ),
        (
            "contextual_response_window_ms",
            cfg.contextual_response_window_ms,
        ),
        (
            "ordinary_response_window_ms",
            cfg.ordinary_response_window_ms,
        ),
        (
            "single_poll_burst_threshold",
            cfg.single_poll_burst_threshold as u64,
        ),
        ("hard_shed_queue_depth", cfg.hard_shed_queue_depth as u64),
        ("max_pending_events", cfg.max_pending_events as u64),
        ("group_quantum_messages", cfg.group_quantum_messages as u64),
    ] {
        if value == 0 {
            return Err(AdapterError::InputRejected(format!(
                "{field} must be greater than 0"
            )));
        }
    }
    if cfg.burst_exit_threshold > cfg.burst_enter_threshold {
        return Err(AdapterError::InputRejected(
            "burst_exit_threshold cannot exceed burst_enter_threshold".to_string(),
        ));
    }
    Ok(())
}

/// `GET /api/scheduler` -> 查询当前洪峰调度策略配置、实时遥测快照与多群监控保护参数
pub async fn get_scheduler(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let config = state.driver.scheduler_config().await?;
    let telemetry = state.driver.scheduler_telemetry().await?;
    Ok(Json(json!({
        "ok": true,
        "config": config,
        "telemetry": telemetry,
        "monitored_groups": state.config.target.effective_monitored_groups(),
        "allow_large_monitored_groups": state.config.target.allow_large_monitored_groups,
        "strict_below_members": state.config.target.strict_below_members,
    })))
}

/// `GET /api/scheduler/policy` -> 读取当前生效的调度器策略配置
pub async fn get_scheduler_policy(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let config = state.driver.scheduler_config().await?;
    Ok(Json(json!({
        "ok": true,
        "config": config,
    })))
}

/// `POST /api/scheduler/policy` -> 增量合并或全量更新调度器策略参数并校验合法边界
pub async fn update_scheduler_policy(
    State(state): State<AppState>,
    Json(patch): Json<Value>,
) -> ApiResult<Json<Value>> {
    let patch_map = patch.as_object().ok_or_else(|| {
        AdapterError::InputRejected("scheduler policy payload must be a JSON object".to_string())
    })?;
    let current = state.driver.scheduler_config().await?;
    let mut merged = serde_json::to_value(current).map_err(|err| {
        AdapterError::Io(std::io::Error::other(format!(
            "failed to serialize scheduler policy: {err}"
        )))
    })?;
    let merged_map = merged.as_object_mut().ok_or_else(|| {
        AdapterError::Io(std::io::Error::other(
            "serialized scheduler policy is not a JSON object",
        ))
    })?;
    for (key, value) in patch_map {
        merged_map.insert(key.clone(), value.clone());
    }
    let next_cfg: SchedulerConfig = serde_json::from_value(merged).map_err(|err| {
        AdapterError::InputRejected(format!("invalid scheduler policy parameters: {err}"))
    })?;
    validate_scheduler_config(&next_cfg)?;
    let updated = state.driver.update_scheduler_config(next_cfg).await?;
    Ok(Json(json!({
        "ok": true,
        "config": updated,
    })))
}

/// `POST /api/sessions/read` -> 校验群目标或私聊白名单后将指定会话标记为已读
pub async fn mark_session_read(
    State(state): State<AppState>,
    Json(req): Json<MarkSessionReadRequest>,
) -> ApiResult<Json<Value>> {
    let target = req
        .target
        .as_deref()
        .map(str::trim)
        .filter(|val| !val.is_empty())
        .map(ToOwned::to_owned);
    let group_name = req
        .group_name
        .as_deref()
        .map(str::trim)
        .filter(|val| !val.is_empty())
        .map(ToOwned::to_owned);
    let nickname = req
        .nickname
        .as_deref()
        .map(str::trim)
        .filter(|val| !val.is_empty())
        .map(ToOwned::to_owned);

    let has_any_identifier = target.is_some()
        || group_name.is_some()
        || req.group_id.as_ref().is_some_and(|v| !v.is_null())
        || req.user_id.as_ref().is_some_and(|v| !v.is_null())
        || nickname.is_some();
    if !has_any_identifier {
        return Err(AdapterError::InputRejected(
            "target, group_name, group_id, user_id or nickname is required".to_string(),
        )
        .into());
    }

    let kind = match req.target_kind {
        Some(explicit) => explicit,
        None => {
            if req.user_id.as_ref().is_some_and(|v| !v.is_null()) || nickname.is_some() {
                ChatTargetKind::Private
            } else {
                ChatTargetKind::Group
            }
        }
    };

    let resolved_target = match kind {
        ChatTargetKind::Group => {
            let payload = OneBotSendPayload {
                group_name: group_name.or(target),
                group_id: req.group_id,
                ..Default::default()
            };
            resolve_send_target_group(&state, &payload).await?
        }
        ChatTargetKind::Private => {
            let payload = OneBotSendPayload {
                user_id: req.user_id,
                nickname: nickname.or(target),
                group_name,
                ..Default::default()
            };
            resolve_send_target_private(&state, &payload).await?
        }
    };

    let _send_guard = state.send_gate.lock().await;
    state
        .driver
        .mark_session_read(&resolved_target, kind)
        .await?;
    Ok(Json(json!({
        "ok": true,
        "target": resolved_target,
        "target_kind": kind,
    })))
}
