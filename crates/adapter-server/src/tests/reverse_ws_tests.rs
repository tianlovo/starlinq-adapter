//! OneBot v11 反向 WebSocket 客户端集成测试：基于临时 `accept_hdr_async` 服务端验证建连与双向调用

use std::sync::Arc;
use std::time::Duration;

use common_types::{AdapterConfig, AdapterEvent, ChatMessage, ChatTargetKind, MessageSegment};
use futures_util::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio_tungstenite::accept_hdr_async;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::Message;

use crate::spawn_server;
use crate::tests::fake_driver::FakeDriver;

/// 临时反向 WebSocket 服务端捕获到的握手请求头信息
#[derive(Debug, Default)]
struct CapturedReverseHeaders {
    self_id: Option<String>,
    client_role: Option<String>,
    authorization: Option<String>,
}

async fn recv_reverse_ws_json(
    ws_stream: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    timeout_duration: Duration,
) -> Value {
    let deadline = tokio::time::Instant::now() + timeout_duration;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(
            !remaining.is_zero(),
            "timed out waiting for reverse WebSocket text frame"
        );
        let frame = tokio::time::timeout(remaining, ws_stream.next())
            .await
            .expect("reverse WS read timed out")
            .expect("reverse WS stream closed")
            .expect("reverse WS read error");
        if let Message::Text(text) = frame {
            return serde_json::from_str(text.as_str()).unwrap();
        }
    }
}

