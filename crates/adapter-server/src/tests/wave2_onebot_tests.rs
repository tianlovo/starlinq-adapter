//! OneBot v11 Wave 2 扩展集成测试：校验 HMAC-SHA1、根路由、异步限速、扩展动作、多角色 WS 与 Webhook

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::State as AxumState;
use axum::http::HeaderMap;
use axum::routing::post;
use axum::{Json, Router};
use common_types::{AdapterConfig, AdapterEvent, ChatMessage, ChatTargetKind, MessageSegment};
use futures_util::SinkExt;
use parking_lot::Mutex;
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{accept_hdr_async, connect_async};

use crate::constants::MEDIA_CACHE_SUBDIR;
use crate::onebot::http_webhook::{hmac_sha1_hex, sha1_digest};
use crate::spawn_server;
use crate::tests::fake_driver::FakeDriver;
use crate::tests::{http_call, http_raw_call, next_ws_json};

/// 测试用本地 Webhook 接收服务端捕获到的请求签名与原始报文记录
#[derive(Debug, Default)]
struct CapturedWebhookCall {
    self_id: Option<String>,
    signature: Option<String>,
    body_bytes: Vec<u8>,
}

/// 保证媒体缓存目录在异常断言下也能按测试前状态还原的 RAII 守卫
struct RuntimeCleanupGuard {
    runtime_dir: PathBuf,
    remove_whole_runtime: bool,
    test_file: Option<PathBuf>,
}

impl Drop for RuntimeCleanupGuard {
    fn drop(&mut self) {
        if let Some(ref file) = self.test_file {
            let _ = std::fs::remove_file(file);
        }
        if self.remove_whole_runtime {
            let _ = std::fs::remove_dir_all(&self.runtime_dir);
        }
    }
}

