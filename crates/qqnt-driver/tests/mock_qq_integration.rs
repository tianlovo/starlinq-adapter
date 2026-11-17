//! 基于 MockQQ 桌面仿真窗口的驱动集成测试：验证人数护栏、后台发送、风控熔断及 V2 OneBot 持久化链路

use std::time::Duration;

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use common_types::constants::MOCK_QQ_MAIN_CLASS;
use common_types::{
    AdapterConfig, AdapterError, AdapterEvent, BotDriver, ChatTargetKind, GroupTargetSpec,
    InputMode, InteractionConfig, MessageSegment, QqClientConfig, SendRequest, StorageConfig,
};
use mock_qq_win::constants::SELF_TEST_PRIVATE_FRIEND;
use mock_qq_win::MockQqServer;
use qqnt_driver::profile::SECURITY_KEYWORDS;
use qqnt_driver::QqntDriver;
use tokio::sync::Mutex;
use uia_core::{
    decode_image_bytes_to_bgra, encode_bgra_to_png_bytes, enumerate_candidate_windows,
    find_windows_with_keywords, DecodedImageBgra,
};

static GUI_LOCK: Mutex<()> = Mutex::const_new(());

/// 等待桌面上由其他并发用例创建的 MockQQ 或安全弹窗窗口彻底销毁，避免彼此干扰
async fn wait_for_clear_mock_desktop() {
    for _ in 0..80 {
        let foreign_mock = enumerate_candidate_windows(&[], &[MOCK_QQ_MAIN_CLASS.to_string()])
            .into_iter()
            .any(|win| win.class_name == MOCK_QQ_MAIN_CLASS && win.pid != std::process::id());
        let foreign_security = !find_windows_with_keywords(&[], SECURITY_KEYWORDS).is_empty();
        if !foreign_mock && !foreign_security {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// 构造隔离的集成测试配置（强制绑定内存 SQLite `:memory:` 且放行 Mock 窗口类）
fn mock_driver_config() -> AdapterConfig {
    AdapterConfig {
        target: GroupTargetSpec {
            group_name: "Demo测试群".to_string(),
            strict_below_members: Some(10),
            require_member_count_verified: false,
            ..Default::default()
        },
        interaction: InteractionConfig {
            default_mode: InputMode::Auto,
            allow_focus_fallback: false,
            poll_interval_ms: 120,
            char_delay_min_ms: 2,
            char_delay_max_ms: 5,
            min_send_interval_ms: 80,
            extra_window_classes: vec![MOCK_QQ_MAIN_CLASS.to_string()],
            ..Default::default()
        },
        storage: StorageConfig {
            db_path: ":memory:".to_string(),
        },
        ..Default::default()
    }
}

#[tokio::test]
async fn member_guard_skips_26_and_opens_5() {
    let _guard = GUI_LOCK.lock().await;
    wait_for_clear_mock_desktop().await;

    let mock = MockQqServer::spawn().expect("spawn mock qq");
    let driver = QqntDriver::new(mock_driver_config()).expect("create QqntDriver");

    let sessions = driver.list_sessions().await.expect("list_sessions");
    assert!(sessions.iter().any(|s| s.title.contains("Demo测试群 (26)")));
    assert!(sessions.iter().any(|s| s.title.contains("Demo测试群 (5)")));

    let status = driver
        .open_group(GroupTargetSpec {
            group_name: "Demo测试群".to_string(),
            strict_below_members: Some(10),
            require_member_count_verified: true,
            ..Default::default()
        })
        .await
        .expect("open Demo测试群 (5)");

    assert_eq!(status.active_group.as_deref(), Some("Demo测试群 (5)"));
    assert_eq!(status.detected_member_count, Some(5));
    assert_eq!(mock.active_group(), "Demo测试群 (5)");

    // 更严格的人数阈值（< 5）必须同时拦截 26 人群与 5 人群并返回 MemberCountExceeded
    let err = driver
        .open_group(GroupTargetSpec {
            group_name: "Demo测试群".to_string(),
            strict_below_members: Some(5),
            require_member_count_verified: true,
            ..Default::default()
        })
        .await
        .expect_err("limit < 5 should reject Demo测试群 (5) and (26)");

    assert!(
        matches!(err, AdapterError::MemberCountExceeded { limit: 5, .. }),
        "expected MemberCountExceeded, got {err:?}"
    );

    drop(driver);
    mock.shutdown();
}

#[tokio::test]
async fn background_send_updates_chat_without_mouse_or_foreground() {
    let _guard = GUI_LOCK.lock().await;
    wait_for_clear_mock_desktop().await;

    let mock = MockQqServer::spawn().expect("spawn mock qq");
    let driver = QqntDriver::new(mock_driver_config()).expect("create QqntDriver");

    driver
        .open_group(GroupTargetSpec {
            group_name: "Demo测试群".to_string(),
            strict_below_members: Some(10),
            require_member_count_verified: true,
            ..Default::default()
        })
        .await
        .expect("open Demo测试群");

    let outcome = driver
        .send_group_message(SendRequest {
            group: "Demo测试群".to_string(),
            text: "Hello Background Driver".to_string(),
            mode: Some(InputMode::BackgroundUia),
            ..Default::default()
        })
        .await
        .expect("background send should succeed");

    assert!(outcome.ok);
    assert_eq!(outcome.used_mode, InputMode::BackgroundUia);
    assert!(!outcome.mouse_touched, "mouse must never be touched");
    assert!(
        !outcome.foreground_touched,
        "background UIA must not touch foreground focus"
    );
    assert!(
        outcome.verified_in_chat,
        "message should be verified in chat"
    );
    let sent_msg_id = outcome
        .onebot_message_id
        .expect("verified sent outcome must carry persisted onebot_message_id");
    let sent_group_id = outcome
        .group_id
        .expect("verified sent outcome must carry group_id");

    let stored = driver
        .get_message(sent_msg_id)
        .await
        .expect("get_message")
        .expect("sent message must be queryable by onebot_message_id");
    assert_eq!(stored.raw_message, "Hello Background Driver");
    assert_eq!(stored.group_id, sent_group_id);
    assert!(stored.is_self);

    let resolved = driver
        .resolve_group_name(sent_group_id)
        .await
        .expect("resolve_group_name");
    assert_eq!(resolved.as_deref(), Some("Demo测试群"));

    let history = mock.messages_for("Demo测试群 (5)");
    assert!(
        history
            .iter()
            .any(|line| line == "BotSelf [now]: Hello Background Driver"),
        "expected sent line in mock history, got: {history:?}"
    );

    drop(driver);
    mock.shutdown();
}

#[tokio::test]
async fn message_diffing_emits_incoming_and_marks_self_echoes() {
    let _guard = GUI_LOCK.lock().await;
    wait_for_clear_mock_desktop().await;

    let mock = MockQqServer::spawn().expect("spawn mock qq");
    let driver = QqntDriver::new(mock_driver_config()).expect("create QqntDriver");
    let mut rx = driver.subscribe_events();

    driver
        .open_group(GroupTargetSpec {
            group_name: "Demo测试群".to_string(),
            strict_below_members: Some(10),
            require_member_count_verified: false,
            ..Default::default()
        })
        .await
        .expect("open Demo测试群");

    // 主动发出的消息必须被标记为 `is_self: true`，且绝对不得触发 `GroupMessage` 事件
    driver
        .send_group_message(SendRequest {
            group: "Demo测试群".to_string(),
            text: "Self Echo Check".to_string(),
            mode: None,
            ..Default::default()
        })
        .await
        .expect("send self message");

    // 等待超过一个后台轮询周期后校验期间未出现任何误报的回环或历史事件
    tokio::time::sleep(Duration::from_millis(220)).await;
    while let Ok(ev) = rx.try_recv() {
        if let AdapterEvent::GroupMessage(msg) = ev {
            panic!("unexpected GroupMessage for baseline/self message: {msg:?}");
        }
    }

    // 注入一条来自外部群成员 `群友A` 的新消息
    mock.inject_incoming_message("Demo测试群 (5)", "群友A [14:05]: ping");

    let incoming = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match rx.recv().await {
                Ok(AdapterEvent::GroupMessage(msg)) => return msg,
                Ok(_) => continue,
                Err(err) => panic!("broadcast channel error: {err}"),
            }
        }
    })
    .await
    .expect("timed out waiting for GroupMessage event");

    assert_eq!(incoming.sender, "群友A");
    assert_eq!(incoming.content, "ping");
    assert!(!incoming.is_self);
    assert!(incoming.onebot_message_id.is_some());
    assert!(incoming.group_id.is_some());
    assert!(incoming.user_id.is_some());

    let polled = driver
        .poll_messages("Demo测试群", 20)
        .await
        .expect("poll_messages");
    let self_msg = polled
        .iter()
        .find(|m| m.content == "Self Echo Check")
        .expect("self message present in polled list");
    assert!(
        self_msg.is_self,
        "BotSelf/outbound message must be marked is_self=true"
    );

    let friend_msg = polled
        .iter()
        .find(|m| m.content == "ping" && m.sender == "群友A")
        .expect("incoming message present in polled list");
    assert!(!friend_msg.is_self);
    assert!(friend_msg.onebot_message_id.is_some());

    let login = driver.get_login_info().await.expect("get_login_info");
    assert!(!login.nickname.is_empty());

    let group_list = driver.get_group_list(false).await.expect("get_group_list");
    assert!(group_list
        .iter()
        .any(|group| group.group_name == "Demo测试群"));

    let group_info = driver
        .get_group_info(None, Some("Demo测试群".to_string()), false)
        .await
        .expect("get_group_info");
    assert_eq!(group_info.group_name, "Demo测试群");
    assert_eq!(group_info.member_count, 5);

    let _members = driver
        .get_group_member_list(Some(group_info.group_id), None, false)
        .await
        .expect("get_group_member_list");

    let stats = driver.storage_stats().await.expect("storage_stats");
    assert!(stats.group_count >= 1);
    assert!(stats.message_count >= 1);

    drop(driver);
    mock.shutdown();
}

