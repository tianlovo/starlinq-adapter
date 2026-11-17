//! Wave v5 Stage 2B 业务风险回归测试：覆盖私聊越权拦截、同名会话隔离、快速操作原子性、调度边界与真实回执上报

use std::sync::Arc;
use std::time::Duration;

use axum::extract::State as AxumState;
use axum::routing::post;
use axum::{Json, Router};
use chrono::Utc;
use common_types::{
    AdapterConfig, AdapterEvent, BotDriver, ChatTargetKind, InputMode, MessageSegment, SendOutcome,
    StoredMessageRecord,
};
use parking_lot::Mutex;
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio_tungstenite::connect_async;

use super::fake_driver::FakeDriver;
use super::{http_call, next_ws_json};
use crate::onebot::translate_adapter_event_to_onebot;
use crate::{build_router_with_state, spawn_server, AppState};

fn now_ms() -> u64 {
    Utc::now().timestamp_millis().max(0) as u64
}

fn make_record(
    id: i64,
    group_name: &str,
    group_id: i64,
    chat_type: ChatTargetKind,
    text: &str,
    is_self: bool,
) -> StoredMessageRecord {
    StoredMessageRecord {
        onebot_message_id: id,
        message_uid: format!("biz-{id}"),
        group_name: group_name.to_string(),
        group_id,
        user_id: if chat_type == ChatTargetKind::Private {
            777_001
        } else {
            20_001
        },
        sender_name: if is_self {
            "BotSelf".to_string()
        } else {
            "测试成员".to_string()
        },
        raw_message: text.to_string(),
        chat_type,
        segments: vec![MessageSegment::Text {
            text: text.to_string(),
        }],
        is_self,
        timestamp_hint: Some("12:00".to_string()),
        captured_at_ms: now_ms(),
    }
}

async fn capture_webhook_handler(
    AxumState(store): AxumState<Arc<Mutex<Vec<Value>>>>,
    body: axum::body::Bytes,
) -> Json<Value> {
    if let Ok(parsed) = serde_json::from_slice::<Value>(&body) {
        store.lock().push(parsed);
    }
    Json(json!({}))
}