async fn webhook_receiver_handler(
    AxumState(store): AxumState<Arc<Mutex<Vec<CapturedWebhookCall>>>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Json<Value> {
    let self_id = headers
        .get("x-self-id")
        .and_then(|v| v.to_str().ok())
        .map(ToOwned::to_owned);
    let signature = headers
        .get("x-signature")
        .and_then(|v| v.to_str().ok())
        .map(ToOwned::to_owned);
    let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    store.lock().push(CapturedWebhookCall {
        self_id,
        signature,
        body_bytes: body.to_vec(),
    });
    if parsed.get("post_type").and_then(Value::as_str) == Some("message") {
        Json(json!({
            "reply": "webhook自动回复",
            "at_sender": true,
        }))
    } else {
        Json(json!({}))
    }
}

#[test]
fn sha1_and_hmac_sha1_rfc_vectors() {
    assert_eq!(
        sha1_digest(b"abc"),
        [
            0xa9, 0x99, 0x3e, 0x36, 0x47, 0x06, 0x81, 0x6a, 0xba, 0x3e, 0x25, 0x71, 0x78, 0x50,
            0xc2, 0x6c, 0x9c, 0xd0, 0xd8, 0x9d
        ]
    );
    assert_eq!(
        hmac_sha1_hex(&[0x0b; 20], b"Hi There"),
        "b617318655057264e28bc0b6fb378c8ef146be00"
    );
    assert_eq!(
        hmac_sha1_hex(b"Jefe", b"what do ya want for nothing?"),
        "effcdf6ae5eb2fa2d27416d5f184df9c259a7c79"
    );
    assert_eq!(
        hmac_sha1_hex(
            &[0xaa; 80],
            b"Test Using Larger Than Block-Size Key - Hash Key First"
        ),
        "aa4ae5e15272d00e95705637ce8a3b55ed402112"
    );
}

#[tokio::test]
async fn root_routes_token_auth_and_form_urlencoded() {
    let driver = Arc::new(FakeDriver::new());
    let mut config = AdapterConfig::default();
    config.server.port = 0;
    config.server.access_token = Some("wave2-tok".to_string());
    let (listen_addr, handle) = spawn_server(driver.clone(), config).await.unwrap();

    // 未携带令牌访问根级 OneBot 动作时应被鉴权中间件拦截
    let (unauth_code, _) = http_call(listen_addr, "GET", "/get_status", None, None).await;
    assert_eq!(unauth_code, 401);

    // 验证 OneBot 习惯的 Authorization Token 前缀与根级 `/{action}` 路由
    let auth_headers = [("Authorization", "Token wave2-tok")];
    let (code, body) = http_raw_call(listen_addr, "GET", "/get_status", &auth_headers, None).await;
    assert_eq!(code, 200);
    assert_eq!(body["retcode"], 0);
    assert_eq!(body["data"]["online"], true);

    let (code, body) =
        http_raw_call(listen_addr, "GET", "/get_version_info", &auth_headers, None).await;
    assert_eq!(code, 200);
    assert_eq!(body["data"]["protocol_version"], "v11");

    // 验证查询参数 `?access_token=` 鉴权透传
    let (code, body) = http_call(
        listen_addr,
        "GET",
        "/get_login_info?access_token=wave2-tok",
        None,
        None,
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(body["data"]["user_id"], 10_001);

    // `post_only` 写动作禁止使用 GET 方法调用
    let (code, body) =
        http_raw_call(listen_addr, "GET", "/send_group_msg", &auth_headers, None).await;
    assert_eq!(code, 405);
    assert_eq!(body["retcode"], 1405);

    // 验证 `application/x-www-form-urlencoded` 参数解码与 `auto_escape=true`
    let form_headers = [
        ("Authorization", "Token wave2-tok"),
        ("Content-Type", "application/x-www-form-urlencoded"),
    ];
    let (code, body) = http_raw_call(
        listen_addr,
        "POST",
        "/send_group_msg",
        &form_headers,
        Some("group_id=555005&message=form+%5BCQ%3Aface%2Cid%3D14%5D&auto_escape=true"),
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(body["retcode"], 0);
    assert_eq!(driver.sent_requests()[0].text, "form [CQ:face,id=14]");

    let (sd_code, _) = http_raw_call(
        listen_addr,
        "POST",
        "/api/shutdown",
        &auth_headers,
        Some("{}"),
    )
    .await;
    assert_eq!(sd_code, 200);
    let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
}

#[tokio::test]
async fn async_and_rate_limited_action_modifiers() {
    let driver = Arc::new(FakeDriver::new());
    let mut config = AdapterConfig::default();
    config.server.port = 0;
    config.onebot.rate_limit_interval_ms = 80;
    let (listen_addr, handle) = spawn_server(driver.clone(), config).await.unwrap();

    let (code, body) = http_call(
        listen_addr,
        "POST",
        "/onebot/v11/send_group_msg_async",
        None,
        Some(json!({
            "group_id": 555_005,
            "message": "async-1"
        })),
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(body["status"], "async");
    assert_eq!(body["retcode"], 1);

    let start = Instant::now();
    for msg in ["rl-1", "rl-2"] {
        let (rl_code, rl_body) = http_call(
            listen_addr,
            "POST",
            "/onebot/v11/send_group_msg_rate_limited",
            None,
            Some(json!({
                "group_id": 555_005,
                "message": msg
            })),
        )
        .await;
        assert_eq!(rl_code, 200);
        assert_eq!(rl_body["status"], "async");
        assert_eq!(rl_body["retcode"], 1);
    }

    let deadline = Instant::now() + Duration::from_secs(3);
    while driver.sent_requests().len() < 3 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
    assert_eq!(driver.sent_requests().len(), 3);
    assert!(
        start.elapsed() >= Duration::from_millis(70),
        "rate_limited requests completed too fast: {:?}",
        start.elapsed()
    );

    let _ = http_call(listen_addr, "POST", "/api/shutdown", None, Some(json!({}))).await;
    let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
}

#[tokio::test]
async fn extended_actions_and_unsupported_matrix() {
    let driver = Arc::new(FakeDriver::new());
    driver.allow_private_friend("测试好友");
    let mut config = AdapterConfig::default();
    config.server.port = 0;
    config.target.allowed_private_friends = vec!["测试好友".to_string()];
    let (listen_addr, handle) = spawn_server(driver.clone(), config).await.unwrap();

    // 1. 陌生人与已知成员/好友资料查询
    let (code, body) = http_call(
        listen_addr,
        "GET",
        "/onebot/v11/get_stranger_info?user_id=20001",
        None,
        None,
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(body["data"]["nickname"], "群友A");

    let (code, body) = http_call(
        listen_addr,
        "GET",
        "/onebot/v11/get_stranger_info?user_id=777001",
        None,
        None,
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(body["data"]["nickname"], "测试好友");

    let (code, body) = http_call(
        listen_addr,
        "GET",
        "/onebot/v11/get_stranger_info?user_id=999999",
        None,
        None,
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(body["retcode"], 0);
    assert_eq!(body["data"]["nickname"], "");

    // 2. 群历史消息与私聊历史消息（含未授权好友 403 拦截）
    let (code, body) = http_call(
        listen_addr,
        "GET",
        "/onebot/v11/get_group_msg_history?group_id=555005&count=10",
        None,
        None,
    )
    .await;
    assert_eq!(code, 200);
    assert!(!body["data"]["messages"].as_array().unwrap().is_empty());

    let (code, body) = http_call(
        listen_addr,
        "POST",
        "/onebot/v11/get_friend_msg_history",
        None,
        Some(json!({ "nickname": "未授权陌生人" })),
    )
    .await;
    assert_eq!(code, 403);
    assert_eq!(body["retcode"], 1403);

    let (code, _) = http_call(
        listen_addr,
        "POST",
        "/onebot/v11/send_private_msg",
        None,
        Some(json!({ "user_id": 777_001, "message": "hi friend" })),
    )
    .await;
    assert_eq!(code, 200);

    let (code, body) = http_call(
        listen_addr,
        "GET",
        "/onebot/v11/get_friend_msg_history?user_id=777001",
        None,
        None,
    )
    .await;
    assert_eq!(code, 200);
    assert!(body["data"]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["raw_message"] == "hi friend"));

    // 3. 严格模式下禁止伪造自定义虚拟转发节点（501/10004 且零落库），真实消息引用节点走原生合并转发并可回查
    let (forbid_fwd_code, forbid_fwd_body) = http_call(
        listen_addr,
        "POST",
        "/onebot/v11/send_group_forward_msg",
        None,
        Some(json!({
            "group_id": 555_005,
            "messages": [
                {"type": "node", "data": {"nickname": "甲", "user_id": "20001", "content": "第一条"}}
            ]
        })),
    )
    .await;
    assert_eq!(forbid_fwd_code, 501);
    assert_eq!(forbid_fwd_body["retcode"], 10004);
    assert_eq!(driver.stored_forward_count(), 0);

    let (code, body) = http_call(
        listen_addr,
        "POST",
        "/onebot/v11/send_group_forward_msg",
        None,
        Some(json!({
            "group_id": 555_005,
            "messages": [
                {"type": "node", "data": {"id": "10001"}}
            ]
        })),
    )
    .await;
    assert_eq!(code, 200);
    let forward_id = body["data"]["forward_id"].as_str().unwrap().to_string();

    let (code, body) = http_call(
        listen_addr,
        "GET",
        &format!("/onebot/v11/get_forward_msg?id={forward_id}"),
        None,
        None,
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(body["data"]["messages"].as_array().unwrap().len(), 1);
    assert_eq!(body["data"]["messages"][0]["data"]["id"], "10001");

    // 4. `get_image` 路径越界防御与 `clean_cache` 清理校验
    {
        let _runtime_fs_guard = super::RUNTIME_FS_LOCK.lock().await;
        let runtime_dir = PathBuf::from(".runtime");
        let media_dir = Path::new(MEDIA_CACHE_SUBDIR);
        let remove_whole_runtime = !runtime_dir.exists();
        std::fs::create_dir_all(media_dir).unwrap();
        let img_path = media_dir.join("wave2_test_img.png");
        std::fs::write(&img_path, b"PNG1234").unwrap();
        let _cleanup_guard = RuntimeCleanupGuard {
            runtime_dir,
            remove_whole_runtime,
            test_file: Some(img_path),
        };

        let (code, body) = http_call(
            listen_addr,
            "GET",
            "/onebot/v11/get_image?file=wave2_test_img.png",
            None,
            None,
        )
        .await;
        assert_eq!(code, 200);
        assert_eq!(body["data"]["size"], 7);

        let (code, body) = http_call(
            listen_addr,
            "GET",
            "/onebot/v11/get_image?file=../Cargo.toml",
            None,
            None,
        )
        .await;
        assert_eq!(code, 400);
        assert_eq!(body["retcode"], 1400);

        let (code, body) = http_call(
            listen_addr,
            "POST",
            "/onebot/v11/clean_cache",
            None,
            Some(json!({})),
        )
        .await;
        assert_eq!(code, 200);
        assert_eq!(body["data"]["cleaned"], true);
        assert!(body["data"]["removed_files"].as_u64().unwrap() >= 1);
    }

    // 5. `.handle_quick_operation` 快速回复与高危字段 10004 拒绝
    let (code, body) = http_call(
        listen_addr,
        "POST",
        "/onebot/v11/.handle_quick_operation",
        None,
        Some(json!({
            "context": {
                "post_type": "message",
                "message_type": "group",
                "group_id": 555_005,
                "user_id": 20_001,
                "message_id": 10_001
            },
            "operation": {
                "reply": "收到快捷回复",
                "at_sender": true
            }
        })),
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(body["data"]["applied"], true);
    let sent_after_quick = driver.sent_requests();
    let last_quick = sent_after_quick.last().unwrap();
    assert!(matches!(
        &last_quick.segments[0],
        MessageSegment::Reply { id } if id == "10001"
    ));
    assert!(matches!(
        &last_quick.segments[1],
        MessageSegment::At { qq, .. } if qq == "20001"
    ));

    let (code, body) = http_call(
        listen_addr,
        "POST",
        "/onebot/v11/.handle_quick_operation",
        None,
        Some(json!({
            "context": { "post_type": "message", "message_type": "group", "group_id": 555_005 },
            "operation": { "ban": true }
        })),
    )
    .await;
    assert_eq!(code, 501);
    assert_eq!(body["retcode"], 10004);

    // 6. 18 个显式未支持动作与未知动作错误码全覆盖校验
    let unsupported_actions = [
        "get_record",
        "send_like",
        "get_cookies",
        "get_csrf_token",
        "get_credentials",
        "set_group_kick",
        "set_group_ban",
        "set_group_anonymous_ban",
        "set_group_whole_ban",
        "set_group_admin",
        "set_group_anonymous",
        "set_group_card",
        "set_group_name",
        "set_group_leave",
        "set_group_special_title",
        "set_group_portrait",
        "set_friend_add_request",
        "set_group_add_request",
    ];
    for action in unsupported_actions {
        let (u_code, u_body) = http_call(
            listen_addr,
            "POST",
            &format!("/onebot/v11/{action}"),
            None,
            Some(json!({})),
        )
        .await;
        assert_eq!(u_code, 501, "expected 501 for unsupported action {action}");
        assert_eq!(u_body["retcode"], 10004);
    }

    let (unknown_code, unknown_body) = http_call(
        listen_addr,
        "POST",
        "/onebot/v11/no_such_action",
        None,
        Some(json!({})),
    )
    .await;
    assert_eq!(unknown_code, 404);
    assert_eq!(unknown_body["retcode"], 1404);

    // 7. 验证 `set_restart` 可调度后被测试服务器优雅停机安全取消而不误拉起外部进程
    let (rst_code, rst_body) = http_call(
        listen_addr,
        "POST",
        "/onebot/v11/set_restart",
        None,
        Some(json!({ "delay": 30_000 })),
    )
    .await;
    assert_eq!(rst_code, 200);
    assert_eq!(rst_body["data"]["restarting"], true);

    let (sd_code, _) = http_call(listen_addr, "POST", "/api/shutdown", None, Some(json!({}))).await;
    assert_eq!(sd_code, 200);
    let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
}

#[allow(clippy::result_large_err)]
async fn accept_once_capture_role(
    listener: TcpListener,
    role_tx: oneshot::Sender<Option<String>>,
) -> tokio_tungstenite::WebSocketStream<tokio::net::TcpStream> {
    let (tcp, _) = listener.accept().await.unwrap();
    let mut captured_role = None;
    let ws = accept_hdr_async(tcp, |req: &Request, resp: Response| {
        captured_role = req
            .headers()
            .get("x-client-role")
            .and_then(|v| v.to_str().ok())
            .map(ToOwned::to_owned);
        Ok(resp)
    })
    .await
    .unwrap();
    let _ = role_tx.send(captured_role);
    ws
}

#[tokio::test]
async fn multi_role_ws_and_post_message_sent_and_notices() {
    let api_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_addr = api_listener.local_addr().unwrap();
    let event_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let event_addr = event_listener.local_addr().unwrap();

    let (api_role_tx, api_role_rx) = oneshot::channel();
    let (event_role_tx, event_role_rx) = oneshot::channel();
    let api_accept = tokio::spawn(accept_once_capture_role(api_listener, api_role_tx));
    let event_accept = tokio::spawn(accept_once_capture_role(event_listener, event_role_tx));

    let driver = Arc::new(FakeDriver::new());
    let mut config = AdapterConfig::default();
    config.server.port = 0;
    config.bot.self_id = 10_001;
    config.onebot.post_message_sent = true;
    config.onebot.reconnect_interval_ms = 200;
    config.onebot.reverse_ws_api_urls = vec![format!("ws://{api_addr}/api")];
    config.onebot.reverse_ws_event_urls = vec![format!("ws://{event_addr}/event")];

    let (listen_addr, handle) = spawn_server(driver.clone(), config).await.unwrap();

    // 验证反向 API 与反向 Event 客户端分别上报 `X-Client-Role: API` 与 `Event`
    let api_role = tokio::time::timeout(Duration::from_secs(3), api_role_rx)
        .await
        .unwrap()
        .unwrap();
    let event_role = tokio::time::timeout(Duration::from_secs(3), event_role_rx)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(api_role.as_deref(), Some("API"));
    assert_eq!(event_role.as_deref(), Some("Event"));
    let _rev_api_ws = api_accept.await.unwrap();
    let _rev_event_ws = event_accept.await.unwrap();

    // 连接正向 `/onebot/v11/ws/event` 与 `/onebot/v11/ws/api` 验证角色隔离
    let (mut fwd_event_ws, _) = connect_async(format!("ws://{listen_addr}/onebot/v11/ws/event"))
        .await
        .unwrap();
    let lifecycle = next_ws_json(&mut fwd_event_ws, Duration::from_secs(2)).await;
    assert_eq!(lifecycle["meta_event_type"], "lifecycle");

    fwd_event_ws
        .send(Message::text(
            json!({ "action": "get_status", "echo": "ev-deny" }).to_string(),
        ))
        .await
        .unwrap();
    let deny_reply = next_ws_json(&mut fwd_event_ws, Duration::from_secs(2)).await;
    assert_eq!(deny_reply["status"], "failed");
    assert_eq!(deny_reply["retcode"], 10004);
    assert_eq!(deny_reply["echo"], "ev-deny");

    let (mut fwd_api_ws, _) = connect_async(format!("ws://{listen_addr}/onebot/v11/ws/api"))
        .await
        .unwrap();
    fwd_api_ws
        .send(Message::text(
            json!({ "action": "get_status", "echo": "api-ok" }).to_string(),
        ))
        .await
        .unwrap();
    let api_reply = next_ws_json(&mut fwd_api_ws, Duration::from_secs(2)).await;
    assert_eq!(api_reply["status"], "ok");
    assert_eq!(api_reply["retcode"], 0);
    assert_eq!(api_reply["echo"], "api-ok");

    // 广播系统 notice 事件（`self_id == 0` 应被自动补齐）
    driver.broadcast_event(AdapterEvent::Notice(json!({
        "time": 1_700_000_000,
        "self_id": 0,
        "post_type": "notice",
        "notice_type": "group_recall",
        "group_id": 555_005,
        "user_id": 20_001,
        "operator_id": 20_001,
        "message_id": 10_001
    })));
    let notice_ev = next_ws_json(&mut fwd_event_ws, Duration::from_secs(2)).await;
    assert_eq!(notice_ev["post_type"], "notice");
    assert_eq!(notice_ev["notice_type"], "group_recall");
    assert_eq!(notice_ev["self_id"], 10_001);

    // `post_message_sent = true` 时，发送动作产生的 `MessageSent` 应推送 `post_type = "message_sent"`
    let (send_code, send_body) = http_call(
        listen_addr,
        "POST",
        "/onebot/v11/send_group_msg",
        None,
        Some(json!({ "group_id": 555_005, "message": "trigger message_sent" })),
    )
    .await;
    assert_eq!(send_code, 200);
    let sent_ev = next_ws_json(&mut fwd_event_ws, Duration::from_secs(2)).await;
    assert_eq!(sent_ev["post_type"], "message_sent");
    assert_eq!(sent_ev["message_type"], "group");
    assert_eq!(sent_ev["message_id"], send_body["data"]["message_id"]);
    assert_eq!(sent_ev["raw_message"], "trigger message_sent");
    assert_eq!(
        sent_ev["message"],
        json!([{ "type": "text", "data": { "text": "trigger message_sent" } }])
    );
    assert_eq!(sent_ev["sender"]["nickname"], "BotSelf");

    let _ = http_call(listen_addr, "POST", "/api/shutdown", None, Some(json!({}))).await;
    let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
}

#[tokio::test]
async fn http_webhook_hmac_signature_and_quick_operation() {
    let captured = Arc::new(Mutex::new(Vec::<CapturedWebhookCall>::new()));
    let hook_app = Router::new()
        .route("/hook", post(webhook_receiver_handler))
        .with_state(captured.clone());
    let hook_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let hook_addr = hook_listener.local_addr().unwrap();
    let hook_task = tokio::spawn(async move {
        let _ = axum::serve(hook_listener, hook_app).await;
    });

    let driver = Arc::new(FakeDriver::new());
    let mut config = AdapterConfig::default();
    config.server.port = 0;
    config.bot.self_id = 10_001;
    config.onebot.http_post_urls = vec![format!("http://{hook_addr}/hook")];
    config.onebot.secret = Some("hook-secret-key".to_string());
    let (listen_addr, server_handle) = spawn_server(driver.clone(), config).await.unwrap();

    // 等待后台 HTTP Webhook 任务完成事件通道订阅
    tokio::time::sleep(Duration::from_millis(80)).await;

    driver.broadcast_event(AdapterEvent::GroupMessage(ChatMessage {
        message_id: "m-hook".to_string(),
        group_name: "Demo测试群 (5)".to_string(),
        sender: "群友A".to_string(),
        content: "ping hook".to_string(),
        timestamp_hint: Some("16:00".to_string()),
        is_self: false,
        captured_at_ms: chrono::Utc::now().timestamp_millis().max(0) as u64,
        onebot_message_id: Some(10_001),
        group_id: Some(555_005),
        user_id: Some(20_001),
        target_kind: ChatTargetKind::Group,
        segments: vec![MessageSegment::Text {
            text: "ping hook".to_string(),
        }],
        ..Default::default()
    }));

    let deadline = Instant::now() + Duration::from_secs(4);
    while (captured.lock().is_empty() || driver.sent_requests().is_empty())
        && Instant::now() < deadline
    {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    {
        let calls = captured.lock();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].self_id.as_deref(), Some("10001"));
        let expected_sig = format!(
            "sha1={}",
            hmac_sha1_hex(b"hook-secret-key", &calls[0].body_bytes)
        );
        assert_eq!(calls[0].signature.as_deref(), Some(expected_sig.as_str()));
    }

    let sent = driver.sent_requests();
    assert_eq!(sent.len(), 1);
    assert!(matches!(
        &sent[0].segments[0],
        MessageSegment::Reply { id } if id == "10001"
    ));
    assert!(matches!(
        &sent[0].segments[1],
        MessageSegment::At { qq, .. } if qq == "20001"
    ));
    assert!(sent[0].text.contains("webhook自动回复"));

    let _ = http_call(listen_addr, "POST", "/api/shutdown", None, Some(json!({}))).await;
    let _ = tokio::time::timeout(Duration::from_secs(2), server_handle).await;
    hook_task.abort();
}

#[tokio::test]
async fn poke_and_recall_actions_roundtrip_http_ws_and_quick_op() {
    let driver = Arc::new(FakeDriver::new());
    driver.allow_private_friend("测试好友");

    let mut config = AdapterConfig::default();
    config.server.port = 0;
    config.bot.self_id = 10_001;
    config.target.allowed_private_friends = vec!["测试好友".to_string()];
    let (listen_addr, handle) = spawn_server(driver.clone(), config).await.unwrap();

    let (mut ws, _) = connect_async(format!("ws://{listen_addr}/onebot/v11/ws"))
        .await
        .unwrap();
    let lifecycle = next_ws_json(&mut ws, Duration::from_secs(2)).await;
    assert_eq!(lifecycle["meta_event_type"], "lifecycle");

    // 1. 群发一条自发消息并验证 `.handle_quick_operation` 的 `delete: true` 撤回 + `group_recall` 通知
    let (g_code, g_body) = http_call(
        listen_addr,
        "POST",
        "/onebot/v11/send_group_msg",
        None,
        Some(json!({ "group_id": 555_005, "message": "recall target one" })),
    )
    .await;
    assert_eq!(g_code, 200);
    let msg_id_1 = g_body["data"]["message_id"].as_i64().unwrap();

    let (quick_code, quick_body) = http_call(
        listen_addr,
        "POST",
        "/onebot/v11/.handle_quick_operation",
        None,
        Some(json!({
            "context": {
                "message_type": "group",
                "group_id": 555_005,
                "message_id": msg_id_1
            },
            "operation": { "delete": true }
        })),
    )
    .await;
    assert_eq!(quick_code, 200);
    assert_eq!(quick_body["retcode"], 0);

    let recall_ev = next_ws_json(&mut ws, Duration::from_secs(2)).await;
    assert_eq!(recall_ev["post_type"], "notice");
    assert_eq!(recall_ev["notice_type"], "group_recall");
    assert_eq!(recall_ev["group_id"], 555_005);
    assert_eq!(recall_ev["message_id"], msg_id_1);

    let (get_after_code, get_after_body) = http_call(
        listen_addr,
        "POST",
        "/onebot/v11/get_msg",
        None,
        Some(json!({ "message_id": msg_id_1 })),
    )
    .await;
    assert_eq!(get_after_code, 400);
    assert_eq!(get_after_body["retcode"], 1400);

    // 2. 私聊发送一条自发消息并用 WS `delete_msg` 撤回 + `friend_recall` 通知
    let (p_code, p_body) = http_call(
        listen_addr,
        "POST",
        "/onebot/v11/send_private_msg",
        None,
        Some(json!({ "user_id": 777_001, "message": "recall private target" })),
    )
    .await;
    assert_eq!(p_code, 200);
    let msg_id_2 = p_body["data"]["message_id"].as_i64().unwrap();

    ws.send(Message::text(
        json!({
            "action": "delete_msg",
            "params": { "message_id": msg_id_2 },
            "echo": "ws-del-priv"
        })
        .to_string(),
    ))
    .await
    .unwrap();
    let frame_a = next_ws_json(&mut ws, Duration::from_secs(2)).await;
    let frame_b = next_ws_json(&mut ws, Duration::from_secs(2)).await;
    let frames = [frame_a, frame_b];
    assert!(
        frames
            .iter()
            .any(|v| v["echo"] == "ws-del-priv" && v["retcode"] == 0),
        "expected delete_msg ws ok reply in {frames:?}"
    );
    assert!(
        frames.iter().any(|v| {
            v["post_type"] == "notice"
                && v["notice_type"] == "friend_recall"
                && v["user_id"] == 777_001
                && v["message_id"] == msg_id_2
        }),
        "expected friend_recall ws notice in {frames:?}"
    );

    // 3. `delete_msg` 负向：不可撤回的他人/超时消息(10001)->501/10004；不存在消息->400/1400
    let (other_del_code, other_del_body) = http_call(
        listen_addr,
        "POST",
        "/onebot/v11/delete_msg",
        None,
        Some(json!({ "message_id": 10_001 })),
    )
    .await;
    assert_eq!(other_del_code, 501);
    assert_eq!(other_del_body["retcode"], 10004);

    let (miss_del_code, miss_del_body) = http_call(
        listen_addr,
        "POST",
        "/onebot/v11/delete_msg",
        None,
        Some(json!({ "message_id": 999_999 })),
    )
    .await;
    assert_eq!(miss_del_code, 400);
    assert_eq!(miss_del_body["retcode"], 1400);

    // 4. 出站戳一戳边界：send_poke / group_poke / friend_poke (HTTP & WS) 与 [CQ:poke] 严格返回 501/10004 且零纯文本降级
    for (route, payload) in [
        (
            "/onebot/v11/send_poke",
            json!({ "group_id": 555_005, "user_id": 20_001 }),
        ),
        (
            "/onebot/v11/group_poke",
            json!({ "group_id": 555_005, "user_id": 20_001 }),
        ),
        ("/onebot/v11/friend_poke", json!({ "user_id": 777_001 })),
        (
            "/onebot/v11/send_group_msg",
            json!({ "group_id": 555_005, "message": "[CQ:poke,qq=20001]" }),
        ),
    ] {
        let (code, body) = http_call(listen_addr, "POST", route, None, Some(payload)).await;
        assert_eq!(code, 501, "expected 501 for outbound poke on {route}");
        assert_eq!(
            body["retcode"], 10004,
            "expected retcode=10004 for outbound poke on {route}: {body}"
        );
    }

    ws.send(Message::text(
        json!({
            "action": "send_poke",
            "params": { "user_id": 777_001 },
            "echo": "ws-poke-priv"
        })
        .to_string(),
    ))
    .await
    .unwrap();
    let ws_poke_reply = next_ws_json(&mut ws, Duration::from_secs(2)).await;
    assert_eq!(ws_poke_reply["echo"], "ws-poke-priv");
    assert_eq!(ws_poke_reply["retcode"], 10004);

    let sent = driver.sent_requests();
    assert!(
        !sent.iter().any(|req| {
            req.segments
                .iter()
                .any(|seg| matches!(seg, MessageSegment::Poke { .. }))
                || req.text.contains("戳一戳")
                || req.text.contains("CQ:poke")
        }),
        "outbound poke must never be forwarded or downgraded into plain text: {sent:?}"
    );

    let _ = http_call(listen_addr, "POST", "/api/shutdown", None, Some(json!({}))).await;
    let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
}
