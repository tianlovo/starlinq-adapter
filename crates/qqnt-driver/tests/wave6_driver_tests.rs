//! Wave v6 驱动层集成测试：覆盖安全熔断受控恢复与后台轮询自愈、深度历史查询及单条转发前置守卫

use std::time::Duration;

use common_types::constants::MOCK_QQ_MAIN_CLASS;
use common_types::{
    AdapterConfig, AdapterError, AdapterEvent, BotDriver, ChatTargetKind, GroupTargetSpec,
    InputMode, InteractionConfig, MessageHistoryFilter, SendRequest, SingleForwardRequest,
    StorageConfig,
};
use mock_qq_win::MockQqServer;
use qqnt_driver::profile::SECURITY_KEYWORDS;
use qqnt_driver::QqntDriver;
use tokio::sync::Mutex;
use uia_core::{enumerate_candidate_windows, find_windows_with_keywords};

static WAVE6_GUI_LOCK: Mutex<()> = Mutex::const_new(());

/// 等待桌面上由其他并发测试创建的 MockQQ 或安全弹窗实例销毁，避免跨测试窗口干扰
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

fn wave6_mock_config() -> AdapterConfig {
    AdapterConfig {
        target: GroupTargetSpec {
            group_name: "Demo测试群".to_string(),
            strict_below_members: Some(10),
            require_member_count_verified: false,
            ..Default::default()
        },
        interaction: InteractionConfig {
            default_mode: InputMode::BackgroundUia,
            allow_focus_fallback: false,
            poll_interval_ms: 80,
            char_delay_min_ms: 1,
            char_delay_max_ms: 3,
            min_send_interval_ms: 40,
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
async fn security_breaker_reset_denies_while_dialog_open_and_resumes_poller_after_close() {
    let _guard = WAVE6_GUI_LOCK.lock().await;
    wait_for_clear_mock_desktop().await;

    let mock = MockQqServer::spawn().expect("spawn mock qq");
    let driver = QqntDriver::new(wave6_mock_config()).expect("create QqntDriver");
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

    // 1. 弹出安全风控弹窗并触发熔断
    mock.open_security_dialog("QQ安全验证");
    let send_err = driver
        .send_group_message(SendRequest {
            group: "Demo测试群".to_string(),
            text: "Should Trip Breaker".to_string(),
            mode: Some(InputMode::BackgroundUia),
            ..Default::default()
        })
        .await
        .expect_err("security prompt must trip breaker");
    assert!(matches!(
        send_err,
        AdapterError::SecurityPromptDetected { .. }
    ));

    let tripped_status = driver.status().await.expect("status while tripped");
    assert!(tripped_status.security_breaker_tripped);

    // 2. 风控弹窗尚未关闭时调用 reset_security_breaker 必须拒绝且写入审计日志
    let reset_denied_err = driver
        .reset_security_breaker()
        .await
        .expect_err("reset must be denied while security dialog remains open");
    assert!(matches!(
        reset_denied_err,
        AdapterError::SecurityPromptDetected { .. }
    ));

    let denied_audits = driver
        .list_audit_events(Some("security_breaker_reset_denied"), 10)
        .await
        .expect("list denied audits");
    assert!(
        !denied_audits.is_empty(),
        "expected security_breaker_reset_denied audit record"
    );

    // 3. 关闭安全弹窗后再复位熔断器，状态清零并自动恢复后台轮询推流
    mock.close_security_dialog();
    let recovered_status = driver
        .reset_security_breaker()
        .await
        .expect("reset should succeed after closing security dialog");
    assert!(!recovered_status.security_breaker_tripped);
    assert!(recovered_status.security_breaker_reason.is_none());

    let reset_audits = driver
        .list_audit_events(Some("security_breaker_reset"), 10)
        .await
        .expect("list reset audits");
    assert!(
        !reset_audits.is_empty(),
        "expected security_breaker_reset audit record"
    );

    // 排水复位前残留事件后再注入入站消息，校验后台轮询已自愈并实时推送 GroupMessage
    while rx.try_recv().is_ok() {}
    mock.inject_incoming_message("Demo测试群 (5)", "群友A [15:10]: breaker recovered ok");

    let recovered_msg = tokio::time::timeout(Duration::from_secs(4), async {
        loop {
            match rx.recv().await {
                Ok(AdapterEvent::GroupMessage(msg)) if msg.content == "breaker recovered ok" => {
                    return msg;
                }
                Ok(_) => continue,
                Err(err) => panic!("broadcast channel error: {err}"),
            }
        }
    })
    .await
    .expect("poller must resume and emit GroupMessage after breaker reset");
    assert_eq!(recovered_msg.sender, "群友A");
    assert!(recovered_msg.onebot_message_id.is_some());

    drop(driver);
    mock.shutdown();
}

#[tokio::test]
async fn query_messages_history_and_forward_single_message_guards() {
    let _guard = WAVE6_GUI_LOCK.lock().await;
    wait_for_clear_mock_desktop().await;

    let mock = MockQqServer::spawn().expect("spawn mock qq");
    let driver = QqntDriver::new(wave6_mock_config()).expect("create QqntDriver");

    driver
        .open_group(GroupTargetSpec {
            group_name: "Demo测试群".to_string(),
            strict_below_members: Some(10),
            require_member_count_verified: false,
            ..Default::default()
        })
        .await
        .expect("open Demo测试群");

    let out1 = driver
        .send_group_message(SendRequest {
            group: "Demo测试群".to_string(),
            text: "Wave6 History Alpha".to_string(),
            mode: Some(InputMode::BackgroundUia),
            ..Default::default()
        })
        .await
        .expect("send Alpha");
    let out2 = driver
        .send_group_message(SendRequest {
            group: "Demo测试群".to_string(),
            text: "Wave6 History Beta".to_string(),
            mode: Some(InputMode::BackgroundUia),
            ..Default::default()
        })
        .await
        .expect("send Beta");
    let id1 = out1.onebot_message_id.expect("id1");
    let id2 = out2.onebot_message_id.expect("id2");

    // 携带群人数后缀的 session_name 需先规范化后查询，并支持关键词与倒序过滤
    let filtered = driver
        .query_messages_history(MessageHistoryFilter {
            session_name: "Demo测试群 (5)".to_string(),
            chat_type: Some(ChatTargetKind::Group),
            keyword: Some("History".to_string()),
            limit: 10,
            reverse_order: true,
            ..Default::default()
        })
        .await
        .expect("query_messages_history");
    assert_eq!(filtered.len(), 2);
    assert_eq!(filtered[0].onebot_message_id, id2);
    assert_eq!(filtered[1].onebot_message_id, id1);

    // 校验 forward_single_message 截止期超时中止与私聊白名单前置拦截
    let expired_err = driver
        .forward_single_message(SingleForwardRequest {
            message_id: id1,
            target: "Demo测试群".to_string(),
            target_kind: ChatTargetKind::Group,
            deadline_unix_ms: Some(1),
        })
        .await
        .expect_err("expired deadline must abort");
    assert!(matches!(
        expired_err,
        AdapterError::ResponseWindowExpired { .. }
    ));
    let tel = driver
        .scheduler_telemetry()
        .await
        .expect("scheduler telemetry");
    assert!(tel.outbound_deadline_aborted_total >= 1);

    let priv_err = driver
        .forward_single_message(SingleForwardRequest {
            message_id: id1,
            target: "未白名单好友".to_string(),
            target_kind: ChatTargetKind::Private,
            deadline_unix_ms: None,
        })
        .await
        .expect_err("non-whitelisted private target must fail");
    assert!(matches!(
        priv_err,
        AdapterError::PrivateTargetNotAllowed { .. }
    ));

    let missing_err = driver
        .forward_single_message(SingleForwardRequest {
            message_id: 999_999_999,
            target: "Demo测试群".to_string(),
            target_kind: ChatTargetKind::Group,
            deadline_unix_ms: None,
        })
        .await
        .expect_err("missing message_id must fail");
    assert!(matches!(missing_err, AdapterError::InputRejected(_)));

    drop(driver);
    mock.shutdown();
}

#[tokio::test]
async fn live_bug_v6_002_timestamp_anchor_and_same_text_outbound_keep_distinct_ids() {
    let _guard = WAVE6_GUI_LOCK.lock().await;
    wait_for_clear_mock_desktop().await;

    let same_text = "WAVE6-DEDUP-SAME-BODY";
    let mock = MockQqServer::spawn_with_groups(vec![(
        "Demo测试群 (5)".to_string(),
        vec![format!("BotSelf [13:57]: {same_text}")],
    )])
    .expect("spawn mock qq with 13:57 anchor");
    let driver = QqntDriver::new(wave6_mock_config()).expect("create QqntDriver");

    driver
        .open_group(GroupTargetSpec {
            group_name: "Demo测试群".to_string(),
            strict_below_members: Some(10),
            require_member_count_verified: false,
            ..Default::default()
        })
        .await
        .expect("open Demo测试群");

    let initial = driver
        .query_messages_history(MessageHistoryFilter {
            session_name: "Demo测试群".to_string(),
            chat_type: Some(ChatTargetKind::Group),
            keyword: Some(same_text.to_string()),
            limit: 10,
            ..Default::default()
        })
        .await
        .expect("query initial anchor");
    assert_eq!(initial.len(), 1);
    let anchor_id = initial[0].onebot_message_id;
    assert_eq!(initial[0].timestamp_hint.as_deref(), Some("13:57"));

    // 模拟同分钟内无时间头同文新气泡入列并经由 SyncAndPoll 持久化，不得倒灌合并进 13:57 锚点
    mock.inject_incoming_message("Demo测试群 (5)", &format!("BotSelf: {same_text}"));
    let mut after_none_ts = Vec::new();
    for _ in 0..10 {
        let _ = driver.poll_messages("Demo测试群", 20).await;
        after_none_ts = driver
            .query_messages_history(MessageHistoryFilter {
                session_name: "Demo测试群".to_string(),
                chat_type: Some(ChatTargetKind::Group),
                keyword: Some(same_text.to_string()),
                limit: 10,
                ..Default::default()
            })
            .await
            .expect("query after none-ts bubble");
        if after_none_ts.len() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(60)).await;
    }
    assert_eq!(after_none_ts.len(), 2);
    assert_eq!(after_none_ts[0].onebot_message_id, anchor_id);
    let none_ts_id = after_none_ts[1].onebot_message_id;
    assert_ne!(none_ts_id, anchor_id);
    assert_eq!(after_none_ts[1].timestamp_hint, None);

    // 再次轮询对账必须严格维持 2 条且编号稳定
    let _ = driver
        .poll_messages("Demo测试群", 20)
        .await
        .expect("re-poll stability");
    let stable_two = driver
        .query_messages_history(MessageHistoryFilter {
            session_name: "Demo测试群".to_string(),
            chat_type: Some(ChatTargetKind::Group),
            keyword: Some(same_text.to_string()),
            limit: 10,
            ..Default::default()
        })
        .await
        .expect("query stable two");
    assert_eq!(stable_two.len(), 2);

    // 继续通过已确认出站通道发送同文消息，必须分配第 3 个全新编号且轮询后零重复
    let out = driver
        .send_group_message(SendRequest {
            group: "Demo测试群".to_string(),
            text: same_text.to_string(),
            mode: Some(InputMode::BackgroundUia),
            ..Default::default()
        })
        .await
        .expect("send same text outbound");
    let outbound_id = out.onebot_message_id.expect("outbound id");
    assert_ne!(outbound_id, anchor_id);
    assert_ne!(outbound_id, none_ts_id);

    let _ = driver
        .poll_messages("Demo测试群", 20)
        .await
        .expect("poll after outbound");
    let final_rows = driver
        .query_messages_history(MessageHistoryFilter {
            session_name: "Demo测试群".to_string(),
            chat_type: Some(ChatTargetKind::Group),
            keyword: Some(same_text.to_string()),
            limit: 10,
            ..Default::default()
        })
        .await
        .expect("query final rows");
    assert_eq!(final_rows.len(), 3);
    assert_eq!(
        final_rows
            .iter()
            .map(|r| r.onebot_message_id)
            .collect::<Vec<_>>(),
        vec![anchor_id, none_ts_id, outbound_id]
    );

    drop(driver);
    mock.shutdown();
}
