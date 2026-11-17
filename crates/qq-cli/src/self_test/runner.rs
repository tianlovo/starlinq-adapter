//! 端到端全链路自测执行器：验证 Win32 模拟窗口、SQLite 隔离存储、REST、正反向 WS 与安全熔断

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use chrono::Utc;
use common_types::constants::{ROUTE_API_AUDIT_EVENTS, ROUTE_API_HEALTH, ROUTE_API_SECURITY_RESET};
use common_types::{AdapterConfig, AdapterStatus, ChatMessage, UiNodeSnapshot, WindowInfo};
use futures_util::SinkExt;
use mock_qq_win::{MockQqHandle, MockQqServer};
use qqnt_driver::QqntDriver;
use reqwest::Method;
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;

use crate::constants::{
    ACTION_GET_FRIEND_LIST, ACTION_GET_GROUP_LIST, ACTION_GET_MSG, ACTION_GET_SCHEDULER_STATUS,
    ACTION_GET_VERSION_INFO, ACTION_GET_WORD_SLICES, ACTION_MARK_GROUP_MSG_AS_READ,
    HEADER_X_CLIENT_ROLE_UNIVERSAL, MOCK_QQ_MAIN_CLASS, ROUTE_API_ENSURE_QQ, ROUTE_API_FRIENDS,
    ROUTE_API_GROUPS, ROUTE_API_MESSAGES, ROUTE_API_SCHEDULER, ROUTE_API_SCHEDULER_POLICY,
    ROUTE_API_SEND, ROUTE_API_SHUTDOWN, ROUTE_API_STATUS, ROUTE_API_STORAGE_STATS, ROUTE_API_TREE,
    ROUTE_API_WINDOWS, ROUTE_ONEBOT_CAN_SEND_IMAGE, ROUTE_ONEBOT_GET_FRIEND_LIST,
    ROUTE_ONEBOT_GET_STATUS, ROUTE_ONEBOT_GET_VERSION_INFO, ROUTE_ONEBOT_SEND_GROUP_MSG,
    ROUTE_ONEBOT_SEND_MSG, ROUTE_ONEBOT_SEND_PRIVATE_MSG, ROUTE_ONEBOT_WS, SELF_TEST_BIG_GROUP,
    SELF_TEST_PNG_BASE64, SELF_TEST_PRIVATE_FRIEND, SELF_TEST_SECURITY_TITLE,
    SELF_TEST_SMALL_GROUP,
};
use crate::http_client::{format_base_url, http_json_request};
use crate::self_test::mock_reverse_server::{
    call_reverse_action, recv_reverse_text_timeout, AcceptedReverseClient, MockReverseWsServer,
};
use crate::self_test::ocr_probe::verify_video_cap_and_ocr_api;
use crate::ws_client::WsClient;

#[derive(Default)]
/// 递归检查模拟 QQ 窗口 UIA 树中是否包含列表、输入框与按钮控件的结果记录
struct NodeFindings {
    has_list: bool,
    has_edit: bool,
    has_button: bool,
}

async fn await_dual_ws_incoming_message(
    forward_ws: &mut WsClient,
    reverse_client: &mut AcceptedReverseClient,
    expected_msg_type: &str,
    expected_sender: &str,
    expected_sub_text: &str,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut forward_ok = false;
    let mut reverse_ok = false;
    while Instant::now() < deadline && (!forward_ok || !reverse_ok) {
        let slice = deadline.saturating_duration_since(Instant::now());
        if !forward_ok {
            if let Some(frame) = forward_ws
                .recv_text_timeout(slice.min(Duration::from_millis(200)))
                .await?
            {
                let event: Value = serde_json::from_str(&frame)?;
                if event["post_type"] == "message"
                    && event["message_type"] == expected_msg_type
                    && event["raw_message"]
                        .as_str()
                        .is_some_and(|raw| raw.contains(expected_sub_text))
                    && event["sender"]["nickname"] == expected_sender
                {
                    forward_ok = true;
                }
            }
        }
        if !reverse_ok {
            if let Some(frame) = recv_reverse_text_timeout(
                &mut reverse_client.ws_stream,
                slice.min(Duration::from_millis(200)),
            )
            .await?
            {
                let event: Value = serde_json::from_str(&frame)?;
                if event["post_type"] == "message"
                    && event["message_type"] == expected_msg_type
                    && event["raw_message"]
                        .as_str()
                        .is_some_and(|raw| raw.contains(expected_sub_text))
                    && event["sender"]["nickname"] == expected_sender
                {
                    reverse_ok = true;
                }
            }
        }
    }
    if !forward_ok || !reverse_ok {
        bail!(
            "timed out waiting for '{expected_msg_type}' WS push (forward_ok={forward_ok}, reverse_ok={reverse_ok})"
        );
    }
    Ok(())
}

