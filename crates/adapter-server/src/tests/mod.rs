//! 服务端集成测试套件门面：汇总模拟驱动桩、REST 与 OneBot 接口测试、反向 WS 测试及 Wave 2 扩展测试

mod biz_risk_tests;
mod fake_driver;
mod qa_regression_tests;
mod rest_onebot_tests;
mod rest_session_recall_tests;
mod reverse_ws_tests;
mod scheduler_onebot_tests;
mod wave2_onebot_tests;
mod wave6_server_tests;

use std::net::SocketAddr;
use std::time::Duration;

use futures_util::StreamExt;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Mutex as AsyncMutex;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

/// 保护 `.runtime/media` 与 `.runtime/staging_uploads` 测试文件操作不并发踩踏
pub(super) static RUNTIME_FS_LOCK: AsyncMutex<()> = AsyncMutex::const_new(());

/// 发送原生 HTTP/1.1 测试请求并解析状态码与 JSON 响应体
pub(super) async fn http_call(
    listen_addr: SocketAddr,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (u16, Value) {
    let mut stream = TcpStream::connect(listen_addr).await.unwrap();
    let body_bytes = body.map(|value| serde_json::to_vec(&value).unwrap());
    let mut raw_request =
        format!("{method} {path} HTTP/1.1\r\nHost: {listen_addr}\r\nConnection: close\r\n");
    if let Some(bearer_token) = token {
        raw_request.push_str(&format!("Authorization: Bearer {bearer_token}\r\n"));
    }
    if let Some(ref bytes) = body_bytes {
        raw_request.push_str("Content-Type: application/json\r\n");
        raw_request.push_str(&format!("Content-Length: {}\r\n", bytes.len()));
    }
    raw_request.push_str("\r\n");

    stream.write_all(raw_request.as_bytes()).await.unwrap();
    if let Some(bytes) = body_bytes {
        stream.write_all(&bytes).await.unwrap();
    }
    stream.flush().await.unwrap();

    let mut raw_response = Vec::new();
    stream.read_to_end(&mut raw_response).await.unwrap();
    let response_text = String::from_utf8_lossy(&raw_response);
    let (head, body_part) = response_text.split_once("\r\n\r\n").unwrap();
    let status_code = head
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse::<u16>()
        .unwrap();
    let parsed: Value = serde_json::from_str(body_part).unwrap();
    (status_code, parsed)
}

/// 发送带自定义请求头与原始请求体的原生 HTTP/1.1 请求并解析状态码与 JSON 响应
pub(super) async fn http_raw_call(
    listen_addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    raw_body: Option<&str>,
) -> (u16, Value) {
    let mut stream = TcpStream::connect(listen_addr).await.unwrap();
    let mut raw_request =
        format!("{method} {path} HTTP/1.1\r\nHost: {listen_addr}\r\nConnection: close\r\n");
    for (name, val) in headers {
        raw_request.push_str(&format!("{name}: {val}\r\n"));
    }
    if let Some(body) = raw_body {
        raw_request.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    raw_request.push_str("\r\n");

    stream.write_all(raw_request.as_bytes()).await.unwrap();
    if let Some(body) = raw_body {
        stream.write_all(body.as_bytes()).await.unwrap();
    }
    stream.flush().await.unwrap();

    let mut raw_response = Vec::new();
    stream.read_to_end(&mut raw_response).await.unwrap();
    let response_text = String::from_utf8_lossy(&raw_response);
    let (head, body_part) = response_text.split_once("\r\n\r\n").unwrap();
    let status_code = head
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse::<u16>()
        .unwrap();
    let parsed: Value = serde_json::from_str(body_part).unwrap();
    (status_code, parsed)
}

/// 从 `tokio_tungstenite` 客户端流中读取下一帧 JSON 文本消息（自动跳过控制帧）
pub(super) async fn next_ws_json(
    ws_stream: &mut WebSocketStream<MaybeTlsStream<TcpStream>>,
    timeout_duration: Duration,
) -> Value {
    let deadline = tokio::time::Instant::now() + timeout_duration;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(
            !remaining.is_zero(),
            "timed out waiting for WebSocket text frame"
        );
        let frame = tokio::time::timeout(remaining, ws_stream.next())
            .await
            .expect("WebSocket read timed out")
            .expect("WebSocket stream ended unexpectedly")
            .expect("WebSocket frame error");
        match frame {
            Message::Text(text) => {
                return serde_json::from_str(text.as_str()).unwrap();
            }
            Message::Ping(_) | Message::Pong(_) => continue,
            other => panic!("unexpected WebSocket frame: {other:?}"),
        }
    }
}
