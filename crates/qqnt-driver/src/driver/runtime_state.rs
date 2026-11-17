//! 驱动运行时状态快照、SQLite 持久化桥接与自账号/群标识归并辅助逻辑

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use common_types::constants::{
    DEFAULT_BOT_SELF_NAME, DEFAULT_ONEBOT_ID_BOUND, DEFAULT_SELF_ID, MOCK_QQ_MAIN_CLASS,
};
use common_types::{
    segments_to_cq_string, stable_onebot_id, AdapterConfig, AdapterError, AdapterEvent,
    AdapterStatus, BotLoginInfo, BurstModeState, ChatMessage, ChatTargetKind, GroupMemberInfo,
    MessageSegment, Result, SchedulerTelemetrySnapshot, SessionSummary, UiNodeSnapshot, WindowInfo,
};
use parking_lot::Mutex;
use storage::{
    RecordMessageInput, Storage, StoredGroupRecord, UpsertFriendInput, UpsertGroupInput,
};
use tokio::sync::{broadcast, Mutex as AsyncMutex, Notify};
use tracing::warn;

use super::outbound_media::OutboundSendPlan;
use super::segment_enricher::has_rich_segments;
use crate::constants::{
    MEDIA_CACHE_SUBDIR, MEDIA_PNG_EXTENSION, SELF_NICKNAME_META_KEY, SELF_USER_ID_META_KEY,
    SESSION_INLINE_SPLIT_REGEX,
};
use crate::humanizer::Humanizer;
use crate::parser::{self, MessageTracker};
use crate::profile::QqntProfile;
use crate::scheduler::FloodScheduler;

const MIGRATION_SESSION_POLLUTION_V2_KEY: &str = "migration.session_pollution_v2";
const MIGRATION_MEDIA_CROP_DPI_V2_KEY: &str = "migration.media_crop_dpi_v2";

/// 保存在工作线程共享状态中的可变运行时上下文与会话变更哨兵缓存
#[derive(Debug, Clone)]
pub(crate) struct DriverRuntimeState {
    pub(crate) desktop_name: String,
    pub(crate) sandbox_isolated: bool,
    pub(crate) qq_pids: Vec<u32>,
    pub(crate) resolved_qq_path: Option<String>,
    pub(crate) attached_window: Option<WindowInfo>,
    pub(crate) active_group: Option<String>,
    pub(crate) active_chat_kind: ChatTargetKind,
    pub(crate) detected_member_count: Option<u32>,
    pub(crate) security_breaker_tripped: bool,
    pub(crate) security_breaker_reason: Option<String>,
    pub(crate) messages_observed: u64,
    pub(crate) messages_sent: u64,
    pub(crate) session_snapshots: HashMap<String, (Option<String>, u32, Option<u32>)>,
    pub(crate) last_session_refresh_at: Option<Instant>,
    pub(crate) last_member_sync_at: HashMap<String, Instant>,
}

impl DriverRuntimeState {
    /// 根据初始配置与桌面信息构建运行时状态对象
    pub(crate) fn new(config: &AdapterConfig) -> Self {
        let (desktop_name, sandbox_isolated) = uia_core::current_desktop_info();
        Self {
            desktop_name,
            sandbox_isolated,
            qq_pids: Vec::new(),
            resolved_qq_path: config.qq_client.executable_path.clone(),
            attached_window: None,
            active_group: None,
            active_chat_kind: ChatTargetKind::Group,
            detected_member_count: None,
            security_breaker_tripped: false,
            security_breaker_reason: None,
            messages_observed: 0,
            messages_sent: 0,
            session_snapshots: HashMap::new(),
            last_session_refresh_at: None,
            last_member_sync_at: HashMap::new(),
        }
    }

    /// 将当前可变状态转换为对外可序列化的状态快照
    pub(crate) fn to_status(&self, config: &AdapterConfig) -> AdapterStatus {
        AdapterStatus {
            desktop_name: self.desktop_name.clone(),
            sandbox_isolated: self.sandbox_isolated,
            qq_pids: self.qq_pids.clone(),
            resolved_qq_path: self.resolved_qq_path.clone(),
            attached_window: self.attached_window.clone(),
            active_group: self.active_group.clone(),
            active_chat_kind: self.active_group.as_ref().map(|_| self.active_chat_kind),
            active_group_id: None,
            active_real_group_id: None,
            detected_member_count: self.detected_member_count,
            security_breaker_tripped: self.security_breaker_tripped,
            security_breaker_reason: self.security_breaker_reason.clone(),
            input_mode: config.interaction.default_mode,
            messages_observed: self.messages_observed,
            messages_sent: self.messages_sent,
            burst_mode_active: false,
            scheduler_pending: 0,
        }
    }
}

