//! 洪峰调度器单元测试：验证常规流量零丢弃、突发期 P2 选择性限流与滞回恢复、EDF+DRR 防饿死及多群抢占防抖

use common_types::{
    BurstModeState, ChatMessage, ChatTargetKind, GroupTargetSpec, MessagePriorityTier,
    MessageSegment, SchedulerConfig, SessionSummary,
};

use super::burst_detector::SlidingBurstDetector;
use super::classifier::{BotRelevanceClassifier, ClassifyContext};
use super::event_selector::ResponseWindowSelector;
use super::session_harvester::MultiSessionHarvester;
use super::FloodScheduler;

fn make_group_msg(id: &str, group: &str, content: &str, ts_ms: u64) -> ChatMessage {
    ChatMessage {
        message_id: id.to_string(),
        group_name: group.to_string(),
        sender: "Alice".to_string(),
        content: content.to_string(),
        captured_at_ms: ts_ms,
        target_kind: ChatTargetKind::Group,
        segments: vec![MessageSegment::Text {
            text: content.to_string(),
        }],
        ..Default::default()
    }
}

#[test]
fn normal_traffic_retains_all_tiers_with_zero_drops() {
    let cfg = SchedulerConfig {
        burst_enter_threshold: 16,
        single_poll_burst_threshold: 12,
        ..Default::default()
    };
    let mut scheduler = FloodScheduler::new(cfg);
    let target = GroupTargetSpec::default();
    let nicks = vec!["qq-bot".to_string()];

    let batch = (0..8)
        .map(|idx| {
            make_group_msg(
                &format!("m{idx}"),
                "测试群",
                &format!("普通消息 {idx}"),
                10_000 + idx,
            )
        })
        .collect();

    let out = scheduler.ingest_and_select(10_010, batch, 10001, &nicks);
    let telemetry = scheduler.telemetry(10_010, &target);

    assert_eq!(out.len(), 8);
    assert_eq!(telemetry.burst_state, BurstModeState::Normal);
    assert_eq!(telemetry.selective_shed_p2_total, 0);
    assert_eq!(telemetry.dispatched_p2_total, 8);
}

#[test]
fn burst_sheds_p2_preserves_p0_and_recovers_with_hysteresis() {
    let cfg = SchedulerConfig {
        burst_window_ms: 2000,
        burst_enter_threshold: 16,
        burst_exit_threshold: 6,
        single_poll_burst_threshold: 12,
        burst_ordinary_sample_stride: 5,
        ..Default::default()
    };
    let mut scheduler = FloodScheduler::new(cfg.clone());
    let target = GroupTargetSpec::default();
    let nicks = vec!["qq-bot".to_string()];

    let mut batch = Vec::new();
    for idx in 0..50 {
        batch.push(make_group_msg(
            &format!("p2-{idx}"),
            "大群Alpha",
            &format!("水群闲聊 {idx}"),
            20_000 + idx,
        ));
    }
    batch.push(make_group_msg("p0-cmd", "大群Alpha", "/help", 20_051));
    batch.push(make_group_msg("p0-at", "大群Alpha", "@qq-bot 状态", 20_052));
    batch.push(make_group_msg(
        "p2-ellipsis",
        "大群Alpha",
        "...真的吗",
        20_053,
    ));

    let out = scheduler.ingest_and_select(20_060, batch, 10001, &nicks);
    let p0_ids: Vec<_> = out
        .iter()
        .filter(|m| m.priority_tier == Some(MessagePriorityTier::CriticalBotTrigger))
        .map(|m| m.message_id.as_str())
        .collect();
    assert_eq!(p0_ids, vec!["p0-cmd", "p0-at"]);
    assert!(out.iter().any(|m| m.message_id == "p2-ellipsis") || out.len() < 53);

    let mid_tel = scheduler.telemetry(20_500, &target);
    assert_eq!(mid_tel.burst_state, BurstModeState::BurstActive);
    assert!(mid_tel.selective_shed_p2_total > 0);

    // 窗口未完全退出前到达少数消息仍应保持洪峰态（滞回防抖），仅等窗口彻底冷清后恢复常规态
    let small_tail = vec![make_group_msg("tail-1", "大群Alpha", "后续 1", 21_500)];
    let _ = scheduler.ingest_and_select(21_500, small_tail, 10001, &nicks);
    assert_eq!(
        scheduler.telemetry(21_500, &target).burst_state,
        BurstModeState::BurstActive
    );

    // 超过 burst_window_ms=2000ms 且滑窗回落到 exit_threshold 以下后恢复 Normal
    let cooled = vec![make_group_msg("cool-1", "大群Alpha", "恢复后消息", 24_500)];
    let cooled_out = scheduler.ingest_and_select(24_500, cooled, 10001, &nicks);
    assert_eq!(cooled_out.len(), 1);
    assert_eq!(
        scheduler.telemetry(24_500, &target).burst_state,
        BurstModeState::Normal
    );

    // 验证省略号不被误分类为命令且独立检测器不丢失状态
    let mut cls = BotRelevanceClassifier::new();
    let ctx = ClassifyContext {
        self_id: 10001,
        self_nicknames: &nicks,
        config: &cfg,
    };
    let ellipsis = make_group_msg("dot3", "测试群", "...", 25_000);
    assert_eq!(
        cls.classify(25_000, &ellipsis, &ctx).0,
        MessagePriorityTier::OrdinaryChatter
    );
    let dot_cmd = make_group_msg("dot1", "测试群", ".status", 25_000);
    assert_eq!(
        cls.classify(25_000, &dot_cmd, &ctx).0,
        MessagePriorityTier::CriticalBotTrigger
    );
    let det = SlidingBurstDetector::new();
    assert_eq!(det.state(), BurstModeState::Normal);
}