async fn verify_v3_segments_and_image_paste(
    base_url: &str,
    mock: &MockQqHandle,
    sent_msg_id: i64,
) -> Result<()> {
    let can_img_resp =
        http_json_request(base_url, Method::GET, ROUTE_ONEBOT_CAN_SEND_IMAGE, None).await?;
    if can_img_resp.status_code != 200 || can_img_resp.json()?["data"]["yes"] != true {
        bail!(
            "expected can_send_image yes==true, got: {}",
            can_img_resp.body
        );
    }

    // 校验结构化 reply + at + text 复合消息段发送与 get_msg 回放一致性
    tokio::time::sleep(Duration::from_millis(120)).await;
    let reply_payload = json!({
        "message": [
            { "type": "reply", "data": { "id": sent_msg_id.to_string() } },
            { "type": "at", "data": { "qq": "10001", "name": "群友A" } },
            { "type": "text", "data": { "text": "V3回复验证" } }
        ]
    });
    let reply_resp = http_json_request(
        base_url,
        Method::POST,
        ROUTE_ONEBOT_SEND_GROUP_MSG,
        Some(&reply_payload),
    )
    .await?;
    if reply_resp.status_code != 200 || reply_resp.json()?["retcode"] != 0 {
        bail!(
            "structured reply+at send failed ({}): {}",
            reply_resp.status_code,
            reply_resp.body
        );
    }
    let reply_sent_id = reply_resp.json()?["data"]["message_id"]
        .as_i64()
        .ok_or_else(|| anyhow::anyhow!("missing message_id for structured send"))?;
    let replay_resp = http_json_request(
        base_url,
        Method::GET,
        &format!("/onebot/v11/get_msg?message_id={reply_sent_id}"),
        None,
    )
    .await?;
    let replay_json = replay_resp.json()?;
    let replay_segs = replay_json["data"]["message"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("get_msg missing segments array: {replay_json}"))?;
    let has_reply = replay_segs.iter().any(|seg| seg["type"] == "reply");
    let has_at = replay_segs.iter().any(|seg| seg["type"] == "at");
    let has_text = replay_segs.iter().any(|seg| seg["type"] == "text");
    if !has_reply || !has_at || !has_text {
        bail!("get_msg structured replay missing reply/at/text segments: {replay_json}");
    }

    // 校验 base64:// PNG 图片经剪贴板 CF_DIB 后台粘贴发送且零触碰物理鼠标
    tokio::time::sleep(Duration::from_millis(120)).await;
    let img_payload = json!({
        "message": [
            { "type": "image", "data": { "file": SELF_TEST_PNG_BASE64 } }
        ]
    });
    let img_resp = http_json_request(
        base_url,
        Method::POST,
        ROUTE_ONEBOT_SEND_GROUP_MSG,
        Some(&img_payload),
    )
    .await?;
    if img_resp.status_code != 200 || img_resp.json()?["retcode"] != 0 {
        bail!(
            "base64 PNG image send failed ({}): {}",
            img_resp.status_code,
            img_resp.body
        );
    }
    if img_resp.json()?["data"]["outcome"]["mouse_touched"] != false {
        bail!("image send unexpectedly touched mouse: {}", img_resp.body);
    }
    if !mock
        .messages_for(SELF_TEST_SMALL_GROUP)
        .iter()
        .any(|msg| msg.contains("[图片]"))
    {
        bail!("mock window did not record pasted '[图片]' marker");
    }
    Ok(())
}

