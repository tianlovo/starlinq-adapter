//! 质量缺陷回归集成测试模块：覆盖 UTF-8 切片安全、异步并发信号量、WS 防阻塞与转发防泄漏

use std::sync::Arc;
use std::time::{Duration, Instant};

use common_types::{AdapterConfig, BotConfig, GroupTargetSpec, ServerConfig};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

use crate::constants::{
    MAX_FORWARD_NODES, MAX_INFLIGHT_ASYNC_ACTIONS, ONEBOT_RETCODE_ASYNC,
    ROUTE_ONEBOT_SEND_GROUP_MSG, ROUTE_ONEBOT_WS,
};
use crate::onebot::dispatch_onebot_action_with_mode;
use crate::tests::fake_driver::FakeDriver;
use crate::tests::{http_call, http_raw_call, next_ws_json};
use crate::{spawn_server, AppState};

#[tokio::test]
async fn form_urlencoded_multibyte_percent_and_utf8_auth_do_not_panic() {
    let driver = Arc::new(FakeDriver::new());
    let config = AdapterConfig {
        server: ServerConfig {
            host: "127.0.0.1".to_string(),
            port: 0,
            access_token: Some("sec+ret&1".to_string()),
        },
        bot: BotConfig::default(),
        ..Default::default()
    };
    let (addr, handle) = spawn_server(driver, config).await.unwrap();

    // 验证 QA-BUG-002：纯多字节中文 Authorization 请求头与空白 Bearer 不会触发切片 panic
    for header_val in ["中文中文中文", "Bearer ", "Token 😀😀"] {
        let (code, body) = http_raw_call(
            addr,
            "GET",
            "/api/health",
            &[("Authorization", header_val)],
            None,
        )
        .await;
        assert_eq!(code, 401);
        assert_eq!(body["code"], "UNAUTHORIZED");
    }

    // 验证 URL 编码后的 ?access_token=sec%2Bret%261 正常通过认证
    let (auth_ok_code, auth_ok_body) = http_raw_call(
        addr,
        "GET",
        "/api/health?access_token=sec%2Bret%261",
        &[],
        None,
    )
    .await;
    assert_eq!(auth_ok_code, 200);
    assert_eq!(auth_ok_body["ok"], true);

    // 验证 QA-BUG-001：form-urlencoded 中包含 "%中" 等非 HEX 多字节百分号后缀不会崩溃
    let (form_code, form_body) = http_raw_call(
        addr,
        "POST",
        "/onebot/v11/send_group_msg?access_token=sec%2Bret%261",
        &[("Content-Type", "application/x-www-form-urlencoded")],
        Some("group_name=Demo%E6%B5%8B%E8%AF%95%E7%BE%A4&message=%中%E6"),
    )
    .await;
    assert_eq!(form_code, 200);
    assert_eq!(form_body["retcode"], 0);

    handle.abort();
}

#[tokio::test]
async fn async_action_semaphore_enforces_backpressure_under_flood() {
    let driver = Arc::new(FakeDriver::new());
    let state = AppState::new(driver, AdapterConfig::default());

    // 占满全部异步排队许可后，下一笔 _async 请求必须被快速拒绝而非无界 spawn 协程
    let mut held_permits = Vec::new();
    for _ in 0..MAX_INFLIGHT_ASYNC_ACTIONS {
        held_permits.push(
            state
                .async_action_semaphore
                .clone()
                .try_acquire_owned()
                .expect("acquire initial permit"),
        );
    }

    let err = dispatch_onebot_action_with_mode(
        &state,
        "send_group_msg_async",
        json!({ "message": "flood" }),
    )
    .await
    .err()
    .expect("saturated async queue must return InputRejected");
    assert!(matches!(err, common_types::AdapterError::InputRejected(_)));

    drop(held_permits);
    let recovered = dispatch_onebot_action_with_mode(&state, "get_status_async", json!({}))
        .await
        .expect("permit release restores async dispatch");
    assert!(matches!(
        recovered,
        crate::onebot::ActionDispatchOutcome::AsyncAccepted
    ));
    let _ = ONEBOT_RETCODE_ASYNC;
}