/// 跨异步调度器、后台轮询协程与 COM 工作线程共享的线程安全状态容器
pub(crate) struct SharedInner {
    pub(crate) config: AdapterConfig,
    pub(crate) profile: QqntProfile,
    pub(crate) storage: Arc<Storage>,
    pub(crate) state: Mutex<DriverRuntimeState>,
    pub(crate) tracker: Mutex<MessageTracker>,
    pub(crate) humanizer: Mutex<Humanizer>,
    pub(crate) scheduler: Mutex<FloodScheduler>,
    pub(crate) event_tx: broadcast::Sender<AdapterEvent>,
    pub(crate) shutdown: AtomicBool,
    pub(crate) shutdown_notify: Notify,
    pub(crate) poller_running: AtomicBool,
    pub(crate) send_gate: AsyncMutex<()>,
    pub(crate) outbound_seq: AtomicU64,
}

impl SharedInner {
    /// 统一构建驱动内部跨线程共享状态容器
    pub(crate) fn new(
        config: AdapterConfig,
        storage: Arc<Storage>,
        event_tx: broadcast::Sender<AdapterEvent>,
    ) -> Self {
        run_session_pollution_migration_v2(&storage);
        if !storage.is_memory() {
            purge_stale_media_crop_files_in_dir(&storage, Path::new(MEDIA_CACHE_SUBDIR));
        }
        let profile = QqntProfile::from_config(&config.interaction);
        let state = DriverRuntimeState::new(&config);
        let tracker = MessageTracker::new(config.bot.self_nicknames.clone());
        let humanizer = Humanizer::from_config(&config.interaction);
        let scheduler = FloodScheduler::new(config.scheduler.clone());
        let shared = Self {
            config,
            profile,
            storage,
            state: Mutex::new(state),
            tracker: Mutex::new(tracker),
            humanizer: Mutex::new(humanizer),
            scheduler: Mutex::new(scheduler),
            event_tx,
            shutdown: AtomicBool::new(false),
            shutdown_notify: Notify::new(),
            poller_running: AtomicBool::new(false),
            send_gate: AsyncMutex::new(()),
            outbound_seq: AtomicU64::new(0),
        };
        let _ = shared.resolve_login_info();
        shared
    }

    /// 无丢失唤醒地等待驱动停机信号（适用于并发关停竞态防护）
    pub(crate) async fn wait_shutdown(&self) {
        let notified = self.shutdown_notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if self.shutdown.load(Ordering::Acquire) {
            return;
        }
        notified.await;
    }

