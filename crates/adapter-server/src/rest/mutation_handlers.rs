//! 原生变更操作与事件推流模块：处理唤醒 QQ、切群、发送消息、优雅关闭及 `/ws/events` 订阅

use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::IntoResponse;
use axum::Json;
use common_types::{
    canonical_group_names_match, AdapterError, AdapterStatus, ChatTargetKind, GroupTargetSpec,
    SendOutcome, SendRequest, WindowInfo,
};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::broadcast::error::RecvError;

use crate::constants::SHUTDOWN_DELAY_MS;
use crate::error::ApiResult;
use crate::onebot::handlers::{ensure_within_outbound_deadline, now_utc_ms};
use crate::onebot::payloads::{deserialize_lenient_opt_u64, resolve_deadline_bounds_ms};
use crate::rest::EnsureQqQuery;
use crate::AppState;

/// `POST /api/send` 请求载荷，兼容原生 `SendRequest` 并支持可选相对 `response_window_ms` 约束
#[derive(Debug, Deserialize)]
pub struct RestSendRequest {
    #[serde(flatten)]
    pub request: SendRequest,
    #[serde(default, deserialize_with = "deserialize_lenient_opt_u64")]
    pub response_window_ms: Option<u64>,
}

/// `POST /api/ensure-qq?force_wake=<opt_bool>` -> 唤醒或启动本地 QQNT 窗口
pub async fn ensure_qq(
    State(state): State<AppState>,
    Query(query): Query<EnsureQqQuery>,
) -> ApiResult<Json<Vec<WindowInfo>>> {
    let list = state.driver.ensure_qq_window_ext(query.force_wake).await?;
    Ok(Json(list))
}

/// `POST /api/shutdown` -> 异步触发优雅停机信号并立即应答调用方
pub async fn shutdown(State(state): State<AppState>) -> Json<Value> {
    let shutdown_state = state.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(SHUTDOWN_DELAY_MS)).await;
        shutdown_state.trigger_shutdown();
    });
    Json(json!({ "ok": true, "shutting_down": true }))
}

/// `POST /api/open-group` -> 在发送通道互斥锁保护下执行安全守卫校验并切换至目标群聊
pub async fn open_group(
    State(state): State<AppState>,
    Json(spec): Json<GroupTargetSpec>,
) -> ApiResult<Json<AdapterStatus>> {
    let _send_guard = state.send_gate.lock().await;
    let updated_status = state.driver.open_group(spec).await?;
    Ok(Json(updated_status))
}

/// `POST /api/send` -> 根据目标类型向群聊或白名单好友私聊发送消息并返回结果报告
pub async fn send(
    State(state): State<AppState>,
    Json(payload): Json<RestSendRequest>,
) -> ApiResult<Json<SendOutcome>> {
    let mut request = payload.request;
    let window_ms_hint = payload.response_window_ms;
    let started_ms = now_utc_ms();
    let deadline_unix_ms =
        resolve_deadline_bounds_ms(request.deadline_unix_ms, window_ms_hint, started_ms);
    request.deadline_unix_ms = deadline_unix_ms;

    // 抢占发送互斥锁前先行预检出站时间窗，避免已过期指令空转等待
    ensure_within_outbound_deadline(
        &request.group,
        deadline_unix_ms,
        window_ms_hint,
        started_ms,
        &state,
    )?;
    let _send_guard = state.send_gate.lock().await;
    // 拿到发送互斥锁后再次校验窗口，阻断排队阻塞后的滞后出站回复
    ensure_within_outbound_deadline(
        &request.group,
        deadline_unix_ms,
        window_ms_hint,
        started_ms,
        &state,
    )?;

    let outcome = match request.target_kind {
        ChatTargetKind::Private => {
            let current_status = state.driver.status().await?;
            let already_open = current_status.attached_window.is_some()
                && current_status.active_chat_kind == Some(ChatTargetKind::Private)
                && current_status
                    .active_group
                    .as_deref()
                    .is_some_and(|active| canonical_group_names_match(active, &request.group));
            if !already_open {
                state.driver.open_private(&request.group).await?;
            }
            state.driver.send_private_message(request).await?
        }
        ChatTargetKind::Group => state.driver.send_group_message(request).await?,
    };
    if !outcome.ok {
        return Err(AdapterError::UiaFailure(format!(
            "send not confirmed in chat UI: {}",
            outcome.detail
        ))
        .into());
    }
    state.record_message_activity();
    Ok(Json(outcome))
}

/// `GET /ws/events` -> 建立原生 `AdapterEvent` 实时事件推流 WebSocket 连接
pub async fn ws_events(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_ws_events(socket, state))
}

async fn handle_ws_events(socket: WebSocket, state: AppState) {
    let mut event_receiver = state.driver.subscribe_events();
    let (mut ws_sender, mut ws_receiver) = socket.split();
    let shutdown = state.wait_shutdown();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            _ = &mut shutdown => {
                let _ = ws_sender.send(Message::Close(None)).await;
                break;
            }
            recv_res = event_receiver.recv() => {
                match recv_res {
                    Ok(event) => {
                        match serde_json::to_string(&event) {
                            Ok(serialized) => {
                                if ws_sender.send(Message::Text(serialized.into())).await.is_err() {
                                    break;
                                }
                            }
                            Err(err) => {
                                tracing::warn!(?err, "failed to serialize AdapterEvent");
                            }
                        }
                    }
                    Err(RecvError::Lagged(skipped)) => {
                        tracing::warn!(skipped, "/ws/events receiver lagged");
                    }
                    Err(RecvError::Closed) => break,
                }
            }
            incoming = ws_receiver.next() => {
                match incoming {
                    Some(Ok(Message::Ping(payload))) => {
                        if ws_sender.send(Message::Pong(payload)).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(_)) => break,
                    _ => {}
                }
            }
        }
    }
}