#[tokio::test]
async fn security_dialog_trips_circuit_breaker() {
    let _guard = GUI_LOCK.lock().await;
    wait_for_clear_mock_desktop().await;

    let mock = MockQqServer::spawn().expect("spawn mock qq");
    let driver = QqntDriver::new(mock_driver_config()).expect("create QqntDriver");
    let mut rx = driver.subscribe_events();

    driver
        .open_group(GroupTargetSpec {
            group_name: "Demo测试群".to_string(),
            strict_below_members: Some(10),
            require_member_count_verified: false,
            ..Default::default()
        })
        .await
        .expect("open Demo测试群");

    mock.open_security_dialog("QQ安全验证");

    let err = driver
        .send_group_message(SendRequest {
            group: "Demo测试群".to_string(),
            text: "Should Be Blocked".to_string(),
            mode: None,
            ..Default::default()
        })
        .await
        .expect_err("security prompt must trip circuit breaker");

    assert!(
        matches!(err, AdapterError::SecurityPromptDetected { .. }),
        "expected SecurityPromptDetected, got {err:?}"
    );

    let alert_received = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match rx.recv().await {
                Ok(AdapterEvent::SecurityAlert { title, .. }) => return title,
                Ok(_) => continue,
                Err(err) => panic!("broadcast error: {err}"),
            }
        }
    })
    .await
    .expect("timed out waiting for SecurityAlert event");
    assert!(alert_received.contains("安全验证") || alert_received == "SecurityBreakerTripped");

    let status = driver.status().await.expect("status");
    assert!(status.security_breaker_tripped);
    assert!(status.security_breaker_reason.is_some());

    mock.close_security_dialog();
    drop(driver);
    mock.shutdown();
}

