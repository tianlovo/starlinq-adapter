//! 原生私聊打开、消息撤回、离线历史查询与强刷群列表 REST 路由集成测试

use std::sync::Arc;

use common_types::{AdapterConfig, ChatTargetKind};
use serde_json::json;

use super::fake_driver::FakeDriver;
use super::http_call;
use crate::spawn_server;

#[tokio::test]
async fn rest_open_private_allows_whitelisted_and_rejects_empty_or_denied() {
    let driver = Arc::new(FakeDriver::new());
    driver.allow_private_friend("测试好友");

    let mut config = AdapterConfig::default();
    config.server.port = 0;
    config.target.allowed_private_friends = vec!["测试好友".to_string()];

    let (listen_addr, handle) = spawn_server(driver.clone(), config).await.unwrap();

    // 1. 空白目标应返回 400 BAD_REQUEST
    let (code, body) = http_call(
        listen_addr,
        "POST",
        "/api/open-private",
        None,
        Some(json!({ "target": "   " })),
    )
    .await;
    assert_eq!(code, 400);
    assert_eq!(body["code"], "BAD_REQUEST");

    // 2. 非白名单好友应返回 403 PRIVATE_TARGET_NOT_ALLOWED
    let (code, body) = http_call(
        listen_addr,
        "POST",
        "/api/open-private",
        None,
        Some(json!({ "nickname": "未授权好友" })),
    )
    .await;
    assert_eq!(code, 403);
    assert_eq!(body["code"], "PRIVATE_TARGET_NOT_ALLOWED");

    // 3. 白名单好友允许附带前后空白并应切换到私聊状态
    let (code, body) = http_call(
        listen_addr,
        "POST",
        "/api/open-private",
        None,
        Some(json!({ "friend": " 测试好友 " })),
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(body["active_chat_kind"], "private");
    assert_eq!(body["active_group"], "测试好友");
    assert_eq!(body["attached_window"]["hwnd"], 42);
    assert_eq!(
        driver.open_private_calls(),
        vec!["未授权好友".to_string(), "测试好友".to_string()]
    );

    handle.abort();
}

#[tokio::test]
async fn rest_send_private_opens_target_once_when_not_yet_active() {
    let driver = Arc::new(FakeDriver::new());
    driver.allow_private_friend("测试好友");

    let mut config = AdapterConfig::default();
    config.server.port = 0;
    config.target.allowed_private_friends = vec!["测试好友".to_string()];

    let (listen_addr, handle) = spawn_server(driver.clone(), config).await.unwrap();

    // 首次私聊发送未事先打开窗口，服务端应在 send_gate 下自动调用 open_private
    let (code, body) = http_call(
        listen_addr,
        "POST",
        "/api/send",
        None,
        Some(json!({
            "group": "测试好友",
            "text": "first private msg",
            "target_kind": "private"
        })),
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(body["ok"], true);
    assert_eq!(body["target_kind"], "private");
    assert_eq!(driver.open_private_calls(), vec!["测试好友".to_string()]);

    // 窗口已停留在目标好友私聊时，再次发送不得重复调用 open_private
    let (code, body) = http_call(
        listen_addr,
        "POST",
        "/api/send",
        None,
        Some(json!({
            "group": "测试好友",
            "text": "second private msg",
            "target_kind": "private"
        })),
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(body["ok"], true);
    assert_eq!(driver.open_private_calls().len(), 1);
    assert_eq!(driver.sent_requests().len(), 2);
    assert_eq!(
        driver.sent_requests()[0].target_kind,
        ChatTargetKind::Private
    );

    handle.abort();
}

#[tokio::test]
async fn rest_delete_msg_handles_valid_non_positive_missing_and_peer_records() {
    let driver = Arc::new(FakeDriver::new());
    let mut config = AdapterConfig::default();
    config.server.port = 0;

    let (listen_addr, handle) = spawn_server(driver.clone(), config).await.unwrap();

    // 1. 非正数 message_id 应直接被路由层拦截并返回 400 BAD_REQUEST
    let (code, body) = http_call(
        listen_addr,
        "POST",
        "/api/delete-msg",
        None,
        Some(json!({ "message_id": 0 })),
    )
    .await;
    assert_eq!(code, 400);
    assert_eq!(body["code"], "BAD_REQUEST");

    let (code, body) = http_call(
        listen_addr,
        "POST",
        "/api/delete-msg",
        None,
        Some(json!({ "message_id": -7 })),
    )
    .await;
    assert_eq!(code, 400);
    assert_eq!(body["code"], "BAD_REQUEST");

    // 2. 不存在的消息 ID 返回 400 BAD_REQUEST
    let (code, body) = http_call(
        listen_addr,
        "POST",
        "/api/delete-msg",
        None,
        Some(json!({ "message_id": 999_999 })),
    )
    .await;
    assert_eq!(code, 400);
    assert_eq!(body["code"], "BAD_REQUEST");

    // 3. 他人发送的初始消息 10001 返回 501 UNSUPPORTED_ACTION
    let (code, body) = http_call(
        listen_addr,
        "POST",
        "/api/delete-msg",
        None,
        Some(json!({ "message_id": 10_001 })),
    )
    .await;
    assert_eq!(code, 501);
    assert_eq!(body["code"], "UNSUPPORTED_ACTION");

    // 4. 发送一条机器人自身消息后再通过 /api/delete-msg 撤回，应返回 200 并持久化清除
    let (code, send_body) = http_call(
        listen_addr,
        "POST",
        "/api/send",
        None,
        Some(json!({ "group": "Demo测试群", "text": "to recall" })),
    )
    .await;
    assert_eq!(code, 200);
    let sent_id = send_body["onebot_message_id"].as_i64().unwrap();

    let (code, del_body) = http_call(
        listen_addr,
        "POST",
        "/api/delete-msg",
        None,
        Some(json!({ "message_id": sent_id })),
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(del_body["ok"], true);
    assert_eq!(del_body["message_id"], sent_id);

    // 再次查询离线历史应不再包含已撤回消息
    let (code, hist_body) = http_call(
        listen_addr,
        "GET",
        "/api/messages/history?group=Demo%E6%B5%8B%E8%AF%95%E7%BE%A4",
        None,
        None,
    )
    .await;
    assert_eq!(code, 200);
    let records = hist_body.as_array().unwrap();
    assert!(records
        .iter()
        .all(|item| item["onebot_message_id"] != sent_id));

    handle.abort();
}

#[tokio::test]
async fn rest_messages_history_is_pure_readonly_and_groups_forwards_no_cache() {
    let driver = Arc::new(FakeDriver::new());
    let mut config = AdapterConfig::default();
    config.server.port = 0;

    let (listen_addr, handle) = spawn_server(driver.clone(), config).await.unwrap();

    // 先切换到一个不同群聊，再发送一条消息到 Demo测试群 的存储记录中
    let (code, _) = http_call(
        listen_addr,
        "POST",
        "/api/open-group",
        None,
        Some(json!({ "group_name": "其他活跃群", "strict_below_members": 50 })),
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(driver.open_group_call_count(), 1);

    let (code, _) = http_call(
        listen_addr,
        "POST",
        "/api/send",
        None,
        Some(json!({ "group": "Demo测试群", "text": "history entry" })),
    )
    .await;
    assert_eq!(code, 200);

    // 通过 session 参数只读查询 Demo测试群 历史消息，不应触发 open_group 或改变当前激活会话
    let (code, body) = http_call(
        listen_addr,
        "GET",
        "/api/messages/history?session=Demo%E6%B5%8B%E8%AF%95%E7%BE%A4&limit=1",
        None,
        None,
    )
    .await;
    assert_eq!(code, 200);
    let records = body.as_array().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["raw_message"], "history entry");
    assert_eq!(driver.open_group_call_count(), 1);

    let (code, status_body) = http_call(listen_addr, "GET", "/api/status", None, None).await;
    assert_eq!(code, 200);
    assert_eq!(status_body["active_group"], "其他活跃群 (5)");

    // 校验 /api/groups 的 no_cache 参数透传
    let (code, groups_default) = http_call(listen_addr, "GET", "/api/groups", None, None).await;
    assert_eq!(code, 200);
    assert_eq!(groups_default[0]["group_name"], "Demo测试群 (5)");

    let (code, groups_refresh) =
        http_call(listen_addr, "GET", "/api/groups?no_cache=true", None, None).await;
    assert_eq!(code, 200);
    assert_eq!(groups_refresh[0]["group_name"], "Demo测试群 (5)");
    assert_eq!(driver.group_list_no_cache_calls(), vec![false, true]);

    handle.abort();
}
