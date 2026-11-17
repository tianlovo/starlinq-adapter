//! OneBot v11 WebSocket 会话共享引擎：统一封装多角色权限控制、生命周期心跳、事件广播与动作分发

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use common_types::{AdapterError, AdapterEvent};
use futures_util::{Sink, SinkExt, Stream, StreamExt};
use serde_json::{json, Value};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{mpsc, Semaphore};
use tokio::task::JoinSet;

use crate::constants::{
    HEADER_X_CLIENT_ROLE_API, HEADER_X_CLIENT_ROLE_EVENT, HEADER_X_CLIENT_ROLE_UNIVERSAL,
    MAX_WS_INFLIGHT_ACTIONS, MIN_HEARTBEAT_INTERVAL_MS, ONEBOT_META_HEARTBEAT,
    ONEBOT_META_LIFECYCLE, ONEBOT_POST_TYPE_META, ONEBOT_STATUS_FAILED, ONEBOT_SUB_TYPE_CONNECT,
};
use crate::error::map_adapter_error;
use crate::onebot::handlers::{
    build_onebot_stat_payload, execute_onebot_ws_envelope, resolve_effective_self_id,
};
use crate::onebot::payloads::{translate_adapter_event_enriched, OneBotWsAction};
use crate::AppState;

/// OneBot v11 WebSocket 端点角色模式（支持 Universal、仅 API 调用、仅事件推送）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WsEndpointRole {
    Universal,
    ApiOnly,
    EventOnly,
}

impl WsEndpointRole {
    /// 判断该端点角色是否对外推送生命周期、心跳与实时聊天通知事件
    pub fn pushes_events(self) -> bool {
        matches!(self, Self::Universal | Self::EventOnly)
    }

    /// 判断该端点角色是否允许接收并执行双向 API 动作调用帧
    pub fn accepts_api(self) -> bool {
        matches!(self, Self::Universal | Self::ApiOnly)
    }

    /// 返回反向 WebSocket 握手头 `X-Client-Role` 对应的标准协议字符串值
    pub fn header_value(self) -> &'static str {
        match self {
            Self::Universal => HEADER_X_CLIENT_ROLE_UNIVERSAL,
            Self::ApiOnly => HEADER_X_CLIENT_ROLE_API,
            Self::EventOnly => HEADER_X_CLIENT_ROLE_EVENT,
        }
    }
}

/// 抽象 WebSocket 帧解码事件，屏蔽正向 Axum 与反向 Tungstenite 的帧类型差异
pub(crate) enum WsFrameEvent {
    Text(String),
    Ping(Vec<u8>),
    Close,
    Ignore,
}

/// WebSocket 传输消息适配器契约（Adapter）：统一正反向连接的文本、Pong 与关闭帧编解码
pub(crate) trait WsMessageAdapter: Sized {
    /// 将底层库消息帧转换为内部无关传输层的 `WsFrameEvent`
    fn into_ws_event(self) -> WsFrameEvent;
    /// 构造文本推送帧
    fn text_frame(text: String) -> Self;
    /// 构造心跳响应 Pong 帧
    fn pong_frame(payload: Vec<u8>) -> Self;
    /// 构造规范关闭连接帧
    fn close_frame() -> Self;
}

fn build_ws_input_error(reason: String, echo: Option<Value>) -> Value {
    let err = AdapterError::InputRejected(reason);
    let (_, error_code, retcode) = map_adapter_error(&err);
    json!({
        "status": ONEBOT_STATUS_FAILED,
        "retcode": retcode,
        "error": err.to_string(),
        "code": error_code,
        "data": Value::Null,
        "echo": echo,
    })
}

fn parse_ws_action_or_error_reply(raw_text: &str) -> std::result::Result<OneBotWsAction, Value> {
    let parsed_value: Value = match serde_json::from_str(raw_text) {
        Ok(val) => val,
        Err(err) => {
            return Err(build_ws_input_error(
                format!("invalid WebSocket OneBot JSON frame: {err}"),
                None,
            ));
        }
    };
    let echo = parsed_value.get("echo").cloned();
    let action_request = match serde_json::from_value::<OneBotWsAction>(parsed_value) {
        Ok(req) => req,
        Err(err) => {
            return Err(build_ws_input_error(
                format!("invalid WebSocket OneBot action frame: {err}"),
                echo,
            ));
        }
    };
    if action_request.action.trim().is_empty() {
        return Err(build_ws_input_error(
            "WebSocket OneBot action field must not be empty".to_string(),
            action_request.echo,
        ));
    }
    Ok(action_request)
}