    /// 原子置位驱动停机标记并立即唤醒所有异步等待者
    pub(crate) fn signal_shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
        self.shutdown_notify.notify_waiters();
    }
    /// 基于 SQLite 群表为运行状态快照回填当前激活群的数字群号与真实群号，并同步洪峰态标识
    pub(crate) fn enrich_status(&self, mut status: AdapterStatus) -> AdapterStatus {
        let tel = self.scheduler_telemetry_snapshot();
        status.burst_mode_active = tel.burst_state == BurstModeState::BurstActive;
        status.scheduler_pending = tel.pending_total;
        if status.active_chat_kind == Some(ChatTargetKind::Private) {
            return status;
        }
        if let Some(ref active_name) = status.active_group {
            if let Some(record) = lookup_group_record(&self.storage, active_name) {
                status.active_group_id = Some(record.effective_group_id());
                status.active_real_group_id = record.real_group_id;
            } else {
                let canon = canonical_storage_group_name(active_name);
                if !canon.is_empty() {
                    status.active_group_id =
                        Some(stable_onebot_id(&canon, DEFAULT_ONEBOT_ID_BOUND));
                }
            }
        }
        status
    }

    /// 获取当前驱动状态只读快照（释放状态锁后再查询数据库以防锁嵌套）
    pub(crate) fn status_snapshot(&self) -> AdapterStatus {
        let raw_status = self.state.lock().to_status(&self.config);
        self.enrich_status(raw_status)
    }

    /// 广播单个适配事件至所有异步订阅方
    pub(crate) fn emit_event(&self, event: AdapterEvent) {
        let _ = self.event_tx.send(event);
    }

    /// 收集当前机器人识别号与自账号昵称列表（不持有其他锁以避免锁嵌套）
    pub(crate) fn scheduler_self_identity(&self) -> (u64, Vec<String>) {
        let login = self.resolve_login_info();
        let mut nicks = self.tracker.lock().self_nicknames().to_vec();
        for cfg_nick in &self.config.bot.self_nicknames {
            let trimmed = cfg_nick.trim();
            if !trimmed.is_empty() && !nicks.iter().any(|existing| existing == trimmed) {
                nicks.push(trimmed.to_string());
            }
        }
        let login_nick = login.nickname.trim();
        if !login_nick.is_empty() && !nicks.iter().any(|existing| existing == login_nick) {
            nicks.push(login_nick.to_string());
        }
        (login.user_id.max(0) as u64, nicks)
    }

    /// 将轮询获取的入站新消息送入洪峰响应窗口选择器，返回按优先级排序并完成选择性丢弃的列表
    pub(crate) fn select_incoming_events(&self, incoming: Vec<ChatMessage>) -> Vec<ChatMessage> {
        let (self_id, nicks) = self.scheduler_self_identity();
        self.scheduler
            .lock()
            .ingest_and_select(now_utc_ms(), incoming, self_id, &nicks)
    }

    /// 获取当前洪峰调度器遥测快照
    pub(crate) fn scheduler_telemetry_snapshot(&self) -> SchedulerTelemetrySnapshot {
        self.scheduler
            .lock()
            .telemetry(now_utc_ms(), &self.config.target)
    }

    /// 校验出站消息是否已超出响应窗口截止期；超时即累加中止计数并返回 `ResponseWindowExpired`
    pub(crate) fn check_outbound_deadline(
        &self,
        target: &str,
        deadline_unix_ms: Option<u64>,
    ) -> Result<()> {
        let Some(deadline) = deadline_unix_ms else {
            return Ok(());
        };
        let now = now_utc_ms();
        if now > deadline {
            let mut scheduler = self.scheduler.lock();
            let window_ms = scheduler.config().critical_response_window_ms;
            scheduler.record_outbound_deadline_abort();
            return Err(AdapterError::ResponseWindowExpired {
                target: target.to_string(),
                elapsed_ms: now.saturating_sub(deadline),
                window_ms,
            });
        }
        Ok(())
    }

    /// 记录一次外部上报的出站截止期中止事件
    pub(crate) fn record_outbound_deadline_abort(&self) {
        self.scheduler.lock().record_outbound_deadline_abort();
    }

    /// 判断当前配置是否显式放行测试用 Mock QQ 窗口类名
    pub(crate) fn allow_mock(&self) -> bool {
        self.profile
            .window_classes()
            .iter()
            .any(|class_name| class_name.eq_ignore_ascii_case(MOCK_QQ_MAIN_CLASS))
    }

    /// 解析当前机器人登录信息（优先读取被动感知缓存的自账号昵称并持久化自编号与真实 QQ 号）
    pub(crate) fn resolve_login_info(&self) -> BotLoginInfo {
        let meta_nick = self
            .storage
            .get_meta(SELF_NICKNAME_META_KEY)
            .ok()
            .flatten()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());

        let config_nick = self
            .config
            .bot
            .self_nicknames
            .iter()
            .map(|nick| nick.trim())
            .find(|nick| !nick.is_empty())
            .map(str::to_string);

        let nickname = meta_nick
            .clone()
            .or(config_nick)
            .unwrap_or_else(|| DEFAULT_BOT_SELF_NAME.to_string());

        let real_qq_id =
            (self.config.bot.self_id != DEFAULT_SELF_ID).then_some(self.config.bot.self_id);
        let user_id = if self.config.bot.self_id != DEFAULT_SELF_ID && self.config.bot.self_id > 0 {
            self.config.bot.self_id
        } else if nickname != DEFAULT_BOT_SELF_NAME {
            stable_onebot_id(&format!("self:{nickname}"), DEFAULT_ONEBOT_ID_BOUND)
        } else {
            self.config.bot.self_id
        };

        if meta_nick.is_none() && nickname != DEFAULT_BOT_SELF_NAME {
            if let Err(err) = self.storage.set_meta(SELF_NICKNAME_META_KEY, &nickname) {
                warn!(error = %err, nickname = %nickname, "写入配置自账号昵称元数据失败");
            }
        }
        if user_id > 0 {
            let user_id_str = user_id.to_string();
            let existing_uid = self.storage.get_meta(SELF_USER_ID_META_KEY).ok().flatten();
            if existing_uid.as_deref() != Some(user_id_str.as_str()) {
                if let Err(err) = self.storage.set_meta(SELF_USER_ID_META_KEY, &user_id_str) {
                    warn!(error = %err, user_id, "写入自账号识别号元数据失败");
                }
            }
        }
        let _ = self
            .storage
            .reconcile_member_mapped_ids(Some(&nickname), Some(user_id));

        BotLoginInfo {
            user_id,
            nickname,
            real_qq_id,
        }
    }

    /// 从 UI 树被动提取自身头像昵称，同步到去重追踪器与 SQLite 元数据表
    pub(crate) fn register_tree_self_nicknames(&self, tree: &UiNodeSnapshot, win_rect: [i32; 4]) {
        let nicks = parser::extract_self_nicknames_from_tree(tree, win_rect);
        if nicks.is_empty() {
            return;
        }

        {
            let mut tracker = self.tracker.lock();
            tracker.add_self_nicknames(&nicks);
        }

        if let Some(primary) = nicks
            .iter()
            .map(|nick| nick.trim())
            .find(|nick| !nick.is_empty() && *nick != DEFAULT_BOT_SELF_NAME)
            .or_else(|| {
                nicks
                    .iter()
                    .map(|nick| nick.trim())
                    .find(|nick| !nick.is_empty())
            })
        {
            if let Err(err) = self.storage.set_meta(SELF_NICKNAME_META_KEY, primary) {
                warn!(error = %err, nickname = primary, "写入自账号昵称元数据失败");
            }
            let _ = self.resolve_login_info();
        }
    }

    /// 批量同步左侧会话列表摘要至 SQLite：按群特征自动分流写入 `groups` 与 `friends` 表
    pub(crate) fn upsert_session_summaries(&self, sessions: &[SessionSummary]) -> Vec<String> {
        let mut changed = Vec::new();
        let mut dirty_items = Vec::new();
        let (is_first_refresh, active_target, active_kind) = {
            let mut state = self.state.lock();
            let first = state.last_session_refresh_at.is_none();
            state.last_session_refresh_at = Some(Instant::now());
            let act = state.active_group.clone();
            let act_kind = state.active_chat_kind;

            for session in sessions {
                if !crate::matcher::is_valid_session_title(session.title.trim()) {
                    continue;
                }
                let canon = canonical_storage_group_name(&session.title);
                if canon.is_empty() || !crate::matcher::is_valid_session_title(&canon) {
                    continue;
                }

                let preview = session
                    .preview
                    .as_ref()
                    .map(|text| text.trim().to_string())
                    .filter(|text| !text.is_empty());
                let unread_count = session.unread_count.unwrap_or(0);
                let member_hint = session.member_count_hint;

                let next_snapshot = (preview.clone(), unread_count, member_hint);
                let (summary_changed, snapshot_dirty) = match state.session_snapshots.get(&canon) {
                    Some(prev) => {
                        let unread_or_preview =
                            prev.0 != next_snapshot.0 || next_snapshot.1 > prev.1;
                        let any_diff = *prev != next_snapshot;
                        (unread_or_preview, any_diff)
                    }
                    None => (next_snapshot.1 > 0, true),
                };
                if summary_changed {
                    changed.push(canon.clone());
                }
                if snapshot_dirty {
                    state.session_snapshots.insert(canon.clone(), next_snapshot);
                    dirty_items.push((
                        canon,
                        member_hint,
                        session.is_group_hint,
                        preview,
                        unread_count,
                    ));
                }
            }
            (first, act, act_kind)
        };

        let mut pending_group_upserts = Vec::new();
        let mut pending_friend_upserts = Vec::new();
        let mut evict_friend_names = Vec::new();
        for (canon, member_hint, is_group_hint, preview, unread_count) in dirty_items {
            let explicitly_whitelisted_friend =
                self.config.target.is_private_friend_allowed(&canon) && member_hint.is_none();
            let configured_target = self.config.target.group_name.trim();
            let matches_configured_group = !configured_target.is_empty()
                && crate::matcher::group_title_matches(&canon, configured_target);
            let known_as_group = lookup_group_record(&self.storage, &canon).is_some();
            let inferred_group = crate::matcher::infer_is_group_session(
                &canon,
                &canon,
                preview.as_deref(),
                member_hint,
            ) == Some(true);
            let is_group = !explicitly_whitelisted_friend
                && (member_hint.is_some()
                    || is_group_hint == Some(true)
                    || matches_configured_group
                    || known_as_group
                    || inferred_group);
            if is_group {
                evict_friend_names.push(canon.clone());
                pending_group_upserts.push(UpsertGroupInput {
                    group_name: canon,
                    real_group_id: None,
                    member_count: member_hint.map(|count| count as i32),
                    max_member_count: None,
                    last_preview: preview,
                    unread_count,
                });
            } else {
                pending_friend_upserts.push(UpsertFriendInput {
                    nickname: canon,
                    remark: String::new(),
                    real_user_id: None,
                    last_preview: preview,
                });
            }
        }

        if !pending_group_upserts.is_empty() {
            if let Err(err) = self.storage.upsert_groups_batch(&pending_group_upserts) {
                warn!(error = %err, "单事务批量同步会话列表群摘要至数据库失败");
            }
            for group_name in evict_friend_names {
                let _ = self.storage.delete_friend_by_name(&group_name);
            }
        }
        if !pending_friend_upserts.is_empty() {
            if let Err(err) = self.storage.upsert_friends_batch(&pending_friend_upserts) {
                warn!(error = %err, "单事务批量同步会话列表好友摘要至数据库失败");
            } else {
                let login = self.resolve_login_info();
                let _ = self
                    .storage
                    .reconcile_member_mapped_ids(Some(&login.nickname), Some(login.user_id));
            }
        }

        // 首次会话同步后释放状态锁，顺带清除旧版本单行拼接群名及错写入好友表的群聊污染行
        if is_first_refresh {
            self.purge_polluted_session_rows();
        }

        self.scheduler.lock().observe_sessions(
            now_utc_ms(),
            sessions,
            &self.config.target,
            active_target.as_deref(),
            active_kind,
        );

        changed
    }

    /// 清理早期单行会话拼接导致的无真实群号、零成员污染群名行，以及错写入好友表的群聊记录
    fn purge_polluted_session_rows(&self) {
        if let Ok(groups) = self.storage.list_groups() {
            for group in groups {
                let is_whitelisted_friend = self
                    .config
                    .target
                    .is_private_friend_allowed(&group.group_name);
                if group.real_group_id.is_none()
                    && group.member_count == 0
                    && (is_whitelisted_friend
                        || SESSION_INLINE_SPLIT_REGEX.is_match(group.group_name.trim()))
                {
                    if let Err(err) = self
                        .storage
                        .delete_transient_group_by_name(&group.group_name)
                    {
                        warn!(error = %err, group = %group.group_name, "清理污染群名缓存行失败");
                    }
                    continue;
                }
                if !is_whitelisted_friend {
                    let _ = self.storage.delete_friend_by_name(&group.group_name);
                }
            }
        }

        if let Ok(friends) = self.storage.list_friends() {
            for friend in friends {
                if self
                    .config
                    .target
                    .is_private_friend_allowed(&friend.nickname)
                {
                    continue;
                }
                let Ok(Some(rec)) = self.storage.get_friend_by_name(&friend.nickname) else {
                    continue;
                };
                if crate::matcher::infer_is_group_session(
                    &rec.nickname,
                    &rec.nickname,
                    rec.last_preview.as_deref(),
                    None,
                ) == Some(true)
                {
                    let _ = self.storage.upsert_group(&UpsertGroupInput {
                        group_name: rec.nickname.clone(),
                        real_group_id: None,
                        member_count: None,
                        max_member_count: None,
                        last_preview: rec.last_preview.clone(),
                        unread_count: 0,
                    });
                    let _ = self.storage.delete_friend_by_name(&rec.nickname);
                }
            }
        }
    }

    /// 解析或登记群元数据并返回生效数字群号（优先真实群号），同时保留既有未读数与抽屉属性
    pub(crate) fn resolve_or_create_group_id(
        &self,
        raw_group_name: &str,
        member_count: Option<u32>,
    ) -> i64 {
        let canon = canonical_storage_group_name(raw_group_name);
        if canon.is_empty() {
            return stable_onebot_id(raw_group_name, DEFAULT_ONEBOT_ID_BOUND);
        }

        let desired_count = member_count.map(|count| count as i32);
        if let Some(existing) = lookup_group_record(&self.storage, raw_group_name) {
            if desired_count.is_some() && existing.member_count != desired_count {
                let update = UpsertGroupInput {
                    group_name: existing.group_name.clone(),
                    real_group_id: existing.real_group_id,
                    member_count: desired_count,
                    max_member_count: existing.max_member_count,
                    last_preview: existing.last_preview.clone(),
                    unread_count: existing.unread_count,
                };
                if let Err(err) = self.storage.upsert_group(&update) {
                    warn!(error = %err, group = %existing.group_name, "更新群成员数缓存失败");
                }
            }
            return existing.effective_group_id();
        }

        let insert = UpsertGroupInput {
            group_name: canon.clone(),
            real_group_id: None,
            member_count: desired_count,
            max_member_count: None,
            last_preview: None,
            unread_count: 0,
        };
        match self.storage.upsert_group(&insert) {
            Ok(record) => record.effective_group_id(),
            Err(err) => {
                warn!(error = %err, group = %canon, "初始化群元数据失败，回退至确定性散列群号");
                stable_onebot_id(&canon, DEFAULT_ONEBOT_ID_BOUND)
            }
        }
    }

    /// 解析或登记私聊好友元数据并返回生效数字用户标识（优先真实 QQ 号）
    pub(crate) fn resolve_or_create_friend_id(&self, raw_friend_name: &str) -> i64 {
        let canon = canonical_storage_group_name(raw_friend_name);
        if canon.is_empty() {
            return stable_onebot_id(
                &format!("friend:{raw_friend_name}"),
                DEFAULT_ONEBOT_ID_BOUND,
            );
        }

        if let Ok(Some(existing)) = self.storage.get_friend_by_name(&canon) {
            return existing.effective_user_id();
        }
        let trimmed = raw_friend_name.trim();
        if !trimmed.is_empty() && trimmed != canon {
            if let Ok(Some(existing)) = self.storage.get_friend_by_name(trimmed) {
                return existing.effective_user_id();
            }
        }

        let insert = UpsertFriendInput {
            nickname: canon.clone(),
            remark: String::new(),
            real_user_id: None,
            last_preview: None,
        };
        match self.storage.upsert_friend(&insert) {
            Ok(record) => record.effective_user_id(),
            Err(err) => {
                warn!(error = %err, friend = %canon, "初始化私聊好友元数据失败，回退至确定性散列用户号");
                stable_onebot_id(&format!("friend:{canon}"), DEFAULT_ONEBOT_ID_BOUND)
            }
        }
    }

    /// 将一批群聊消息幂等写入 SQLite，并回填每条消息的 `onebot_message_id`、`group_id` 与 `user_id`
    pub(crate) fn persist_messages_for_group(
        &self,
        raw_group_name: &str,
        member_count: Option<u32>,
        messages: &mut [ChatMessage],
    ) -> i64 {
        self.persist_messages_with_kind(
            raw_group_name,
            ChatTargetKind::Group,
            member_count,
            messages,
        )
    }

    /// 将一批私聊消息幂等写入 SQLite，并回填每条消息的 `onebot_message_id`、`user_id` 与结构化消息段
    pub(crate) fn persist_messages_for_private(
        &self,
        raw_friend_name: &str,
        messages: &mut [ChatMessage],
    ) -> i64 {
        self.persist_messages_with_kind(raw_friend_name, ChatTargetKind::Private, None, messages)
    }

    /// 解析目标会话规范名称、目标实体数字编号与群成员列表，复用批量及单条持久化前置上下文
    fn resolve_session_persistence_target(
        &self,
        raw_session_name: &str,
        chat_kind: ChatTargetKind,
        member_count: Option<u32>,
    ) -> (String, i64, Vec<GroupMemberInfo>) {
        let canon = canonical_storage_group_name(raw_session_name);
        match chat_kind {
            ChatTargetKind::Group => {
                let gid = self.resolve_or_create_group_id(raw_session_name, member_count);
                let mems = self
                    .storage
                    .list_group_members(&canon, gid)
                    .unwrap_or_default();
                (canon, gid, mems)
            }
            ChatTargetKind::Private => {
                let fid = self.resolve_or_create_friend_id(raw_session_name);
                (canon, fid, Vec::new())
            }
        }
    }

    /// 统一持久化群聊或私聊消息批次并返回目标实体数字编号（群号或私聊好友用户号）
    fn persist_messages_with_kind(
        &self,
        raw_session_name: &str,
        chat_kind: ChatTargetKind,
        member_count: Option<u32>,
        messages: &mut [ChatMessage],
    ) -> i64 {
        let (canon, target_entity_id, members) =
            self.resolve_session_persistence_target(raw_session_name, chat_kind, member_count);
        if messages.is_empty() {
            return target_entity_id;
        }

        let self_user_id = self.resolve_login_info().user_id;
        let batch_inputs: Vec<RecordMessageInput> = messages
            .iter()
            .map(|msg| {
                build_record_message_input(
                    &canon,
                    chat_kind,
                    target_entity_id,
                    self_user_id,
                    &members,
                    msg,
                )
            })
            .collect();

        let assigned = match self.storage.record_messages_batch(&batch_inputs) {
            Ok(ids) => Some(ids),
            Err(err) => {
                warn!(error = %err, count = batch_inputs.len(), "单事务批量持久化聊天消息失败");
                None
            }
        };

        let mut self_onebot_ids = Vec::new();
        for (idx, (msg, input)) in messages.iter_mut().zip(batch_inputs.iter()).enumerate() {
            msg.target_kind = chat_kind;
            msg.user_id = Some(input.user_id);
            msg.group_id = match chat_kind {
                ChatTargetKind::Group => Some(target_entity_id),
                ChatTargetKind::Private => None,
            };
            if let Some(ref ids) = assigned {
                if let Some(onebot_msg_id) = ids.get(idx).copied() {
                    msg.onebot_message_id = Some(onebot_msg_id);
                    if msg.is_self {
                        self_onebot_ids.push(onebot_msg_id);
                    }
                }
            }
        }
        if !self_onebot_ids.is_empty() {
            let mut tracker = self.tracker.lock();
            for id in self_onebot_ids {
                tracker.record_confirmed_self_onebot_id(id);
            }
        }

        target_entity_id
    }

    /// 持久化已确认回显成功的出站执行计划（优先使用 UI 原生消息 UID）并返回 `(onebot_message_id, target_entity_id, stored_uid)`
    pub(crate) fn persist_confirmed_sent_plan(
        &self,
        target_name: &str,
        target_kind: ChatTargetKind,
        plan: &OutboundSendPlan,
        verified_msg: Option<&ChatMessage>,
        member_count: Option<u32>,
    ) -> (Option<i64>, i64, String) {
        let ui_uid = verified_msg
            .map(|msg| msg.message_id.trim())
            .filter(|uid| !uid.is_empty());
        let ts_hint = verified_msg.and_then(|msg| msg.timestamp_hint.clone());
        self.persist_confirmed_sent_inner(
            target_name,
            target_kind,
            &plan.summary_text,
            &plan.canonical_segments,
            (member_count, ui_uid, ts_hint),
        )
    }

    /// 持久化已确认回显成功的本端发出消息并返回 `(onebot_message_id, target_entity_id)`
    #[cfg(test)]
    pub(crate) fn persist_confirmed_sent_message(
        &self,
        target_name: &str,
        target_kind: ChatTargetKind,
        text: &str,
        segments: &[MessageSegment],
        member_count: Option<u32>,
    ) -> (Option<i64>, i64) {
        let (onebot_id, entity_id, _) = self.persist_confirmed_sent_inner(
            target_name,
            target_kind,
            text,
            segments,
            (member_count, None, None),
        );
        (onebot_id, entity_id)
    }

    fn persist_confirmed_sent_inner(
        &self,
        target_name: &str,
        target_kind: ChatTargetKind,
        text: &str,
        segments: &[MessageSegment],
        extras: (Option<u32>, Option<&str>, Option<String>),
    ) -> (Option<i64>, i64, String) {
        let (member_count, explicit_uid, timestamp_hint) = extras;
        let login = self.resolve_login_info();
        let (canon, target_entity_id, members) =
            self.resolve_session_persistence_target(target_name, target_kind, member_count);
        let captured_at_ms = now_utc_ms();
        let uid = if let Some(real_uid) = explicit_uid {
            real_uid.to_string()
        } else {
            let seq = self.outbound_seq.fetch_add(1, Ordering::Relaxed) + 1;
            let outbound_salt = format!("out:{captured_at_ms}:{seq}");
            parser::compute_message_id(&canon, &login.nickname, Some(&outbound_salt), text, 0)
        };
        let synthetic = ChatMessage {
            message_id: uid.clone(),
            group_name: canon.clone(),
            sender: login.nickname,
            content: text.to_string(),
            timestamp_hint,
            is_self: true,
            captured_at_ms,
            target_kind,
            segments: segments.to_vec(),
            ..Default::default()
        };
        let input = build_record_message_input(
            &canon,
            target_kind,
            target_entity_id,
            login.user_id,
            &members,
            &synthetic,
        );
        match self.storage.record_confirmed_outbound_message(&input) {
            Ok((new_id, stored_uid)) => {
                self.tracker.lock().record_confirmed_self_onebot_id(new_id);
                (Some(new_id), target_entity_id, stored_uid)
            }
            Err(err) => {
                warn!(error = %err, uid = %uid, "持久化已确认出站消息失败");
                (None, target_entity_id, uid)
            }
        }
    }
}

