//! 基于 `tokio-tungstenite` 的 WebSocket 客户端适配器：用于正向 `/onebot/v11/ws` 联调与自测

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::{http::Uri, ClientRequestBuilder, Message};
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

use crate::constants::HEADER_AUTHORIZATION;

/// 正向 OneBot v11 WebSocket 测试与诊断客户端封装
pub struct WsClient {
    stream: WebSocketStream<MaybeTlsStream<TcpStream>>,
}

impl WsClient {
    /// 连接到指定服务端地址和路径，可选附加 Bearer 鉴权请求头
    pub async fn connect(listen_addr: SocketAddr, path: &str, token: Option<&str>) -> Result<Self> {
        let normalized_path = if path.starts_with('/') {
            path.to_string()
        } else {
            format!("/{path}")
        };
        let ws_url = format!("ws://{listen_addr}{normalized_path}");
        let uri: Uri = ws_url
            .parse()
            .with_context(|| format!("invalid WebSocket URI '{ws_url}'"))?;
        let mut request_builder = ClientRequestBuilder::new(uri);
        if let Some(bearer_token) = token.map(str::trim).filter(|val| !val.is_empty()) {
            request_builder =
                request_builder.with_header(HEADER_AUTHORIZATION, format!("Bearer {bearer_token}"));
        }

        let (stream, _) = connect_async(request_builder)
            .await
            .with_context(|| format!("failed to connect WebSocket at {ws_url}"))?;
        Ok(Self { stream })
    }

    /// 在给定超时时间内读取下一条文本帧（自动响应 Ping 并跳过其他非文本帧）
    pub async fn recv_text_timeout(&mut self, timeout: Duration) -> Result<Option<String>> {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            match tokio::time::timeout(remaining, self.stream.next()).await {
                Err(_) => return Ok(None),
                Ok(None) => return Ok(None),
                Ok(Some(Err(err))) => return Err(err).context("WebSocket receive error"),
                Ok(Some(Ok(Message::Text(text)))) => return Ok(Some(text.as_str().to_owned())),
                Ok(Some(Ok(Message::Ping(payload)))) => {
                    self.stream
                        .send(Message::Pong(payload))
                        .await
                        .context("failed sending WebSocket Pong")?;
                }
                Ok(Some(Ok(Message::Close(_)))) => return Ok(None),
                Ok(Some(Ok(_))) => continue,
            }
        }
    }

    /// 通过 WebSocket 信道发送 OneBot v11 动作并等待匹配的 `echo` 回包
    #[allow(dead_code)]
    pub async fn call_action(&mut self, action: &str, params: Value, echo: &str) -> Result<Value> {
        let payload = json!({
            "action": action,
            "params": params,
            "echo": echo,
        });
        self.stream
            .send(Message::text(payload.to_string()))
            .await
            .with_context(|| format!("failed sending WS action '{action}'"))?;

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let Some(frame) = self.recv_text_timeout(remaining).await? else {
                anyhow::bail!("timed out waiting for WS reply for action '{action}'");
            };
            let parsed: Value = serde_json::from_str(&frame)?;
            if parsed.get("echo").and_then(Value::as_str) == Some(echo) {
                return Ok(parsed);
            }
        }
    }

    /// 主动发送关闭帧以优雅结束连接
    pub async fn close(mut self) {
        let _ = self.stream.send(Message::Close(None)).await;
    }
}