async fn handle_ws_action(
    state: &AppState,
    action_request: OneBotWsAction,
    role: WsEndpointRole,
) -> Value {
    if !role.accepts_api() {
        let err = AdapterError::UnsupportedAction {
            action: action_request.action,
            reason: "event-only WebSocket endpoint does not accept API action calls".to_string(),
        };
        let (_, error_code, retcode) = map_adapter_error(&err);
        return json!({
            "status": ONEBOT_STATUS_FAILED,
            "retcode": retcode,
            "error": err.to_string(),
            "code": error_code,
            "data": Value::Null,
            "echo": action_request.echo,
        });
    }
    execute_onebot_ws_envelope(
        state,
        &action_request.action,
        action_request.params,
        action_request.echo,
    )
    .await
}

/// 运行标准化 OneBot v11 双向/单向 WebSocket 会话主循环，返回 `true` 表示因停机信号退出
pub(crate) async fn run_onebot_ws_session<M, S, R, E>(
    state: AppState,
    ws_sender: S,
    ws_receiver: R,
    role: WsEndpointRole,
    channel_label: &str,
) -> bool
where
    M: WsMessageAdapter,
    S: Sink<M, Error = E> + Unpin,
    R: Stream<Item = std::result::Result<M, E>> + Unpin,
{
    let disconnect_counter = state.clone();
    let clean_shutdown =
        run_onebot_ws_session_inner(state, ws_sender, ws_receiver, role, channel_label).await;
    if !clean_shutdown {
        disconnect_counter.record_ws_disconnect();
    }
    clean_shutdown
}