async fn verify_v3_private_chat_and_ws_push(
    base_url: &str,
    mock: &MockQqHandle,
    forward_ws: &mut WsClient,
    reverse_client: &mut AcceptedReverseClient,
) -> Result<()> {
    // 非白名单私聊好友必须直接被 403 / PRIVATE_TARGET_NOT_ALLOWED 拦截
    let blocked_priv_payload = json!({
        "nickname": "未授权路人",
        "message": "blocked private"
    });
    let blocked_priv_resp = http_json_request(
        base_url,
        Method::POST,
        ROUTE_ONEBOT_SEND_PRIVATE_MSG,
        Some(&blocked_priv_payload),
    )
    .await?;
    let blocked_priv_json = blocked_priv_resp.json()?;
    if blocked_priv_resp.status_code != 403
        || blocked_priv_json["retcode"] != 1403
        || blocked_priv_json["code"] != "PRIVATE_TARGET_NOT_ALLOWED"
    {
        bail!(
            "expected 403 PRIVATE_TARGET_NOT_ALLOWED for unauthorized private target, got ({}): {}",
            blocked_priv_resp.status_code,
            blocked_priv_resp.body
        );
    }

    // 校验 get_friend_list（HTTP 与 Reverse WS）及 /api/friends 能发现模拟好友
    let friends_http = http_json_request(
        base_url,
        Method::GET,
        &format!("{ROUTE_ONEBOT_GET_FRIEND_LIST}?no_cache=true"),
        None,
    )
    .await?;
    let friends_rest = http_json_request(base_url, Method::GET, ROUTE_API_FRIENDS, None).await?;
    let friends_ws = call_reverse_action(
        &mut reverse_client.ws_stream,
        ACTION_GET_FRIEND_LIST,
        json!({ "no_cache": false }),
        "st-friends-1",
    )
    .await?;
    let http_has_friend = friends_http.json()?["data"].as_array().is_some_and(|arr| {
        arr.iter()
            .any(|item| item["nickname"] == SELF_TEST_PRIVATE_FRIEND)
    });
    let rest_has_friend = friends_rest.json()?.as_array().is_some_and(|arr| {
        arr.iter()
            .any(|item| item["nickname"] == SELF_TEST_PRIVATE_FRIEND)
    });
    let ws_has_friend = friends_ws["retcode"] == 0
        && friends_ws["data"].as_array().is_some_and(|arr| {
            arr.iter()
                .any(|item| item["nickname"] == SELF_TEST_PRIVATE_FRIEND)
        });
    if !http_has_friend || !rest_has_friend || !ws_has_friend {
        bail!(
            "friend list missing '{SELF_TEST_PRIVATE_FRIEND}': http={}, rest={}, ws={}",
            friends_http.body,
            friends_rest.body,
            friends_ws
        );
    }

    // 白名单好友私聊发送成功且不向 WS 反向泄漏自身回显
    tokio::time::sleep(Duration::from_millis(120)).await;
    let priv_send_payload = json!({
        "nickname": SELF_TEST_PRIVATE_FRIEND,
        "message": "Hello V3 Private"
    });
    let priv_send_resp = http_json_request(
        base_url,
        Method::POST,
        ROUTE_ONEBOT_SEND_PRIVATE_MSG,
        Some(&priv_send_payload),
    )
    .await?;
    if priv_send_resp.status_code != 200 || priv_send_resp.json()?["retcode"] != 0 {
        bail!(
            "whitelisted send_private_msg failed ({}): {}",
            priv_send_resp.status_code,
            priv_send_resp.body
        );
    }
    if !mock
        .messages_for(SELF_TEST_PRIVATE_FRIEND)
        .iter()
        .any(|msg| msg.contains("Hello V3 Private"))
    {
        bail!("sent private message not found in mock {SELF_TEST_PRIVATE_FRIEND}");
    }

    // 白名单好友私聊经统一 send_msg 入口同样可达
    tokio::time::sleep(Duration::from_millis(120)).await;
    let send_msg_priv = json!({
        "message_type": "private",
        "nickname": SELF_TEST_PRIVATE_FRIEND,
        "message": "Hello V3 Via send_msg"
    });
    let send_msg_resp = http_json_request(
        base_url,
        Method::POST,
        ROUTE_ONEBOT_SEND_MSG,
        Some(&send_msg_priv),
    )
    .await?;
    if send_msg_resp.status_code != 200 || send_msg_resp.json()?["retcode"] != 0 {
        bail!(
            "send_msg(private) failed ({}): {}",
            send_msg_resp.status_code,
            send_msg_resp.body
        );
    }

    // 注入好友来信并验证正反向 WS 均推流 message.private.friend
    mock.inject_incoming_message(SELF_TEST_PRIVATE_FRIEND, "测试好友 [14:07]: 私聊问候");
    await_dual_ws_incoming_message(
        forward_ws,
        reverse_client,
        "private",
        SELF_TEST_PRIVATE_FRIEND,
        "私聊问候",
    )
    .await
}

fn scan_ui_tree(node: &UiNodeSnapshot, findings: &mut NodeFindings) {
    let control_type = node.control_type.to_ascii_lowercase();
    let class_name = node.class_name.to_ascii_lowercase();
    if control_type.contains("list") || class_name.contains("listbox") {
        findings.has_list = true;
    }
    if control_type.contains("edit") || class_name.contains("edit") {
        findings.has_edit = true;
    }
    if control_type.contains("button") || class_name.contains("button") {
        findings.has_button = true;
    }
    for child in &node.children {
        scan_ui_tree(child, findings);
    }
}

