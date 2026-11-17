//! 多会话轮巡与紧急抢占采集器：观测侧边栏未读变化、防抖驻留并在多群间按 DRR 公平切换

use std::collections::HashMap;

use common_types::{
    canonical_group_names_match, ChatTargetKind, GroupTargetSpec, SchedulerConfig, SessionSummary,
};

use crate::parser::canonical_group_name;

const MAX_HARVESTER_SESSIONS: usize = 512;

/// 采集器计算出的下一次建议切换目标及紧急抢占标记
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HarvestSwitchTarget {
    pub(crate) target: String,
    pub(crate) canonical_key: String,
    pub(crate) target_kind: ChatTargetKind,
    pub(crate) unread_delta: u32,
    pub(crate) is_urgent: bool,
}

/// 单个受监控群聊或白名单私聊会话的未读变化与访问历史状态
#[derive(Debug, Default, Clone)]
struct HarvesterSessionState {
    display_name: String,
    target_kind: ChatTargetKind,
    last_unread: u32,
    pending_delta: u32,
    has_at_me: bool,
    has_at_all: bool,
    first_unread_seen_ms: Option<u64>,
    last_visited_ms: u64,
    initialized: bool,
}

/// 跟踪多群聊及允许私聊的侧边栏未读增量、紧急标记与防抖驻留策略的多会话调度器
#[derive(Debug, Default)]
pub(crate) struct MultiSessionHarvester {
    sessions: HashMap<String, HarvesterSessionState>,
    group_rr_cursor: usize,
    last_switch_ms: u64,
    last_patrol_ms: u64,
    switches_total: u64,
}

impl MultiSessionHarvester {
    /// 创建一个新的多会话采集状态机实例
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// 返回累计触发的多会话切换总次数
    pub(crate) fn switches_total(&self) -> u64 {
        self.switches_total
    }

    /// 根据最新提取的左侧会话列表刷新受监控群与白名单私聊的未读增量与 @ 标记
    pub(crate) fn observe_sessions(
        &mut self,
        now_ms: u64,
        summaries: &[SessionSummary],
        target: &GroupTargetSpec,
        active_target: Option<&str>,
        active_kind: ChatTargetKind,
    ) {
        for summary in summaries {
            let Some((key, kind)) = resolve_monitored_session_key(summary, target) else {
                continue;
            };
            let is_active = active_target.is_some_and(|act| {
                active_kind == kind && target_name_matches(act, &summary.title, kind)
            });
            let unread = summary.unread_count.unwrap_or(0);
            self.evict_excess_sessions(&key);
            let entry = self
                .sessions
                .entry(key)
                .or_insert_with(|| HarvesterSessionState {
                    display_name: summary.title.clone(),
                    target_kind: kind,
                    ..Default::default()
                });
            entry.display_name = summary.title.clone();
            entry.target_kind = kind;

            if is_active {
                clear_pending_unread(entry);
                entry.last_visited_ms = now_ms;
                entry.initialized = true;
                continue;
            }

            if !entry.initialized {
                entry.initialized = true;
                entry.last_unread = unread;
                if unread > 0 {
                    entry.pending_delta = unread;
                    entry.first_unread_seen_ms = Some(now_ms);
                }
            } else if unread == 0 {
                clear_pending_unread(entry);
            } else if unread > entry.last_unread {
                let delta = unread.saturating_sub(entry.last_unread);
                entry.pending_delta = entry.pending_delta.saturating_add(delta);
                entry.last_unread = unread;
                if entry.first_unread_seen_ms.is_none() {
                    entry.first_unread_seen_ms = Some(now_ms);
                }
            } else {
                entry.last_unread = unread;
            }

            if unread > 0 {
                entry.has_at_me |= summary.has_at_me_hint;
                entry.has_at_all |= summary.has_at_all_hint;
            }
        }
    }

