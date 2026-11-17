//! 响应窗口选择器：实现 P0>P1>P2 分层、最早截止优先（EDF）、跨群赤字轮转（DRR）与洪峰 P2 抽样丢弃

use std::collections::{HashMap, VecDeque};

use common_types::{ChatMessage, MessagePriorityTier, SchedulerConfig};

use crate::parser::canonical_group_name;

const MAX_P2_SAMPLE_COUNTERS: usize = 512;

/// 进入响应窗口选择器排队的带截止期消息包装项
#[derive(Debug, Clone)]
pub(crate) struct QueuedChatEvent {
    pub(crate) seq: u64,
    pub(crate) deadline_ms: u64,
    pub(crate) tier: MessagePriorityTier,
    pub(crate) canon_group: String,
    pub(crate) message: ChatMessage,
}

/// 负责分层入队限流、窗口过期淘汰与跨群防饿死派发的响应窗口选择器
#[derive(Debug, Default)]
pub(crate) struct ResponseWindowSelector {
    next_seq: u64,
    groups: HashMap<String, [Vec<QueuedChatEvent>; 3]>,
    group_rr: VecDeque<String>,
    active_tier: Option<usize>,
    quantum_used: usize,
    pending_by_tier: [usize; 3],
    pending_total: usize,
    p2_sample_counters: HashMap<String, usize>,
    p2_sample_order: VecDeque<String>,
    pub(crate) received_total: u64,
    pub(crate) dispatched_by_tier: [u64; 3],
    pub(crate) selective_shed_p2_total: u64,
    pub(crate) expired_window_total: u64,
    pub(crate) last_dispatched_group: Option<String>,
}

impl ResponseWindowSelector {
    /// 创建一个空的响应窗口选择器实例
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// 返回当前仍滞留在选择器队列中的总事件条数
    pub(crate) fn pending_total(&self) -> usize {
        self.pending_total
    }

    /// 按 `[P0, P1, P2]` 顺序返回各优先级排队消息条数
    pub(crate) fn pending_by_tier(&self) -> [usize; 3] {
        self.pending_by_tier
    }

    /// 评估一条 P2 消息在当前采样步长下是否将被选择性丢弃，且不修改状态计数器
    pub(crate) fn would_shed_p2_at(
        &self,
        group_name: &str,
        offset_in_group: usize,
        stride: usize,
    ) -> bool {
        if stride == 0 {
            return true;
        }
        let canon = normalize_group_key(group_name);
        let base = self.p2_sample_counters.get(&canon).copied().unwrap_or(0);
        !base.wrapping_add(offset_in_group).is_multiple_of(stride)
    }

    /// 批量接收已分类消息：常规流量 100% 无损保留，洪峰或深积压下仅对 P2 执行确定性抽样丢弃
    pub(crate) fn ingest_batch(
        &mut self,
        now_ms: u64,
        burst_active: bool,
        items: Vec<(ChatMessage, MessagePriorityTier, u64)>,
        cfg: &SchedulerConfig,
    ) {
        if cfg.drop_expired_events {
            self.evict_expired(now_ms);
        }

        let max_pending = cfg.max_pending_events.max(1);
        let hard_shed_depth = cfg.hard_shed_queue_depth.max(1);

        for (mut msg, tier, deadline_ms) in items {
            self.received_total = self.received_total.saturating_add(1);
            let canon = normalize_group_key(&msg.group_name);
            msg.priority_tier = Some(tier);
            msg.response_deadline_ms = Some(deadline_ms);

            if cfg.drop_expired_events && now_ms > deadline_ms {
                self.expired_window_total = self.expired_window_total.saturating_add(1);
                continue;
            }

            let overload = cfg.enabled && (burst_active || self.pending_total >= hard_shed_depth);
            if tier == MessagePriorityTier::OrdinaryChatter
                && overload
                && !self.should_keep_p2_sample(&canon, cfg.burst_ordinary_sample_stride)
            {
                self.selective_shed_p2_total = self.selective_shed_p2_total.saturating_add(1);
                continue;
            }

            if self.pending_total >= max_pending && !self.evict_one_for_overflow(tier) {
                if tier == MessagePriorityTier::OrdinaryChatter {
                    self.selective_shed_p2_total = self.selective_shed_p2_total.saturating_add(1);
                }
                continue;
            }

            self.push_queued_event(QueuedChatEvent {
                seq: self.next_seq,
                deadline_ms,
                tier,
                canon_group: canon,
                message: msg,
            });
            self.next_seq = self.next_seq.saturating_add(1);
        }
    }

