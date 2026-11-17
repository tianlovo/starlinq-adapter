//! 原生 REST、OneBot v11 全量动作、正向 WebSocket 与纯 Rust 重启测试模块

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use axum::response::IntoResponse;
use common_types::{
    AdapterConfig, AdapterError, AdapterEvent, ChatMessage, ChatTargetKind, MessageSegment,
};
use serde_json::json;
use tokio_tungstenite::connect_async;

use crate::error::{map_adapter_error, ApiError};
use crate::rest::restart_ops::{
    build_restarter_command, prepare_restarter_binary, resolve_restarter_source_with_fallback,
};
use crate::rest::{prepend_path_dir, RESTART_CREATION_FLAGS};
use crate::spawn_server;
use crate::tests::fake_driver::FakeDriver;
use crate::tests::{http_call, next_ws_json};

/// 测试结束后自动清理临时目录的 RAII 守卫
struct TempDirGuard(std::path::PathBuf);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn rest_and_onebot_endpoints_and_error_mapping() {
    let driver = Arc::new(FakeDriver::new());
    let mut config = AdapterConfig::default();
    config.server.port = 0;
    let (listen_addr, handle) = spawn_server(driver.clone(), config).await.unwrap();

    let (code, body) = http_call(listen_addr, "GET", "/api/health", None, None).await;
    assert_eq!(code, 200);
    assert_eq!(body["ok"], true);
    assert_eq!(body["pid"].as_u64(), Some(u64::from(std::process::id())));