/// 按会话类型与群成员列表解析发送者数字编号，并拼装单条 `RecordMessageInput` 持久化载荷
fn build_record_message_input(
    canon: &str,
    chat_kind: ChatTargetKind,
    entity_id: i64,
    self_user_id: i64,
    members: &[GroupMemberInfo],
    msg: &ChatMessage,
) -> RecordMessageInput {
    let trimmed_sender = msg.sender.trim();
    let (group_id, user_id) = match chat_kind {
        ChatTargetKind::Private => (0, if msg.is_self { self_user_id } else { entity_id }),
        ChatTargetKind::Group => {
            let uid = if msg.is_self
                && (trimmed_sender.is_empty()
                    || trimmed_sender == DEFAULT_BOT_SELF_NAME
                    || trimmed_sender.eq_ignore_ascii_case("unknown"))
            {
                self_user_id
            } else if let Some(member_uid) = find_member_user_id(members, trimmed_sender) {
                member_uid
            } else if msg.is_self {
                self_user_id
            } else {
                stable_onebot_id(
                    &format!("member:{canon}:{trimmed_sender}"),
                    DEFAULT_ONEBOT_ID_BOUND,
                )
            };
            (entity_id, uid)
        }
    };
    let stored_segments = if msg.segments.is_empty() && !msg.content.is_empty() {
        vec![MessageSegment::Text {
            text: msg.content.clone(),
        }]
    } else {
        msg.segments.clone()
    };
    let raw_message = if has_rich_segments(&stored_segments) {
        segments_to_cq_string(&stored_segments)
    } else {
        msg.content.clone()
    };
    RecordMessageInput {
        message_uid: msg.message_id.clone(),
        group_name: canon.to_string(),
        group_id,
        user_id,
        sender_name: msg.sender.clone(),
        raw_message,
        is_self: msg.is_self,
        timestamp_hint: msg.timestamp_hint.clone(),
        captured_at_ms: msg.captured_at_ms,
        chat_type: chat_kind,
        segments: stored_segments,
    }
}