#[tokio::test]
async fn ws_malformed_json_replies_1400_and_ping_bypasses_slow_action() {
    let driver = Arc::new(FakeDriver::new());
    let driver_for_gate = Arc::clone(&driver);
    let config = AdapterConfig {
        server: ServerConfig {
            host: "127.0.0.1".to_string(),
            port: 0,
            access_token: None,
        },
        ..Default::default()
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let state = AppState::new(driver_for_gate, config);
    let app = crate::build_router_with_state(state.clone());
    let server_handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let ws_url = format!("ws://{addr}{ROUTE_ONEBOT_WS}");
    let (mut ws, _) = connect_async(&ws_url).await.expect("connect ws");
    let connect_evt = next_ws_json(&mut ws, Duration::from_secs(2)).await;
    assert_eq!(connect_evt["sub_type"], "connect");

    // 验证 QA-BUG-006：畸形 JSON 与缺失字段均返回显式 retcode=1400 且保留 echo
    ws.send(Message::Text("{broken_json".into())).await.unwrap();
    let err_1 = next_ws_json(&mut ws, Duration::from_secs(2)).await;
    assert_eq!(err_1["retcode"], 1400);
    assert_eq!(err_1["code"], "BAD_REQUEST");

    ws.send(Message::Text(
        json!({"params": {}, "echo": "bad-no-action"})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    let err_2 = next_ws_json(&mut ws, Duration::from_secs(2)).await;
    assert_eq!(err_2["retcode"], 1400);
    assert_eq!(err_2["echo"], "bad-no-action");

    // 人工占用 send_gate 200ms 模拟慢发送，同时发送 Ping 与同步查询验证主循环不被阻塞
    let held_gate = state.send_gate.lock().await;
    ws.send(Message::Text(
        json!({
            "action": "send_group_msg",
            "params": { "message": "slow_send" },
            "echo": "slow-1",
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(15)).await;

    let ping_started = Instant::now();
    ws.send(Message::Ping(vec![1, 2, 3].into())).await.unwrap();
    ws.send(Message::Text(
        json!({
            "action": "get_version_info",
            "params": {},
            "echo": "fast-ver",
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();

    let mut saw_pong = false;
    let mut saw_fast_ver = false;
    while !saw_pong || !saw_fast_ver {
        let frame = tokio::time::timeout(Duration::from_millis(150), ws.next())
            .await
            .expect("Ping/fast RPC must not wait behind blocked send_gate")
            .unwrap()
            .unwrap();
        match frame {
            Message::Pong(p) if p.as_ref() == [1, 2, 3] => {
                assert!(ping_started.elapsed() < Duration::from_millis(120));
                saw_pong = true;
            }
            Message::Text(txt) => {
                let parsed: Value = serde_json::from_str(txt.as_str()).unwrap();
                if parsed["echo"] == "fast-ver" {
                    assert_eq!(parsed["retcode"], 0);
                    saw_fast_ver = true;
                }
            }
            _ => {}
        }
    }

    drop(held_gate);
    let slow_reply = next_ws_json(&mut ws, Duration::from_secs(2)).await;
    assert_eq!(slow_reply["echo"], "slow-1");
    assert_eq!(slow_reply["retcode"], 0);

    // 验证超大 HTTP Body 被 413 拒收且不崩
    let mut raw_stream = TcpStream::connect(addr).await.unwrap();
    let oversized_len = crate::constants::HTTP_MAX_BODY_BYTES + 64;
    let req_head = format!(
        "POST {ROUTE_ONEBOT_SEND_GROUP_MSG} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {oversized_len}\r\n\r\n"
    );
    raw_stream.write_all(req_head.as_bytes()).await.unwrap();
    let mut small_resp = [0_u8; 256];
    let n = tokio::time::timeout(Duration::from_secs(2), raw_stream.read(&mut small_resp))
        .await
        .unwrap()
        .unwrap();
    let head_str = String::from_utf8_lossy(&small_resp[..n]);
    assert!(head_str.contains("413"));

    server_handle.abort();
}

#[tokio::test]
async fn unauthorized_forward_msg_does_not_leak_orphan_nodes_and_caps_node_count() {
    let driver = Arc::new(FakeDriver::new());
    driver.allow_private_friend("测试好友");
    let config = AdapterConfig {
        server: ServerConfig {
            host: "127.0.0.1".to_string(),
            port: 0,
            access_token: None,
        },
        target: GroupTargetSpec {
            allowed_private_friends: vec!["测试好友".to_string()],
            ..Default::default()
        },
        ..Default::default()
    };
    let (addr, handle) = spawn_server(Arc::clone(&driver) as Arc<_>, config)
        .await
        .unwrap();

    // 验证 QA-BUG-009：未授权私聊目标发起合并转发必须在入库节点前即返回 403
    let (forbid_code, forbid_body) = http_call(
        addr,
        "POST",
        "/onebot/v11/send_private_forward_msg",
        None,
        Some(json!({
            "nickname": "未授权路人",
            "messages": [
                { "type": "node", "data": { "nickname": "A", "content": "leak test" } }
            ]
        })),
    )
    .await;
    assert_eq!(forbid_code, 403);
    assert_eq!(forbid_body["retcode"], 1403);

    // 验证合并转发节点数超过 MAX_FORWARD_NODES 时在持久化前直接拒绝
    let over_nodes: Vec<Value> = (0..=MAX_FORWARD_NODES)
        .map(
            |i| json!({ "type": "node", "data": { "nickname": "A", "content": format!("m-{i}") } }),
        )
        .collect();
    let (cap_code, cap_body) = http_call(
        addr,
        "POST",
        "/onebot/v11/send_private_forward_msg",
        None,
        Some(json!({
            "nickname": "测试好友",
            "messages": over_nodes,
        })),
    )
    .await;
    assert_eq!(cap_code, 400);
    assert_eq!(cap_body["retcode"], 1400);

    handle.abort();
}

#[tokio::test]
async fn unconfirmed_send_outcome_returns_1500_across_rest_onebot_and_ws() {
    let driver = Arc::new(FakeDriver::new());
    driver.set_force_unconfirmed_send(true);
    let config = AdapterConfig {
        server: ServerConfig {
            host: "127.0.0.1".to_string(),
            port: 0,
            access_token: None,
        },
        target: GroupTargetSpec {
            group_name: "Demo测试群".to_string(),
            ..Default::default()
        },
        ..Default::default()
    };
    let (addr, handle) = spawn_server(Arc::clone(&driver) as Arc<_>, config)
        .await
        .unwrap();

    // 验证 LIVE-BUG-001：底层未确认入屏（ok=false）时 OneBot HTTP 必须返回 500/retcode=1500
    let (ob_code, ob_body) = http_call(
        addr,
        "POST",
        ROUTE_ONEBOT_SEND_GROUP_MSG,
        None,
        Some(json!({ "group_id": 555_005, "message": "unconfirmed-http" })),
    )
    .await;
    assert_eq!(ob_code, 500);
    assert_eq!(ob_body["retcode"], 1500);
    assert_eq!(ob_body["ok"], false);

    // 验证 REST /api/send 在 outcome.ok=false 时同样不得返回 200 OK
    let (rest_code, rest_body) = http_call(
        addr,
        "POST",
        "/api/send",
        None,
        Some(json!({
            "group": "Demo测试群",
            "text": "unconfirmed-rest",
            "target_kind": "group"
        })),
    )
    .await;
    assert_eq!(rest_code, 500);
    assert_eq!(rest_body["retcode"], 1500);
    assert_eq!(rest_body["ok"], false);

    // 验证正向 WS send_group_msg 在 outcome.ok=false 时返回 retcode=1500（绝非 0/10000/10001）
    let ws_url = format!("ws://{addr}{ROUTE_ONEBOT_WS}");
    let (mut ws, _) = connect_async(&ws_url).await.expect("connect ws");
    let _ = next_ws_json(&mut ws, Duration::from_secs(2)).await;
    ws.send(Message::Text(
        json!({
            "action": "send_group_msg",
            "params": { "group_id": 555_005, "message": "unconfirmed-ws" },
            "echo": "unconfirmed-ws-1"
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();
    let ws_reply = next_ws_json(&mut ws, Duration::from_secs(2)).await;
    assert_eq!(ws_reply["echo"], "unconfirmed-ws-1");
    assert_eq!(ws_reply["status"], "failed");
    assert_eq!(ws_reply["retcode"], 1500);

    handle.abort();
}

#[tokio::test]
async fn unsupported_record_and_video_return_10004_by_default_and_opt_in_succeeds() {
    let driver_default = Arc::new(FakeDriver::new());
    let config_default = AdapterConfig {
        server: ServerConfig {
            host: "127.0.0.1".to_string(),
            port: 0,
            access_token: None,
        },
        target: GroupTargetSpec {
            group_name: "Demo测试群".to_string(),
            ..Default::default()
        },
        ..Default::default()
    };
    let (addr_default, handle_default) =
        spawn_server(Arc::clone(&driver_default) as Arc<_>, config_default)
            .await
            .unwrap();

    // 默认禁止将 record/video 静默降级为纯文本占位符，必须返回 HTTP 501 / retcode=10004
    let (rec_code, rec_body) = http_call(
        addr_default,
        "POST",
        ROUTE_ONEBOT_SEND_GROUP_MSG,
        None,
        Some(json!({
            "group_id": 555_005,
            "message": [{ "type": "record", "data": { "file": "voice.amr" } }]
        })),
    )
    .await;
    assert_eq!(rec_code, 501);
    assert_eq!(rec_body["retcode"], 10004);

    let (vid_url_code, vid_url_body) = http_call(
        addr_default,
        "POST",
        ROUTE_ONEBOT_SEND_GROUP_MSG,
        None,
        Some(json!({
            "group_id": 555_005,
            "message": "[CQ:video,file=http://example.com/clip.mp4]"
        })),
    )
    .await;
    assert_eq!(vid_url_code, 501);
    assert_eq!(vid_url_body["retcode"], 10004);

    let (file_url_code, file_url_body) = http_call(
        addr_default,
        "POST",
        ROUTE_ONEBOT_SEND_GROUP_MSG,
        None,
        Some(json!({
            "group_id": 555_005,
            "message": "[CQ:file,file=https://example.com/archive.zip]"
        })),
    )
    .await;
    assert_eq!(file_url_code, 501);
    assert_eq!(file_url_body["retcode"], 10004);

    let (vid_mix_code, vid_mix_body) = http_call(
        addr_default,
        "POST",
        ROUTE_ONEBOT_SEND_GROUP_MSG,
        None,
        Some(json!({
            "group_id": 555_005,
            "message": "[CQ:video,file=clip.mp4] 附带文本"
        })),
    )
    .await;
    assert_eq!(vid_mix_code, 501);
    assert_eq!(vid_mix_body["retcode"], 10004);
    assert!(driver_default.sent_requests().is_empty());

    // 独立本地或 base64:// 视频段在严格模式下放行投递
    let (vid_local_code, vid_local_body) = http_call(
        addr_default,
        "POST",
        ROUTE_ONEBOT_SEND_GROUP_MSG,
        None,
        Some(json!({
            "group_id": 555_005,
            "message": "[CQ:video,file=clip.mp4]"
        })),
    )
    .await;
    assert_eq!(vid_local_code, 200);
    assert_eq!(vid_local_body["retcode"], 0);

    let (vid_b64_code, vid_b64_body) = http_call(
        addr_default,
        "POST",
        ROUTE_ONEBOT_SEND_GROUP_MSG,
        None,
        Some(json!({
            "group_id": 555_005,
            "message": "[CQ:video,file=base64://AAAA]"
        })),
    )
    .await;
    assert_eq!(vid_b64_code, 200);
    assert_eq!(vid_b64_body["retcode"], 0);
    assert_eq!(driver_default.sent_requests().len(), 2);
    handle_default.abort();

    // 显式开启 allow_degraded_media_summary=true 后允许降级占位文本发送
    let driver_optin = Arc::new(FakeDriver::new());
    let mut config_optin = AdapterConfig {
        server: ServerConfig {
            host: "127.0.0.1".to_string(),
            port: 0,
            access_token: None,
        },
        target: GroupTargetSpec {
            group_name: "Demo测试群".to_string(),
            ..Default::default()
        },
        ..Default::default()
    };
    config_optin.onebot.allow_degraded_media_summary = true;
    let (addr_optin, handle_optin) =
        spawn_server(Arc::clone(&driver_optin) as Arc<_>, config_optin)
            .await
            .unwrap();

    let (ok_rec_code, ok_rec_body) = http_call(
        addr_optin,
        "POST",
        ROUTE_ONEBOT_SEND_GROUP_MSG,
        None,
        Some(json!({
            "group_id": 555_005,
            "message": [{ "type": "record", "data": { "file": "voice.amr" } }]
        })),
    )
    .await;
    assert_eq!(ok_rec_code, 200);
    assert_eq!(ok_rec_body["retcode"], 0);
    assert!(ok_rec_body["data"]["message_id"].as_i64().is_some());

    let (ok_vid_code, ok_vid_body) = http_call(
        addr_optin,
        "POST",
        ROUTE_ONEBOT_SEND_GROUP_MSG,
        None,
        Some(json!({
            "group_id": 555_005,
            "message": "[CQ:video,file=clip.mp4]"
        })),
    )
    .await;
    assert_eq!(ok_vid_code, 200);
    assert_eq!(ok_vid_body["retcode"], 0);
    assert_eq!(driver_optin.sent_requests().len(), 2);
    handle_optin.abort();
}

#[tokio::test]
async fn get_forward_msg_resolves_both_fwd_string_and_numeric_message_id() {
    let driver = Arc::new(FakeDriver::new());
    let config = AdapterConfig {
        server: ServerConfig {
            host: "127.0.0.1".to_string(),
            port: 0,
            access_token: None,
        },
        target: GroupTargetSpec {
            group_name: "Demo测试群".to_string(),
            ..Default::default()
        },
        ..Default::default()
    };
    let (addr, handle) = spawn_server(Arc::clone(&driver) as Arc<_>, config)
        .await
        .unwrap();

    // 默认严格模式下伪造虚拟节点必须返回 501/10004 且不写入转发缓存，仅真实消息引用节点放行
    let (fake_code, fake_body) = http_call(
        addr,
        "POST",
        "/onebot/v11/send_group_forward_msg",
        None,
        Some(json!({
            "group_id": 555_005,
            "messages": [
                { "type": "node", "data": { "nickname": "Alice", "content": "node-hello" } }
            ]
        })),
    )
    .await;
    assert_eq!(fake_code, 501);
    assert_eq!(fake_body["retcode"], 10004);
    assert_eq!(driver.stored_forward_count(), 0);

    let (send_code, send_body) = http_call(
        addr,
        "POST",
        "/onebot/v11/send_group_forward_msg",
        None,
        Some(json!({
            "group_id": 555_005,
            "messages": [
                { "type": "node", "data": { "id": "10001" } }
            ]
        })),
    )
    .await;
    assert_eq!(send_code, 200);
    assert_eq!(send_body["retcode"], 0);
    let forward_id = send_body["data"]["forward_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(forward_id.starts_with("fwd-"));
    let numeric_msg_id = send_body["data"]["message_id"].as_i64().unwrap();

    // 通过 fwd-* 字符串与数字 message_id 均可回查原生合并转发关联消息节点
    for query in [
        json!({ "id": forward_id }),
        json!({ "id": numeric_msg_id }),
        json!({ "id": numeric_msg_id.to_string() }),
        json!({ "message_id": numeric_msg_id }),
    ] {
        let (fwd_code, fwd_body) = http_call(
            addr,
            "POST",
            "/onebot/v11/get_forward_msg",
            None,
            Some(query),
        )
        .await;
        assert_eq!(fwd_code, 200);
        assert_eq!(fwd_body["retcode"], 0);
        assert_eq!(fwd_body["data"]["messages"][0]["data"]["id"], "10001");
    }
    handle.abort();

    // 显式开启 allow_degraded_media_summary=true 时允许降级自定义节点并持久化唯一 fwd-* 标识
    let optin_driver = Arc::new(FakeDriver::new());
    let mut optin_cfg = AdapterConfig {
        server: ServerConfig {
            host: "127.0.0.1".to_string(),
            port: 0,
            access_token: None,
        },
        target: GroupTargetSpec {
            group_name: "Demo测试群".to_string(),
            ..Default::default()
        },
        ..Default::default()
    };
    optin_cfg.onebot.allow_degraded_media_summary = true;
    let (optin_addr, optin_handle) = spawn_server(Arc::clone(&optin_driver) as Arc<_>, optin_cfg)
        .await
        .unwrap();
    let (optin_code, optin_body) = http_call(
        optin_addr,
        "POST",
        "/onebot/v11/send_group_forward_msg",
        None,
        Some(json!({
            "group_id": 555_005,
            "messages": [
                { "type": "node", "data": { "nickname": "Alice", "content": "node-hello" } }
            ]
        })),
    )
    .await;
    assert_eq!(optin_code, 200);
    let optin_fwd = optin_body["data"]["forward_id"].as_str().unwrap();
    let (_, q_body) = http_call(
        optin_addr,
        "POST",
        "/onebot/v11/get_forward_msg",
        None,
        Some(json!({ "id": optin_fwd })),
    )
    .await;
    assert_eq!(
        q_body["data"]["messages"][0]["data"]["content"],
        "node-hello"
    );
    optin_handle.abort();
}

#[tokio::test]
async fn strict_mode_enforces_10004_and_exposes_screenshot_probe_and_async_audit() {
    let driver = Arc::new(FakeDriver::new());
    driver.allow_private_friend("测试好友");
    let config = AdapterConfig {
        server: ServerConfig {
            host: "127.0.0.1".to_string(),
            port: 0,
            access_token: None,
        },
        target: GroupTargetSpec {
            group_name: "Demo测试群".to_string(),
            allowed_private_friends: vec!["测试好友".to_string()],
            ..Default::default()
        },
        ..Default::default()
    };
    let (addr, handle) = spawn_server(Arc::clone(&driver) as Arc<_>, config)
        .await
        .unwrap();

    // 验证严格模式下未支持段与非法混发段全部拒绝为 501 / retcode=10004
    for (route, payload) in [
        (
            ROUTE_ONEBOT_SEND_GROUP_MSG,
            json!({ "group_id": 555_005, "message": "[CQ:json,data={}]" }),
        ),
        (
            ROUTE_ONEBOT_SEND_GROUP_MSG,
            json!({ "group_id": 555_005, "message": "[CQ:face,id=999999]" }),
        ),
        (
            ROUTE_ONEBOT_SEND_GROUP_MSG,
            json!({ "group_id": 555_005, "message": "[CQ:shake]" }),
        ),
        (
            ROUTE_ONEBOT_SEND_GROUP_MSG,
            json!({ "group_id": 555_005, "message": "[CQ:rps]text" }),
        ),
        (
            ROUTE_ONEBOT_SEND_GROUP_MSG,
            json!({ "group_id": 555_005, "message": "[CQ:unknown_tag,foo=1]" }),
        ),
        (
            ROUTE_ONEBOT_SEND_GROUP_MSG,
            json!({ "group_id": 555_005, "message": "[CQ:poke]" }),
        ),
        (
            ROUTE_ONEBOT_SEND_GROUP_MSG,
            json!({ "group_id": 555_005, "message": [{ "type": "unknown_seg", "data": {} }] }),
        ),
        (
            "/onebot/v11/send_private_msg",
            json!({ "nickname": "测试好友", "message": "[CQ:at,qq=777001]" }),
        ),
    ] {
        let (code, body) = http_call(addr, "POST", route, None, Some(payload)).await;
        assert_eq!(code, 501, "expected 501 for {route}");
        assert_eq!(body["retcode"], 10004);
    }

    // 验证新增 `/api/diag/probe-ui` 与 `/api/screenshot` 诊断路由可用
    let (probe_code, probe_body) = http_call(
        addr,
        "POST",
        "/api/diag/probe-ui",
        None,
        Some(json!({ "action": "selfcheck_state" })),
    )
    .await;
    assert_eq!(probe_code, 200);
    assert_eq!(probe_body["action"], "selfcheck_state");

    let mut raw_stream = TcpStream::connect(addr).await.unwrap();
    let req = format!("GET /api/screenshot HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    raw_stream.write_all(req.as_bytes()).await.unwrap();
    let mut raw_resp = Vec::new();
    raw_stream.read_to_end(&mut raw_resp).await.unwrap();
    let resp_str = String::from_utf8_lossy(&raw_resp);
    assert!(resp_str.contains("200 OK"));
    assert!(resp_str.contains("image/png"));
    assert!(raw_resp.windows(4).any(|w| w == b"\x89PNG"));

    // 验证 `_async` 后台执行失败后自动写入 `async_action_failed` 审计记录
    driver.set_next_error(common_types::AdapterError::UiaFailure(
        "async mock boom".to_string(),
    ));
    let (async_code, async_body) = http_call(
        addr,
        "POST",
        "/onebot/v11/send_group_msg_async",
        None,
        Some(json!({ "group_id": 555_005, "message": "hello" })),
    )
    .await;
    assert_eq!(async_code, 200);
    assert_eq!(async_body["retcode"], ONEBOT_RETCODE_ASYNC);
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert!(
        driver
            .audit_events()
            .iter()
            .any(|(kind, detail)| kind == "async_action_failed"
                && detail.contains("async mock boom")),
        "expected async_action_failed audit record"
    );

    handle.abort();
}
