//! Wave v6 服务端与 OneBot/REST 集成回归测试：覆盖 42 项动作清单、深分页、单条转发、最近联系人、审计脱敏、清缓存与熔断恢复

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use common_types::{
    AdapterConfig, BotDriver, ChatTargetKind, GroupInfoSummary, MessageSegment, SessionSummary,
    StoredMessageRecord,
};
use serde_json::{json, Value};
use tokio::net::TcpListener;

use super::fake_driver::FakeDriver;
use super::{http_call, RUNTIME_FS_LOCK};
use crate::constants::{MEDIA_CACHE_SUBDIR, STAGING_UPLOADS_SUBDIR};
use crate::{build_router_with_state, spawn_server, AppState};

/// 保证 `.runtime` 测试目录在异常断言下也能按测试前状态安全收尾的 RAII 守卫
struct CacheDirCleanupGuard {
    remove_runtime: bool,
    remove_media: bool,
    remove_staging: bool,
}

impl Drop for CacheDirCleanupGuard {
    fn drop(&mut self) {
        if self.remove_media {
            let _ = std::fs::remove_dir_all(MEDIA_CACHE_SUBDIR);
        }
        if self.remove_staging {
            let _ = std::fs::remove_dir_all(STAGING_UPLOADS_SUBDIR);
        }
        if self.remove_runtime {
            let _ = std::fs::remove_dir_all(".runtime");
        }
    }
}

fn make_stored_msg(
    id: i64,
    group_name: &str,
    chat_type: ChatTargetKind,
    sender_name: &str,
    text: &str,
    captured_at_ms: u64,
) -> StoredMessageRecord {
    StoredMessageRecord {
        onebot_message_id: id,
        message_uid: format!("w6-{id}"),
        group_name: group_name.to_string(),
        group_id: if chat_type == ChatTargetKind::Private {
            0
        } else {
            555_005
        },
        user_id: if chat_type == ChatTargetKind::Private {
            777_001
        } else {
            20_001
        },
        sender_name: sender_name.to_string(),
        raw_message: text.to_string(),
        chat_type,
        segments: vec![MessageSegment::Text {
            text: text.to_string(),
        }],
        is_self: false,
        timestamp_hint: Some("14:00".to_string()),
        captured_at_ms,
    }
}