/// 按版本号幂等清理旧版会话列表解析误写入 SQLite 的非有效群聊与好友污染行
fn run_session_pollution_migration_v2(storage: &Storage) {
    if storage
        .get_meta(MIGRATION_SESSION_POLLUTION_V2_KEY)
        .ok()
        .flatten()
        .is_some()
    {
        return;
    }
    if let Err(err) = storage.purge_invalid_friends(crate::matcher::is_valid_session_title) {
        warn!(error = %err, "启动迁移清理污染好友记录失败");
    }
    if let Err(err) = storage.purge_invalid_groups(crate::matcher::is_valid_session_title) {
        warn!(error = %err, "启动迁移清理污染群聊记录失败");
    }
    if let Err(err) = storage.set_meta(MIGRATION_SESSION_POLLUTION_V2_KEY, "1") {
        warn!(error = %err, "写入会话污染清理迁移标记失败");
    }
}

/// 按版本号幂等清理旧版 DPI 裁切生成的 `<hash>_<idx>.png` 媒体缓存文件以便重新清晰截切
pub(crate) fn purge_stale_media_crop_files_in_dir(storage: &Storage, media_dir: &Path) -> usize {
    if storage
        .get_meta(MIGRATION_MEDIA_CROP_DPI_V2_KEY)
        .ok()
        .flatten()
        .is_some()
    {
        return 0;
    }
    let mut removed = 0usize;
    if media_dir.exists() {
        if let Ok(entries) = std::fs::read_dir(media_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                let is_png = path
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .is_some_and(|ext| ext.eq_ignore_ascii_case(MEDIA_PNG_EXTENSION));
                let has_underscore_stem = path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .is_some_and(|stem| stem.contains('_'));
                if is_png && has_underscore_stem && std::fs::remove_file(&path).is_ok() {
                    removed += 1;
                }
            }
        }
    }
    if let Err(err) = storage.set_meta(MIGRATION_MEDIA_CROP_DPI_V2_KEY, "1") {
        warn!(error = %err, "写入媒体截图 DPI 迁移标记失败");
    }
    removed
}

