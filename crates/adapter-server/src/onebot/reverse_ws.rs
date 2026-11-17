//! OneBot v11 反向 WebSocket 客户端：支持标准身份请求头与受限指数退避重连状态机

use std::time::Duration;

use common_types::AdapterConfig;
use futures_util::StreamExt;
use tokio::task::JoinHandle;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::{http::Uri, ClientRequestBuilder, Message};

use crate::constants::{
    BEARER_PREFIX, HEADER_AUTHORIZATION, HEADER_USER_AGENT, HEADER_X_CLIENT_ROLE, HEADER_X_SELF_ID,
    MAX_REVERSE_RECONNECT_MS, MIN_REVERSE_RECONNECT_MS, REVERSE_WS_USER_AGENT,
};
use crate::onebot::handlers::resolve_effective_self_id;
use crate::onebot::ws_runner::{
    run_onebot_ws_session, WsEndpointRole, WsFrameEvent, WsMessageAdapter,
};
use crate::AppState;

impl WsMessageAdapter for Message {
    fn into_ws_event(self) -> WsFrameEvent {
        match self {
            Self::Text(text) => WsFrameEvent::Text(text.as_str().to_owned()),
            Self::Binary(data) => match String::from_utf8(data.to_vec()) {
                Ok(text) => WsFrameEvent::Text(text),
                Err(_) => WsFrameEvent::Ignore,
            },
            Self::Ping(payload) => WsFrameEvent::Ping(payload.to_vec()),
            Self::Close(_) => WsFrameEvent::Close,
            Self::Pong(_) | Self::Frame(_) => WsFrameEvent::Ignore,
        }
    }

    fn text_frame(text: String) -> Self {
        Self::text(text)
    }

    fn pong_frame(payload: Vec<u8>) -> Self {
        Self::Pong(payload.into())
    }

    fn close_frame() -> Self {
        Self::Close(None)
    }
}

/// 按 OneBot 独立配置优先、其次回退至全局服务端配置解析出站鉴权访问令牌
pub(crate) fn resolve_reverse_access_token(config: &AdapterConfig) -> Option<&str> {
    config
        .onebot
        .access_token
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .or_else(|| {
            config
                .server
                .access_token
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
        })
}

fn build_reverse_ws_request(
    uri: Uri,
    state: &AppState,
    self_id: i64,
    role: WsEndpointRole,
) -> ClientRequestBuilder {
    let mut builder = ClientRequestBuilder::new(uri)
        .with_header(HEADER_X_SELF_ID, self_id.to_string())
        .with_header(HEADER_X_CLIENT_ROLE, role.header_value())
        .with_header(HEADER_USER_AGENT, REVERSE_WS_USER_AGENT);

    if let Some(token) = resolve_reverse_access_token(&state.config) {
        builder = builder.with_header(HEADER_AUTHORIZATION, format!("{BEARER_PREFIX}{token}"));
    }

    builder
}

async fn run_reverse_ws_loop(state: AppState, target_url: String, role: WsEndpointRole) {
    let Ok(target_uri) = target_url.parse::<Uri>() else {
        tracing::error!(url = %target_url, "invalid reverse WS URL; skipping reconnect loop");
        return;
    };

    let base_delay_ms = state
        .config
        .onebot
        .reconnect_interval_ms
        .max(MIN_REVERSE_RECONNECT_MS);
    let mut current_delay_ms = base_delay_ms;

    loop {
        let self_id = resolve_effective_self_id(&state).await;
        let request = build_reverse_ws_request(target_uri.clone(), &state, self_id, role);
        let connect_result = tokio::select! {
            _ = state.wait_shutdown() => break,
            result = connect_async(request) => result,
        };

        match connect_result {
            Ok((ws_stream, _)) => {
                tracing::info!(
                    url = %target_url,
                    role = role.header_value(),
                    "OneBot reverse WebSocket connected"
                );
                current_delay_ms = base_delay_ms;
                let (ws_sender, ws_receiver) = ws_stream.split();
                let stopped = run_onebot_ws_session(
                    state.clone(),
                    ws_sender,
                    ws_receiver,
                    role,
                    "reverse-ws",
                )
                .await;
                if stopped {
                    break;
                }
                tracing::warn!(url = %target_url, "OneBot reverse WebSocket disconnected");
            }
            Err(err) => {
                tracing::warn!(url = %target_url, ?err, "OneBot reverse WebSocket connect failed");
            }
        }

        let sleep_duration = Duration::from_millis(current_delay_ms);
        tokio::select! {
            _ = state.wait_shutdown() => break,
            _ = tokio::time::sleep(sleep_duration) => {}
        }
        current_delay_ms = current_delay_ms
            .saturating_mul(2)
            .min(MAX_REVERSE_RECONNECT_MS);
    }
}

fn spawn_role_reverse_tasks(
    state: &AppState,
    urls: &[String],
    role: WsEndpointRole,
    out: &mut Vec<JoinHandle<()>>,
) {
    for raw in urls {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        let task_state = state.clone();
        let owned_url = trimmed.to_string();
        out.push(tokio::spawn(async move {
            run_reverse_ws_loop(task_state, owned_url, role).await;
        }));
    }
}

/// 为配置中的 Universal、API-only 与 Event-only 反向 WS 地址分别启动独立重连守护任务
pub fn spawn_reverse_ws_clients(state: AppState) -> Vec<JoinHandle<()>> {
    let mut handles = Vec::new();
    spawn_role_reverse_tasks(
        &state,
        &state.config.onebot.reverse_ws_urls,
        WsEndpointRole::Universal,
        &mut handles,
    );
    spawn_role_reverse_tasks(
        &state,
        &state.config.onebot.reverse_ws_api_urls,
        WsEndpointRole::ApiOnly,
        &mut handles,
    );
    spawn_role_reverse_tasks(
        &state,
        &state.config.onebot.reverse_ws_event_urls,
        WsEndpointRole::EventOnly,
        &mut handles,
    );
    handles
}