#[tokio::test]
// `accept_hdr_async` 的回调错误类型 `ErrorResponse` 体积固定较大，属第三方 trait 约束
#[allow(clippy::result_large_err)]
async fn reverse_ws_client_connects_pushes_events_and_dispatches_actions() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mock_addr = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(CapturedReverseHeaders::default()));
    let (conn_tx, conn_rx) = oneshot::channel();

    let captured_clone = Arc::clone(&captured);
    let accept_task = tokio::spawn(async move {
        let (tcp_stream, _) = listener.accept().await.unwrap();
        let callback = |request: &Request, response: Response| {
            let mut guard = captured_clone.lock();
            guard.self_id = request
                .headers()
                .get("x-self-id")
                .and_then(|val| val.to_str().ok())
                .map(ToOwned::to_owned);
            guard.client_role = request
                .headers()
                .get("x-client-role")
                .and_then(|val| val.to_str().ok())
                .map(ToOwned::to_owned);
            guard.authorization = request
                .headers()
                .get("authorization")
                .and_then(|val| val.to_str().ok())
                .map(ToOwned::to_owned);
            Ok(response)
        };
        let ws_stream = accept_hdr_async(tcp_stream, callback).await.unwrap();
        let _ = conn_tx.send(ws_stream);
    });

    let driver = Arc::new(FakeDriver::new());
    let mut config = AdapterConfig::default();
    config.server.port = 0;
    config.bot.self_id = 10_001;
    config.onebot.reverse_ws_urls = vec![format!("ws://{mock_addr}/onebot")];
    config.onebot.access_token = Some("rev-secret-token".to_string());
    config.onebot.reconnect_interval_ms = 200;
    config.onebot.heartbeat_interval_ms = 5_000;

    let (_server_addr, server_handle) = spawn_server(driver.clone(), config).await.unwrap();
    let mut ws_stream = tokio::time::timeout(Duration::from_secs(3), conn_rx)
        .await
        .expect("reverse WS client did not connect in time")
        .unwrap();
    accept_task.await.unwrap();

    // 校验反向建连时上报的 OneBot 标准请求头
    {
        let headers_guard = captured.lock();
        assert_eq!(headers_guard.self_id.as_deref(), Some("10001"));
        assert_eq!(headers_guard.client_role.as_deref(), Some("Universal"));
        assert_eq!(
            headers_guard.authorization.as_deref(),
            Some("Bearer rev-secret-token")
        );
    }

    // 第一帧必须是 lifecycle connect 元事件
    let lifecycle = recv_reverse_ws_json(&mut ws_stream, Duration::from_secs(2)).await;
    assert_eq!(lifecycle["post_type"], "meta_event");
    assert_eq!(lifecycle["meta_event_type"], "lifecycle");
    assert_eq!(lifecycle["sub_type"], "connect");
    assert_eq!(lifecycle["self_id"], 10_001);

    // 通过反向 WebSocket 下发 get_version_info 并校验 echo 回包
    ws_stream
        .send(Message::text(
            json!({
                "action": "get_version_info",
                "params": {},
                "echo": "rev-ver-1"
            })
            .to_string(),
        ))
        .await
        .unwrap();
    let version_reply = recv_reverse_ws_json(&mut ws_stream, Duration::from_secs(2)).await;
    assert_eq!(version_reply["status"], "ok");
    assert_eq!(version_reply["retcode"], 0);
    assert_eq!(version_reply["echo"], "rev-ver-1");
    assert_eq!(version_reply["data"]["protocol_version"], "v11");

    // 通过反向 WebSocket 下发 get_group_list 并校验 echo 回包
    ws_stream
        .send(Message::text(
            json!({
                "action": "get_group_list",
                "params": { "no_cache": true },
                "echo": "rev-groups-1"
            })
            .to_string(),
        ))
        .await
        .unwrap();
    let groups_reply = recv_reverse_ws_json(&mut ws_stream, Duration::from_secs(2)).await;
    assert_eq!(groups_reply["retcode"], 0);
    assert_eq!(groups_reply["echo"], "rev-groups-1");
    assert_eq!(groups_reply["data"][0]["group_id"], 555_005);

    // 通过反向 WebSocket 下发 send_group_msg 并校验实际投递及 echo 回包
    ws_stream
        .send(Message::text(
            json!({
                "action": "send_group_msg",
                "params": {
                    "group_id": 555_005,
                    "message": "reverse ws hello"
                },
                "echo": "rev-send-1"
            })
            .to_string(),
        ))
        .await
        .unwrap();
    let send_reply = recv_reverse_ws_json(&mut ws_stream, Duration::from_secs(2)).await;
    assert_eq!(send_reply["retcode"], 0);
    assert_eq!(send_reply["echo"], "rev-send-1");
    assert_eq!(
        driver.sent_requests().last().unwrap().text,
        "reverse ws hello"
    );

    // 广播新收到的群聊消息并校验反向 WS 主动上推
    driver.broadcast_event(AdapterEvent::GroupMessage(ChatMessage {
        message_id: "rev-peer-1".to_string(),
        group_name: "Demo测试群 (5)".to_string(),
        sender: "群友B".to_string(),
        content: "reverse ping".to_string(),
        timestamp_hint: Some("15:00".to_string()),
        is_self: false,
        captured_at_ms: 1_700_000_010_000,
        onebot_message_id: Some(88_001),
        group_id: Some(555_005),
        user_id: Some(30_001),
        ..Default::default()
    }));
    let pushed = recv_reverse_ws_json(&mut ws_stream, Duration::from_secs(2)).await;
    assert_eq!(pushed["post_type"], "message");
    assert_eq!(pushed["message_id"], 88_001);
    assert_eq!(pushed["raw_message"], "reverse ping");
    assert_eq!(pushed["sender"]["nickname"], "群友B");

    // 广播私聊来信并校验反向 WS 主动上推 message.private.friend
    driver.broadcast_event(AdapterEvent::PrivateMessage(ChatMessage {
        message_id: "rev-priv-1".to_string(),
        group_name: "测试好友".to_string(),
        sender: "测试好友".to_string(),
        content: "私聊来信".to_string(),
        timestamp_hint: Some("15:01".to_string()),
        is_self: false,
        captured_at_ms: 1_700_000_011_000,
        onebot_message_id: Some(88_002),
        group_id: None,
        user_id: Some(777_001),
        target_kind: ChatTargetKind::Private,
        segments: vec![MessageSegment::Text {
            text: "私聊来信".to_string(),
        }],
        ..Default::default()
    }));
    let priv_pushed = recv_reverse_ws_json(&mut ws_stream, Duration::from_secs(2)).await;
    assert_eq!(priv_pushed["post_type"], "message");
    assert_eq!(priv_pushed["message_type"], "private");
    assert_eq!(priv_pushed["sub_type"], "friend");
    assert!(priv_pushed.get("group_id").is_none());
    assert_eq!(priv_pushed["user_id"], 777_001);
    assert_eq!(priv_pushed["raw_message"], "私聊来信");

    server_handle.abort();
}