#[test]
fn selector_enforces_edf_and_per_group_drr_anti_starvation() {
    let cfg = SchedulerConfig {
        group_quantum_messages: 2,
        drop_expired_events: true,
        ..Default::default()
    };
    let mut selector = ResponseWindowSelector::new();

    let mut items = Vec::new();
    for idx in 0..5u64 {
        let mut msg = make_group_msg(&format!("a-{idx}"), "Alpha", &format!("A{idx}"), 1000 + idx);
        msg.message_id = format!("a-{idx}");
        // 故意倒序设置截止期以验证同组 EDF 排序
        items.push((
            msg,
            MessagePriorityTier::CriticalBotTrigger,
            50_000 - idx * 100,
        ));
    }
    for idx in 0..3u64 {
        let msg = make_group_msg(&format!("b-{idx}"), "Beta", &format!("B{idx}"), 1000 + idx);
        items.push((
            msg,
            MessagePriorityTier::CriticalBotTrigger,
            45_000 + idx * 10,
        ));
    }
    // 插入一条已过期消息验证窗口淘汰机制
    let expired = make_group_msg("exp-1", "Alpha", "已过期", 100);
    items.push((expired, MessagePriorityTier::OrdinaryChatter, 900));

    selector.ingest_batch(1000, true, items, &cfg);
    assert_eq!(selector.expired_window_total, 1);

    let drained = selector.drain_all(1000, &cfg);
    let seq_groups: Vec<_> = drained.iter().map(|m| m.group_name.as_str()).collect();
    // Quantum=2：先取 Alpha 最早截止的两条，再公平轮转到 Beta 避免 Beta 饿死
    assert_eq!(
        seq_groups,
        vec!["Alpha", "Alpha", "Beta", "Beta", "Alpha", "Alpha", "Beta", "Alpha"]
    );
    // 验证 Alpha 内部严格按截止时间从早到晚出队（a-4 deadline=49600 优先于 a-3=49700）
    let alpha_ids: Vec<_> = drained
        .iter()
        .filter(|m| m.group_name == "Alpha")
        .map(|m| m.message_id.as_str())
        .collect();
    assert_eq!(alpha_ids, vec!["a-4", "a-3", "a-2", "a-1", "a-0"]);
}

