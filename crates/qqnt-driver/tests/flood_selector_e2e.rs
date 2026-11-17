//! Win32 MockQQ 端到端洪峰调度与多超大群抢占集成测试：覆盖常规无损、单群洪峰 P2 限流与滞回恢复、三超大群并发抢占收割

use std::collections::HashSet;
use std::time::Duration;

use common_types::constants::MOCK_QQ_MAIN_CLASS;
use common_types::{
    AdapterConfig, AdapterError, AdapterEvent, BotConfig, BotDriver, BurstModeState,
    ChatTargetKind, GroupTargetSpec, InputMode, InteractionConfig, MessagePriorityTier,
    SchedulerConfig, SendRequest, StorageConfig,
};
use mock_qq_win::MockQqServer;
use qqnt_driver::parser::canonical_group_name;
use qqnt_driver::profile::SECURITY_KEYWORDS;
use qqnt_driver::QqntDriver;
use tokio::sync::Mutex;
use uia_core::{enumerate_candidate_windows, find_windows_with_keywords};

static GUI_LOCK: Mutex<()> = Mutex::const_new(());

/// 等待桌面上其他集成测试拉起的 MockQQ 或安全弹窗实例彻底退出，防止窗口标题混淆
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

/// 构造支持受监控超大群列表及自定义调度策略的 Mock 测试配置
fn flood_test_config(monitored_groups: Vec<String>, scheduler: SchedulerConfig) -> AdapterConfig {
    let primary = monitored_groups
        .first()
        .cloned()
        .unwrap_or_else(|| "大群Alpha".to_string());
    AdapterConfig {
        target: GroupTargetSpec {
            group_name: primary,
            strict_below_members: Some(10),
            require_member_count_verified: false,
            monitored_groups,
            allow_large_monitored_groups: true,
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
        bot: BotConfig {
            self_id: 2888001,
            self_nicknames: vec!["qq-bot".to_string(), "测试机器人".to_string()],
            auto_ping_reply: false,
        },
        scheduler,
        ..Default::default()
    }
}

#[tokio::test]
async fn e2e_normal_traffic_lossless_in_large_monitored_group() {
    let _guard = GUI_LOCK.lock().await;
    wait_for_clear_mock_desktop().await;

    let groups = vec![(
        "大群Alpha (520)".to_string(),
        vec!["群主 [11:59]: 初始基线".to_string()],
    )];
    let mock = MockQqServer::spawn_with_groups(groups).expect("spawn mock qq");

    let cfg = flood_test_config(
        vec!["大群Alpha".to_string()],
        SchedulerConfig {
            burst_window_ms: 3000,
            burst_enter_threshold: 16,
            burst_exit_threshold: 6,
            single_poll_burst_threshold: 12,
            ..Default::default()
        },
    );
    let driver = QqntDriver::new(cfg).expect("create QqntDriver");
    let mut rx = driver.subscribe_events();

    driver
        .open_group(GroupTargetSpec {
            group_name: "大群Alpha".to_string(),
            strict_below_members: None,
            monitored_groups: vec!["大群Alpha".to_string()],
            allow_large_monitored_groups: true,
            ..Default::default()
        })
        .await
        .expect("open monitored large group");

    let batch: Vec<String> = (1..=8)
        .map(|i| format!("群友{i} [12:0{i}]: 常规闲聊第 {i} 条"))
        .collect();
    mock.inject_incoming_batch("大群Alpha (520)", &batch);

    let mut observed = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(4);
    while observed.len() < 8 && tokio::time::Instant::now() < deadline {
        if let Ok(Ok(AdapterEvent::GroupMessage(msg))) =
            tokio::time::timeout(Duration::from_millis(300), rx.recv()).await
        {
            observed.push(msg);
        }
    }

    assert_eq!(observed.len(), 8, "正常流量 8 条必须 100% 无损投递");
    let tel = driver.scheduler_telemetry().await.expect("telemetry");
    assert_eq!(tel.burst_state, BurstModeState::Normal);
    assert_eq!(tel.selective_shed_p2_total, 0);

    // 同配置下严格限制未受监控且超编的无关大群仍被安全护栏拦截
    let unmonitored_err = driver
        .open_group(GroupTargetSpec {
            group_name: "大群Alpha".to_string(),
            strict_below_members: Some(10),
            require_member_count_verified: false,
            ..Default::default()
        })
        .await
        .expect_err("unmonitored explicit strict guard should block 520-member group");
    assert!(matches!(
        unmonitored_err,
        AdapterError::MemberCountExceeded { .. }
    ));

    // 同时验证已过期的出站截止期被零 UIA 接触直接拦截
    let expired_err = driver
        .send_group_message(SendRequest {
            group: "大群Alpha".to_string(),
            text: "已过期发送".to_string(),
            deadline_unix_ms: Some(1),
            ..Default::default()
        })
        .await
        .expect_err("expired outbound send must abort cleanly");
    assert!(matches!(
        expired_err,
        AdapterError::ResponseWindowExpired { .. }
    ));
    assert_eq!(
        driver
            .scheduler_telemetry()
            .await
            .expect("telemetry after abort")
            .outbound_deadline_aborted_total,
        1
    );

    drop(driver);
    mock.shutdown();
}

#[tokio::test]
async fn e2e_single_group_burst_sheds_p2_delivers_all_p0_and_recovers() {
    let _guard = GUI_LOCK.lock().await;
    wait_for_clear_mock_desktop().await;

    let groups = vec![(
        "大群Alpha (520)".to_string(),
        vec!["群主 [11:59]: 初始基线".to_string()],
    )];
    let mock = MockQqServer::spawn_with_groups(groups).expect("spawn mock qq");

    let cfg = flood_test_config(
        vec!["大群Alpha".to_string()],
        SchedulerConfig {
            burst_window_ms: 900,
            burst_enter_threshold: 16,
            burst_exit_threshold: 6,
            single_poll_burst_threshold: 12,
            burst_ordinary_sample_stride: 5,
            hard_shed_queue_depth: 64,
            ..Default::default()
        },
    );
    let driver = QqntDriver::new(cfg).expect("create QqntDriver");
    let mut rx = driver.subscribe_events();

    driver
        .open_group(GroupTargetSpec {
            group_name: "大群Alpha".to_string(),
            monitored_groups: vec!["大群Alpha".to_string()],
            allow_large_monitored_groups: true,
            ..Default::default()
        })
        .await
        .expect("open 大群Alpha");

    let mut flood_batch = Vec::with_capacity(104);
    for idx in 0..50 {
        flood_batch.push(format!("水友{idx} [12:01]: 洪峰刷屏消息 #{idx}"));
    }
    flood_batch.push("关键人A [12:01]: @qq-bot 报数一".to_string());
    flood_batch.push("关键人B [12:01]: /help 查指令".to_string());
    for idx in 50..100 {
        flood_batch.push(format!("水友{idx} [12:02]: 洪峰刷屏消息 #{idx}"));
    }
    flood_batch.push("关键人C [12:02]: @qq-bot 状态检查".to_string());
    flood_batch.push("关键人D [12:02]: /ping 测试".to_string());

    mock.inject_incoming_batch("大群Alpha (520)", &flood_batch);

    let mut p0_contents = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(6);
    while p0_contents.len() < 4 && tokio::time::Instant::now() < deadline {
        if let Ok(Ok(AdapterEvent::GroupMessage(msg))) =
            tokio::time::timeout(Duration::from_millis(250), rx.recv()).await
        {
            if msg.priority_tier == Some(MessagePriorityTier::CriticalBotTrigger) {
                p0_contents.push(msg.content);
            }
        }
    }

    assert_eq!(
        p0_contents.len(),
        4,
        "100 条洪峰期间 4 条 P0 触发消息必须全部保活送达，实际收到: {p0_contents:?}"
    );
    let mid_tel = driver.scheduler_telemetry().await.expect("mid telemetry");
    assert_eq!(mid_tel.burst_state, BurstModeState::BurstActive);
    assert!(mid_tel.selective_shed_p2_total > 0);
    assert_eq!(mid_tel.dispatched_p0_total, 4);

    // 冷却超过 burst_window_ms(900ms) 后再次检测状态应滞回恢复至 Normal
    tokio::time::sleep(Duration::from_millis(1050)).await;
    let recovered = driver
        .scheduler_telemetry()
        .await
        .expect("recovered telemetry");
    assert_eq!(recovered.burst_state, BurstModeState::Normal);

    drop(driver);
    mock.shutdown();
}

#[tokio::test]
async fn e2e_multi_large_group_flood_harvests_all_at_triggers() {
    let _guard = GUI_LOCK.lock().await;
    wait_for_clear_mock_desktop().await;

    let groups = vec![
        (
            "大群Alpha (520)".to_string(),
            vec!["管理A [11:50]: Alpha 基线".to_string()],
        ),
        (
            "大群Beta (680)".to_string(),
            vec!["管理B [11:51]: Beta 基线".to_string()],
        ),
        (
            "大群Gamma (840)".to_string(),
            vec!["管理C [11:52]: Gamma 基线".to_string()],
        ),
    ];
    let mock = MockQqServer::spawn_with_groups(groups).expect("spawn multi-group mock");

    let monitored = vec![
        "大群Alpha".to_string(),
        "大群Beta".to_string(),
        "大群Gamma".to_string(),
    ];
    let cfg = flood_test_config(
        monitored.clone(),
        SchedulerConfig {
            burst_window_ms: 3000,
            burst_enter_threshold: 16,
            burst_exit_threshold: 6,
            single_poll_burst_threshold: 12,
            burst_ordinary_sample_stride: 4,
            urgent_preempt_dwell_ms: 250,
            session_dwell_min_ms: 600,
            patrol_interval_ms: 1000,
            ..Default::default()
        },
    );
    let driver = QqntDriver::new(cfg).expect("create QqntDriver");
    let mut rx = driver.subscribe_events();

    driver
        .open_group(GroupTargetSpec {
            group_name: "大群Alpha".to_string(),
            monitored_groups: monitored,
            allow_large_monitored_groups: true,
            ..Default::default()
        })
        .await
        .expect("attach to 大群Alpha");

    let mut alpha_msgs: Vec<String> = (0..18)
        .map(|i| format!("A路人{i} [12:05]: Alpha洪峰 #{i}"))
        .collect();
    alpha_msgs.push("A触发 [12:05]: @qq-bot Alpha报告".to_string());

    let mut beta_msgs: Vec<String> = (0..20)
        .map(|i| format!("B路人{i} [12:05]: Beta洪峰 #{i}"))
        .collect();
    beta_msgs.push("B触发 [12:05]: @qq-bot Beta紧急求助".to_string());

    let mut gamma_msgs: Vec<String> = (0..20)
        .map(|i| format!("C路人{i} [12:05]: Gamma洪峰 #{i}"))
        .collect();
    gamma_msgs.push("C触发 [12:05]: @qq-bot Gamma立即排查".to_string());

    // 先对后台 Beta 与 Gamma 注入带 @qq-bot 的洪峰未读，同时继续在已附着 Alpha 刷屏
    mock.inject_incoming_batch("大群Beta (680)", &beta_msgs);
    mock.inject_incoming_batch("大群Gamma (840)", &gamma_msgs);
    mock.inject_incoming_batch("大群Alpha (520)", &alpha_msgs);

    assert!(mock.unread_for("大群Beta (680)") > 0);
    assert!(mock.unread_for("大群Gamma (840)") > 0);

    let mut p0_groups = HashSet::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while p0_groups.len() < 3 && tokio::time::Instant::now() < deadline {
        if let Ok(Ok(AdapterEvent::GroupMessage(msg))) =
            tokio::time::timeout(Duration::from_millis(250), rx.recv()).await
        {
            if msg.priority_tier == Some(MessagePriorityTier::CriticalBotTrigger) {
                p0_groups.insert(canonical_group_name(&msg.group_name));
            }
        }
    }

    assert!(
        p0_groups.contains("大群Alpha")
            && p0_groups.contains("大群Beta")
            && p0_groups.contains("大群Gamma"),
        "三群并发洪峰中必须收割到所有群的 @qq-bot P0 触发消息，实际命中群: {p0_groups:?}"
    );

    let tel = driver.scheduler_telemetry().await.expect("multi telemetry");
    assert!(
        tel.multi_group_switches_total >= 2,
        "多群抢占切换次数应至少为 2，实际为 {}",
        tel.multi_group_switches_total
    );

    driver
        .mark_session_read("大群Beta", ChatTargetKind::Group)
        .await
        .expect("mark_session_read succeeds");

    drop(driver);
    mock.shutdown();
}