#[tokio::test]
async fn ensure_qq_window_and_status_report_resolved_path_without_launching_real_qq() {
    let _guard = GUI_LOCK.lock().await;
    wait_for_clear_mock_desktop().await;

    let temp_exe = std::env::temp_dir().join(format!(
        "qqnt_driver_integration_stub_{}.exe",
        std::process::id()
    ));
    std::fs::write(&temp_exe, b"stub").expect("write temp exe");
    let stub_path = temp_exe.to_string_lossy().to_string();

    let mock = MockQqServer::spawn().expect("spawn mock qq");
    let mut cfg = mock_driver_config();
    cfg.qq_client = QqClientConfig {
        executable_path: Some(stub_path.clone()),
        auto_launch_or_wake: true,
        launch_args: vec!["--force-renderer-accessibility".to_string()],
        startup_timeout_ms: 1000,
    };

    let driver = QqntDriver::new(cfg).expect("create QqntDriver");
    let status = driver.status().await.expect("status");
    assert_eq!(status.resolved_qq_path.as_deref(), Some(stub_path.as_str()));

    let ensured = driver.ensure_qq_window().await.expect("ensure_qq_window");
    assert!(
        ensured
            .iter()
            .any(|win| win.hwnd == mock.hwnd() && win.class_name == MOCK_QQ_MAIN_CLASS),
        "expected MockQQNTWndClass window from ensure_qq_window, got: {ensured:?}"
    );

    drop(driver);
    mock.shutdown();
    let _ = std::fs::remove_file(&temp_exe);

    // 当设置了 extra_window_classes 且不存在 Mock 窗口时，即使启用了自动拉起且路径无效也不得启动真实 QQ
    let mut no_launch_cfg = mock_driver_config();
    no_launch_cfg.qq_client = QqClientConfig {
        executable_path: Some(r"Z:\nonexistent\QQ.exe".to_string()),
        auto_launch_or_wake: true,
        launch_args: vec![],
        startup_timeout_ms: 500,
    };
    let no_launch_driver = QqntDriver::new(no_launch_cfg).expect("create no-launch driver");
    let windows = no_launch_driver
        .ensure_qq_window()
        .await
        .expect("ensure_qq_window should skip real QQ launch when extra_window_classes is set");
    assert!(
        windows
            .iter()
            .all(|win| win.class_name != MOCK_QQ_MAIN_CLASS || win.pid != std::process::id()),
        "no in-process mock window should remain after shutdown"
    );
    drop(no_launch_driver);
}

