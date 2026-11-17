//! 滑动窗口洪峰状态检测器：基于滞回双阈值避免常规流量误触发与洪峰边界震荡

use std::collections::VecDeque;

use common_types::{BurstModeState, SchedulerConfig};

/// 维护滑动时间窗口到达率与滞回状态机的洪峰检测器
#[derive(Debug, Default)]
pub(crate) struct SlidingBurstDetector {
    state: BurstModeState,
    burst_enter_count: u64,
    arrivals: VecDeque<(u64, usize)>,
    total_in_window: usize,
}

impl SlidingBurstDetector {
    /// 创建一个处于常规平稳态的滑动窗口洪峰检测器
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// 返回当前所处的洪峰状态机阶段
    pub(crate) fn state(&self) -> BurstModeState {
        self.state
    }

    /// 返回当前是否已激活突发消息洪峰保护
    pub(crate) fn is_burst_active(&self) -> bool {
        self.state == BurstModeState::BurstActive
    }

    /// 返回累计触发进入洪峰状态的次数
    pub(crate) fn burst_enter_count(&self) -> u64 {
        self.burst_enter_count
    }

    /// 返回当前滑动窗口内保留的到达事件总数
    pub(crate) fn recent_window_arrivals(&self) -> usize {
        self.total_in_window
    }

    /// 记录本轮到达批次并结合积压深度推进常规与洪峰滞回状态机
    pub(crate) fn record_batch(
        &mut self,
        now_ms: u64,
        batch_len: usize,
        pending_total: usize,
        cfg: &SchedulerConfig,
    ) -> BurstModeState {
        self.prune_expired(now_ms, cfg.burst_window_ms);
        if batch_len > 0 {
            self.arrivals.push_back((now_ms, batch_len));
            self.total_in_window = self.total_in_window.saturating_add(batch_len);
        }

        if !cfg.enabled {
            self.state = BurstModeState::Normal;
            return self.state;
        }

        let enter_threshold = cfg.burst_enter_threshold.max(1);
        let single_poll_threshold = cfg.single_poll_burst_threshold.max(1);
        let hard_shed_depth = cfg.hard_shed_queue_depth.max(1);
        let should_enter = self.total_in_window >= enter_threshold
            || batch_len >= single_poll_threshold
            || pending_total >= hard_shed_depth;

        if should_enter {
            if self.state == BurstModeState::Normal {
                self.state = BurstModeState::BurstActive;
                self.burst_enter_count = self.burst_enter_count.saturating_add(1);
            }
            return self.state;
        }

        if self.state == BurstModeState::BurstActive
            && self.total_in_window <= cfg.burst_exit_threshold
            && pending_total < hard_shed_depth
        {
            self.state = BurstModeState::Normal;
        }

        self.state
    }

    /// 在无新入站消息时按当前时间推进滑动窗口过期淘汰与滞回退出判定
    pub(crate) fn refresh_at(
        &mut self,
        now_ms: u64,
        pending_total: usize,
        cfg: &SchedulerConfig,
    ) -> BurstModeState {
        self.record_batch(now_ms, 0, pending_total, cfg)
    }

    /// 清理滑动时间窗口之前到达的历史批次计数
    fn prune_expired(&mut self, now_ms: u64, window_ms: u64) {
        while let Some(&(ts, count)) = self.arrivals.front() {
            if now_ms.saturating_sub(ts) <= window_ms {
                break;
            }
            self.arrivals.pop_front();
            self.total_in_window = self.total_in_window.saturating_sub(count);
        }
    }
}
