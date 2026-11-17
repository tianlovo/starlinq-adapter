//! 洪峰调度器总成模块：编排滑动洪峰检测、相关性分级、响应窗口选择器与多群轮巡采集

pub(crate) mod burst_detector;
pub(crate) mod classifier;
pub(crate) mod event_selector;
pub(crate) mod session_harvester;
#[cfg(test)]
mod tests;

use common_types::{
    ChatMessage, ChatTargetKind, GroupTargetSpec, MessagePriorityTier, SchedulerConfig,
    SchedulerTelemetrySnapshot, SessionSummary,
};

use crate::parser::canonical_group_name;

use self::burst_detector::SlidingBurstDetector;
use self::classifier::{BotRelevanceClassifier, ClassifyContext};
use self::event_selector::ResponseWindowSelector;
use self::session_harvester::{HarvestSwitchTarget, MultiSessionHarvester};

/// 整合洪峰检测、消息优先级分类、响应窗口调度与多群切换采集的内部统一门面
#[derive(Debug, Default)]
pub struct FloodScheduler {
    config: SchedulerConfig,
    detector: SlidingBurstDetector,
    classifier: BotRelevanceClassifier,
    selector: ResponseWindowSelector,
    harvester: MultiSessionHarvester,
    outbound_deadline_aborted_total: u64,
}

impl FloodScheduler {
    /// 根据初始调度策略构造洪峰调度器实例
    pub fn new(config: SchedulerConfig) -> Self {
        Self {
            config,
            detector: SlidingBurstDetector::new(),
            classifier: BotRelevanceClassifier::new(),
            selector: ResponseWindowSelector::new(),
            harvester: MultiSessionHarvester::new(),
            outbound_deadline_aborted_total: 0,
        }
    }

    /// 返回当前生效的洪峰与响应窗口配置副本
    pub fn config(&self) -> SchedulerConfig {
        self.config.clone()
    }

    /// 热更新运行期洪峰调度参数，并立即刷新滑动状态机判定
    pub fn update_config(&mut self, config: SchedulerConfig, now_ms: u64) {
        self.config = config;
        self.detector
            .refresh_at(now_ms, self.selector.pending_total(), &self.config);
    }

    /// 在执行高代价图片裁剪前预判每条原始消息是否需要保留截图提取
    pub fn pre_classify_keep_image_mask(
        &self,
        now_ms: u64,
        messages: &[&ChatMessage],
        self_id: u64,
        self_nicknames: &[String],
    ) -> Vec<bool> {
        let single_threshold = self.config.single_poll_burst_threshold.max(1);
        let will_burst = self.config.enabled
            && (self.detector.is_burst_active()
                || messages.len() >= single_threshold
                || self.selector.pending_total() >= self.config.hard_shed_queue_depth.max(1));
        if !will_burst {
            return vec![true; messages.len()];
        }

        let ctx = ClassifyContext {
            self_id,
            self_nicknames,
            config: &self.config,
        };
        let mut group_p2_offsets: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        let mut mask = Vec::with_capacity(messages.len());
        for msg in messages {
            let tier = self.classifier.peek_tier(now_ms, msg, &ctx);
            if tier != MessagePriorityTier::OrdinaryChatter {
                mask.push(true);
                continue;
            }
            let canon = canonical_group_name(&msg.group_name);
            let offset = group_p2_offsets.entry(canon).or_insert(0);
            let keep = !self.selector.would_shed_p2_at(
                &msg.group_name,
                *offset,
                self.config.burst_ordinary_sample_stride,
            );
            *offset = offset.saturating_add(1);
            mask.push(keep);
        }
        mask
    }

    /// 接收单轮增量消息批次，完成分级、限流选择与有序派发
    pub fn ingest_and_select(
        &mut self,
        now_ms: u64,
        incoming: Vec<ChatMessage>,
        self_id: u64,
        self_nicknames: &[String],
    ) -> Vec<ChatMessage> {
        let state = self.detector.record_batch(
            now_ms,
            incoming.len(),
            self.selector.pending_total(),
            &self.config,
        );
        let burst_active = state == common_types::BurstModeState::BurstActive;
        let ctx = ClassifyContext {
            self_id,
            self_nicknames,
            config: &self.config,
        };
        let classified = incoming
            .into_iter()
            .map(|msg| {
                let (tier, deadline) = self.classifier.classify(now_ms, &msg, &ctx);
                (msg, tier, deadline)
            })
            .collect();
        self.selector
            .ingest_batch(now_ms, burst_active, classified, &self.config);
        self.selector.drain_all(now_ms, &self.config)
    }

    /// 喂入最新提取的侧边栏会话列表以更新多群未读增量视图
    pub fn observe_sessions(
        &mut self,
        now_ms: u64,
        summaries: &[SessionSummary],
        target: &GroupTargetSpec,
        active_target: Option<&str>,
        active_kind: ChatTargetKind,
    ) {
        self.harvester
            .observe_sessions(now_ms, summaries, target, active_target, active_kind);
    }

    /// 决策后台轮巡下一跳是否需要切换到其它群聊或紧急私聊
    pub(crate) fn next_switch_target(
        &mut self,
        now_ms: u64,
        active_target: Option<&str>,
        active_kind: ChatTargetKind,
        target: &GroupTargetSpec,
    ) -> Option<HarvestSwitchTarget> {
        self.harvester
            .next_switch_target(now_ms, active_target, active_kind, target, &self.config)
    }

    /// 记录驱动已实际打开或切换到目标会话，刷新驻留保护时间窗
    pub fn record_switch(
        &mut self,
        now_ms: u64,
        target: &str,
        target_kind: ChatTargetKind,
        count_switch: bool,
    ) {
        self.harvester
            .record_switch(now_ms, target, target_kind, count_switch);
    }

    /// 标记指定会话为已读并清理多群调度器中的对应未读状态
    pub fn mark_session_read(&mut self, target: &str) {
        self.harvester.mark_session_read(target);
    }

    /// 累加一次出站发送前因响应窗口超时而主动中止的统计计数
    pub fn record_outbound_deadline_abort(&mut self) {
        self.outbound_deadline_aborted_total =
            self.outbound_deadline_aborted_total.saturating_add(1);
    }

    /// 生成供观测接口与运行期状态上报使用的全量洪峰遥测快照
    pub fn telemetry(
        &mut self,
        now_ms: u64,
        target: &GroupTargetSpec,
    ) -> SchedulerTelemetrySnapshot {
        self.detector
            .refresh_at(now_ms, self.selector.pending_total(), &self.config);
        let pending_by_tier = self.selector.pending_by_tier();
        SchedulerTelemetrySnapshot {
            burst_state: self.detector.state(),
            burst_enter_count: self.detector.burst_enter_count(),
            recent_window_arrivals: self.detector.recent_window_arrivals(),
            pending_total: self.selector.pending_total(),
            pending_p0: pending_by_tier[0],
            pending_p1: pending_by_tier[1],
            pending_p2: pending_by_tier[2],
            received_total: self.selector.received_total,
            dispatched_p0_total: self.selector.dispatched_by_tier[0],
            dispatched_p1_total: self.selector.dispatched_by_tier[1],
            dispatched_p2_total: self.selector.dispatched_by_tier[2],
            selective_shed_p2_total: self.selector.selective_shed_p2_total,
            expired_window_total: self.selector.expired_window_total,
            outbound_deadline_aborted_total: self.outbound_deadline_aborted_total,
            multi_group_switches_total: self.harvester.switches_total(),
            last_dispatched_group: self.selector.last_dispatched_group.clone(),
            active_monitored_groups: target.effective_monitored_groups().len(),
        }
    }
}