#[tokio::test]
async fn get_msg_and_messages_history_enforce_private_whitelist() {
    let driver = Arc::new(FakeDriver::new());
    driver.allow_private_friend("测试好友");
    driver.insert_stored_message(make_record(
        10_100,
        "测试好友",
        0,
        ChatTargetKind::Private,
        "allowed private msg",
        false,
    ));
    driver.insert_stored_message(make_record(
        10_101,
        "陌生路人",
        0,
        ChatTargetKind::Private,
        "secret private msg",
        false,
    ));
    driver.insert_stored_message(make_record(
        10_102,
        "同名混合",
        555_099,
        ChatTargetKind::Group,
        "public group msg",
        false,
    ));
    driver.insert_stored_message(make_record(
        10_103,
        "同名混合",
        0,
        ChatTargetKind::Private,
        "hidden private msg",
        false,
    ));

    let mut config = AdapterConfig::default();
    config.server.port = 0;
    config.target.group_name = "Demo测试群 (5)".to_string();
    config.target.allowed_private_friends = vec!["测试好友".to_string()];

    let (addr, handle) = spawn_server(driver, config).await.unwrap();

    // 1. get_msg 对非白名单私聊消息返回 403/1403，白名单私聊消息放行
    let (deny_get_code, deny_get_body) = http_call(
        addr,
        "GET",
        "/onebot/v11/get_msg?message_id=10101",
        None,
        None,
    )
    .await;
    assert_eq!(deny_get_code, 403);
    assert_eq!(deny_get_body["retcode"], 1403);
    assert_eq!(deny_get_body["code"], "PRIVATE_TARGET_NOT_ALLOWED");

    let (deny_post_code, deny_post_body) = http_call(
        addr,
        "POST",
        "/onebot/v11/get_msg",
        None,
        Some(json!({ "message_id": 10_101 })),
    )
    .await;
    assert_eq!(deny_post_code, 403);
    assert_eq!(deny_post_body["retcode"], 1403);

    let (allow_get_code, allow_get_body) = http_call(
        addr,
        "GET",
        "/onebot/v11/get_msg?message_id=10100",
        None,
        None,
    )
    .await;
    assert_eq!(allow_get_code, 200);
    assert_eq!(allow_get_body["data"]["raw_message"], "allowed private msg");

    // 2. /api/messages/history 显式 chat_type=private 对非白名单目标立即拦截返回 403
    let (hist_explicit_deny, hist_explicit_body) = http_call(
        addr,
        "GET",
        "/api/messages/history?session=%E9%99%8C%E7%94%9F%E8%B7%AF%E4%BA%BA&chat_type=private",
        None,
        None,
    )
    .await;
    assert_eq!(hist_explicit_deny, 403);
    assert_eq!(hist_explicit_body["code"], "PRIVATE_TARGET_NOT_ALLOWED");

    let (grp_as_priv_code, _) = http_call(
        addr,
        "GET",
        "/api/messages/history?session=Demo%E6%B5%8B%E8%AF%95%E7%BE%A4&chat_type=private",
        None,
        None,
    )
    .await;
    assert_eq!(grp_as_priv_code, 403);

    // 3. 未传 chat_type 时过滤非白名单私聊记录；若全部被过滤则同样返回 403
    let (hist_all_filtered_code, hist_all_filtered_body) = http_call(
        addr,
        "GET",
        "/api/messages/history?session=%E9%99%8C%E7%94%9F%E8%B7%AF%E4%BA%BA",
        None,
        None,
    )
    .await;
    assert_eq!(hist_all_filtered_code, 403);
    assert_eq!(hist_all_filtered_body["code"], "PRIVATE_TARGET_NOT_ALLOWED");

    // 同名混合会话未显式传 chat_type 时仅保留群记录，不得泄漏非白名单私聊记录
    let (mixed_code, mixed_body) = http_call(
        addr,
        "GET",
        "/api/messages/history?session=%E5%90%8C%E5%90%8D%E6%B7%B7%E5%90%88&limit=10",
        None,
        None,
    )
    .await;
    assert_eq!(mixed_code, 200);
    let mixed_arr = mixed_body.as_array().unwrap();
    assert_eq!(mixed_arr.len(), 1);
    assert_eq!(mixed_arr[0]["raw_message"], "public group msg");
    assert_eq!(mixed_arr[0]["chat_type"], "group");

    // 白名单好友历史查询放行
    let (allow_hist_code, allow_hist_body) = http_call(
        addr,
        "GET",
        "/api/messages/history?session=%E6%B5%8B%E8%AF%95%E5%A5%BD%E5%8F%8B&chat_type=private",
        None,
        None,
    )
    .await;
    assert_eq!(allow_hist_code, 200);
    assert_eq!(allow_hist_body.as_array().unwrap().len(), 1);
    assert_eq!(allow_hist_body[0]["raw_message"], "allowed private msg");

    handle.abort();
}