    /// 按 优先级 > 最早截止优先 > 跨群配额轮转 弹出下一条待派发事件
    pub(crate) fn pop_next(&mut self, now_ms: u64, cfg: &SchedulerConfig) -> Option<ChatMessage> {
        if cfg.drop_expired_events {
            self.evict_expired(now_ms);
        }

        let tier_idx = self.highest_non_empty_tier()?;
        let quantum = cfg.group_quantum_messages.max(1);
        let chosen_group = self.select_next_group_for_tier(tier_idx, quantum)?;

        let event = {
            let buckets = self.groups.get_mut(&chosen_group)?;
            pop_earliest_deadline(&mut buckets[tier_idx])?
        };
        let t_idx = tier_index(event.tier);
        self.pending_by_tier[t_idx] = self.pending_by_tier[t_idx].saturating_sub(1);
        self.pending_total = self.pending_total.saturating_sub(1);
        self.dispatched_by_tier[t_idx] = self.dispatched_by_tier[t_idx].saturating_add(1);
        self.last_dispatched_group = Some(event.canon_group.clone());
        self.prune_group_if_empty(&chosen_group);
        Some(event.message)
    }

    /// 一次性按调度顺序排空当前所有未过期的排队消息
    pub(crate) fn drain_all(&mut self, now_ms: u64, cfg: &SchedulerConfig) -> Vec<ChatMessage> {
        let mut out = Vec::with_capacity(self.pending_total);
        while let Some(msg) = self.pop_next(now_ms, cfg) {
            out.push(msg);
        }
        out
    }

    /// 淘汰全部队列中已超出响应截止时间的消息，并累加过期计数
    pub(crate) fn evict_expired(&mut self, now_ms: u64) -> usize {
        let mut removed_total = 0usize;
        let mut empty_groups = Vec::new();
        for (group, buckets) in &mut self.groups {
            for (t_idx, vec) in buckets.iter_mut().enumerate() {
                let before = vec.len();
                vec.retain(|item| now_ms <= item.deadline_ms);
                let removed = before.saturating_sub(vec.len());
                if removed > 0 {
                    removed_total = removed_total.saturating_add(removed);
                    self.pending_by_tier[t_idx] =
                        self.pending_by_tier[t_idx].saturating_sub(removed);
                    self.pending_total = self.pending_total.saturating_sub(removed);
                }
            }
            if buckets.iter().all(|v| v.is_empty()) {
                empty_groups.push(group.clone());
            }
        }
        for group in empty_groups {
            self.groups.remove(&group);
            self.group_rr.retain(|g| g != &group);
        }
        self.expired_window_total = self
            .expired_window_total
            .saturating_add(removed_total as u64);
        removed_total
    }

    /// 判断群内当前 P2 消息是否命中确定性采样保留窗口（stride == 0 全丢弃），并维持采样计数映射有界
    fn should_keep_p2_sample(&mut self, canon_group: &str, stride: usize) -> bool {
        if stride == 0 {
            return false;
        }
        if !self.p2_sample_counters.contains_key(canon_group) {
            while self.p2_sample_counters.len() >= MAX_P2_SAMPLE_COUNTERS {
                if let Some(oldest) = self.p2_sample_order.pop_front() {
                    self.p2_sample_counters.remove(&oldest);
                } else if let Some(any_key) = self.p2_sample_counters.keys().next().cloned() {
                    self.p2_sample_counters.remove(&any_key);
                } else {
                    break;
                }
            }
            self.p2_sample_order.push_back(canon_group.to_string());
            self.p2_sample_counters.insert(canon_group.to_string(), 0);
        }
        let counter = self
            .p2_sample_counters
            .get_mut(canon_group)
            .expect("inserted p2 sample counter");
        let keep = (*counter).is_multiple_of(stride);
        *counter = counter.wrapping_add(1);
        keep
    }

    /// 当达到 `max_pending_events` 上限时，从最低优先级队列中挤出一条历史事件
    fn evict_one_for_overflow(&mut self, incoming_tier: MessagePriorityTier) -> bool {
        let max_evict_tier = match incoming_tier {
            MessagePriorityTier::OrdinaryChatter => return false,
            MessagePriorityTier::HighContextual => 2,
            MessagePriorityTier::CriticalBotTrigger => 2,
        };
        let min_evict_tier = match incoming_tier {
            MessagePriorityTier::CriticalBotTrigger => 1,
            MessagePriorityTier::HighContextual => 2,
            MessagePriorityTier::OrdinaryChatter => 2,
        };

        for t_idx in (min_evict_tier..=max_evict_tier).rev() {
            if self.pending_by_tier[t_idx] == 0 {
                continue;
            }
            if let Some(canon) = self.find_group_with_tier(t_idx) {
                if let Some(buckets) = self.groups.get_mut(&canon) {
                    if !buckets[t_idx].is_empty() {
                        buckets[t_idx].remove(0);
                        self.pending_by_tier[t_idx] = self.pending_by_tier[t_idx].saturating_sub(1);
                        self.pending_total = self.pending_total.saturating_sub(1);
                        if t_idx == 2 {
                            self.selective_shed_p2_total =
                                self.selective_shed_p2_total.saturating_add(1);
                        }
                        self.prune_group_if_empty(&canon);
                        return true;
                    }
                }
            }
        }
        false
    }