async fn run_onebot_ws_session_inner<M, S, R, E>(
    state: AppState,
    mut ws_sender: S,
    mut ws_receiver: R,
    role: WsEndpointRole,
    channel_label: &str,
) -> bool
where
    M: WsMessageAdapter,
    S: Sink<M, Error = E> + Unpin,
    R: Stream<Item = std::result::Result<M, E>> + Unpin,
{
    let mut event_receiver = state.driver.subscribe_events();
    if role.pushes_events() {
        let initial_self_id = resolve_effective_self_id(&state).await;
        let lifecycle_payload = json!({
            "time": Utc::now().timestamp(),
            "self_id": initial_self_id,
            "post_type": ONEBOT_POST_TYPE_META,
            "meta_event_type": ONEBOT_META_LIFECYCLE,
            "sub_type": ONEBOT_SUB_TYPE_CONNECT,
        });
        if ws_sender
            .send(M::text_frame(lifecycle_payload.to_string()))
            .await
            .is_err()
        {
            return false;
        }
    }

    let heartbeat_ms = state
        .config
        .onebot
        .heartbeat_interval_ms
        .max(MIN_HEARTBEAT_INTERVAL_MS);
    let mut heartbeat = tokio::time::interval(Duration::from_millis(heartbeat_ms));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // 先消耗首次立即触发的节拍，避免刚推送 connect 就紧跟着发送心跳包
    heartbeat.tick().await;

    // 使用有界异步队列与 JoinSet 并行分发动作，避免慢动作阻塞 Ping/Pong 或其他事件响应
    let (reply_tx, mut reply_rx) = mpsc::channel::<Value>(MAX_WS_INFLIGHT_ACTIONS * 2);
    let action_sem = Arc::new(Semaphore::new(MAX_WS_INFLIGHT_ACTIONS));
    let mut action_tasks = JoinSet::new();

    let shutdown = state.wait_shutdown();
    tokio::pin!(shutdown);

    loop {
        while action_tasks.try_join_next().is_some() {}
        tokio::select! {
            _ = &mut shutdown => {
                let _ = ws_sender.send(M::close_frame()).await;
                return true;
            }
            Some(reply_payload) = reply_rx.recv() => {
                if ws_sender
                    .send(M::text_frame(reply_payload.to_string()))
                    .await
                    .is_err()
                {
                    return false;
                }
            }
            recv_res = event_receiver.recv(), if role.pushes_events() => {
                match recv_res {
                    Ok(event) => {
                        if matches!(
                            event,
                            AdapterEvent::GroupMessage(_)
                                | AdapterEvent::PrivateMessage(_)
                                | AdapterEvent::MessageSent(_)
                        ) {
                            state.record_message_activity();
                        }
                        let current_self_id = resolve_effective_self_id(&state).await;
                        let Some(event_json) =
                            translate_adapter_event_enriched(&state, current_self_id, &event).await
                        else {
                            continue;
                        };
                        if ws_sender
                            .send(M::text_frame(event_json.to_string()))
                            .await
                            .is_err()
                        {
                            return false;
                        }
                    }
                    Err(RecvError::Lagged(skipped)) => {
                        tracing::warn!(skipped, channel = channel_label, "OneBot WS receiver lagged");
                    }
                    Err(RecvError::Closed) => return false,
                }
            }
            _ = heartbeat.tick(), if role.pushes_events() => {
                // 优先读取无锁缓存状态，避免长耗时 UIA 发送阻塞 WebSocket 心跳保活
                let status_opt = if let Some(st) = state.driver.cached_status() {
                    Some(st)
                } else {
                    state.driver.status().await.ok()
                };
                let good = status_opt
                    .as_ref()
                    .is_some_and(|status| !status.security_breaker_tripped);
                let telemetry = state
                    .driver
                    .scheduler_telemetry()
                    .await
                    .unwrap_or_default();
                let current_self_id = resolve_effective_self_id(&state).await;
                let heartbeat_payload = json!({
                    "time": Utc::now().timestamp(),
                    "self_id": current_self_id,
                    "post_type": ONEBOT_POST_TYPE_META,
                    "meta_event_type": ONEBOT_META_HEARTBEAT,
                    "status": {
                        "online": true,
                        "good": good,
                        "stat": build_onebot_stat_payload(status_opt.as_ref(), &telemetry, &state),
                        "scheduler": telemetry,
                    },
                    "interval": heartbeat_ms,
                });
                if ws_sender
                    .send(M::text_frame(heartbeat_payload.to_string()))
                    .await
                    .is_err()
                {
                    return false;
                }
            }
            incoming = ws_receiver.next() => {
                match incoming {
                    Some(Ok(raw_frame)) => match raw_frame.into_ws_event() {
                        WsFrameEvent::Ping(payload) => {
                            if ws_sender.send(M::pong_frame(payload)).await.is_err() {
                                return false;
                            }
                        }
                        WsFrameEvent::Text(text) => {
                            match parse_ws_action_or_error_reply(&text) {
                                Ok(action_request) => match action_sem.clone().try_acquire_owned() {
                                    Ok(permit) => {
                                        let task_state = state.clone();
                                        let tx = reply_tx.clone();
                                        action_tasks.spawn(async move {
                                            let _permit = permit;
                                            let reply =
                                                handle_ws_action(&task_state, action_request, role)
                                                    .await;
                                            let _ = tx.send(reply).await;
                                        });
                                    }
                                    Err(_) => {
                                        let busy_reply = build_ws_input_error(
                                            "too many concurrent WebSocket OneBot actions".to_string(),
                                            action_request.echo,
                                        );
                                        if ws_sender
                                            .send(M::text_frame(busy_reply.to_string()))
                                            .await
                                            .is_err()
                                        {
                                            return false;
                                        }
                                    }
                                },
                                Err(err_reply) => {
                                    if ws_sender
                                        .send(M::text_frame(err_reply.to_string()))
                                        .await
                                        .is_err()
                                    {
                                        return false;
                                    }
                                }
                            }
                        }
                        WsFrameEvent::Close => return false,
                        WsFrameEvent::Ignore => {}
                    },
                    Some(Err(_)) | None => return false,
                }
            }
        }
    }
}