    /// 依据紧急优先级、最小驻留时长与多群轮巡间隔选出下一跳会话目标
    pub(crate) fn next_switch_target(
        &mut self,
        now_ms: u64,
        active_target: Option<&str>,
        active_kind: ChatTargetKind,
        target: &GroupTargetSpec,
        cfg: &SchedulerConfig,
    ) -> Option<HarvestSwitchTarget> {
        let monitored_groups = target.effective_monitored_groups();
        let has_private_unread = self.sessions.iter().any(|(_, st)| {
            st.target_kind == ChatTargetKind::Private
                && st.pending_delta > 0
                && !is_currently_active(active_target, active_kind, st)
        });

        // 仅监控单个群且无任何白名单私聊未读时永不切出，彻底杜绝单群无谓抖动
        if monitored_groups.len() <= 1 && !has_private_unread {
            return None;
        }

        let since_switch = now_ms.saturating_sub(self.last_switch_ms);
        let urgent_allowed = active_target.is_none() || since_switch >= cfg.urgent_preempt_dwell_ms;
        if urgent_allowed {
            if let Some(pick) =
                self.pick_best_candidate(active_target, active_kind, &monitored_groups, true, 0)
            {
                return Some(pick);
            }
        }

        if !cfg.enabled {
            return None;
        }

        let ordinary_allowed = active_target.is_none() || since_switch >= cfg.session_dwell_min_ms;
        if ordinary_allowed {
            if let Some(pick) =
                self.pick_best_candidate(active_target, active_kind, &monitored_groups, false, 1)
            {
                return Some(pick);
            }
            if let Some(pick) =
                self.pick_best_candidate(active_target, active_kind, &monitored_groups, false, 2)
            {
                return Some(pick);
            }
        }

        if monitored_groups.len() >= 2
            && active_target.is_some()
            && since_switch >= cfg.patrol_interval_ms.max(cfg.session_dwell_min_ms)
            && now_ms.saturating_sub(self.last_patrol_ms) >= cfg.patrol_interval_ms
        {
            return self.pick_patrol_group(now_ms, active_target, active_kind, &monitored_groups);
        }

        None
    }

    /// 记录调度器已切入指定会话，重置该会话未读增量与驻留计时起点
    pub(crate) fn record_switch(
        &mut self,
        now_ms: u64,
        target: &str,
        target_kind: ChatTargetKind,
        count_switch: bool,
    ) {
        let key = session_key(target, target_kind);
        self.evict_excess_sessions(&key);
        let entry = self
            .sessions
            .entry(key)
            .or_insert_with(|| HarvesterSessionState {
                display_name: target.trim().to_string(),
                target_kind,
                ..Default::default()
            });
        clear_pending_unread(entry);
        entry.last_visited_ms = now_ms;
        entry.initialized = true;
        self.last_switch_ms = now_ms;
        if count_switch {
            self.switches_total = self.switches_total.saturating_add(1);
        }
    }

    /// 显式将目标会话标记为已读，清除其未读增量与 @ 抢占状态
    pub(crate) fn mark_session_read(&mut self, target: &str) {
        let group_key = session_key(target, ChatTargetKind::Group);
        let private_key = session_key(target, ChatTargetKind::Private);
        for key in [group_key, private_key] {
            if let Some(entry) = self.sessions.get_mut(&key) {
                clear_pending_unread(entry);
            }
        }
    }

    /// 在特定紧迫层（0=私聊或[有人@我]，1=@全体，2=普通未读）内按访问公平性筛选候选目标
    fn pick_best_candidate(
        &mut self,
        active_target: Option<&str>,
        active_kind: ChatTargetKind,
        monitored_groups: &[String],
        is_urgent: bool,
        urgency_level: u8,
    ) -> Option<HarvestSwitchTarget> {
        let mut best: Option<(u64, u64, String, HarvestSwitchTarget)> = None;
        for (key, st) in &self.sessions {
            if is_currently_active(active_target, active_kind, st) {
                continue;
            }
            let matches_tier = match urgency_level {
                0 => {
                    (st.target_kind == ChatTargetKind::Private && st.pending_delta > 0)
                        || (st.target_kind == ChatTargetKind::Group
                            && st.has_at_me
                            && (st.pending_delta > 0 || st.last_unread > 0))
                }
                1 => {
                    st.target_kind == ChatTargetKind::Group && st.has_at_all && st.pending_delta > 0
                }
                _ => st.pending_delta > 0,
            };
            if !matches_tier {
                continue;
            }
            // 同级优先级下优先调度久未访问且最早产生未读的会话，防止多群同时被 @ 时饿死
            let first_seen = st.first_unread_seen_ms.unwrap_or(u64::MAX);
            let candidate_sort = (st.last_visited_ms, first_seen);
            let should_replace = best
                .as_ref()
                .is_none_or(|(bv, bf, _, _)| candidate_sort < (*bv, *bf));
            if should_replace {
                let target_name = if st.target_kind == ChatTargetKind::Group {
                    monitored_groups
                        .iter()
                        .find(|g| canonical_group_names_match(g, &st.display_name))
                        .cloned()
                        .unwrap_or_else(|| canonical_group_name(&st.display_name))
                } else {
                    st.display_name.clone()
                };
                best = Some((
                    candidate_sort.0,
                    candidate_sort.1,
                    key.clone(),
                    HarvestSwitchTarget {
                        target: target_name,
                        canonical_key: key.clone(),
                        target_kind: st.target_kind,
                        unread_delta: st.pending_delta.max(st.last_unread),
                        is_urgent,
                    },
                ));
            }
        }
        best.map(|(_, _, _, pick)| pick)
    }