#[tokio::test]
async fn v3_private_guard_and_image_paste_work_against_mock_qq() {
    let _guard = GUI_LOCK.lock().await;
    wait_for_clear_mock_desktop().await;

    let mock = MockQqServer::spawn().expect("spawn mock qq");

    // 1. 默认空白名单状态下，任何私聊发送必须被安全拦截并返回 PrivateTargetNotAllowed
    let locked_driver = QqntDriver::new(mock_driver_config()).expect("create locked driver");
    let blocked_err = locked_driver
        .send_private_message(SendRequest {
            group: SELF_TEST_PRIVATE_FRIEND.to_string(),
            text: "应当被拦截的私聊".to_string(),
            target_kind: ChatTargetKind::Private,
            ..Default::default()
        })
        .await
        .expect_err("empty private whitelist must reject send_private_message");
    assert!(
        matches!(blocked_err, AdapterError::PrivateTargetNotAllowed { .. }),
        "expected PrivateTargetNotAllowed, got: {blocked_err:?}"
    );
    drop(locked_driver);

    // 2. 显式将测试好友加入白名单后，验证 get_friend_list、私聊发信与 PrivateMessage 推送
    let mut allowed_cfg = mock_driver_config();
    allowed_cfg.target.allowed_private_friends = vec![SELF_TEST_PRIVATE_FRIEND.to_string()];
    let driver = QqntDriver::new(allowed_cfg).expect("create whitelisted driver");
    let mut rx = driver.subscribe_events();

    let friends = driver.get_friend_list(true).await.expect("get_friend_list");
    assert!(
        friends
            .iter()
            .any(|friend| friend.nickname == SELF_TEST_PRIVATE_FRIEND),
        "expected {SELF_TEST_PRIVATE_FRIEND} in friend list, got: {friends:?}"
    );

    let private_outcome = driver
        .send_private_message(SendRequest {
            group: SELF_TEST_PRIVATE_FRIEND.to_string(),
            text: "Hello V3 Private Mock".to_string(),
            mode: Some(InputMode::BackgroundUia),
            target_kind: ChatTargetKind::Private,
            ..Default::default()
        })
        .await
        .expect("whitelisted private send should succeed");
    assert!(private_outcome.ok);
    assert_eq!(private_outcome.target_kind, ChatTargetKind::Private);
    assert!(!private_outcome.mouse_touched);
    assert!(private_outcome.user_id.is_some());

    mock.inject_incoming_message(SELF_TEST_PRIVATE_FRIEND, "测试好友 [14:06]: 收到 V3 私聊");
    let incoming_private = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match rx.recv().await {
                Ok(AdapterEvent::PrivateMessage(msg)) => return msg,
                Ok(_) => continue,
                Err(err) => panic!("broadcast error: {err}"),
            }
        }
    })
    .await
    .expect("timed out waiting for PrivateMessage event");
    assert_eq!(incoming_private.target_kind, ChatTargetKind::Private);
    assert_eq!(incoming_private.sender, "测试好友");
    assert_eq!(incoming_private.content, "收到 V3 私聊");

    // 3. 切回小群并通过 base64:// PNG 验证图文剪贴板粘贴发送与零鼠标碰触
    let _ = driver
        .open_group(GroupTargetSpec {
            group_name: "Demo测试群".to_string(),
            strict_below_members: Some(10),
            require_member_count_verified: true,
            ..Default::default()
        })
        .await
        .expect("open Demo测试群");

    let png_bytes = encode_bgra_to_png_bytes(&DecodedImageBgra {
        width: 1,
        height: 1,
        bgra_top_down: vec![0, 128, 255, 255],
    })
    .expect("encode 1x1 png");
    let _ = decode_image_bytes_to_bgra(&png_bytes).expect("sanity check encoded png");
    let b64_src = format!("base64://{}", STANDARD.encode(&png_bytes));

    let img_outcome = driver
        .send_group_message(SendRequest {
            group: "Demo测试群".to_string(),
            text: String::new(),
            mode: Some(InputMode::BackgroundUia),
            target_kind: ChatTargetKind::Group,
            segments: vec![
                MessageSegment::Text {
                    text: "V3图".to_string(),
                },
                MessageSegment::Image {
                    file: b64_src,
                    url: None,
                    summary: None,
                    image_type: None,
                },
            ],
            ..Default::default()
        })
        .await
        .expect("image paste send should succeed");
    assert!(img_outcome.ok, "image outcome: {img_outcome:?}");
    assert!(!img_outcome.mouse_touched);
    assert!(img_outcome.verified_in_chat);

    let history = mock.messages_for("Demo测试群 (5)");
    assert!(
        history
            .iter()
            .any(|line| line.contains("V3图") && line.contains("[图片]")),
        "expected pasted [图片] line in mock history, got: {history:?}"
    );

    drop(driver);
    mock.shutdown();
}