async fn verify_scheduler_and_onebot_flood_apis(base_url: &str) -> Result<()> {
    // 1. 验证 GET /api/scheduler 与 POST /api/scheduler/policy 策略热更新及边界校验
    let sched_resp = http_json_request(base_url, Method::GET, ROUTE_API_SCHEDULER, None).await?;
    let sched_json = sched_resp.json()?;
    if sched_resp.status_code != 200 || sched_json["ok"] != true {
        bail!(
            "GET /api/scheduler failed ({}): {}",
            sched_resp.status_code,
            sched_resp.body
        );
    }
    let orig_window_ms = sched_json["config"]["burst_window_ms"]
        .as_u64()
        .ok_or_else(|| anyhow::anyhow!("GET /api/scheduler missing burst_window_ms"))?;

    let patch_ok = http_json_request(
        base_url,
        Method::POST,
        ROUTE_API_SCHEDULER_POLICY,
        Some(&json!({ "burst_window_ms": 4_000 })),
    )
    .await?;
    if patch_ok.status_code != 200 || patch_ok.json()?["config"]["burst_window_ms"] != 4_000 {
        bail!(
            "POST /api/scheduler/policy valid update failed ({}): {}",
            patch_ok.status_code,
            patch_ok.body
        );
    }

    let patch_bad = http_json_request(
        base_url,
        Method::POST,
        ROUTE_API_SCHEDULER_POLICY,
        Some(&json!({ "burst_window_ms": 0 })),
    )
    .await?;
    if patch_bad.status_code != 400 {
        bail!(
            "expected HTTP 400 on burst_window_ms=0, got ({}): {}",
            patch_bad.status_code,
            patch_bad.body
        );
    }

    let _restore = http_json_request(
        base_url,
        Method::POST,
        ROUTE_API_SCHEDULER_POLICY,
        Some(&json!({ "burst_window_ms": orig_window_ms })),
    )
    .await?;

    // 2. 验证 OneBot .get_scheduler_status、.get_word_slices 与 mark_group_msg_as_read
    let ob_sched = http_json_request(
        base_url,
        Method::GET,
        &format!("/onebot/v11/{ACTION_GET_SCHEDULER_STATUS}"),
        None,
    )
    .await?;
    let ob_sched_json = ob_sched.json()?;
    if ob_sched.status_code != 200
        || ob_sched_json["retcode"] != 0
        || !ob_sched_json["data"]["telemetry"].is_object()
    {
        bail!(
            ".get_scheduler_status failed ({}): {}",
            ob_sched.status_code,
            ob_sched.body
        );
    }

    let slices_resp = http_json_request(
        base_url,
        Method::POST,
        &format!("/onebot/v11/{ACTION_GET_WORD_SLICES}"),
        Some(&json!({ "content": "Hello, QQ机器人!" })),
    )
    .await?;
    let slices_json = slices_resp.json()?;
    if slices_resp.status_code != 200
        || slices_json["retcode"] != 0
        || slices_json["data"]["slices"] != json!(["Hello", ",", "QQ", "机", "器", "人", "!"])
    {
        bail!(
            ".get_word_slices unexpected result ({}): {}",
            slices_resp.status_code,
            slices_resp.body
        );
    }

    let read_resp = http_json_request(
        base_url,
        Method::POST,
        &format!("/onebot/v11/{ACTION_MARK_GROUP_MSG_AS_READ}"),
        Some(&json!({ "group_name": SELF_TEST_SMALL_GROUP })),
    )
    .await?;
    if read_resp.status_code != 200
        || read_resp.json()?["retcode"] != 0
        || read_resp.json()?["data"]["marked"] != true
    {
        bail!(
            "mark_group_msg_as_read failed ({}): {}",
            read_resp.status_code,
            read_resp.body
        );
    }

    // 3. 验证过期 deadline_unix_ms 出站必须返回 HTTP 408 与 OneBot retcode 1408
    let expired_ms = (Utc::now().timestamp_millis().max(0) as u64).saturating_sub(5_000);
    let expired_ob = http_json_request(
        base_url,
        Method::POST,
        ROUTE_ONEBOT_SEND_GROUP_MSG,
        Some(&json!({
            "group_name": SELF_TEST_SMALL_GROUP,
            "message": "expired self-test onebot reply",
            "deadline_unix_ms": expired_ms,
        })),
    )
    .await?;
    let expired_ob_json = expired_ob.json()?;
    if expired_ob.status_code != 408
        || expired_ob_json["retcode"] != 1408
        || expired_ob_json["code"] != "RESPONSE_WINDOW_EXPIRED"
    {
        bail!(
            "expected HTTP 408 retcode 1408 for expired deadline_unix_ms, got ({}): {}",
            expired_ob.status_code,
            expired_ob.body
        );
    }

    let expired_rest = http_json_request(
        base_url,
        Method::POST,
        ROUTE_API_SEND,
        Some(&json!({
            "group": SELF_TEST_SMALL_GROUP,
            "text": "expired self-test rest reply",
            "deadline_unix_ms": expired_ms,
        })),
    )
    .await?;
    if expired_rest.status_code != 408 || expired_rest.json()?["code"] != "RESPONSE_WINDOW_EXPIRED"
    {
        bail!(
            "expected HTTP 408 RESPONSE_WINDOW_EXPIRED on /api/send, got ({}): {}",
            expired_rest.status_code,
            expired_rest.body
        );
    }
    Ok(())
}

