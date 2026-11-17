//! 洪峰调度器、响应窗口截止保护、已读定位、词切片与 OneBot 协议精细化集成测试

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use chrono::Utc;
use common_types::{
    AdapterConfig, BurstModeState, ChatMessage, ChatTargetKind, MessagePriorityTier,
    MessageSegment, SchedulerTelemetrySnapshot, StoredMessageRecord,
};
use futures_util::SinkExt;
use serde_json::json;
use tokio::net::TcpListener;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

use super::fake_driver::FakeDriver;
use super::{http_call, next_ws_json};
use crate::constants::{MEDIA_CACHE_SUBDIR, ROUTE_API_OCR, STAGING_UPLOADS_SUBDIR};
use crate::onebot::payloads::chat_message_to_onebot_event;
use crate::{build_router_with_state, spawn_server, AppState};

fn now_ms() -> u64 {
    Utc::now().timestamp_millis().max(0) as u64
}

#[tokio::test]
async fn outbound_deadline_expiration_returns_408_and_1408_across_send_rest_and_quick_ops() {
    let driver = Arc::new(FakeDriver::new());
    let mut config = AdapterConfig::default();
    config.server.port = 0;
    config.target.group_name = "Demo测试群 (5)".to_string();

    let (addr, handle) = spawn_server(driver.clone(), config).await.unwrap();
    let expired_deadline = now_ms().saturating_sub(5_000);

    // 1. OneBot send_group_msg 绝对截止时间已过期应返回 HTTP 408 与 retcode 1408
    let (code, body) = http_call(
        addr,
        "POST",
        "/onebot/v11/send_group_msg",
        None,
        Some(json!({
            "group_id": 555_005,
            "message": "too late",
            "deadline_unix_ms": expired_deadline,
        })),
    )
    .await;
    assert_eq!(code, 408);
    assert_eq!(body["code"], "RESPONSE_WINDOW_EXPIRED");
    assert_eq!(body["retcode"], 1408);
    assert_eq!(body["status"], "failed");

    // 2. OneBot send_group_msg 传入 response_window_ms = 0 也应立即触发超时中止
    let (code, body) = http_call(
        addr,
        "POST",
        "/onebot/v11/send_group_msg",
        None,
        Some(json!({
            "group_id": 555_005,
            "message": "zero window",
            "response_window_ms": 0,
        })),
    )
    .await;
    assert_eq!(code, 408);
    assert_eq!(body["retcode"], 1408);

    // 3. 原生 POST /api/send 携带过期截止时间戳应返回 HTTP 408
    let (code, body) = http_call(
        addr,
        "POST",
        "/api/send",
        None,
        Some(json!({
            "group": "Demo测试群 (5)",
            "text": "stale rest message",
            "target_kind": "group",
            "deadline_unix_ms": expired_deadline,
        })),
    )
    .await;
    assert_eq!(code, 408);
    assert_eq!(body["code"], "RESPONSE_WINDOW_EXPIRED");

    // 4. .handle_quick_operation 携带已过期的 response_deadline_ms 应被拦截返回 1408
    let (code, body) = http_call(
        addr,
        "POST",
        "/onebot/v11/.handle_quick_operation",
        None,
        Some(json!({
            "context": {
                "message_type": "group",
                "group_id": 555_005,
                "message_id": 10_001,
                "response_deadline_ms": expired_deadline,
            },
            "operation": {
                "reply": "stale quick reply"
            }
        })),
    )
    .await;
    assert_eq!(code, 408);
    assert_eq!(body["retcode"], 1408);

    // 5. .handle_quick_operation 仅携带陈旧 captured_at_ms（超出 critical_response_window_ms）也应被拦截
    let stale_capture = now_ms().saturating_sub(120_000);
    let (code, body) = http_call(
        addr,
        "POST",
        "/onebot/v11/.handle_quick_operation",
        None,
        Some(json!({
            "context": {
                "message_type": "group",
                "group_id": 555_005,
                "message_id": 10_001,
                "captured_at_ms": stale_capture,
            },
            "operation": {
                "reply": "expired by captured_at_ms"
            }
        })),
    )
    .await;
    assert_eq!(code, 408);
    assert_eq!(body["retcode"], 1408);
    assert!(driver.sent_requests().is_empty());

    // 6. 合法有效窗口内的 POST /api/send 应正常下发并透传计算后的截止时间给驱动
    let (code, body) = http_call(
        addr,
        "POST",
        "/api/send",
        None,
        Some(json!({
            "group": "Demo测试群 (5)",
            "text": "fresh reply",
            "target_kind": "group",
            "response_window_ms": 10_000,
        })),
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(body["ok"], true);
    let sent = driver.sent_requests();
    assert_eq!(sent.len(), 1);
    assert!(sent[0].deadline_unix_ms.is_some());
    assert_eq!(driver.outbound_deadline_abort_count(), 5);

    handle.abort();
}

#[tokio::test]
async fn outbound_deadline_aborts_after_waiting_on_send_gate() {
    let driver = Arc::new(FakeDriver::new());
    let mut config = AdapterConfig::default();
    config.server.port = 0;
    config.target.group_name = "Demo测试群 (5)".to_string();

    let state = AppState::new(driver.clone(), config);
    let router = build_router_with_state(state.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server_handle = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    // 先由测试协程持有全局发送互斥锁，模拟前一轮 UIA 发送占用发送队列
    let gate_guard = state.send_gate.lock().await;
    let onebot_task = tokio::spawn(http_call(
        addr,
        "POST",
        "/onebot/v11/send_group_msg",
        None,
        Some(json!({
            "group_id": 555_005,
            "message": "queued onebot",
            "response_window_ms": 60,
        })),
    ));
    let rest_task = tokio::spawn(http_call(
        addr,
        "POST",
        "/api/send",
        None,
        Some(json!({
            "group": "Demo测试群 (5)",
            "text": "queued rest",
            "target_kind": "group",
            "response_window_ms": 60,
        })),
    ));

    tokio::time::sleep(Duration::from_millis(180)).await;
    drop(gate_guard);

    let (ob_code, ob_body) = onebot_task.await.unwrap();
    let (rest_code, rest_body) = rest_task.await.unwrap();

    assert_eq!(ob_code, 408);
    assert_eq!(ob_body["retcode"], 1408);
    assert_eq!(ob_body["code"], "RESPONSE_WINDOW_EXPIRED");
    assert_eq!(rest_code, 408);
    assert_eq!(rest_body["code"], "RESPONSE_WINDOW_EXPIRED");
    assert!(driver.sent_requests().is_empty());
    assert_eq!(driver.outbound_deadline_abort_count(), 2);

    server_handle.abort();
}

#[tokio::test]
async fn mark_read_actions_and_private_whitelist_enforcement() {
    let driver = Arc::new(FakeDriver::new());
    driver.allow_private_friend("测试好友");
    driver.insert_stored_message(StoredMessageRecord {
        onebot_message_id: 10_050,
        message_uid: "m-priv-ok".to_string(),
        group_name: "测试好友".to_string(),
        group_id: 0,
        user_id: 777_001,
        sender_name: "测试好友".to_string(),
        raw_message: "hi".to_string(),
        chat_type: ChatTargetKind::Private,
        segments: vec![MessageSegment::Text {
            text: "hi".to_string(),
        }],
        is_self: false,
        timestamp_hint: Some("14:05".to_string()),
        captured_at_ms: now_ms(),
    });
    driver.insert_stored_message(StoredMessageRecord {
        onebot_message_id: 10_051,
        message_uid: "m-priv-deny".to_string(),
        group_name: "陌生人B".to_string(),
        group_id: 0,
        user_id: 777_999,
        sender_name: "陌生人B".to_string(),
        raw_message: "secret".to_string(),
        chat_type: ChatTargetKind::Private,
        segments: vec![MessageSegment::Text {
            text: "secret".to_string(),
        }],
        is_self: false,
        timestamp_hint: Some("14:06".to_string()),
        captured_at_ms: now_ms(),
    });

    let mut config = AdapterConfig::default();
    config.server.port = 0;
    config.target.group_name = "Demo测试群 (5)".to_string();
    config.target.allowed_private_friends = vec!["测试好友".to_string()];

    let (addr, handle) = spawn_server(driver.clone(), config).await.unwrap();

    // 1. mark_msg_as_read 定位群消息与白名单私聊消息均应成功
    let (code, body) = http_call(
        addr,
        "POST",
        "/onebot/v11/mark_msg_as_read",
        None,
        Some(json!({ "message_id": 10_001 })),
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(body["retcode"], 0);
    assert_eq!(body["data"]["target"], "Demo测试群 (5)");

    let (code, body) = http_call(
        addr,
        "POST",
        "/onebot/v11/mark_msg_as_read",
        None,
        Some(json!({ "message_id": 10_050 })),
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(body["retcode"], 0);
    assert_eq!(body["data"]["target"], "测试好友");

    // 2. mark_msg_as_read 涉及非白名单私聊消息必须返回 403 / 1403
    let (code, body) = http_call(
        addr,
        "POST",
        "/onebot/v11/mark_msg_as_read",
        None,
        Some(json!({ "message_id": 10_051 })),
    )
    .await;
    assert_eq!(code, 403);
    assert_eq!(body["code"], "PRIVATE_TARGET_NOT_ALLOWED");
    assert_eq!(body["retcode"], 1403);

    // 3. mark_group_msg_as_read 按群号解析与未知群号报错校验
    let (code, body) = http_call(
        addr,
        "POST",
        "/onebot/v11/mark_group_msg_as_read",
        None,
        Some(json!({ "group_id": 555_005 })),
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(body["retcode"], 0);
    assert_eq!(body["data"]["target"], "Demo测试群 (5)");

    let (code, body) = http_call(
        addr,
        "POST",
        "/onebot/v11/mark_group_msg_as_read",
        None,
        Some(json!({ "group_id": 999_999 })),
    )
    .await;
    assert_eq!(code, 404);
    assert_eq!(body["code"], "NOT_FOUND");
    assert_eq!(body["retcode"], 1404);

    // 4. mark_private_msg_as_read 白名单放行与非白名单拦截校验
    let (code, body) = http_call(
        addr,
        "POST",
        "/onebot/v11/mark_private_msg_as_read",
        None,
        Some(json!({ "user_id": 777_001 })),
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(body["retcode"], 0);
    assert_eq!(body["data"]["target"], "测试好友");

    let (code, body) = http_call(
        addr,
        "POST",
        "/onebot/v11/mark_private_msg_as_read",
        None,
        Some(json!({ "nickname": "陌生人B" })),
    )
    .await;
    assert_eq!(code, 403);
    assert_eq!(body["retcode"], 1403);

    let recorded = driver.mark_read_calls();
    assert_eq!(
        recorded,
        vec![
            ("Demo测试群 (5)".to_string(), ChatTargetKind::Group),
            ("测试好友".to_string(), ChatTargetKind::Private),
            ("Demo测试群 (5)".to_string(), ChatTargetKind::Group),
            ("测试好友".to_string(), ChatTargetKind::Private),
        ]
    );

    handle.abort();
}

#[tokio::test]
async fn word_slices_and_scheduler_status_actions() {
    let driver = Arc::new(FakeDriver::new());
    driver.set_scheduler_telemetry(SchedulerTelemetrySnapshot {
        burst_state: BurstModeState::BurstActive,
        burst_enter_count: 3,
        pending_total: 12,
        pending_p0: 2,
        selective_shed_p2_total: 19,
        active_monitored_groups: 2,
        ..Default::default()
    });

    let mut config = AdapterConfig::default();
    config.server.port = 0;

    let (addr, handle) = spawn_server(driver.clone(), config).await.unwrap();

    // 1. .get_word_slices 验证 CJK 字符、ASCII 单词与标点的确定性切分
    let (code, body) = http_call(
        addr,
        "POST",
        "/onebot/v11/.get_word_slices",
        None,
        Some(json!({ "content": "Hello, QQ机器人123!" })),
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(body["retcode"], 0);
    assert_eq!(
        body["data"]["slices"],
        json!(["Hello", ",", "QQ", "机", "器", "人", "123", "!"])
    );

    let (code, body) = http_call(
        addr,
        "POST",
        "/onebot/v11/.get_word_slices",
        None,
        Some(json!({ "content": "   " })),
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(body["data"]["slices"], json!([]));

    let (code, body) = http_call(
        addr,
        "POST",
        "/onebot/v11/.get_word_slices",
        None,
        Some(json!({})),
    )
    .await;
    assert_eq!(code, 400);
    assert_eq!(body["code"], "BAD_REQUEST");

    // 2. .get_scheduler_status 返回调度器策略配置与遥测快照
    let (code, body) =
        http_call(addr, "GET", "/onebot/v11/.get_scheduler_status", None, None).await;
    assert_eq!(code, 200);
    assert_eq!(body["retcode"], 0);
    assert!(body["data"]["config"]["burst_window_ms"].as_u64().is_some());
    assert_eq!(body["data"]["telemetry"]["burst_state"], "burst_active");
    assert_eq!(body["data"]["telemetry"]["selective_shed_p2_total"], 19);

    handle.abort();
}

#[tokio::test]
async fn rest_scheduler_and_policy_and_sessions_read_routes() {
    let driver = Arc::new(FakeDriver::new());
    driver.allow_private_friend("测试好友");

    let mut config = AdapterConfig::default();
    config.server.port = 0;
    config.target.group_name = "Demo测试群 (5)".to_string();
    config.target.monitored_groups = vec!["Demo测试群 (5)".to_string(), "大群A".to_string()];
    config.target.allow_large_monitored_groups = true;
    config.target.strict_below_members = Some(10);
    config.target.allowed_private_friends = vec!["测试好友".to_string()];

    let (addr, handle) = spawn_server(driver.clone(), config).await.unwrap();

    // 1. GET /api/scheduler 返回配置、遥测与多群监控守卫信息
    let (code, body) = http_call(addr, "GET", "/api/scheduler", None, None).await;
    assert_eq!(code, 200);
    assert_eq!(body["ok"], true);
    assert_eq!(body["allow_large_monitored_groups"], true);
    assert_eq!(body["strict_below_members"], 10);
    assert_eq!(body["monitored_groups"], json!(["Demo测试群 (5)", "大群A"]));

    // 2. POST /api/scheduler/policy 支持增量合法更新并拒绝 4 类非法边界
    let (code, body) = http_call(
        addr,
        "POST",
        "/api/scheduler/policy",
        None,
        Some(json!({
            "burst_window_ms": 4_500,
            "burst_enter_threshold": 24,
            "burst_exit_threshold": 9,
        })),
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(body["ok"], true);
    assert_eq!(body["config"]["burst_window_ms"], 4_500);
    assert_eq!(body["config"]["burst_enter_threshold"], 24);
    assert_eq!(body["config"]["burst_exit_threshold"], 9);

    let (code, body) = http_call(addr, "GET", "/api/scheduler/policy", None, None).await;
    assert_eq!(code, 200);
    assert_eq!(body["config"]["burst_window_ms"], 4_500);

    for invalid_patch in [
        json!({ "burst_window_ms": 0 }),
        json!({ "critical_response_window_ms": 0 }),
        json!({ "burst_enter_threshold": 5, "burst_exit_threshold": 6 }),
        json!({ "max_pending_events": 0 }),
    ] {
        let (code, body) = http_call(
            addr,
            "POST",
            "/api/scheduler/policy",
            None,
            Some(invalid_patch),
        )
        .await;
        assert_eq!(code, 400);
        assert_eq!(body["code"], "BAD_REQUEST");
    }

    // 3. POST /api/sessions/read 支持群号、私聊 user_id 与白名单/参数校验
    let (code, body) = http_call(
        addr,
        "POST",
        "/api/sessions/read",
        None,
        Some(json!({ "group_id": 555_005 })),
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(body["ok"], true);
    assert_eq!(body["target"], "Demo测试群 (5)");
    assert_eq!(body["target_kind"], "group");

    let (code, body) = http_call(
        addr,
        "POST",
        "/api/sessions/read",
        None,
        Some(json!({ "user_id": 777_001 })),
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(body["target"], "测试好友");
    assert_eq!(body["target_kind"], "private");

    let (code, body) = http_call(
        addr,
        "POST",
        "/api/sessions/read",
        None,
        Some(json!({ "target": "非白名单好友", "target_kind": "private" })),
    )
    .await;
    assert_eq!(code, 403);
    assert_eq!(body["code"], "PRIVATE_TARGET_NOT_ALLOWED");

    let (code, body) = http_call(addr, "POST", "/api/sessions/read", None, Some(json!({}))).await;
    assert_eq!(code, 400);
    assert_eq!(body["code"], "BAD_REQUEST");

    handle.abort();
}

#[tokio::test]
async fn unsupported_qqnt_onebot_actions_return_10004() {
    let driver = Arc::new(FakeDriver::new());
    let mut config = AdapterConfig::default();
    config.server.port = 0;

    let (addr, handle) = spawn_server(driver, config).await.unwrap();

    for action in [
        "get_group_honor_info",
        "get_group_system_msg",
        "get_group_at_all_remain",
        "get_essence_msg_list",
        "get_group_root_files",
        "download_file",
        "set_qq_profile",
        "set_msg_emoji_like",
        "fetch_emoji_like",
        "set_essence_msg",
        "delete_essence_msg",
        "_send_group_notice",
        "_get_group_notice",
        "delete_friend",
        "set_qq_avatar",
        "get_group_file_system_info",
        "get_group_files_by_folder",
        "get_group_file_url",
    ] {
        let path = format!("/onebot/v11/{action}");
        let (code, body) = http_call(addr, "POST", &path, None, Some(json!({}))).await;
        assert_eq!(code, 501, "expected 501 for {action}");
        assert_eq!(body["retcode"], 10004, "expected 10004 for {action}");
        assert_eq!(body["status"], "failed");
        assert_eq!(body["code"], "UNSUPPORTED_ACTION");
    }

    handle.abort();
}

#[tokio::test]
async fn upgraded_onebot_payloads_get_status_and_ws_heartbeat() {
    let ts_ms = now_ms();
    let deadline_ms = ts_ms.saturating_add(15_000);
    let event_json = chat_message_to_onebot_event(
        10_001,
        &ChatMessage {
            message_id: "m-up".to_string(),
            group_name: "Demo测试群 (5)".to_string(),
            sender: "群友A".to_string(),
            content: "hello".to_string(),
            timestamp_hint: Some("15:00".to_string()),
            is_self: false,
            captured_at_ms: ts_ms,
            onebot_message_id: Some(88_001),
            group_id: Some(555_005),
            user_id: Some(20_001),
            target_kind: ChatTargetKind::Group,
            segments: vec![MessageSegment::Text {
                text: "hello".to_string(),
            }],
            sender_role: Some("admin".to_string()),
            sender_card: Some("群管名片".to_string()),
            priority_tier: Some(MessagePriorityTier::CriticalBotTrigger),
            response_deadline_ms: Some(deadline_ms),
            ..Default::default()
        },
    );
    assert_eq!(event_json["font"], 0);
    assert_eq!(event_json["message_seq"], 88_001);
    assert_eq!(event_json["real_id"], 88_001);
    assert!(event_json["anonymous"].is_null());
    assert_eq!(event_json["sender"]["card"], "群管名片");
    assert_eq!(event_json["sender"]["role"], "admin");
    assert_eq!(event_json["sender"]["sex"], "unknown");
    assert_eq!(event_json["captured_at_ms"], ts_ms);
    assert_eq!(event_json["response_deadline_ms"], deadline_ms);
    assert_eq!(event_json["priority_tier"], "critical_bot_trigger");

    let driver = Arc::new(FakeDriver::new());
    driver.set_scheduler_telemetry(SchedulerTelemetrySnapshot {
        burst_state: BurstModeState::Normal,
        expired_window_total: 2,
        outbound_deadline_aborted_total: 1,
        ..Default::default()
    });
    let mut config = AdapterConfig::default();
    config.server.port = 0;
    config.target.group_name = "Demo测试群 (5)".to_string();
    config.onebot.heartbeat_interval_ms = 500;

    let (addr, handle) = spawn_server(driver, config).await.unwrap();

    // 校验 get_msg 升级字段
    let (code, body) = http_call(
        addr,
        "GET",
        "/onebot/v11/get_msg?message_id=10001",
        None,
        None,
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(body["data"]["font"], 0);
    assert_eq!(body["data"]["message_seq"], 10_001);
    assert_eq!(body["data"]["real_id"], 10_001);
    assert!(body["data"]["anonymous"].is_null());
    assert_eq!(body["data"]["sender"]["role"], "member");
    assert_eq!(body["data"]["sender"]["card"], "群友A");

    // 先发送一条群消息触发 last_message_time 记录，再核验 get_status 与 WS 心跳载荷
    let (code, _) = http_call(
        addr,
        "POST",
        "/onebot/v11/send_group_msg",
        None,
        Some(json!({
            "group_id": 555_005,
            "message": "ping",
        })),
    )
    .await;
    assert_eq!(code, 200);

    let (code, body) = http_call(addr, "GET", "/onebot/v11/get_status", None, None).await;
    assert_eq!(code, 200);
    let stat = &body["data"]["stat"];
    assert_eq!(stat["message_received"], 1);
    assert_eq!(stat["message_sent"], 1);
    assert_eq!(stat["packet_received"], 1);
    assert_eq!(stat["packet_sent"], 1);
    assert_eq!(stat["packet_lost"], 3);
    assert_eq!(stat["lost_times"], 1);
    assert!(stat["last_message_time"].as_i64().unwrap_or(0) > 0);
    assert_eq!(body["data"]["scheduler"]["expired_window_total"], 2);

    let ws_url = format!("ws://{addr}/onebot/v11/ws");
    let (mut ws_stream, _) = connect_async(&ws_url).await.unwrap();
    let connect_frame = next_ws_json(&mut ws_stream, Duration::from_secs(2)).await;
    assert_eq!(connect_frame["meta_event_type"], "lifecycle");

    let hb_frame = next_ws_json(&mut ws_stream, Duration::from_secs(3)).await;
    assert_eq!(hb_frame["meta_event_type"], "heartbeat");
    assert_eq!(hb_frame["status"]["stat"]["message_sent"], 1);
    assert_eq!(hb_frame["status"]["stat"]["packet_lost"], 3);
    assert_eq!(
        hb_frame["status"]["scheduler"]["outbound_deadline_aborted_total"],
        1
    );

    handle.abort();
}

/// 清理 `get_file` 集成测试创建的临时 `.runtime` 目录与测试文件
struct StagingRuntimeGuard {
    runtime_dir: PathBuf,
    remove_whole_runtime: bool,
    created_paths: Vec<PathBuf>,
}

impl Drop for StagingRuntimeGuard {
    fn drop(&mut self) {
        for path in &self.created_paths {
            if path.is_dir() {
                let _ = std::fs::remove_dir_all(path);
            } else {
                let _ = std::fs::remove_file(path);
            }
        }
        if self.remove_whole_runtime {
            let _ = std::fs::remove_dir_all(&self.runtime_dir);
        }
    }
}

#[tokio::test]
async fn ocr_video_upload_file_and_sandboxed_get_file_work_across_rest_http_and_ws() {
    let driver = Arc::new(FakeDriver::new());
    driver.allow_private_friend("测试好友");

    let mut config = AdapterConfig::default();
    config.server.port = 0;
    config.target.group_name = "Demo测试群 (5)".to_string();
    config.target.allowed_private_friends = vec!["测试好友".to_string()];

    let (addr, handle) = spawn_server(driver.clone(), config).await.unwrap();

    // 1. 原生 REST POST /api/ocr 校验空输入拒绝与成功识别返回
    let (empty_code, empty_body) = http_call(
        addr,
        "POST",
        ROUTE_API_OCR,
        None,
        Some(json!({ "image": "  " })),
    )
    .await;
    assert_eq!(empty_code, 400);
    assert_eq!(empty_body["code"], "BAD_REQUEST");

    let (ocr_rest_code, ocr_rest_body) = http_call(
        addr,
        "POST",
        ROUTE_API_OCR,
        None,
        Some(json!({ "image": "base64://AAAA", "language": "en-US" })),
    )
    .await;
    assert_eq!(ocr_rest_code, 200);
    assert_eq!(ocr_rest_body["language"], "en-US");
    assert_eq!(ocr_rest_body["texts"][0]["text"], "MOCK OCR 2026");

    // 2. OneBot HTTP ocr_image / .ocr_image 与 can_send_video
    let (ocr_ob_code, ocr_ob_body) = http_call(
        addr,
        "POST",
        "/onebot/v11/ocr_image",
        None,
        Some(json!({ "image": "sample.png", "lang": "zh-Hans-CN" })),
    )
    .await;
    assert_eq!(ocr_ob_code, 200);
    assert_eq!(ocr_ob_body["retcode"], 0);
    assert_eq!(ocr_ob_body["data"]["texts"][0]["confidence"], 95);

    let (dot_ocr_code, dot_ocr_body) = http_call(
        addr,
        "POST",
        "/onebot/v11/.ocr_image",
        None,
        Some(json!({ "file": "sample_dot.png" })),
    )
    .await;
    assert_eq!(dot_ocr_code, 200);
    assert_eq!(dot_ocr_body["retcode"], 0);
    assert_eq!(driver.ocr_calls().len(), 3);

    let (vid_cap_code, vid_cap_body) =
        http_call(addr, "GET", "/onebot/v11/can_send_video", None, None).await;
    assert_eq!(vid_cap_code, 200);
    assert_eq!(vid_cap_body["data"]["yes"], true);

    // 3. upload_group_file: 拒绝子目录、拒绝 http(s)://，根目录/省略则走群文件投递
    let (bad_folder_code, bad_folder_body) = http_call(
        addr,
        "POST",
        "/onebot/v11/upload_group_file",
        None,
        Some(json!({
            "group_id": 555_005,
            "file": "report.pdf",
            "folder": "/reports"
        })),
    )
    .await;
    assert_eq!(bad_folder_code, 501);
    assert_eq!(bad_folder_body["retcode"], 10004);

    let (bad_url_code, bad_url_body) = http_call(
        addr,
        "POST",
        "/onebot/v11/upload_group_file",
        None,
        Some(json!({
            "group_id": 555_005,
            "file": "http://example.com/report.pdf"
        })),
    )
    .await;
    assert_eq!(bad_url_code, 501);
    assert_eq!(bad_url_body["retcode"], 10004);

    let (up_grp_code, up_grp_body) = http_call(
        addr,
        "POST",
        "/onebot/v11/upload_group_file",
        None,
        Some(json!({
            "group_id": 555_005,
            "file": "report.pdf",
            "name": "月报.pdf",
            "folder": "/"
        })),
    )
    .await;
    assert_eq!(up_grp_code, 200);
    assert_eq!(up_grp_body["retcode"], 0);
    assert_eq!(up_grp_body["data"]["group_id"], 555_005);

    // 4. upload_private_file: 白名单拦截与放行发送单 File 段
    let (denied_priv_code, denied_priv_body) = http_call(
        addr,
        "POST",
        "/onebot/v11/upload_private_file",
        None,
        Some(json!({
            "nickname": "未白名单路人",
            "file": "secret.txt"
        })),
    )
    .await;
    assert_eq!(denied_priv_code, 403);
    assert_eq!(denied_priv_body["retcode"], 1403);

    let (up_priv_code, up_priv_body) = http_call(
        addr,
        "POST",
        "/onebot/v11/upload_private_file",
        None,
        Some(json!({
            "user_id": 777_001,
            "file": "notes.txt",
            "name": "备忘.txt"
        })),
    )
    .await;
    assert_eq!(up_priv_code, 200);
    assert_eq!(up_priv_body["retcode"], 0);
    assert_eq!(up_priv_body["data"]["user_id"], 777_001);

    let sent = driver.sent_requests();
    assert_eq!(sent.len(), 2);
    assert_eq!(
        sent[0].segments,
        vec![MessageSegment::File {
            file: "report.pdf".to_string(),
            name: Some("月报.pdf".to_string()),
            size: None,
        }]
    );
    assert_eq!(
        sent[1].segments,
        vec![MessageSegment::File {
            file: "notes.txt".to_string(),
            name: Some("备忘.txt".to_string()),
            size: None,
        }]
    );

    // 5. 正向 WS 调用 ocr_image / can_send_video / upload_group_file
    let ws_url = format!("ws://{addr}/onebot/v11/ws");
    let (mut ws, _) = connect_async(&ws_url).await.unwrap();
    let _lifecycle = next_ws_json(&mut ws, Duration::from_secs(2)).await;

    ws.send(Message::text(
        json!({
            "action": "can_send_video",
            "params": {},
            "echo": "ws-vid-cap"
        })
        .to_string(),
    ))
    .await
    .unwrap();
    let ws_vid = next_ws_json(&mut ws, Duration::from_secs(2)).await;
    assert_eq!(ws_vid["echo"], "ws-vid-cap");
    assert_eq!(ws_vid["data"]["yes"], true);

    ws.send(Message::text(
        json!({
            "action": "ocr_image",
            "params": { "image": "ws_shot.png" },
            "echo": "ws-ocr"
        })
        .to_string(),
    ))
    .await
    .unwrap();
    let ws_ocr = next_ws_json(&mut ws, Duration::from_secs(2)).await;
    assert_eq!(ws_ocr["echo"], "ws-ocr");
    assert_eq!(ws_ocr["retcode"], 0);
    assert_eq!(ws_ocr["data"]["texts"][0]["text"], "MOCK OCR 2026");

    ws.send(Message::text(
        json!({
            "action": "upload_group_file",
            "params": { "group_id": 555_005, "file": "ws_doc.bin" },
            "echo": "ws-up-grp"
        })
        .to_string(),
    ))
    .await
    .unwrap();
    let ws_up = next_ws_json(&mut ws, Duration::from_secs(2)).await;
    assert_eq!(ws_up["echo"], "ws-up-grp");
    assert_eq!(ws_up["retcode"], 0);
    assert_eq!(driver.sent_requests().len(), 3);

    // 6. get_file 沙箱解析（.runtime/media 与 .runtime/staging_uploads/<hash>/）及路径越界/外链阻断
    {
        let _runtime_fs_guard = super::RUNTIME_FS_LOCK.lock().await;
        let runtime_dir = PathBuf::from(".runtime");
        let remove_whole_runtime = !runtime_dir.exists();
        let media_dir = Path::new(MEDIA_CACHE_SUBDIR);
        let staging_subdir = Path::new(STAGING_UPLOADS_SUBDIR).join("hash_9abc");
        std::fs::create_dir_all(media_dir).unwrap();
        std::fs::create_dir_all(&staging_subdir).unwrap();

        let cached_path = media_dir.join("sched_cached.txt");
        let staged_path = staging_subdir.join("staged_payload.bin");
        std::fs::write(&cached_path, b"MEDIA_OK").unwrap();
        std::fs::write(&staged_path, b"STAGED_BYTES").unwrap();

        let _cleanup_guard = StagingRuntimeGuard {
            runtime_dir,
            remove_whole_runtime,
            created_paths: vec![cached_path.clone(), staging_subdir.clone()],
        };

        let (gf_media_code, gf_media_body) = http_call(
            addr,
            "GET",
            "/onebot/v11/get_file?file_id=sched_cached.txt",
            None,
            None,
        )
        .await;
        assert_eq!(gf_media_code, 200);
        assert_eq!(gf_media_body["data"]["file_name"], "sched_cached.txt");
        assert_eq!(gf_media_body["data"]["file_size"], 8);
        assert_eq!(
            gf_media_body["data"]["base64"],
            BASE64_STANDARD.encode(b"MEDIA_OK")
        );

        let (gf_staged_code, gf_staged_body) = http_call(
            addr,
            "GET",
            "/onebot/v11/get_file?file=staged_payload.bin",
            None,
            None,
        )
        .await;
        assert_eq!(gf_staged_code, 200);
        assert_eq!(gf_staged_body["data"]["file_name"], "staged_payload.bin");
        assert_eq!(gf_staged_body["data"]["file_size"], 12);
        assert_eq!(
            gf_staged_body["data"]["base64"],
            BASE64_STANDARD.encode(b"STAGED_BYTES")
        );

        let (gf_http_code, gf_http_body) = http_call(
            addr,
            "POST",
            "/onebot/v11/get_file",
            None,
            Some(json!({ "file": "https://example.com/payload.bin" })),
        )
        .await;
        assert_eq!(gf_http_code, 501);
        assert_eq!(gf_http_body["retcode"], 10004);

        let (gf_trav_code, gf_trav_body) = http_call(
            addr,
            "GET",
            "/onebot/v11/get_file?file=../Cargo.toml",
            None,
            None,
        )
        .await;
        assert_eq!(gf_trav_code, 400);
        assert_eq!(gf_trav_body["retcode"], 1400);

        let external_abs = std::fs::canonicalize("Cargo.toml").unwrap();
        let (gf_ext_code, gf_ext_body) = http_call(
            addr,
            "POST",
            "/onebot/v11/get_file",
            None,
            Some(json!({ "file": external_abs.to_string_lossy() })),
        )
        .await;
        assert_eq!(gf_ext_code, 400);
        assert_eq!(gf_ext_body["retcode"], 1400);
    }

    handle.abort();
}