#[tokio::test]
async fn action_matrix_covers_42_supported_and_39_unsupported_actions() {
    let driver = Arc::new(FakeDriver::new());
    driver.allow_private_friend("测试好友");
    let mut config = AdapterConfig::default();
    config.server.port = 0;
    config.target.allowed_private_friends = vec!["测试好友".to_string()];
    let (addr, handle) = spawn_server(driver, config).await.unwrap();

    let supported_actions = [
        ("get_status", "GET", json!({})),
        ("get_login_info", "GET", json!({})),
        ("get_version_info", "GET", json!({})),
        (
            "send_group_msg",
            "POST",
            json!({ "group_id": 555_005, "message": "m1" }),
        ),
        (
            "send_private_msg",
            "POST",
            json!({ "user_id": 777_001, "message": "p1" }),
        ),
        (
            "send_msg",
            "POST",
            json!({ "group_id": 555_005, "message": "m2" }),
        ),
        ("get_msg", "GET", json!({ "message_id": 10_001 })),
        ("get_friend_list", "GET", json!({})),
        ("get_group_list", "GET", json!({})),
        ("get_group_info", "GET", json!({ "group_id": 555_005 })),
        (
            "get_group_member_list",
            "GET",
            json!({ "group_id": 555_005 }),
        ),
        (
            "get_group_member_info",
            "GET",
            json!({ "group_id": 555_005, "user_id": 20_001 }),
        ),
        ("can_send_image", "GET", json!({})),
        ("can_send_record", "GET", json!({})),
        ("get_stranger_info", "GET", json!({ "user_id": 20_001 })),
        ("get_image", "GET", json!({ "file": "missing.png" })),
        (
            "get_group_msg_history",
            "GET",
            json!({ "group_id": 555_005 }),
        ),
        (
            "get_friend_msg_history",
            "GET",
            json!({ "user_id": 777_001 }),
        ),
        (
            "send_group_forward_msg",
            "POST",
            json!({ "group_id": 555_005, "messages": [{ "type": "node", "data": { "id": "10001" } }] }),
        ),
        (
            "send_private_forward_msg",
            "POST",
            json!({ "user_id": 777_001, "messages": [{ "type": "node", "data": { "id": "10001" } }] }),
        ),
        (
            "send_forward_msg",
            "POST",
            json!({ "group_id": 555_005, "messages": [{ "type": "node", "data": { "id": "10001" } }] }),
        ),
        ("get_forward_msg", "GET", json!({ "id": "missing" })),
        ("clean_cache", "POST", json!({})),
        ("set_restart", "POST", json!({ "delay": "not_an_int" })),
        (
            ".handle_quick_operation",
            "POST",
            json!({ "context": {}, "operation": {} }),
        ),
        ("delete_msg", "POST", json!({ "message_id": 999_999 })),
        ("mark_msg_as_read", "GET", json!({ "message_id": 10_001 })),
        (
            "mark_group_msg_as_read",
            "GET",
            json!({ "group_id": 555_005 }),
        ),
        (
            "mark_private_msg_as_read",
            "GET",
            json!({ "user_id": 777_001 }),
        ),
        (".get_word_slices", "GET", json!({ "content": "你好QQ" })),
        (".get_scheduler_status", "GET", json!({})),
        ("can_send_video", "GET", json!({})),
        ("ocr_image", "GET", json!({ "image": "base64://AAAA" })),
        (".ocr_image", "GET", json!({ "image": "base64://AAAA" })),
        (
            "upload_group_file",
            "POST",
            json!({ "group_id": 555_005, "file": "demo.txt" }),
        ),
        (
            "upload_private_file",
            "POST",
            json!({ "user_id": 777_001, "file": "demo.txt" }),
        ),
        ("get_file", "GET", json!({ "file": "missing.bin" })),
        (
            "forward_group_single_msg",
            "POST",
            json!({ "message_id": 10_001, "group_id": 555_005 }),
        ),
        (
            "forward_friend_single_msg",
            "POST",
            json!({ "message_id": 10_001, "user_id": 777_001 }),
        ),
        ("get_recent_contact", "GET", json!({})),
        (".get_audit_events", "GET", json!({})),
        (".reset_security_breaker", "POST", json!({})),
    ];
    assert_eq!(supported_actions.len(), 42);

    for (action, method, payload) in supported_actions {
        let _fs_guard = if action == "clean_cache" {
            Some(RUNTIME_FS_LOCK.lock().await)
        } else {
            None
        };
        let path = format!("/onebot/v11/{action}");
        let body = (method == "POST").then_some(payload);
        let (code, resp) = http_call(addr, method, &path, None, body).await;
        assert_ne!(code, 404, "supported action {action} returned 404: {resp}");
        assert_ne!(
            resp["retcode"], 10004,
            "supported action {action} unexpectedly returned 10004: {resp}"
        );
    }

    let unsupported_actions = [
        "send_poke",
        "group_poke",
        "friend_poke",
        "get_record",
        "send_like",
        "get_cookies",
        "get_csrf_token",
        "get_credentials",
        "get_group_honor_info",
        "get_group_system_msg",
        "get_group_at_all_remain",
        "get_essence_msg_list",
        "get_group_root_files",
        "download_file",
        "set_qq_profile",
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
    ];
    assert_eq!(unsupported_actions.len(), 39);

    for action in unsupported_actions {
        let (code, resp) = http_call(
            addr,
            "POST",
            &format!("/onebot/v11/{action}"),
            None,
            Some(json!({})),
        )
        .await;
        assert_eq!(code, 501, "expected 501 for {action}");
        assert_eq!(resp["retcode"], 10004, "expected 10004 for {action}");
    }

    handle.abort();
}