    let (code, body) = http_call(
        listen_addr,
        "POST",
        "/onebot/v11/send_group_msg",
        None,
        Some(json!({
            "message": [{"type": "text", "data": {"text": "segment hello"}}]
        })),
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(body["retcode"], 0);
    assert_eq!(body["data"]["outcome"]["mouse_touched"], false);
    assert_eq!(driver.sent_requests()[0].text, "segment hello");
    assert_eq!(driver.sent_requests()[0].group, "Demo测试群");

    // 测试仅提供数字 group_id 时自动反查对应群名称再发送
    let (code, body) = http_call(
        listen_addr,
        "POST",
        "/onebot/v11/send_msg",
        None,
        Some(json!({
            "group_id": 555_005,
            "message": "via group_id"
        })),
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(body["retcode"], 0);
    assert_eq!(body["data"]["group_id"], 555_005);
    let sent_id = body["data"]["message_id"].as_i64().unwrap();
    assert_eq!(driver.sent_requests()[1].group, "Demo测试群 (5)");

    // 显式传入不存在的 group_id 时必须严格拒绝，不得误回退到默认目标群
    let (code, body) = http_call(
        listen_addr,
        "POST",
        "/onebot/v11/send_group_msg",
        None,
        Some(json!({
            "group_id": 999_999_999_i64,
            "message": "should_not_route"
        })),
    )
    .await;
    assert_eq!(code, 404);
    assert_eq!(body["retcode"], 1404);
    assert_eq!(driver.sent_requests().len(), 2);

    // 测试 get_msg 通过数字消息编号回查已入库消息
    let (code, body) = http_call(
        listen_addr,
        "GET",
        &format!("/onebot/v11/get_msg?message_id={sent_id}"),
        None,
        None,
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(body["retcode"], 0);
    assert_eq!(body["data"]["raw_message"], "via group_id");

    let (code, body) = http_call(listen_addr, "GET", "/api/messages", None, None).await;
    assert_eq!(code, 200);
    assert_eq!(body[0]["sender"], "群友A");

    driver.set_next_error(AdapterError::SecurityPromptDetected {
        title: "QQ安全验证".to_string(),
        detail: "captcha".to_string(),
    });
    let (code, body) = http_call(
        listen_addr,
        "POST",
        "/api/send",
        None,
        Some(json!({"group": "Demo测试群", "text": "blocked"})),
    )
    .await;
    assert_eq!(code, 423);
    assert_eq!(body["ok"], false);
    assert_eq!(body["code"], "SECURITY_PROMPT_DETECTED");

    driver.set_next_error(AdapterError::MemberCountExceeded {
        group: "Demo测试群".to_string(),
        detected: Some(26),
        limit: 10,
    });
    let (code, _) = http_call(
        listen_addr,
        "POST",
        "/api/open-group",
        None,
        Some(json!({"group_name": "Demo测试群"})),
    )
    .await;
    assert_eq!(code, 403);

    driver.set_next_error(AdapterError::RateLimited {
        retry_after_ms: 500,
    });
    let (code, _) = http_call(listen_addr, "GET", "/api/windows", None, None).await;
    assert_eq!(code, 429);

    handle.abort();
}

#[tokio::test]
async fn extended_onebot_actions_and_storage_rest_endpoints() {
    let driver = Arc::new(FakeDriver::new());
    let mut config = AdapterConfig::default();
    config.server.port = 0;
    let (listen_addr, handle) = spawn_server(driver, config).await.unwrap();

    let (code, body) = http_call(
        listen_addr,
        "GET",
        "/onebot/v11/get_version_info",
        None,
        None,
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(body["data"]["app_name"], "qq-uia-adapter");
    assert_eq!(body["data"]["protocol_version"], "v11");

    let (code, body) =
        http_call(listen_addr, "GET", "/onebot/v11/get_login_info", None, None).await;
    assert_eq!(code, 200);
    assert_eq!(body["data"]["user_id"], 10_001);
    assert_eq!(body["data"]["nickname"], "BotSelf");

    let (code, body) =
        http_call(listen_addr, "GET", "/onebot/v11/get_group_list", None, None).await;
    assert_eq!(code, 200);
    assert_eq!(body["data"][0]["group_id"], 555_005);

    let (code, body) = http_call(
        listen_addr,
        "POST",
        "/onebot/v11/get_group_info",
        None,
        Some(json!({ "group_id": 555_005 })),
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(body["data"]["member_count"], 5);

    let (code, body) = http_call(
        listen_addr,
        "GET",
        "/onebot/v11/get_group_member_list?group_id=555005",
        None,
        None,
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(body["data"].as_array().unwrap().len(), 2);

    let (code, body) = http_call(
        listen_addr,
        "POST",
        "/onebot/v11/get_group_member_info",
        None,
        Some(json!({ "group_id": 555_005, "user_id": 444_555_666 })),
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(body["data"]["nickname"], "群友A");

    let (code, body) =
        http_call(listen_addr, "GET", "/onebot/v11/can_send_image", None, None).await;
    assert_eq!(code, 200);
    assert_eq!(body["data"]["yes"], true);

    let (code, body) = http_call(
        listen_addr,
        "GET",
        "/onebot/v11/can_send_record",
        None,
        None,
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(body["data"]["yes"], false);

    let (code, body) = http_call(listen_addr, "GET", "/api/groups", None, None).await;
    assert_eq!(code, 200);
    assert_eq!(body[0]["group_name"], "Demo测试群 (5)");

    let (code, body) = http_call(
        listen_addr,
        "GET",
        "/api/group-info?group_id=987654321",
        None,
        None,
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(body["real_group_id"], 987_654_321);

    let (code, body) = http_call(
        listen_addr,
        "GET",
        "/api/group-members?group_id=555005",
        None,
        None,
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(body.as_array().unwrap().len(), 2);

    let (code, body) = http_call(listen_addr, "GET", "/api/storage/stats", None, None).await;
    assert_eq!(code, 200);
    assert_eq!(body["db_path"], ":memory:");

    let (code, body) = http_call(listen_addr, "GET", "/api/storage", None, None).await;
    assert_eq!(code, 200);
    assert_eq!(body["stats"]["group_count"], 1);
    assert_eq!(body["groups"][0]["group_id"], 555_005);
    assert_eq!(body["friends"][0]["nickname"], "测试好友");

    handle.abort();
}

#[tokio::test]
async fn bearer_auth_and_onebot_ws_events() {
    let driver = Arc::new(FakeDriver::new());
    let mut config = AdapterConfig::default();
    config.server.port = 0;
    config.server.access_token = Some("secret-tok".to_string());
    let (listen_addr, handle) = spawn_server(driver.clone(), config).await.unwrap();

    let (code, _) = http_call(listen_addr, "GET", "/api/status", None, None).await;
    assert_eq!(code, 401);

    let (code, _) = http_call(listen_addr, "GET", "/api/status", Some("secret-tok"), None).await;
    assert_eq!(code, 200);

    let ws_url = format!("ws://{listen_addr}/onebot/v11/ws?access_token=secret-tok");
    let (mut ws_stream, _) = connect_async(&ws_url).await.unwrap();

    let first_event = next_ws_json(&mut ws_stream, Duration::from_secs(2)).await;
    assert_eq!(first_event["post_type"], "meta_event");
    assert_eq!(first_event["meta_event_type"], "lifecycle");
    assert_eq!(first_event["sub_type"], "connect");

    // 自身发送的消息回显不得通过 OneBot v11 WS 对外推送
    driver.broadcast_event(AdapterEvent::GroupMessage(ChatMessage {
        message_id: "self-1".to_string(),
        group_name: "Demo测试群 (5)".to_string(),
        sender: "BotSelf".to_string(),
        content: "echo".to_string(),
        timestamp_hint: None,
        is_self: true,
        captured_at_ms: 1_700_000_000_000,
        ..Default::default()
    }));

    // 他人发来的群消息必须附带真实数值标识实时转发
    driver.broadcast_event(AdapterEvent::GroupMessage(ChatMessage {
        message_id: "peer-1".to_string(),
        group_name: "Demo测试群 (5)".to_string(),
        sender: "群友A".to_string(),
        content: "ping".to_string(),
        timestamp_hint: Some("14:05".to_string()),
        is_self: false,
        captured_at_ms: 1_700_000_001_000,
        onebot_message_id: Some(77_001),
        group_id: Some(555_005),
        user_id: Some(20_001),
        ..Default::default()
    }));

    let incoming_event = next_ws_json(&mut ws_stream, Duration::from_secs(2)).await;
    assert_eq!(incoming_event["post_type"], "message");
    assert_eq!(incoming_event["message_type"], "group");
    assert_eq!(incoming_event["message_id"], 77_001);
    assert_eq!(incoming_event["group_id"], 555_005);
    assert_eq!(incoming_event["user_id"], 20_001);
    assert_eq!(incoming_event["sender"]["nickname"], "群友A");
    assert_eq!(incoming_event["raw_message"], "ping");

    // 私聊来信事件应推送为标准 OneBot message.private.friend
    driver.broadcast_event(AdapterEvent::PrivateMessage(ChatMessage {
        message_id: "priv-peer-1".to_string(),
        group_name: "测试好友".to_string(),
        sender: "测试好友".to_string(),
        content: "hi bot".to_string(),
        timestamp_hint: Some("14:06".to_string()),
        is_self: false,
        captured_at_ms: 1_700_000_002_000,
        onebot_message_id: Some(77_002),
        group_id: None,
        user_id: Some(777_001),
        target_kind: ChatTargetKind::Private,
        segments: vec![MessageSegment::Text {
            text: "hi bot".to_string(),
        }],
        ..Default::default()
    }));
    let priv_event = next_ws_json(&mut ws_stream, Duration::from_secs(2)).await;
    assert_eq!(priv_event["post_type"], "message");
    assert_eq!(priv_event["message_type"], "private");
    assert_eq!(priv_event["sub_type"], "friend");
    assert!(priv_event.get("group_id").is_none());
    assert_eq!(priv_event["user_id"], 777_001);
    assert_eq!(priv_event["raw_message"], "hi bot");

    handle.abort();
}

#[tokio::test]
async fn ensure_qq_and_graceful_shutdown_endpoints() {
    let driver = Arc::new(FakeDriver::new());
    let mut config = AdapterConfig::default();
    config.server.port = 0;
    let (listen_addr, handle) = spawn_server(driver, config).await.unwrap();

    let (code, body) = http_call(listen_addr, "POST", "/api/ensure-qq", None, None).await;
    assert_eq!(code, 200);
    assert_eq!(body[0]["class_name"], "MockQQNTWndClass");

    let (code, body) = http_call(listen_addr, "POST", "/api/shutdown", None, None).await;
    assert_eq!(code, 200);
    assert_eq!(body["ok"], true);
    assert_eq!(body["shutting_down"], true);

    tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("server did not shut down cleanly")
        .unwrap();
}

#[test]
fn restarter_resolution_copy_and_command_builder_avoid_powershell() {
    let missing = Path::new("nonexistent/release/qq-uia-adapter.exe");
    let error = resolve_restarter_source_with_fallback(missing, None).unwrap_err();
    let (status_code, code, _) = map_adapter_error(&error);
    assert_eq!(status_code, StatusCode::BAD_REQUEST);
    assert_eq!(code, "BAD_REQUEST");

    let response = ApiError::from(error).into_response();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "qq-uia-restarter-test-{}-{}",
        std::process::id(),
        nanos
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let _cleanup = TempDirGuard(dir.clone());

    let fake_release = dir.join("release.exe");
    let fake_fallback = dir.join("fallback.exe");
    std::fs::write(&fake_release, b"release-bytes").unwrap();
    std::fs::write(&fake_fallback, b"fallback-bytes").unwrap();

    assert_eq!(
        resolve_restarter_source_with_fallback(&fake_release, Some(&fake_fallback)).unwrap(),
        fake_release
    );
    assert_eq!(
        resolve_restarter_source_with_fallback(missing, Some(&fake_fallback)).unwrap(),
        fake_fallback
    );

    let runtime_bin = dir.join("bin");
    let copied = prepare_restarter_binary(&fake_release, &runtime_bin).unwrap();
    assert_eq!(copied, runtime_bin.join("qq-uia-adapter-restarter.exe"));
    assert_eq!(std::fs::read(&copied).unwrap(), b"release-bytes");

    let log_path = dir.join("logs").join("restart.log");
    let command = build_restarter_command(&copied, Some(&log_path));
    assert_eq!(command.get_program(), copied.as_os_str());
    assert_eq!(command.get_args().collect::<Vec<_>>(), vec!["start-daemon"]);
    assert!(log_path.is_file());
    assert_eq!(RESTART_CREATION_FLAGS, 0x0800_0200);
    assert_ne!(RESTART_CREATION_FLAGS & 0x0000_0008, 0x0000_0008);

    let prepended = prepend_path_dir(
        Some(std::ffi::OsStr::new(r"C:\Windows\System32")),
        r"C:\DevTools\mingw64\bin",
    );
    assert_eq!(prepended, r"C:\DevTools\mingw64\bin;C:\Windows\System32");
    let unchanged = prepend_path_dir(
        Some(std::ffi::OsStr::new(r"C:\Windows;c:\devtools\mingw64\bin\")),
        r"C:\DevTools\mingw64\bin",
    );
    assert_eq!(unchanged, r"C:\Windows;c:\devtools\mingw64\bin\");
}

#[tokio::test]
async fn diag_and_ensure_qq_force_wake_endpoints_forward_queries() {
    let driver = Arc::new(FakeDriver::new());
    let mut config = AdapterConfig::default();
    config.server.port = 0;
    let (listen_addr, handle) = spawn_server(driver.clone(), config).await.unwrap();

    let (code, body) = http_call(listen_addr, "POST", "/api/ensure-qq", None, None).await;
    assert_eq!(code, 200);
    assert_eq!(body[0]["hwnd"], 42);

    let (code, body) = http_call(
        listen_addr,
        "POST",
        "/api/ensure-qq?force_wake=true",
        None,
        None,
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(body[0]["hwnd"], 42);
    assert_eq!(driver.ensure_force_wake_calls(), vec![false, true]);

    let (code, body) = http_call(listen_addr, "GET", "/api/diag", None, None).await;
    assert_eq!(code, 200);
    assert_eq!(body["target_hwnd"], 42);
    assert_eq!(body["wake_report"]["visited_msaa_nodes"], 7);
    assert_eq!(body["uia_semantic_count"], 5);

    let (code, body) = http_call(listen_addr, "GET", "/api/diag?hwnd=12345", None, None).await;
    assert_eq!(code, 200);
    assert_eq!(body["target_hwnd"], 12345);
    assert_eq!(driver.diag_request_calls(), vec![None, Some(12345)]);

    handle.abort();
}

#[tokio::test]
async fn v3_segments_private_chat_and_whitelist_guard() {
    // 1. 空白名单下，私聊入口必须统一返回 403 / 1403 拒绝放行
    let blocked_driver = Arc::new(FakeDriver::new());
    let mut blocked_cfg = AdapterConfig::default();
    blocked_cfg.server.port = 0;
    let (blocked_addr, blocked_handle) = spawn_server(blocked_driver.clone(), blocked_cfg)
        .await
        .unwrap();

    let (code, body) = http_call(
        blocked_addr,
        "POST",
        "/onebot/v11/send_private_msg",
        None,
        Some(json!({ "nickname": "测试好友", "message": "should block" })),
    )
    .await;
    assert_eq!(code, 403);
    assert_eq!(body["retcode"], 1403);
    assert_eq!(body["code"], "PRIVATE_TARGET_NOT_ALLOWED");

    let (code, body) = http_call(
        blocked_addr,
        "POST",
        "/onebot/v11/send_private_msg",
        None,
        Some(json!({ "user_id": 777_001, "message": "should block by id" })),
    )
    .await;
    assert_eq!(code, 403);
    assert_eq!(body["retcode"], 1403);

    let (code, body) = http_call(
        blocked_addr,
        "POST",
        "/api/send",
        None,
        Some(json!({
            "group": "测试好友",
            "text": "should block direct",
            "target_kind": "private"
        })),
    )
    .await;
    assert_eq!(code, 403);
    assert_eq!(body["code"], "PRIVATE_TARGET_NOT_ALLOWED");
    assert!(blocked_driver.sent_requests().is_empty());
    blocked_handle.abort();

    // 2. 配置好友白名单后，校验好友查询、私聊发送、复合消息段及 get_msg 结构化回放
    let driver = Arc::new(FakeDriver::new());
    driver.allow_private_friend("测试好友");
    let mut config = AdapterConfig::default();
    config.server.port = 0;
    config.target.allowed_private_friends = vec!["测试好友".to_string()];
    let (listen_addr, handle) = spawn_server(driver.clone(), config).await.unwrap();

    let (code, body) = http_call(
        listen_addr,
        "GET",
        "/onebot/v11/get_friend_list?no_cache=true",
        None,
        None,
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(body["data"][0]["user_id"], 777_001);
    assert_eq!(body["data"][0]["nickname"], "测试好友");

    let (code, body) = http_call(
        listen_addr,
        "GET",
        "/api/friends?no_cache=false",
        None,
        None,
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(body[0]["nickname"], "测试好友");

    let (code, body) = http_call(
        listen_addr,
        "POST",
        "/onebot/v11/send_private_msg",
        None,
        Some(json!({ "user_id": 777_001, "message": "hello friend" })),
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(body["retcode"], 0);
    assert_eq!(body["data"]["user_id"], 777_001);
    assert_eq!(
        driver.sent_requests()[0].target_kind,
        ChatTargetKind::Private
    );

    let (code, body) = http_call(
        listen_addr,
        "POST",
        "/onebot/v11/send_msg",
        None,
        Some(json!({
            "message_type": "private",
            "nickname": "测试好友",
            "message": "via send_msg"
        })),
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(body["retcode"], 0);

    let (code, body) = http_call(
        listen_addr,
        "POST",
        "/onebot/v11/send_group_msg",
        None,
        Some(json!({
            "group_id": 555_005,
            "message": [
                { "type": "reply", "data": { "id": "10001" } },
                { "type": "at", "data": { "qq": "20001", "name": "群友A" } },
                { "type": "text", "data": { "text": "收到" } },
                { "type": "image", "data": { "file": "base64://iVBORw0KGgo=" } }
            ]
        })),
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(body["retcode"], 0);
    let sent_id = body["data"]["message_id"].as_i64().unwrap();
    let recorded_segs = &driver.sent_requests()[2].segments;
    assert_eq!(recorded_segs.len(), 4);
    assert_eq!(
        recorded_segs[0],
        MessageSegment::Reply {
            id: "10001".to_string()
        }
    );

    let (code, body) = http_call(
        listen_addr,
        "GET",
        &format!("/onebot/v11/get_msg?message_id={sent_id}"),
        None,
        None,
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(body["data"]["message_type"], "group");
    assert_eq!(body["data"]["message"][0]["type"], "reply");
    assert_eq!(body["data"]["message"][0]["data"]["id"], "10001");
    assert_eq!(body["data"]["message"][1]["type"], "at");
    assert_eq!(body["data"]["message"][3]["type"], "image");
    assert!(body["data"]["raw_message"]
        .as_str()
        .unwrap()
        .contains("[CQ:reply,id=10001]"));

    handle.abort();
}

#[tokio::test]
async fn shutdown_latch_and_concurrent_send_serialization_work() {
    let state = crate::AppState::new(Arc::new(FakeDriver::new()), AdapterConfig::default());
    state.trigger_shutdown();
    assert!(
        tokio::time::timeout(Duration::from_millis(50), state.wait_shutdown())
            .await
            .is_ok(),
        "wait_shutdown triggered before subscription must return immediately"
    );

    let driver = Arc::new(FakeDriver::new());
    driver.allow_private_friend("测试好友");
    let mut config = AdapterConfig::default();
    config.server.port = 0;
    config.target.allowed_private_friends = vec!["测试好友".to_string()];
    let (listen_addr, handle) = spawn_server(driver.clone(), config).await.unwrap();

    let ((group_code, group_body), (private_code, private_body)) = tokio::join!(
        http_call(
            listen_addr,
            "POST",
            "/onebot/v11/send_group_msg",
            None,
            Some(json!({ "group_id": 555_005, "message": "concurrent group" })),
        ),
        http_call(
            listen_addr,
            "POST",
            "/onebot/v11/send_private_msg",
            None,
            Some(json!({ "user_id": 777_001, "message": "concurrent private" })),
        ),
    );

    assert_eq!(group_code, 200);
    assert_eq!(group_body["retcode"], 0);
    assert_eq!(private_code, 200);
    assert_eq!(private_body["retcode"], 0);
    let group_msg_id = group_body["data"]["message_id"].as_i64().unwrap();
    let private_msg_id = private_body["data"]["message_id"].as_i64().unwrap();
    assert_ne!(group_msg_id, private_msg_id);
    assert_eq!(driver.sent_requests().len(), 2);

    handle.abort();
}