#[tokio::test]
async fn same_name_group_and_private_histories_are_strictly_isolated() {
    let driver = Arc::new(FakeDriver::new());
    driver.allow_private_friend("测试好友");

    // 先写入 2 条名为“测试好友”的群聊记录，再写入 4 条更晚的同名私聊记录
    driver.insert_stored_message(make_record(
        10_201,
        "测试好友",
        555_088,
        ChatTargetKind::Group,
        "same-name-group-1",
        false,
    ));
    driver.insert_stored_message(make_record(
        10_202,
        "测试好友",
        555_088,
        ChatTargetKind::Group,
        "same-name-group-2",
        false,
    ));
    for idx in 1..=4 {
        driver.insert_stored_message(make_record(
            10_210 + idx,
            "测试好友",
            0,
            ChatTargetKind::Private,
            &format!("same-name-priv-{idx}"),
            false,
        ));
    }

    // 校验 FakeDriver::get_recent_messages_by_kind 在截断 limit 前已按 chat_type 精确隔离
    let group_direct = driver
        .get_recent_messages_by_kind("测试好友", Some(ChatTargetKind::Group), 2)
        .await
        .unwrap();
    assert_eq!(group_direct.len(), 2);
    assert!(group_direct
        .iter()
        .all(|r| r.chat_type == ChatTargetKind::Group));

    let priv_direct = driver
        .get_recent_messages_by_kind("测试好友", Some(ChatTargetKind::Private), 2)
        .await
        .unwrap();
    assert_eq!(priv_direct.len(), 2);
    assert_eq!(priv_direct[0].raw_message, "same-name-priv-3");
    assert_eq!(priv_direct[1].raw_message, "same-name-priv-4");

    let mut config = AdapterConfig::default();
    config.server.port = 0;
    config.target.allowed_private_friends = vec!["测试好友".to_string()];
    let (addr, handle) = spawn_server(driver, config).await.unwrap();

    // 1. OneBot get_group_msg_history 与 get_friend_msg_history 严格隔离同名会话
    let (grp_code, grp_body) = http_call(
        addr,
        "POST",
        "/onebot/v11/get_group_msg_history",
        None,
        Some(json!({ "group_name": "测试好友", "count": 2 })),
    )
    .await;
    assert_eq!(grp_code, 200);
    let grp_msgs = grp_body["data"]["messages"].as_array().unwrap();
    assert_eq!(grp_msgs.len(), 2);
    assert_eq!(grp_msgs[0]["message_type"], "group");
    assert_eq!(grp_msgs[0]["raw_message"], "same-name-group-1");
    assert_eq!(grp_msgs[1]["raw_message"], "same-name-group-2");

    let (priv_code, priv_body) = http_call(
        addr,
        "POST",
        "/onebot/v11/get_friend_msg_history",
        None,
        Some(json!({ "nickname": "测试好友", "count": 2 })),
    )
    .await;
    assert_eq!(priv_code, 200);
    let priv_msgs = priv_body["data"]["messages"].as_array().unwrap();
    assert_eq!(priv_msgs.len(), 2);
    assert_eq!(priv_msgs[0]["message_type"], "private");
    assert_eq!(priv_msgs[0]["raw_message"], "same-name-priv-3");
    assert_eq!(priv_msgs[1]["raw_message"], "same-name-priv-4");

    // 2. REST /api/messages/history 按 chat_type=group / private 严格隔离
    let (rest_grp_code, rest_grp_body) = http_call(
        addr,
        "GET",
        "/api/messages/history?session=%E6%B5%8B%E8%AF%95%E5%A5%BD%E5%8F%8B&chat_type=group&limit=2",
        None,
        None,
    )
    .await;
    assert_eq!(rest_grp_code, 200);
    let rest_grp = rest_grp_body.as_array().unwrap();
    assert_eq!(rest_grp.len(), 2);
    assert!(rest_grp.iter().all(|m| m["chat_type"] == "group"));

    let (rest_priv_code, rest_priv_body) = http_call(
        addr,
        "GET",
        "/api/messages/history?session=%E6%B5%8B%E8%AF%95%E5%A5%BD%E5%8F%8B&chat_type=private&limit=2",
        None,
        None,
    )
    .await;
    assert_eq!(rest_priv_code, 200);
    let rest_priv = rest_priv_body.as_array().unwrap();
    assert_eq!(rest_priv.len(), 2);
    assert!(rest_priv.iter().all(|m| m["chat_type"] == "private"));

    handle.abort();
}

