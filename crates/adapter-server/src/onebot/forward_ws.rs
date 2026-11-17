//! OneBot v11 正向 WebSocket 服务端接入点：提供 Universal、API-only 与 Event-only 三角色入口

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::IntoResponse;
use futures_util::StreamExt;

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
            Self::Pong(_) => WsFrameEvent::Ignore,
        }
    }

    fn text_frame(text: String) -> Self {
        Self::Text(text.into())
    }

    fn pong_frame(payload: Vec<u8>) -> Self {
        Self::Pong(payload.into())
    }

    fn close_frame() -> Self {
        Self::Close(None)
    }
}

/// `GET /onebot/v11/ws` 与 `/ws` -> 标准双向通用 OneBot v11 正向 WebSocket 会话
pub async fn ws_onebot(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| {
        handle_ws_onebot(socket, state, WsEndpointRole::Universal, "forward-ws")
    })
}

/// `GET /onebot/v11/ws/api` 与 `/onebot/v11/api` -> 仅接收动作请求的 API-only 正向 WebSocket 会话
pub async fn ws_onebot_api(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| {
        handle_ws_onebot(socket, state, WsEndpointRole::ApiOnly, "forward-ws-api")
    })
}

/// `GET /onebot/v11/ws/event`、`/onebot/v11/event` 与 `/ws/event` -> 仅推送事件的 Event-only 正向 WS
pub async fn ws_onebot_event(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| {
        handle_ws_onebot(socket, state, WsEndpointRole::EventOnly, "forward-ws-event")
    })
}

async fn handle_ws_onebot(
    socket: WebSocket,
    state: AppState,
    role: WsEndpointRole,
    channel_label: &str,
) {
    let (ws_sender, ws_receiver) = socket.split();
    let _ = run_onebot_ws_session(state, ws_sender, ws_receiver, role, channel_label).await;
}