#[tokio::test]
async fn deep_history_pagination_message_seq_zero_and_reverse_order_work() {
    let driver = Arc::new(FakeDriver::new());
    driver.allow_private_friend("测试好友");
    for idx in 1..=260_i64 {
        let sender = if idx % 2 == 0 { "Alice" } else { "Bob" };
        driver.insert_stored_message(make_stored_msg(
            10_010 + idx,
            "Demo测试群 (5)",
            ChatTargetKind::Group,
            sender,
            &format!("deep-msg-{idx}"),
            1_700_000_010_000 + idx as u64,
        ));
    }
    for idx in 1..=30_i64 {
        driver.insert_stored_message(make_stored_msg(
            11_000 + idx,
            "测试好友",
            ChatTargetKind::Private,
            "测试好友",
            &format!("priv-msg-{idx}"),
            1_700_000_020_000 + idx as u64,
        ));
    }

    let mut config = AdapterConfig::default();
    config.server.port = 0;
    config.target.allowed_private_friends = vec!["测试好友".to_string()];
    let (addr, handle) = spawn_server(driver, config).await.unwrap();

    // 1. message_seq: 0 视作 None，拉取最新一页
    let (code, latest_body) = http_call(
        addr,
        "POST",
        "/onebot/v11/get_group_msg_history",
        None,
        Some(json!({ "group_id": 555_005, "message_seq": 0, "count": 5 })),
    )
    .await;
    assert_eq!(code, 200);
    let latest_ids: Vec<i64> = latest_body["data"]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["message_id"].as_i64().unwrap())
        .collect();
    assert_eq!(latest_ids, vec![10_266, 10_267, 10_268, 10_269, 10_270]);

    // 2. reverseOrder: true 返回倒序，且支持跨越 200 条阈值深分页
    let (code, rev_body) = http_call(
        addr,
        "POST",
        "/onebot/v11/get_group_msg_history",
        None,
        Some(json!({
            "group_id": 555_005,
            "message_seq": 10_040,
            "count": 4,
            "reverseOrder": true
        })),
    )
    .await;
    assert_eq!(code, 200);
    let rev_ids: Vec<i64> = rev_body["data"]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["message_id"].as_i64().unwrap())
        .collect();
    assert_eq!(rev_ids, vec![10_040, 10_039, 10_038, 10_037]);

    // 3. get_friend_msg_history 支持 message_seq=0 与 reverse_order
    let (code, friend_rev) = http_call(
        addr,
        "POST",
        "/onebot/v11/get_friend_msg_history",
        None,
        Some(json!({ "user_id": 777_001, "message_seq": 0, "count": 3, "reverse_order": true })),
    )
    .await;
    assert_eq!(code, 200);
    let f_ids: Vec<i64> = friend_rev["data"]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["message_id"].as_i64().unwrap())
        .collect();
    assert_eq!(f_ids, vec![11_030, 11_029, 11_028]);

    // 4. REST /api/messages/history 游标、发送者、关键词与倒序过滤
    let (code, rest_filtered) = http_call(
        addr,
        "GET",
        "/api/messages/history?session=Demo%E6%B5%8B%E8%AF%95%E7%BE%A4&after_id=10020&before_id=10030&sender=alice&keyword=deep-msg-1&limit=3&reverse_order=true",
        None,
        None,
    )
    .await;
    assert_eq!(code, 200);
    let rest_ids: Vec<i64> = rest_filtered
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["onebot_message_id"].as_i64().unwrap())
        .collect();
    assert_eq!(rest_ids, vec![10_028, 10_026, 10_024]);

    handle.abort();
}