#[tokio::test]
async fn quick_operation_combo_flags_standalone_media_and_delete_deadline() {
    let driver = Arc::new(FakeDriver::new());
    driver.insert_stored_message(make_record(
        10_301,
        "其他群 (9)",
        666_001,
        ChatTargetKind::Group,
        "other group msg",
        false,
    ));
    driver.insert_stored_message(make_record(
        10_302,
        "测试好友",
        0,
        ChatTargetKind::Private,
        "private msg",
        false,
    ));
    let mut config = AdapterConfig::default();
    config.server.port = 0;
    config.target.group_name = "Demo测试群 (5)".to_string();

    let state = AppState::new(driver.clone(), config);
    let router = build_router_with_state(state.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    // 先发送两条自发消息作为快速撤回目标
    let (s1_code, s1_body) = http_call(
        addr,
        "POST",
        "/onebot/v11/send_group_msg",
        None,
        Some(json!({ "group_id": 555_005, "message": "self-msg-1" })),
    )
    .await;
    assert_eq!(s1_code, 200);
    let self_id_1 = s1_body["data"]["message_id"].as_i64().unwrap();

    let (s2_code, s2_body) = http_call(
        addr,
        "POST",
        "/onebot/v11/send_group_msg",
        None,
        Some(json!({ "group_id": 555_005, "message": "self-msg-2" })),
    )
    .await;
    assert_eq!(s2_code, 200);
    let self_id_2 = s2_body["data"]["message_id"].as_i64().unwrap();
    let baseline_sent_len = driver.sent_requests().len();

    // 1. 组合管理标志（reply+ban / delete+kick / reject_add_request / ban_duration / set_card）必须返回 501 且零发信零撤回
    for op in [
        json!({ "reply": "should not send", "ban": true }),
        json!({ "delete": true, "kick": true }),
        json!({ "reply": "nope", "reject_add_request": true }),
        json!({ "reply": "nope", "ban_duration": 60 }),
        json!({ "set_card": "新名片" }),
    ] {
        let (code, body) = http_call(
            addr,
            "POST",
            "/onebot/v11/.handle_quick_operation",
            None,
            Some(json!({
                "context": {
                    "post_type": "message",
                    "message_type": "group",
                    "group_id": 555_005,
                    "user_id": 20_001,
                    "message_id": self_id_1
                },
                "operation": op
            })),
        )
        .await;
        assert_eq!(code, 501);
        assert_eq!(body["retcode"], 10004);
    }
    assert_eq!(driver.sent_requests().len(), baseline_sent_len);
    let (still_code, _) = http_call(
        addr,
        "GET",
        &format!("/onebot/v11/get_msg?message_id={self_id_1}"),
        None,
        None,
    )
    .await;
    assert_eq!(still_code, 200);

    // 2. 独占原生段（[CQ:video] / [CQ:file] / [CQ:dice]）快速回复禁止前置 Reply/At/" "
    for standalone_cq in [
        "[CQ:video,file=clip.mp4]",
        "[CQ:file,file=doc.pdf]",
        "[CQ:dice]",
    ] {
        let (code, body) = http_call(
            addr,
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
                    "reply": standalone_cq,
                    "at_sender": true
                }
            })),
        )
        .await;
        assert_eq!(code, 200, "standalone quick reply failed: {body}");
        assert_eq!(body["data"]["applied"], true);
        let latest = driver.sent_requests().pop().unwrap();
        assert_eq!(
            latest.segments.len(),
            1,
            "standalone quick reply must not inject Reply or At: {:?}",
            latest.segments
        );
    }

    // 3. 已显式包含 Reply/At 或跨会话 message_id 时，禁止重复或错群挂载
    let (dedup_code, _) = http_call(
        addr,
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
                "reply": "[CQ:reply,id=10001][CQ:at,qq=20001] 已含引用",
                "at_sender": true
            }
        })),
    )
    .await;
    assert_eq!(dedup_code, 200);
    let dedup_req = driver.sent_requests().pop().unwrap();
    let reply_cnt = dedup_req
        .segments
        .iter()
        .filter(|s| matches!(s, MessageSegment::Reply { .. }))
        .count();
    let at_cnt = dedup_req
        .segments
        .iter()
        .filter(|s| matches!(s, MessageSegment::At { .. }))
        .count();
    assert_eq!(reply_cnt, 1);
    assert_eq!(at_cnt, 1);

    for cross_id in [10_301_i64, 10_302_i64] {
        let (cross_code, _) = http_call(
            addr,
            "POST",
            "/onebot/v11/.handle_quick_operation",
            None,
            Some(json!({
                "context": {
                    "post_type": "message",
                    "message_type": "group",
                    "group_id": 555_005,
                    "user_id": 20_001,
                    "message_id": cross_id
                },
                "operation": {
                    "reply": "无跨群引用",
                    "at_sender": false
                }
            })),
        )
        .await;
        assert_eq!(cross_code, 200);
        let cross_req = driver.sent_requests().pop().unwrap();
        assert!(!cross_req
            .segments
            .iter()
            .any(|s| matches!(s, MessageSegment::Reply { .. })));
    }

    // 4. {"delete": true} 在超过 response_deadline_ms、stale captured_at_ms 与排队锁超时时均返回 408/1408 且不撤回
    let stale_capture = now_ms().saturating_sub(120_000);
    let (exp_code_1, exp_body_1) = http_call(
        addr,
        "POST",
        "/onebot/v11/.handle_quick_operation",
        None,
        Some(json!({
            "context": {
                "message_type": "group",
                "group_id": 555_005,
                "message_id": self_id_1,
                "captured_at_ms": stale_capture
            },
            "operation": { "delete": true }
        })),
    )
    .await;
    assert_eq!(exp_code_1, 408);
    assert_eq!(exp_body_1["retcode"], 1408);

    let expired_deadline = now_ms().saturating_sub(1_000);
    let (exp_code_2, exp_body_2) = http_call(
        addr,
        "POST",
        "/onebot/v11/.handle_quick_operation",
        None,
        Some(json!({
            "context": {
                "message_type": "group",
                "group_id": 555_005,
                "message_id": self_id_1,
                "response_deadline_ms": expired_deadline
            },
            "operation": { "delete": true }
        })),
    )
    .await;
    assert_eq!(exp_code_2, 408);
    assert_eq!(exp_body_2["retcode"], 1408);

    let held_gate = state.send_gate.lock().await;
    let tight_deadline = now_ms().saturating_add(60);
    let queued_del = tokio::spawn(http_call(
        addr,
        "POST",
        "/onebot/v11/.handle_quick_operation",
        None,
        Some(json!({
            "context": {
                "message_type": "group",
                "group_id": 555_005,
                "message_id": self_id_2,
                "response_deadline_ms": tight_deadline
            },
            "operation": { "delete": true }
        })),
    ));
    tokio::time::sleep(Duration::from_millis(150)).await;
    drop(held_gate);
    let (exp_code_3, exp_body_3) = queued_del.await.unwrap();
    assert_eq!(exp_code_3, 408);
    assert_eq!(exp_body_3["retcode"], 1408);
    assert_eq!(driver.outbound_deadline_abort_count(), 3);

    // 确认未过期快速撤回仍可成功删除目标消息
    let (ok_del_code, _) = http_call(
        addr,
        "POST",
        "/onebot/v11/.handle_quick_operation",
        None,
        Some(json!({
            "context": {
                "message_type": "group",
                "group_id": 555_005,
                "message_id": self_id_2,
                "response_deadline_ms": now_ms().saturating_add(5_000)
            },
            "operation": { "delete": true }
        })),
    )
    .await;
    assert_eq!(ok_del_code, 200);

    handle.abort();
}

