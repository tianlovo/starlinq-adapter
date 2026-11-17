//! 自测专用临时反向 WebSocket 服务端：基于 `tokio_tungstenite::accept_hdr_async` 验证握手头并交互测试

use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_tungstenite::accept_hdr_async;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

use crate::constants::{HEADER_AUTHORIZATION, HEADER_X_CLIENT_ROLE, HEADER_X_SELF_ID};

/// 反向 WebSocket 握手期间服务端提取到的身份认证请求头快照
#[derive(Debug, Default)]
pub struct ReverseHandshakeInfo {
    pub self_id: Option<String>,
    pub client_role: Option<String>,
    pub authorization: Option<String>,
}

/// 已经完成升级握手的反向 WebSocket 客户端句柄
pub struct AcceptedReverseClient {
    pub headers: ReverseHandshakeInfo,
    pub ws_stream: WebSocketStream<TcpStream>,
}

/// 临时本地反向 WebSocket 监听服务，用于接收 `adapter-server` 主动发起的反向连接
pub struct MockReverseWsServer {
    pub url: String,
    accept_rx: oneshot::Receiver<AcceptedReverseClient>,
    accept_task: JoinHandle<()>,
}

impl MockReverseWsServer {
    /// 在回环临时端口绑定监听并等待 `adapter-server` 发起反向 WebSocket 建连
    // `accept_hdr_async` 的回调错误类型 `ErrorResponse` 体积固定较大，属第三方 trait 约束
    #[allow(clippy::result_large_err)]
    pub async fn spawn() -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .context("failed to bind MockReverseWsServer")?;
        let listen_addr = listener.local_addr()?;
        let url = format!("ws://{listen_addr}/onebot");
        let (accept_tx, accept_rx) = oneshot::channel();

        let accept_task = tokio::spawn(async move {
            let Ok((tcp_stream, _)) = listener.accept().await else {
                return;
            };
            let mut captured = ReverseHandshakeInfo::default();
            let callback = |request: &Request, response: Response| {
                captured.self_id = request
                    .headers()
                    .get(HEADER_X_SELF_ID)
                    .and_then(|val| val.to_str().ok())
                    .map(ToOwned::to_owned);
                captured.client_role = request
                    .headers()
                    .get(HEADER_X_CLIENT_ROLE)
                    .and_then(|val| val.to_str().ok())
                    .map(ToOwned::to_owned);
                captured.authorization = request
                    .headers()
                    .get(HEADER_AUTHORIZATION)
                    .and_then(|val| val.to_str().ok())
                    .map(ToOwned::to_owned);
                Ok(response)
            };
            if let Ok(ws_stream) = accept_hdr_async(tcp_stream, callback).await {
                let _ = accept_tx.send(AcceptedReverseClient {
                    headers: captured,
                    ws_stream,
                });
            }
        });

        Ok(Self {
            url,
            accept_rx,
            accept_task,
        })
    }

    /// 等待被测服务完成反向建连并返回可收发帧的连接句柄
    pub async fn wait_connected(self, timeout: Duration) -> Result<AcceptedReverseClient> {
        match tokio::time::timeout(timeout, self.accept_rx).await {
            Ok(Ok(client)) => Ok(client),
            Ok(Err(_)) => {
                self.accept_task.abort();
                bail!("MockReverseWsServer accept task dropped before handshake completed")
            }
            Err(_) => {
                self.accept_task.abort();
                bail!("timed out waiting for adapter-server reverse WebSocket connection")
            }
        }
    }

    /// 主动中止后台监听任务
    pub fn abort(&self) {
        self.accept_task.abort();
    }
}

/// 从已接受的反向 WebSocket 流中读取下一帧 JSON 文本消息
pub async fn recv_reverse_text_timeout(
    ws_stream: &mut WebSocketStream<TcpStream>,
    timeout: Duration,
) -> Result<Option<String>> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(None);
        }
        match tokio::time::timeout(remaining, ws_stream.next()).await {
            Err(_) | Ok(None) => return Ok(None),
            Ok(Some(Err(err))) => return Err(err).context("reverse WS receive error"),
            Ok(Some(Ok(Message::Text(text)))) => return Ok(Some(text.as_str().to_owned())),
            Ok(Some(Ok(Message::Ping(payload)))) => {
                ws_stream
                    .send(Message::Pong(payload))
                    .await
                    .context("failed sending reverse WS Pong")?;
            }
            Ok(Some(Ok(Message::Close(_)))) => return Ok(None),
            Ok(Some(Ok(_))) => continue,
        }
    }
}

/// 通过反向 WebSocket 下发 OneBot v11 动作调用并等待匹配 `echo` 的响应帧
pub async fn call_reverse_action(
    ws_stream: &mut WebSocketStream<TcpStream>,
    action: &str,
    params: Value,
    echo: &str,
) -> Result<Value> {
    let request_payload = json!({
        "action": action,
        "params": params,
        "echo": echo,
    });
    ws_stream
        .send(Message::text(request_payload.to_string()))
        .await
        .with_context(|| format!("failed sending reverse WS action '{action}'"))?;

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let Some(frame) = recv_reverse_text_timeout(ws_stream, remaining).await? else {
            bail!("timed out waiting for reverse WS reply for '{action}'");
        };
        let parsed: Value = serde_json::from_str(&frame)?;
        if parsed.get("echo").and_then(Value::as_str) == Some(echo) {
            return Ok(parsed);
        }
    }
}