/// 规范化用于 SQLite 索引的群名称（剥离括号成员人数后缀并保留兜底文本）
pub(crate) fn canonical_storage_group_name(raw: &str) -> String {
    let canonical = parser::canonical_group_name(raw);
    if canonical.is_empty() {
        raw.trim().to_string()
    } else {
        canonical
    }
}

/// 在 SQLite 中按规范群名及原始群名两级查找群元数据记录
pub(crate) fn lookup_group_record(
    storage: &Storage,
    raw_group_name: &str,
) -> Option<StoredGroupRecord> {
    let canon = canonical_storage_group_name(raw_group_name);
    if let Ok(Some(record)) = storage.get_group_by_name(&canon) {
        return Some(record);
    }
    let trimmed = raw_group_name.trim();
    if !trimmed.is_empty() && trimmed != canon {
        if let Ok(Some(record)) = storage.get_group_by_name(trimmed) {
            return Some(record);
        }
    }
    None
}

/// 在当前群成员列表中按昵称或群名片匹配发送者并返回生效数字用户标识
pub(crate) fn find_member_user_id(members: &[GroupMemberInfo], sender: &str) -> Option<i64> {
    if sender.is_empty() {
        return None;
    }
    members
        .iter()
        .find(|member| {
            member.nickname == sender || (!member.card.is_empty() && member.card == sender)
        })
        .map(|member| member.real_user_id.unwrap_or(member.user_id))
}