    /// 无未读增量时按 DRR 游标选出下一个待巡检的后台受监控群聊
    fn pick_patrol_group(
        &mut self,
        now_ms: u64,
        active_target: Option<&str>,
        active_kind: ChatTargetKind,
        monitored_groups: &[String],
    ) -> Option<HarvestSwitchTarget> {
        let len = monitored_groups.len();
        if len < 2 {
            return None;
        }
        for offset in 0..len {
            let idx = (self.group_rr_cursor + offset) % len;
            let candidate = &monitored_groups[idx];
            let same_active = active_kind == ChatTargetKind::Group
                && active_target.is_some_and(|act| canonical_group_names_match(act, candidate));
            if same_active {
                continue;
            }
            self.group_rr_cursor = (idx + 1) % len;
            self.last_patrol_ms = now_ms;
            return Some(HarvestSwitchTarget {
                target: candidate.clone(),
                canonical_key: session_key(candidate, ChatTargetKind::Group),
                target_kind: ChatTargetKind::Group,
                unread_delta: 0,
                is_urgent: false,
            });
        }
        None
    }

    /// 限制 `sessions` 映射总容量不超过 `MAX_HARVESTER_SESSIONS`，优先淘汰无未读积压的最旧会话
    fn evict_excess_sessions(&mut self, incoming_key: &str) {
        if self.sessions.contains_key(incoming_key) {
            return;
        }
        while self.sessions.len() >= MAX_HARVESTER_SESSIONS {
            let Some(evict_key) = self
                .sessions
                .iter()
                .min_by(|(kl, sl), (kr, sr)| {
                    (sl.pending_delta > 0)
                        .cmp(&(sr.pending_delta > 0))
                        .then_with(|| sl.last_visited_ms.cmp(&sr.last_visited_ms))
                        .then_with(|| kl.cmp(kr))
                })
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            self.sessions.remove(&evict_key);
        }
    }

    /// 返回当前已缓存的会话状态条目数量（仅测试校验有界容量上限）
    #[cfg(test)]
    pub(crate) fn session_count(&self) -> usize {
        self.sessions.len()
    }
}

/// 将单条侧边栏摘要映射到受监控键及聊天类型，不在策略白名单内则返回 `None`
fn resolve_monitored_session_key(
    summary: &SessionSummary,
    target: &GroupTargetSpec,
) -> Option<(String, ChatTargetKind)> {
    let is_monitored_group = target.is_monitored_group(&summary.title);
    let is_allowed_private = target.is_private_friend_allowed(&summary.title);
    match summary.is_group_hint {
        Some(true) if is_monitored_group => Some((
            session_key(&summary.title, ChatTargetKind::Group),
            ChatTargetKind::Group,
        )),
        Some(false) if is_allowed_private => Some((
            session_key(&summary.title, ChatTargetKind::Private),
            ChatTargetKind::Private,
        )),
        None if is_monitored_group => Some((
            session_key(&summary.title, ChatTargetKind::Group),
            ChatTargetKind::Group,
        )),
        None if is_allowed_private => Some((
            session_key(&summary.title, ChatTargetKind::Private),
            ChatTargetKind::Private,
        )),
        _ => None,
    }
}

/// 生成跨类型唯一的会话存储键（群聊与私聊统一归一化名称并回退到去空白原串）
fn session_key(name: &str, kind: ChatTargetKind) -> String {
    let canon = canonical_group_name(name);
    let norm = if canon.is_empty() {
        name.trim()
    } else {
        canon.as_str()
    };
    match kind {
        ChatTargetKind::Group => format!("group:{norm}"),
        ChatTargetKind::Private => format!("private:{norm}"),
    }
}

/// 判断给定候选名称是否与当前激活会话一致
fn target_name_matches(lhs: &str, rhs: &str, kind: ChatTargetKind) -> bool {
    match kind {
        ChatTargetKind::Group => canonical_group_names_match(lhs, rhs),
        ChatTargetKind::Private => {
            lhs.trim() == rhs.trim() || canonical_group_names_match(lhs, rhs)
        }
    }
}

/// 判断记录项是否正处于前台激活状态
fn is_currently_active(
    active_target: Option<&str>,
    active_kind: ChatTargetKind,
    st: &HarvesterSessionState,
) -> bool {
    active_target.is_some_and(|act| {
        active_kind == st.target_kind && target_name_matches(act, &st.display_name, st.target_kind)
    })
}

/// 清除单个会话记录的待处理未读计数、@ 标记和首次发现时间戳
fn clear_pending_unread(entry: &mut HarvesterSessionState) {
    entry.last_unread = 0;
    entry.pending_delta = 0;
    entry.has_at_me = false;
    entry.has_at_all = false;
    entry.first_unread_seen_ms = None;
}