#[tokio::test]
async fn scheduler_policy_zero_values_rejected_and_mark_read_uses_send_gate() {
    let driver = Arc::new(FakeDriver::new());
    let mut config = AdapterConfig::default();
    config.server.port = 0;

    let state = AppState::new(driver.clone(), config);
    let router = build_router_with_state(state.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    // 1. 校验 5 个新增调度策略 0 值参数均返回 400 BAD_REQUEST / 1400
    for zero_patch in [
        json!({ "contextual_response_window_ms": 0 }),
        json!({ "ordinary_response_window_ms": 0 }),
        json!({ "single_poll_burst_threshold": 0 }),
        json!({ "hard_shed_queue_depth": 0 }),
        json!({ "group_quantum_messages": 0 }),
    ] {
        let (code, body) = http_call(
            addr,
            "POST",
            "/api/scheduler/policy",
            None,
            Some(zero_patch.clone()),
        )
        .await;
        assert_eq!(code, 400, "expected 400 for {zero_patch}");
        assert_eq!(body["code"], "BAD_REQUEST");
        assert_eq!(body["retcode"], 1400);
    }

    // 2. 验证 mark_group_msg_as_read 与 POST /api/sessions/read 受 send_gate 互斥锁序列化
    let gate_guard = state.send_gate.lock().await;
    let mark_onebot_task = tokio::spawn(http_call(
        addr,
        "POST",
        "/onebot/v11/mark_group_msg_as_read",
        None,
        Some(json!({ "group_id": 555_005 })),
    ));
    let mark_rest_task = tokio::spawn(http_call(
        addr,
        "POST",
        "/api/sessions/read",
        None,
        Some(json!({ "group_id": 555_005 })),
    ));
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert!(
        driver.mark_read_calls().is_empty(),
        "mark_session_read must wait behind send_gate"
    );
    drop(gate_guard);

    let (ob_code, _) = mark_onebot_task.await.unwrap();
    let (rest_code, _) = mark_rest_task.await.unwrap();
    assert_eq!(ob_code, 200);
    assert_eq!(rest_code, 200);
    assert_eq!(driver.mark_read_calls().len(), 2);

    handle.abort();
}

#[tokio::test]
async fn post_message_sent_emits_real_body_segments_and_suppresses_failures() {
    // 1. 验证 translate_adapter_event_to_onebot 对 ok=false 与缺失 ID 返回 None
    let failed_outcome = SendOutcome {
        ok: false,
        group: "Demo测试群 (5)".to_string(),
        detail: "failed".to_string(),
        onebot_message_id: Some(10_500),
        target_kind: ChatTargetKind::Group,
        ..Default::default()
    };
    assert!(translate_adapter_event_to_onebot(
        10_001,
        true,
        &AdapterEvent::MessageSent(failed_outcome.clone())
    )
    .is_none());

    let no_id_outcome = SendOutcome {
        ok: true,
        group: "Demo测试群 (5)".to_string(),
        detail: "sent".to_string(),
        onebot_message_id: None,
        target_kind: ChatTargetKind::Group,
        ..Default::default()
    };
    assert!(translate_adapter_event_to_onebot(
        10_001,
        true,
        &AdapterEvent::MessageSent(no_id_outcome.clone())
    )
    .is_none());

    // 2. 联调正向 WS Event 与 HTTP Webhook：成功发送上报真实消息与段数组，失败零上报
    let webhook_events = Arc::new(Mutex::new(Vec::<Value>::new()));
    let hook_app = Router::new()
        .route("/hook", post(capture_webhook_handler))
        .with_state(webhook_events.clone());
    let hook_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let hook_addr = hook_listener.local_addr().unwrap();
    let hook_task = tokio::spawn(async move {
        let _ = axum::serve(hook_listener, hook_app).await;
    });

    let driver = Arc::new(FakeDriver::new());
    driver.allow_private_friend("测试好友");

    let mut config = AdapterConfig::default();
    config.server.port = 0;
    config.bot.self_id = 10_001;
    config.target.allowed_private_friends = vec!["测试好友".to_string()];
    config.onebot.post_message_sent = true;
    config.onebot.http_post_urls = vec![format!("http://{hook_addr}/hook")];

    let (addr, handle) = spawn_server(driver.clone(), config).await.unwrap();
    tokio::time::sleep(Duration::from_millis(60)).await;

    let (mut ws, _) = connect_async(format!("ws://{addr}/onebot/v11/ws/event"))
        .await
        .unwrap();
    let lifecycle = next_ws_json(&mut ws, Duration::from_secs(2)).await;
    assert_eq!(lifecycle["meta_event_type"], "lifecycle");

    // 群复合消息成功发送 -> 推送真实 raw_message、segments 数组与 BotSelf 昵称
    let (grp_code, grp_body) = http_call(
        addr,
        "POST",
        "/onebot/v11/send_group_msg",
        None,
        Some(json!({
            "group_id": 555_005,
            "message": "[CQ:at,qq=20001] 真实回执正文"
        })),
    )
    .await;
    assert_eq!(grp_code, 200);
    let grp_msg_id = grp_body["data"]["message_id"].as_i64().unwrap();

    let ws_grp_sent = next_ws_json(&mut ws, Duration::from_secs(2)).await;
    assert_eq!(ws_grp_sent["post_type"], "message_sent");
    assert_eq!(ws_grp_sent["message_id"], grp_msg_id);
    assert_eq!(ws_grp_sent["raw_message"], "[CQ:at,qq=20001] 真实回执正文");
    assert_eq!(
        ws_grp_sent["message"],
        json!([
            { "type": "at", "data": { "qq": "20001" } },
            { "type": "text", "data": { "text": " 真实回执正文" } }
        ])
    );
    assert_eq!(ws_grp_sent["sender"]["nickname"], "BotSelf");
    assert_eq!(ws_grp_sent["sender"]["card"], "BotSelf");

    // 私聊成功发送 -> 同样回填真实正文与机器人昵称
    let (priv_code, priv_body) = http_call(
        addr,
        "POST",
        "/onebot/v11/send_private_msg",
        None,
        Some(json!({
            "user_id": 777_001,
            "message": "私聊真实正文"
        })),
    )
    .await;
    assert_eq!(priv_code, 200);
    let ws_priv_sent = next_ws_json(&mut ws, Duration::from_secs(2)).await;
    assert_eq!(ws_priv_sent["post_type"], "message_sent");
    assert_eq!(ws_priv_sent["message_type"], "private");
    assert_eq!(ws_priv_sent["message_id"], priv_body["data"]["message_id"]);
    assert_eq!(ws_priv_sent["raw_message"], "私聊真实正文");
    assert_eq!(ws_priv_sent["sender"]["nickname"], "BotSelf");

    // 失败发送（ok=false 与无 message_id 广播 + unconfirmed 发送）均不得触发 message_sent，后续标记 notice 应紧随其后到达
    driver.broadcast_event(AdapterEvent::MessageSent(failed_outcome));
    driver.broadcast_event(AdapterEvent::MessageSent(no_id_outcome));
    driver.set_force_unconfirmed_send(true);
    let (fail_code, _) = http_call(
        addr,
        "POST",
        "/onebot/v11/send_group_msg",
        None,
        Some(json!({
            "group_id": 555_005,
            "message": "unconfirmed-should-not-emit"
        })),
    )
    .await;
    assert_eq!(fail_code, 500);
    driver.set_force_unconfirmed_send(false);

    driver.broadcast_event(AdapterEvent::Notice(json!({
        "post_type": "notice",
        "notice_type": "group_recall",
        "group_id": 555_005,
        "user_id": 20_001,
        "operator_id": 20_001,
        "message_id": 10_001
    })));
    let marker_ev = next_ws_json(&mut ws, Duration::from_secs(2)).await;
    assert_eq!(marker_ev["post_type"], "notice");

    // 校验 HTTP Webhook 同步收到上述 2 条真实 message_sent 与 1 条 marker notice，且无失败事件
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while webhook_events.lock().len() < 3 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let hooks = webhook_events.lock().clone();
    assert_eq!(hooks.len(), 3, "unexpected webhook events: {hooks:?}");
    assert_eq!(hooks[0]["post_type"], "message_sent");
    assert_eq!(hooks[0]["raw_message"], "[CQ:at,qq=20001] 真实回执正文");
    assert_eq!(hooks[0]["sender"]["nickname"], "BotSelf");
    assert_eq!(hooks[1]["post_type"], "message_sent");
    assert_eq!(hooks[1]["raw_message"], "私聊真实正文");
    assert_eq!(hooks[2]["post_type"], "notice");

    let _ = InputMode::Auto;
    handle.abort();
    hook_task.abort();
}