#[tokio::test]
async fn single_forward_routes_enforce_whitelist_deadline_and_confirm_id() {
    let driver = Arc::new(FakeDriver::new());
    driver.allow_private_friend("测试好友");
    driver.insert_stored_message(make_stored_msg(
        10_501,
        "陌生人私聊",
        ChatTargetKind::Private,
        "陌生人",
        "secret message",
        1_700_000_050_000,
    ));

    let mut config = AdapterConfig::default();
    config.server.port = 0;
    config.target.allowed_private_friends = vec!["测试好友".to_string()];
    let (addr, handle) = spawn_server(driver.clone(), config).await.unwrap();

    // 1. 转发群消息至群与白名单好友私聊均成功返回正数 message_id
    let (code, grp_fwd) = http_call(
        addr,
        "POST",
        "/onebot/v11/forward_group_single_msg",
        None,
        Some(json!({ "message_id": 10_001, "group_id": 555_005 })),
    )
    .await;
    assert_eq!(code, 200);
    assert!(grp_fwd["data"]["message_id"].as_i64().unwrap() > 10_001);

    let (code, priv_fwd) = http_call(
        addr,
        "POST",
        "/onebot/v11/forward_friend_single_msg",
        None,
        Some(json!({ "message_id": 10_001, "user_id": 777_001 })),
    )
    .await;
    assert_eq!(code, 200);
    assert!(priv_fwd["data"]["message_id"].as_i64().unwrap() > 10_001);

    // 2. REST POST /api/forward-msg 成功透传并记录活动时间戳
    let (code, rest_fwd) = http_call(
        addr,
        "POST",
        "/api/forward-msg",
        None,
        Some(json!({ "message_id": 10_001, "group": "Demo测试群" })),
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(rest_fwd["ok"], true);
    assert!(rest_fwd["onebot_message_id"].as_i64().is_some());
    assert_eq!(driver.forward_single_calls().len(), 3);

    // 3. 目标或源消息涉及非白名单私聊时必须返回 403/1403
    let (deny_target_code, deny_target_body) = http_call(
        addr,
        "POST",
        "/onebot/v11/forward_friend_single_msg",
        None,
        Some(json!({ "message_id": 10_001, "nickname": "未授权路人" })),
    )
    .await;
    assert_eq!(deny_target_code, 403);
    assert_eq!(deny_target_body["retcode"], 1403);

    let (deny_src_code, deny_src_body) = http_call(
        addr,
        "POST",
        "/onebot/v11/forward_group_single_msg",
        None,
        Some(json!({ "message_id": 10_501, "group_id": 555_005 })),
    )
    .await;
    assert_eq!(deny_src_code, 403);
    assert_eq!(deny_src_body["retcode"], 1403);

    // 4. 过期窗口返回 408/1408；未确认返回 ID 时报 UiaFailure 500/1500
    let expired_ms = (Utc::now().timestamp_millis().max(0) as u64).saturating_sub(2_000);
    let (exp_code, exp_body) = http_call(
        addr,
        "POST",
        "/api/forward-msg",
        None,
        Some(json!({
            "message_id": 10_001,
            "group": "Demo测试群",
            "deadline_unix_ms": expired_ms
        })),
    )
    .await;
    assert_eq!(exp_code, 408);
    assert_eq!(exp_body["retcode"], 1408);

    driver.set_force_unconfirmed_send(true);
    let (unconf_code, unconf_body) = http_call(
        addr,
        "POST",
        "/onebot/v11/forward_group_single_msg",
        None,
        Some(json!({ "message_id": 10_001, "group_id": 555_005 })),
    )
    .await;
    assert_eq!(unconf_code, 500);
    assert_eq!(unconf_body["retcode"], 1500);
    driver.set_force_unconfirmed_send(false);

    handle.abort();
}

#[tokio::test]
async fn recent_contact_and_audit_events_filter_unauthorized_sessions() {
    let driver = Arc::new(FakeDriver::new());
    driver.allow_private_friend("测试好友");

    // 注入合法小群、大型禁入群、白名单好友与非白名单陌生人会话及消息
    driver.add_session(SessionSummary {
        title: "第二小群 (4)".to_string(),
        raw_name: "第二小群 (4)".to_string(),
        preview: Some("小群预览".to_string()),
        unread_count: Some(3),
        member_count_hint: Some(4),
        is_group_hint: Some(true),
        ..Default::default()
    });
    driver.add_session(SessionSummary {
        title: "超大禁入群 (250)".to_string(),
        raw_name: "超大禁入群 (250)".to_string(),
        unread_count: Some(9),
        member_count_hint: Some(250),
        is_group_hint: Some(true),
        ..Default::default()
    });
    driver.add_group_info(GroupInfoSummary {
        group_id: 888_002,
        real_group_id: Some(888_002),
        group_name: "超大禁入群".to_string(),
        member_count: 250,
        max_member_count: 500,
    });
    driver.add_session(SessionSummary {
        title: "陌生路人甲".to_string(),
        raw_name: "陌生路人甲".to_string(),
        is_group_hint: Some(false),
        ..Default::default()
    });
    driver.add_session(SessionSummary {
        title: "无提示陌生人".to_string(),
        raw_name: "无提示陌生人".to_string(),
        is_group_hint: None,
        ..Default::default()
    });
    let mut big_msg = make_stored_msg(
        10_601,
        "超大禁入群",
        ChatTargetKind::Group,
        "路人",
        "big group leak",
        1_700_000_090_000,
    );
    big_msg.group_id = 888_002;
    driver.insert_stored_message(big_msg);
    driver.insert_stored_message(make_stored_msg(
        10_602,
        "陌生路人甲",
        ChatTargetKind::Private,
        "陌生路人甲",
        "stranger leak",
        1_700_000_091_000,
    ));
    driver.insert_stored_message(make_stored_msg(
        10_603,
        "测试好友",
        ChatTargetKind::Private,
        "测试好友",
        "allowed friend latest",
        1_700_000_092_000,
    ));

    // 注入混合审计日志：含非白名单私聊泄漏风险记录与合规系统/群/白名单私聊记录
    driver
        .record_audit_event("private_whitelist_abort", r#"{"target":"秘密路人X"}"#)
        .await
        .unwrap();
    driver
        .record_audit_event(
            "message_recalled",
            r#"{"chat_type":"private","target":"秘密路人Y","message_id":10999}"#,
        )
        .await
        .unwrap();
    driver
        .record_audit_event(
            "async_action_failed",
            r#"{"action":"send_private_msg","error":"private chat target '秘密路人Z' is not allowed"}"#,
        )
        .await
        .unwrap();
    driver
        .record_audit_event(
            "message_recalled",
            r#"{"chat_type":"private","target":"测试好友","message_id":10603}"#,
        )
        .await
        .unwrap();
    driver
        .record_audit_event(
            "message_recalled",
            r#"{"chat_type":"group","target":"Demo测试群","message_id":10001}"#,
        )
        .await
        .unwrap();

    let mut config = AdapterConfig::default();
    config.server.port = 0;
    config.target.strict_below_members = Some(10);
    config.target.allowed_private_friends = vec!["测试好友".to_string()];
    let (addr, handle) = spawn_server(driver, config).await.unwrap();

    // 1. 校验 get_recent_contact 过滤大群与非白名单私聊并按最近活跃倒序返回
    let (code, contacts_body) = http_call(
        addr,
        "GET",
        "/onebot/v11/get_recent_contact?count=20",
        None,
        None,
    )
    .await;
    assert_eq!(code, 200);
    let contacts = contacts_body["data"].as_array().unwrap();
    let names: Vec<&str> = contacts
        .iter()
        .map(|c| c["peer_name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["测试好友", "Demo测试群 (5)", "第二小群 (4)"]);
    assert_eq!(contacts[0]["chat_type"], "private");
    assert_eq!(contacts[0]["user_id"], 777_001);
    assert_eq!(contacts[0]["group_id"], Value::Null);
    assert_eq!(contacts[0]["last_msg_id"], 10_603);
    assert_eq!(contacts[0]["last_msg_text"], "allowed friend latest");
    assert_eq!(contacts[2]["unread_count"], 3);

    // 2. 校验 .get_audit_events 与 GET /api/audit-events 彻底剥离非白名单私聊用户名
    let (ob_audit_code, ob_audit_body) = http_call(
        addr,
        "GET",
        "/onebot/v11/.get_audit_events?limit=50",
        None,
        None,
    )
    .await;
    assert_eq!(ob_audit_code, 200);
    let ob_text = ob_audit_body["data"].to_string();
    assert!(!ob_text.contains("秘密路人"));
    assert!(ob_text.contains("测试好友"));
    assert!(ob_text.contains("Demo测试群"));

    let (rest_audit_code, rest_audit_body) = http_call(
        addr,
        "GET",
        "/api/audit-events?event_kind=message_recalled&limit=10",
        None,
        None,
    )
    .await;
    assert_eq!(rest_audit_code, 200);
    let arr = rest_audit_body.as_array().unwrap();
    assert_eq!(arr.len(), 2);
    assert!(!rest_audit_body.to_string().contains("秘密路人"));

    handle.abort();
}

#[tokio::test]
async fn cache_clean_and_security_reset_endpoints_enforce_locks_and_423_200_flow() {
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

    // 1. POST /api/cache/clean 同时清理 .runtime/media 与嵌套 .runtime/staging_uploads 并保留根目录
    {
        let _fs_lock = RUNTIME_FS_LOCK.lock().await;
        let remove_runtime = !Path::new(".runtime").exists();
        let remove_media = !Path::new(MEDIA_CACHE_SUBDIR).exists();
        let remove_staging = !Path::new(STAGING_UPLOADS_SUBDIR).exists();
        std::fs::create_dir_all(MEDIA_CACHE_SUBDIR).unwrap();
        let nested_dir = PathBuf::from(STAGING_UPLOADS_SUBDIR).join("abcd1234");
        std::fs::create_dir_all(&nested_dir).unwrap();
        let _cleanup_guard = CacheDirCleanupGuard {
            remove_runtime,
            remove_media,
            remove_staging,
        };

        std::fs::write(Path::new(MEDIA_CACHE_SUBDIR).join("w6_img.png"), b"12345").unwrap();
        std::fs::write(nested_dir.join("w6_clip.mp4"), b"1234567890").unwrap();
        std::fs::write(
            Path::new(STAGING_UPLOADS_SUBDIR).join("w6_direct.bin"),
            b"123",
        )
        .unwrap();

        let (code, body) = http_call(addr, "POST", "/api/cache/clean", None, Some(json!({}))).await;
        assert_eq!(code, 200);
        assert_eq!(body["cleaned"], true);
        assert!(body["media_removed"].as_u64().unwrap() >= 1);
        assert!(body["staging_removed"].as_u64().unwrap() >= 2);
        assert!(body["freed_bytes"].as_u64().unwrap() >= 18);
        assert!(Path::new(MEDIA_CACHE_SUBDIR).is_dir());
        assert!(Path::new(STAGING_UPLOADS_SUBDIR).is_dir());
        assert!(!nested_dir.exists());
    }

    // 2. 安全弹窗仍在时 POST /api/security/reset 与 .reset_security_breaker 返回 423/1423
    driver.set_security_breaker_state(true, true, Some("mock security dialog"));
    let (denied_code, denied_body) =
        http_call(addr, "POST", "/api/security/reset", None, Some(json!({}))).await;
    assert_eq!(denied_code, 423);
    assert_eq!(denied_body["code"], "SECURITY_PROMPT_DETECTED");
    assert_eq!(denied_body["retcode"], 1423);

    // 3. 关闭弹窗后恢复成功并返回最新 AdapterStatus，审计流水可见 denied 与 reset 记录
    driver.set_security_breaker_state(true, false, Some("mock security dialog"));
    let (ok_code, ok_body) = http_call(
        addr,
        "POST",
        "/onebot/v11/.reset_security_breaker",
        None,
        Some(json!({})),
    )
    .await;
    assert_eq!(ok_code, 200);
    assert_eq!(ok_body["retcode"], 0);
    assert_eq!(ok_body["data"]["security_breaker_tripped"], false);

    let (audit_code, audit_body) =
        http_call(addr, "GET", "/api/audit-events?limit=10", None, None).await;
    assert_eq!(audit_code, 200);
    let kinds: Vec<&str> = audit_body
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v["event_kind"].as_str())
        .collect();
    assert!(kinds.contains(&"security_breaker_reset_denied"));
    assert!(kinds.contains(&"security_breaker_reset"));

    let _ = Duration::from_millis(1);
    handle.abort();
}