    /// 按群聊与优先级桶插入新事件，并把新活跃群追加进轮转队列末尾
    fn push_queued_event(&mut self, item: QueuedChatEvent) {
        let t_idx = tier_index(item.tier);
        let canon = item.canon_group.clone();
        if !self.groups.contains_key(&canon) {
            self.groups
                .insert(canon.clone(), [Vec::new(), Vec::new(), Vec::new()]);
            self.group_rr.push_back(canon.clone());
        }
        if let Some(buckets) = self.groups.get_mut(&canon) {
            buckets[t_idx].push(item);
            self.pending_by_tier[t_idx] = self.pending_by_tier[t_idx].saturating_add(1);
            self.pending_total = self.pending_total.saturating_add(1);
        }
    }

    /// 查找当前仍包含排队事件的最高优先级索引（0=P0, 1=P1, 2=P2）
    fn highest_non_empty_tier(&self) -> Option<usize> {
        (0..3).find(|&idx| self.pending_by_tier[idx] > 0)
    }

    /// 结合当前配额与跨群轮转队列，为指定优先级选择下一个服务群聊
    fn select_next_group_for_tier(&mut self, tier_idx: usize, quantum: usize) -> Option<String> {
        let tier_switched = self.active_tier != Some(tier_idx);
        if tier_switched {
            self.active_tier = Some(tier_idx);
            self.quantum_used = 0;
        }

        if self.quantum_used > 0 && self.quantum_used < quantum {
            if let Some(front) = self.group_rr.front() {
                if self
                    .groups
                    .get(front)
                    .is_some_and(|b| !b[tier_idx].is_empty())
                {
                    self.quantum_used = self.quantum_used.saturating_add(1);
                    return Some(front.clone());
                }
            }
        }

        if self.quantum_used >= quantum {
            if let Some(prev) = self.group_rr.pop_front() {
                self.group_rr.push_back(prev);
            }
        }

        let len = self.group_rr.len();
        for _ in 0..len {
            let candidate = self.group_rr.front()?.clone();
            if self
                .groups
                .get(&candidate)
                .is_some_and(|b| !b[tier_idx].is_empty())
            {
                self.quantum_used = 1;
                return Some(candidate);
            }
            if let Some(rotated) = self.group_rr.pop_front() {
                self.group_rr.push_back(rotated);
            }
        }
        None
    }

    /// 按轮转顺序寻找任意拥有指定层级事件的群聊（用于溢出淘汰）
    fn find_group_with_tier(&self, tier_idx: usize) -> Option<String> {
        self.group_rr.iter().find_map(|g| {
            self.groups
                .get(g)
                .is_some_and(|b| !b[tier_idx].is_empty())
                .then(|| g.clone())
        })
    }

    /// 若某群聊的三个优先级桶均已清空，则从活跃群映射与轮转队列中移除
    fn prune_group_if_empty(&mut self, canon: &str) {
        let is_empty = self
            .groups
            .get(canon)
            .is_some_and(|b| b.iter().all(|vec| vec.is_empty()));
        if is_empty {
            self.groups.remove(canon);
            self.group_rr.retain(|g| g != canon);
            self.quantum_used = 0;
        }
    }

    /// 返回当前 P2 采样计数器条目数量（仅测试校验有界容量上限）
    #[cfg(test)]
    pub(crate) fn p2_counter_count(&self) -> usize {
        self.p2_sample_counters.len()
    }
}

/// 从同群同级桶内弹出最早截止时间（同截止期按序号升序）的事件
fn pop_earliest_deadline(bucket: &mut Vec<QueuedChatEvent>) -> Option<QueuedChatEvent> {
    if bucket.is_empty() {
        return None;
    }
    let mut best_idx = 0usize;
    for (idx, item) in bucket.iter().enumerate().skip(1) {
        let best = &bucket[best_idx];
        if (item.deadline_ms, item.seq) < (best.deadline_ms, best.seq) {
            best_idx = idx;
        }
    }
    Some(bucket.remove(best_idx))
}

/// 将优先级枚举映射到数组索引 `[0..=2]`
fn tier_index(tier: MessagePriorityTier) -> usize {
    match tier {
        MessagePriorityTier::CriticalBotTrigger => 0,
        MessagePriorityTier::HighContextual => 1,
        MessagePriorityTier::OrdinaryChatter => 2,
    }
}

/// 将群名统一规范化为无人数后缀键，空群名回退为空串常量键
fn normalize_group_key(name: &str) -> String {
    let canon = canonical_group_name(name);
    if canon.is_empty() {
        name.trim().to_string()
    } else {
        canon
    }
}