/// 在当前桌面执行 13 步全链路端到端自测并验证正反向 WebSocket、调度接口与内存 SQLite 隔离
pub async fn run_self_test() -> Result<()> {
    println!("[self-test] 1/14 spawning MockQqServer and MockReverseWsServer...");
    let mock = MockQqServer::spawn().context("failed to spawn MockQqServer")?;
    let mock_hwnd = mock.hwnd();
    let mock_reverse = MockReverseWsServer::spawn().await?;
    println!(
        "[self-test]     MockQQNTWndClass hwnd={mock_hwnd}, reverse_ws={}",
        mock_reverse.url
    );

    println!("[self-test] 2/14 initializing QqntDriver with isolated :memory: SQLite...");
    let mut config = AdapterConfig::default();
    config.server.host = "127.0.0.1".to_string();
    config.server.port = 0;
    // 自测强制使用内存数据库，严禁污染工作区默认持久化库文件
    config.storage.db_path = ":memory:".to_string();
    config.target.group_name = "Demo测试群".to_string();
    config.target.strict_below_members = Some(10);
    config.target.require_member_count_verified = false;
    config.target.allowed_private_friends = vec![SELF_TEST_PRIVATE_FRIEND.to_string()];
    config.interaction.extra_window_classes = vec![MOCK_QQ_MAIN_CLASS.to_string()];
    config.interaction.char_delay_min_ms = 2;
    config.interaction.char_delay_max_ms = 5;
    config.interaction.min_send_interval_ms = 100;
    config.interaction.poll_interval_ms = 150;
    config.bot.auto_ping_reply = false;
    config.onebot.reverse_ws_urls = vec![mock_reverse.url.clone()];
    config.onebot.heartbeat_interval_ms = 5_000;
    config.onebot.reconnect_interval_ms = 200;
    // 模拟 Win32 窗口不含 QQNT 右键回复与成员弹窗，故自测启用降级段模式以覆盖 reply+at+text 回放链路
    config.onebot.allow_degraded_media_summary = true;

    let driver = match QqntDriver::new(config.clone()) {
        Ok(driver) => Arc::new(driver),
        Err(err) => {
            mock_reverse.abort();
            mock.shutdown();
            return Err(err).context("failed to start QqntDriver");
        }
    };

    println!("[self-test] 3/14 starting adapter-server and verifying Forward + Reverse WS...");
    let (listen_addr, server_handle) = match adapter_server::spawn_server(driver, config).await {
        Ok(started) => started,
        Err(err) => {
            mock_reverse.abort();
            mock.shutdown();
            return Err(err).context("failed to spawn adapter-server");
        }
    };
    let base_url = format_base_url(&listen_addr.ip().to_string(), listen_addr.port());
    println!("[self-test]     listening at {base_url}");

    let test_result = async {
        let mut forward_ws = WsClient::connect(listen_addr, ROUTE_ONEBOT_WS, None)
            .await
            .context("connect /onebot/v11/ws")?;
        let first_forward_frame = forward_ws
            .recv_text_timeout(Duration::from_secs(3))
            .await?
            .ok_or_else(|| anyhow::anyhow!("did not receive forward lifecycle connect event"))?;
        let forward_lifecycle: Value = serde_json::from_str(&first_forward_frame)?;
        if forward_lifecycle["post_type"] != "meta_event"
            || forward_lifecycle["meta_event_type"] != "lifecycle"
            || forward_lifecycle["sub_type"] != "connect"
        {
            bail!("unexpected first forward WS event: {forward_lifecycle}");
        }

        let mut reverse_client = mock_reverse.wait_connected(Duration::from_secs(4)).await?;
        if reverse_client.headers.self_id.as_deref() != Some("10001")
            || reverse_client.headers.client_role.as_deref()
                != Some(HEADER_X_CLIENT_ROLE_UNIVERSAL)
            || reverse_client.headers.authorization.is_some()
        {
            bail!(
                "unexpected reverse WS handshake headers: {:?}",
                reverse_client.headers
            );
        }
        let first_reverse_frame =
            recv_reverse_text_timeout(&mut reverse_client.ws_stream, Duration::from_secs(3))
                .await?
                .ok_or_else(|| {
                    anyhow::anyhow!("did not receive reverse lifecycle connect event")
                })?;
        let reverse_lifecycle: Value = serde_json::from_str(&first_reverse_frame)?;
        if reverse_lifecycle["meta_event_type"] != "lifecycle"
            || reverse_lifecycle["sub_type"] != "connect"
        {
            bail!("unexpected first reverse WS event: {reverse_lifecycle}");
        }
        println!("[self-test]     verified Forward WS and Reverse WS lifecycle connect events");

        println!(
            "[self-test] 4/14 verifying POST /api/ensure-qq, GET /api/windows, and GET /api/tree..."
        );
        let ensure_resp =
            http_json_request(&base_url, Method::POST, ROUTE_API_ENSURE_QQ, None).await?;
        if ensure_resp.status_code != 200 {
            bail!(
                "POST /api/ensure-qq failed ({}): {}",
                ensure_resp.status_code,
                ensure_resp.body
            );
        }
        let ensured_windows: Vec<WindowInfo> = serde_json::from_str(&ensure_resp.body)?;
        if !ensured_windows
            .iter()
            .any(|window| window.hwnd == mock_hwnd && window.class_name == MOCK_QQ_MAIN_CLASS)
        {
            bail!(
                "{MOCK_QQ_MAIN_CLASS} hwnd={mock_hwnd} missing from POST /api/ensure-qq: {ensured_windows:?}"
            );
        }

        let win_resp =
            http_json_request(&base_url, Method::GET, ROUTE_API_WINDOWS, None).await?;
        if win_resp.status_code != 200 {
            bail!(
                "GET /api/windows failed ({}): {}",
                win_resp.status_code,
                win_resp.body
            );
        }
        let windows: Vec<WindowInfo> = serde_json::from_str(&win_resp.body)?;
        if !windows
            .iter()
            .any(|window| window.hwnd == mock_hwnd && window.class_name == MOCK_QQ_MAIN_CLASS)
        {
            bail!("{MOCK_QQ_MAIN_CLASS} hwnd={mock_hwnd} missing from /api/windows: {windows:?}");
        }

        let tree_path = format!("{ROUTE_API_TREE}?hwnd={mock_hwnd}&depth=5");
        let tree_resp = http_json_request(&base_url, Method::GET, &tree_path, None).await?;
        if tree_resp.status_code != 200 {
            bail!(
                "GET /api/tree failed ({}): {}",
                tree_resp.status_code,
                tree_resp.body
            );
        }
        let tree: UiNodeSnapshot = serde_json::from_str(&tree_resp.body)?;
        let mut findings = NodeFindings::default();
        scan_ui_tree(&tree, &mut findings);
        if !findings.has_list || !findings.has_edit || !findings.has_button {
            bail!(
                "UIA tree missing expected controls (list={}, edit={}, button={})",
                findings.has_list,
                findings.has_edit,
                findings.has_button
            );
        }
        println!("[self-test]     captured {MOCK_QQ_MAIN_CLASS} with LISTBOX/EDIT/BUTTON nodes");

        println!("[self-test] 5/14 sending via POST /onebot/v11/send_group_msg...");
        let send_payload = json!({ "message": "Hello Demo UIA Test" });
        let send_resp = http_json_request(
            &base_url,
            Method::POST,
            ROUTE_ONEBOT_SEND_GROUP_MSG,
            Some(&send_payload),
        )
        .await?;
        if send_resp.status_code != 200 {
            bail!(
                "POST /onebot/v11/send_group_msg failed ({}): {}",
                send_resp.status_code,
                send_resp.body
            );
        }
        let send_json = send_resp.json()?;
        if send_json["retcode"] != 0 {
            bail!("unexpected retcode in send_group_msg: {send_json}");
        }
        let sent_msg_id = send_json["data"]["message_id"]
            .as_i64()
            .ok_or_else(|| anyhow::anyhow!("missing message_id in send_group_msg: {send_json}"))?;
        let outcome = &send_json["data"]["outcome"];
        if outcome["mouse_touched"] != false || outcome["foreground_touched"] != false {
            bail!("expected mouse_touched==false and foreground_touched==false, got: {outcome}");
        }

        let status_resp =
            http_json_request(&base_url, Method::GET, ROUTE_API_STATUS, None).await?;
        let status: AdapterStatus = serde_json::from_str(&status_resp.body)?;
        if status.detected_member_count != Some(5)
            || !status
                .active_group
                .as_deref()
                .is_some_and(|name| name.contains("Demo测试群") && name.contains('5'))
        {
            bail!(
                "expected Demo测试群 (5) with 5 members attached, got {:?} ({:?})",
                status.active_group,
                status.detected_member_count
            );
        }

        if mock
            .messages_for(SELF_TEST_BIG_GROUP)
            .iter()
            .any(|msg| msg.contains("Hello Demo UIA Test"))
        {
            bail!("SAFETY VIOLATION: message leaked into {SELF_TEST_BIG_GROUP}!");
        }
        if !mock
            .messages_for(SELF_TEST_SMALL_GROUP)
            .iter()
            .any(|msg| msg.contains("Hello Demo UIA Test"))
        {
            bail!("sent message not found in mock {SELF_TEST_SMALL_GROUP}");
        }

        let msgs_resp =
            http_json_request(&base_url, Method::GET, ROUTE_API_MESSAGES, None).await?;
        if msgs_resp.status_code != 200 {
            bail!(
                "GET /api/messages failed ({}): {}",
                msgs_resp.status_code,
                msgs_resp.body
            );
        }
        let captured: Vec<ChatMessage> = serde_json::from_str(&msgs_resp.body)?;
        match captured
            .iter()
            .find(|msg| msg.content.contains("Hello Demo UIA Test"))
        {
            Some(msg) if msg.is_self => {}
            Some(msg) => bail!("sent message was not marked is_self=true: {msg:?}"),
            None => bail!("sent message missing from GET /api/messages: {captured:?}"),
        }
        println!("[self-test]     verified <10 guard, zero mouse/focus steal, and is_self=true");

        println!("[self-test] 6/14 verifying self-sent echo does not emit OneBot group message...");
        let quiet_until = Instant::now() + Duration::from_millis(550);
        while Instant::now() < quiet_until {
            let slice = quiet_until.saturating_duration_since(Instant::now());
            if let Some(frame) = forward_ws.recv_text_timeout(slice).await? {
                let event: Value = serde_json::from_str(&frame)?;
                if event["post_type"] == "message" {
                    bail!("self-sent message leaked as incoming Forward WS event: {event}");
                }
            }
        }
        if let Some(frame) =
            recv_reverse_text_timeout(&mut reverse_client.ws_stream, Duration::from_millis(200))
                .await?
        {
            let event: Value = serde_json::from_str(&frame)?;
            if event["post_type"] == "message" {
                bail!("self-sent message leaked as incoming Reverse WS event: {event}");
            }
        }

        println!(
            "[self-test] 7/14 verifying :memory: SQLite stats and Reverse WS V2 OneBot actions..."
        );
        let stats_resp =
            http_json_request(&base_url, Method::GET, ROUTE_API_STORAGE_STATS, None).await?;
        if stats_resp.status_code != 200 {
            bail!("GET /api/storage/stats failed: {}", stats_resp.body);
        }
        let stats_json = stats_resp.json()?;
        if stats_json["db_path"] != ":memory:" {
            bail!("expected isolated db_path ':memory:', got: {stats_json}");
        }

        let groups_resp =
            http_json_request(&base_url, Method::GET, ROUTE_API_GROUPS, None).await?;
        if groups_resp.status_code != 200 || groups_resp.json()?.as_array().is_none() {
            bail!("GET /api/groups failed: {}", groups_resp.body);
        }

        let ver_http_resp =
            http_json_request(&base_url, Method::GET, ROUTE_ONEBOT_GET_VERSION_INFO, None).await?;
        if ver_http_resp.status_code != 200
            || ver_http_resp.json()?["data"]["protocol_version"] != "v11"
        {
            bail!(
                "GET /onebot/v11/get_version_info failed: {}",
                ver_http_resp.body
            );
        }

        let ver_reply = call_reverse_action(
            &mut reverse_client.ws_stream,
            ACTION_GET_VERSION_INFO,
            json!({}),
            "st-ver-1",
        )
        .await?;
        if ver_reply["retcode"] != 0 || ver_reply["data"]["protocol_version"] != "v11" {
            bail!("unexpected Reverse WS get_version_info reply: {ver_reply}");
        }

        let group_list_reply = call_reverse_action(
            &mut reverse_client.ws_stream,
            ACTION_GET_GROUP_LIST,
            json!({ "no_cache": false }),
            "st-groups-1",
        )
        .await?;
        if group_list_reply["retcode"] != 0
            || !group_list_reply["data"]
                .as_array()
                .is_some_and(|arr| !arr.is_empty())
        {
            bail!("unexpected Reverse WS get_group_list reply: {group_list_reply}");
        }

        let get_msg_reply = call_reverse_action(
            &mut reverse_client.ws_stream,
            ACTION_GET_MSG,
            json!({ "message_id": sent_msg_id }),
            "st-get-msg-1",
        )
        .await?;
        if get_msg_reply["retcode"] != 0
            || !get_msg_reply["data"]["raw_message"]
                .as_str()
                .is_some_and(|raw| raw.contains("Hello Demo UIA Test"))
        {
            bail!("unexpected Reverse WS get_msg reply for id={sent_msg_id}: {get_msg_reply}");
        }
        println!("[self-test]     verified Reverse WS get_version_info, get_group_list, get_msg");

        println!(
            "[self-test] 8/14 injecting incoming peer message and awaiting Forward + Reverse WS push..."
        );
        mock.inject_incoming_message(SELF_TEST_SMALL_GROUP, "群友A [14:05]: ping");
        await_dual_ws_incoming_message(
            &mut forward_ws,
            &mut reverse_client,
            "group",
            "群友A",
            "ping",
        )
        .await?;
        println!("[self-test]     received OneBot v11 group message push on both Forward & Reverse WS");

        println!(
            "[self-test] 9/14 verifying V3 can_send_image, reply+at+text replay, and base64:// PNG paste..."
        );
        verify_v3_segments_and_image_paste(&base_url, &mock, sent_msg_id).await?;
        println!("[self-test]     verified structured segments and background image paste (mouse_touched=false)");

        println!(
            "[self-test] 10/14 verifying V3 private whitelist 403 guard, get_friend_list, and private WS push..."
        );
        verify_v3_private_chat_and_ws_push(&base_url, &mock, &mut forward_ws, &mut reverse_client)
            .await?;
        println!("[self-test]     verified 403 private guard, friend discovery, and Forward/Reverse private WS push");

        println!(
            "[self-test] 11/14 verifying scheduler REST/OneBot APIs, mark_group_msg_as_read, word slices, and 408/1408 deadline guard..."
        );
        verify_scheduler_and_onebot_flood_apis(&base_url).await?;
        println!("[self-test]     verified scheduler policy, .get_word_slices, mark_group_msg_as_read, and 408/1408 deadline expiry");

        println!("[self-test] 12/14 verifying can_send_video and synthetic PNG POST /api/ocr...");
        verify_video_cap_and_ocr_api(&base_url).await?;
        println!("[self-test]     verified can_send_video yes=true and POST /api/ocr line+box output");

        println!("[self-test] 13/14 verifying security prompt circuit breaker...");
        mock.open_security_dialog(SELF_TEST_SECURITY_TITLE);
        tokio::time::sleep(Duration::from_millis(120)).await;
        let blocked_payload = json!({
            "group": "Demo测试群",
            "text": "should be blocked by breaker"
        });
        let blocked_resp =
            http_json_request(&base_url, Method::POST, ROUTE_API_SEND, Some(&blocked_payload))
                .await?;
        if blocked_resp.status_code < 400 {
            bail!(
                "expected HTTP error when security dialog is open, got {}: {}",
                blocked_resp.status_code,
                blocked_resp.body
            );
        }
        if blocked_resp.json()?["ok"] != false {
            bail!("expected ok=false on security prompt block: {}", blocked_resp.body);
        }

        let ob_status_resp =
            http_json_request(&base_url, Method::GET, ROUTE_ONEBOT_GET_STATUS, None).await?;
        if ob_status_resp.json()?["data"]["good"] != false {
            bail!(
                "expected OneBot get_status good=false after breaker trip: {}",
                ob_status_resp.body
            );
        }

        // 弹窗仍开启时手动重置必须返回 423，关闭后重置返回 200 并写入 denied 与 reset 审计流水
        let denied_reset =
            http_json_request(&base_url, Method::POST, ROUTE_API_SECURITY_RESET, None).await?;
        if denied_reset.status_code != 423 {
            bail!(
                "expected HTTP 423 on POST /api/security/reset while dialog open, got {}: {}",
                denied_reset.status_code,
                denied_reset.body
            );
        }

        mock.close_security_dialog();
        tokio::time::sleep(Duration::from_millis(120)).await;
        let ok_reset =
            http_json_request(&base_url, Method::POST, ROUTE_API_SECURITY_RESET, None).await?;
        if ok_reset.status_code != 200
            || ok_reset.json()?["security_breaker_tripped"] != false
        {
            bail!(
                "expected HTTP 200 with security_breaker_tripped=false after dialog closed, got {}: {}",
                ok_reset.status_code,
                ok_reset.body
            );
        }

        let audit_resp = http_json_request(
            &base_url,
            Method::GET,
            &format!("{ROUTE_API_AUDIT_EVENTS}?limit=50"),
            None,
        )
        .await?;
        if audit_resp.status_code != 200 {
            bail!(
                "GET /api/audit-events failed ({}): {}",
                audit_resp.status_code,
                audit_resp.body
            );
        }
        let audit_arr = audit_resp
            .json()?
            .as_array()
            .cloned()
            .unwrap_or_default();
        let has_denied = audit_arr
            .iter()
            .any(|ev| ev["event_kind"] == "security_breaker_reset_denied");
        let has_reset = audit_arr
            .iter()
            .any(|ev| ev["event_kind"] == "security_breaker_reset");
        if !has_denied || !has_reset {
            bail!(
                "expected both security_breaker_reset_denied and security_breaker_reset in audit events: {}",
                audit_resp.body
            );
        }

        let health_resp =
            http_json_request(&base_url, Method::GET, ROUTE_API_HEALTH, None).await?;
        if health_resp.status_code != 200 || health_resp.json()?["ok"] != true {
            bail!(
                "expected healthy daemon after breaker reset ({}): {}",
                health_resp.status_code,
                health_resp.body
            );
        }
        println!(
            "[self-test]     security breaker tripped (HTTP {}), reset verified (423->200) and audited",
            blocked_resp.status_code
        );

        forward_ws.close().await;
        let _ = reverse_client.ws_stream.send(Message::Close(None)).await;

        println!("[self-test] 14/14 verifying POST /api/shutdown and cleaning up mock window...");
        let shutdown_resp =
            http_json_request(&base_url, Method::POST, ROUTE_API_SHUTDOWN, None).await?;
        if shutdown_resp.status_code != 200 {
            bail!(
                "POST /api/shutdown failed ({}): {}",
                shutdown_resp.status_code,
                shutdown_resp.body
            );
        }
        let shutdown_json = shutdown_resp.json()?;
        if shutdown_json["ok"] != true || shutdown_json["shutting_down"] != true {
            bail!("unexpected POST /api/shutdown payload: {shutdown_json}");
        }

        Ok::<(), anyhow::Error>(())
    }
    .await;

    match test_result {
        Ok(()) => {
            tokio::time::timeout(Duration::from_secs(3), server_handle)
                .await
                .context("timed out waiting for adapter-server graceful shutdown")?
                .context("adapter-server task panicked")?;
            mock.shutdown();
            println!("SELF_TEST_PASSED");
            Ok(())
        }
        Err(err) => {
            server_handle.abort();
            mock.shutdown();
            Err(err)
        }
    }
}