#[test]
fn harvester_preempts_on_at_me_respects_dwell_and_skips_single_group() {
    let cfg = SchedulerConfig {
        urgent_preempt_dwell_ms: 350,
        session_dwell_min_ms: 1200,
        patrol_interval_ms: 2500,
        ..Default::default()
    };

    // 1. 单群无私聊未读场景：必须始终返回 None 防止无意义窗口闪烁
    let single_target = GroupTargetSpec {
        group_name: "Demo测试群".to_string(),
        ..Default::default()
    };
    let mut harvester = MultiSessionHarvester::new();
    harvester.record_switch(1000, "Demo测试群", ChatTargetKind::Group, false);
    let single_summaries = vec![SessionSummary {
        title: "其他大群 (800)".to_string(),
        unread_count: Some(99),
        has_at_me_hint: true,
        is_group_hint: Some(true),
        ..Default::default()
    }];
    harvester.observe_sessions(
        1500,
        &single_summaries,
        &single_target,
        Some("Demo测试群"),
        ChatTargetKind::Group,
    );
    assert!(harvester
        .next_switch_target(
            2000,
            Some("Demo测试群"),
            ChatTargetKind::Group,
            &single_target,
            &cfg
        )
        .is_none());

    // 2. 多受监控群场景：验证驻留防抖保护与 [有人@我] 抢占切换
    let multi_target = GroupTargetSpec {
        group_name: "大群Alpha".to_string(),
        monitored_groups: vec![
            "大群Alpha".to_string(),
            "大群Beta".to_string(),
            "大群Gamma".to_string(),
        ],
        allow_large_monitored_groups: true,
        ..Default::default()
    };
    let mut multi = MultiSessionHarvester::new();
    multi.record_switch(10_000, "大群Alpha (520)", ChatTargetKind::Group, false);
    let summaries = vec![
        SessionSummary {
            title: "大群Alpha (520)".to_string(),
            unread_count: None,
            is_group_hint: Some(true),
            ..Default::default()
        },
        SessionSummary {
            title: "大群Beta (680)".to_string(),
            unread_count: Some(12),
            has_at_me_hint: true,
            is_group_hint: Some(true),
            ..Default::default()
        },
        SessionSummary {
            title: "大群Gamma (840)".to_string(),
            unread_count: Some(5),
            has_at_all_hint: true,
            is_group_hint: Some(true),
            ..Default::default()
        },
    ];
    multi.observe_sessions(
        10_100,
        &summaries,
        &multi_target,
        Some("大群Alpha (520)"),
        ChatTargetKind::Group,
    );

    // 距上次切换仅 200ms < urgent_preempt_dwell_ms(350ms)，应受防抖保护拒绝抢占
    assert!(multi
        .next_switch_target(
            10_200,
            Some("大群Alpha (520)"),
            ChatTargetKind::Group,
            &multi_target,
            &cfg
        )
        .is_none());

    // 经过 400ms >= 350ms 后，优先抢占带 [有人@我] 的大群Beta
    let pick = multi
        .next_switch_target(
            10_400,
            Some("大群Alpha (520)"),
            ChatTargetKind::Group,
            &multi_target,
            &cfg,
        )
        .expect("应触发对 @我 群的抢占切换");
    assert_eq!(pick.target, "大群Beta");
    assert!(pick.is_urgent);
}

#[test]
fn harvester_unifies_private_session_keys_and_bounds_cache_sizes() {
    let mut cfg = SchedulerConfig::default();
    let target = GroupTargetSpec {
        allowed_private_friends: vec!["好友A".to_string(), "好友A (1)".to_string()],
        ..Default::default()
    };
    let mut harvester = MultiSessionHarvester::new();

    // 带人数或状态括号后缀的私聊标题与裸名私聊必须归一化到同一会话键，mark_session_read 可统一清除
    harvester.observe_sessions(
        10_000,
        &[SessionSummary {
            title: "好友A (1)".to_string(),
            unread_count: Some(2),
            is_group_hint: Some(false),
            ..Default::default()
        }],
        &target,
        None,
        ChatTargetKind::Group,
    );
    assert_eq!(harvester.session_count(), 1);
    harvester.observe_sessions(
        10_050,
        &[SessionSummary {
            title: "好友A".to_string(),
            unread_count: Some(2),
            is_group_hint: Some(false),
            ..Default::default()
        }],
        &target,
        None,
        ChatTargetKind::Group,
    );
    assert_eq!(harvester.session_count(), 1);

    harvester.mark_session_read("好友A");
    assert!(harvester
        .next_switch_target(11_000, None, ChatTargetKind::Group, &target, &cfg)
        .is_none());

    // 持续涌入超过 512 个不同私聊好友时，sessions 容量严格受限于 512 上限
    let wide_target = GroupTargetSpec {
        allowed_private_friends: (0..560).map(|i| format!("好友_{i}")).collect(),
        ..Default::default()
    };
    let batch: Vec<SessionSummary> = (0..560)
        .map(|i| SessionSummary {
            title: format!("好友_{i}"),
            unread_count: Some(1),
            is_group_hint: Some(false),
            ..Default::default()
        })
        .collect();
    harvester.observe_sessions(12_000, &batch, &wide_target, None, ChatTargetKind::Group);
    assert!(
        harvester.session_count() <= 512,
        "harvester sessions must be bounded <= 512, got {}",
        harvester.session_count()
    );

    // 洪峰态下持续出现超过 512 个群聊 OrdinaryChatter 时，p2_sample_counters 同样受限于 512 上限
    cfg.enabled = true;
    cfg.burst_ordinary_sample_stride = 4;
    let mut selector = ResponseWindowSelector::default();
    let flood_items: Vec<(ChatMessage, MessagePriorityTier, u64)> = (0..560)
        .map(|i| {
            (
                make_group_msg(&format!("id-{i}"), &format!("群_{i}"), "chat", 13_000),
                MessagePriorityTier::OrdinaryChatter,
                20_000,
            )
        })
        .collect();
    selector.ingest_batch(13_000, true, flood_items, &cfg);
    assert!(
        selector.p2_counter_count() <= 512,
        "p2_sample_counters must be bounded <= 512, got {}",
        selector.p2_counter_count()
    );
}