/// 获取当前 Unix UTC 毫秒时间戳
pub(crate) fn now_utc_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|dur| dur.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use common_types::StorageConfig;
    use storage::constants::MEMORY_DB_PATH;

    #[test]
    fn persist_confirmed_sent_plan_allocates_distinct_ids_across_anchor_and_collision() {
        let cfg = AdapterConfig {
            storage: StorageConfig {
                db_path: MEMORY_DB_PATH.to_string(),
            },
            ..Default::default()
        };
        let storage = Arc::new(Storage::open(&cfg.storage.db_path).expect("open memory db"));
        let (tx, _) = broadcast::channel(8);
        let shared = Arc::new(SharedInner::new(cfg, Arc::clone(&storage), tx));
        let body = "LIVE-V6-002-FWD".to_string();
        let plan = OutboundSendPlan {
            combined_text: body.clone(),
            images: Vec::new(),
            canonical_segments: vec![MessageSegment::Text { text: body.clone() }],
            summary_text: body.clone(),
            verify_norm: body.clone(),
            degraded_segments: Vec::new(),
        };
        let mk = |uid: &str, ts: Option<&str>, ms: u64| ChatMessage {
            message_id: uid.to_string(),
            group_name: "Demo测试群".to_string(),
            sender: "BotSelf".to_string(),
            content: body.clone(),
            timestamp_hint: ts.map(str::to_string),
            is_self: true,
            captured_at_ms: ms,
            target_kind: ChatTargetKind::Group,
            ..Default::default()
        };
        let anchor = mk("ui-anchor-1357", Some("13:57"), 1_700_000_000_000);
        let fwd = mk("ui-fwd-none", None, 1_700_000_010_000);
        let collide = mk("ui-anchor-1357", None, 1_700_000_020_000);

        let (id1, _, uid1) = shared.persist_confirmed_sent_plan(
            "Demo测试群",
            ChatTargetKind::Group,
            &plan,
            Some(&anchor),
            Some(5),
        );
        let (id2, _, uid2) = shared.persist_confirmed_sent_plan(
            "Demo测试群",
            ChatTargetKind::Group,
            &plan,
            Some(&fwd),
            Some(5),
        );
        let (anchor_id, fwd_id) = (id1.expect("anchor id"), id2.expect("fwd id"));
        assert_ne!(fwd_id, anchor_id);
        assert_eq!(
            (uid1.as_str(), uid2.as_str()),
            ("ui-anchor-1357", "ui-fwd-none")
        );

        // 紧随历史时间锚点之后持久化无时间头同文单条转发，经轮询对账必须保持 2 条不合并
        let mut polled = vec![anchor, fwd];
        let _ = shared.persist_messages_for_group("Demo测试群", Some(5), &mut polled);
        assert_eq!(
            (polled[0].onebot_message_id, polled[1].onebot_message_id),
            (Some(anchor_id), Some(fwd_id))
        );
        assert_eq!(storage.stats().expect("stats 2").message_count, 2);

        // 若旧气泡滚出视口导致 occurrence=0 UID 碰撞，需自动加盐并写入第 3 条全新记录
        let (id3, _, uid3) = shared.persist_confirmed_sent_plan(
            "Demo测试群",
            ChatTargetKind::Group,
            &plan,
            Some(&collide),
            Some(5),
        );
        let col_id = id3.expect("collide id");
        assert!(col_id != anchor_id && col_id != fwd_id);
        assert!(uid3.starts_with("ui-anchor-1357#out:"));
        let tracker = shared.tracker.lock();
        assert!(tracker.is_confirmed_self_onebot_id(fwd_id));
        assert!(tracker.is_confirmed_self_onebot_id(col_id));
        drop(tracker);
        let _ = shared.persist_messages_for_group("Demo测试群", Some(5), &mut polled);
        assert_eq!(storage.stats().expect("stats 3").message_count, 3);
    }
}
