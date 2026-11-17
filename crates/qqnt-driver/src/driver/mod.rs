//! 异步 `QqntDriver` 门面实现：对外暴露完整 `BotDriver` V1 与 V2 OneBot 数据面接口

mod commands;
pub(crate) mod group_handlers;
mod onebot_queries;
pub(crate) mod outbound_media;
mod poller;
pub(crate) mod private_handlers;
pub(crate) mod runtime_state;
mod segment_enricher;
pub(crate) mod send_handlers;
pub(crate) mod window_discovery;
pub(crate) mod worker_loop;

#[cfg(test)]
mod extended_tests;
#[cfg(test)]
mod tests;

use std::sync::atomic::Ordering;
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};

use async_trait::async_trait;
use common_types::{
    AdapterConfig, AdapterError, AdapterEvent, AdapterStatus, AuditEventRecord, BotDriver,
    BotLoginInfo, ChatMessage, ChatTargetKind, ForwardSendOutcome, ForwardSendRequest, FriendInfo,
    GroupInfoSummary, GroupMemberInfo, GroupTargetSpec, MessageHistoryFilter, Result,
    SchedulerConfig, SchedulerTelemetrySnapshot, SendOutcome, SendRequest, SessionSummary,
    SingleForwardRequest, StorageStats, StoredMessageRecord, UiNodeSnapshot,
    WindowDiagnosticReport, WindowInfo,
};
use parking_lot::Mutex;
use serde_json::Value;
use storage::Storage;
use tokio::sync::{broadcast, oneshot};
use tracing::debug;
use uia_core::UiaSession;

use commands::{DriverCommand, PriorityCommandQueue};
use poller::run_background_poller;
use runtime_state::{now_utc_ms, SharedInner};
use worker_loop::{run_worker_loop, WorkerContext};

use crate::constants::{BROADCAST_CAPACITY, MAX_SEND_COOLDOWN_WAIT_MS};
use crate::humanizer;

/// 基于独立 COM 工作线程与嵌入式 SQLite 存储的 QQNT 异步机器人驱动门面
pub struct QqntDriver {
    shared: Arc<SharedInner>,
    cmd_queue: Arc<PriorityCommandQueue>,
    worker_join: Mutex<Option<JoinHandle<()>>>,
}

/// 使用外部存储与登录态上下文对已解析消息切片执行结构化富媒体段提取（供回放测试与离线归档使用）
pub fn enrich_parsed_messages_with_storage(
    storage: &Storage,
    session_key: &str,
    chat_kind: ChatTargetKind,
    bot_login: &BotLoginInfo,
    items: &mut [crate::parser::ParsedTreeMessage],
) {
    let ctx = segment_enricher::EnrichContext {
        storage,
        session_key,
        chat_kind,
        hwnd: 0,
        members: &[],
        bot_login,
    };
    segment_enricher::enrich_parsed_messages(&ctx, items);
}

impl QqntDriver {
    /// 初始化数据库、拟人化器与消息追踪器，并启动专用 COM UIA 工作线程
    pub fn new(config: AdapterConfig) -> Result<Self> {
        let storage = Arc::new(Storage::open(&config.storage.db_path)?);
        let (event_tx, _) = broadcast::channel(BROADCAST_CAPACITY);

        let shared = Arc::new(SharedInner::new(config, storage, event_tx));

        let cmd_queue = Arc::new(PriorityCommandQueue::new());
        let worker_cmd_queue = Arc::clone(&cmd_queue);
        let (init_tx, init_rx) = mpsc::sync_channel::<Result<()>>(1);
        let worker_shared = Arc::clone(&shared);

        let worker_join = thread::Builder::new()
            .name("qqnt-uia-worker".to_string())
            .spawn(move || {
                let session = match UiaSession::new() {
                    Ok(session) => {
                        let _ = init_tx.send(Ok(()));
                        session
                    }
                    Err(err) => {
                        let _ = init_tx.send(Err(err));
                        return;
                    }
                };
                let ctx = WorkerContext {
                    session,
                    verified_cache: None,
                };
                run_worker_loop(ctx, &worker_shared, worker_cmd_queue);
            })
            .map_err(AdapterError::Io)?;

        init_rx.recv().map_err(|err| {
            AdapterError::UiaFailure(format!("UIA worker thread failed during startup: {err}"))
        })??;

        let driver = Self {
            shared,
            cmd_queue,
            worker_join: Mutex::new(Some(worker_join)),
        };

        driver.ensure_poller_started();
        Ok(driver)
    }

    /// 惰性确保在存在 Tokio 运行时上下文且未熔断时拉起后台事件驱动与保底轮询协程
    fn ensure_poller_started(&self) {
        if self.shared.shutdown.load(Ordering::Acquire) {
            return;
        }
        if self.shared.state.lock().security_breaker_tripped {
            return;
        }
        if self.shared.poller_running.load(Ordering::Acquire) {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        if self
            .shared
            .poller_running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }

        let shared = Arc::clone(&self.shared);
        let cmd_queue = Arc::clone(&self.cmd_queue);
        handle.spawn(async move {
            run_background_poller(shared, cmd_queue).await;
        });
    }

    /// 投递一条控制指令至后台 COM 工作线程并异步等待执行结果
    async fn dispatch<T>(
        &self,
        build: impl FnOnce(oneshot::Sender<Result<T>>) -> DriverCommand,
    ) -> Result<T> {
        self.ensure_poller_started();
        let (tx, rx) = oneshot::channel();
        self.cmd_queue
            .push(build(tx))
            .map_err(|_| AdapterError::UiaFailure("UIA worker thread has stopped".to_string()))?;
        rx.await.map_err(|_| {
            AdapterError::UiaFailure("UIA worker dropped response channel".to_string())
        })?
    }

    /// 按拟人化节流窗口执行冷却等待与发前拟人停顿（供文本与多选合并转发共用）
    async fn wait_send_humanizer_gate(&self) -> Result<()> {
        let cooldown_wait_ms = self.shared.humanizer.lock().remaining_cooldown_ms();
        if cooldown_wait_ms > 0 && cooldown_wait_ms <= MAX_SEND_COOLDOWN_WAIT_MS {
            tokio::select! {
                _ = self.shared.wait_shutdown() => {
                    return Err(AdapterError::UiaFailure("driver shutting down".to_string()));
                }
                _ = tokio::time::sleep(std::time::Duration::from_millis(cooldown_wait_ms)) => {}
            }
        }

        let pause = {
            let mut humanizer = self.shared.humanizer.lock();
            humanizer.check_cooldown()?;
            humanizer.mark_sent();
            humanizer.sample_pre_typing_pause()
        };

        tokio::select! {
            _ = self.shared.wait_shutdown() => {
                return Err(AdapterError::UiaFailure("driver shutting down".to_string()));
            }
            _ = tokio::time::sleep(pause) => {}
        }
        Ok(())
    }

    /// 发送全流程编排：异步预校验白名单与远端 HTTP 图片拉取后，经冷却停顿投递至 COM 线程
    async fn execute_send(&self, mut req: SendRequest) -> Result<SendOutcome> {
        self.shared
            .check_outbound_deadline(&req.group, req.deadline_unix_ms)?;
        if req.target_kind == ChatTargetKind::Private {
            private_handlers::verify_private_whitelist(&self.shared, &req.group)?;
        }

        let mut segments = outbound_media::extract_effective_segments_checked(
            &req,
            self.shared.config.onebot.allow_degraded_media_summary,
        )?;
        outbound_media::prefetch_http_image_segments(&mut segments).await?;
        req.segments = segments;

        let _send_guard = self.shared.send_gate.lock().await;
        self.shared
            .check_outbound_deadline(&req.group, req.deadline_unix_ms)?;
        self.dispatch(DriverCommand::CheckSecurity).await?;
        self.wait_send_humanizer_gate().await?;
        self.shared
            .check_outbound_deadline(&req.group, req.deadline_unix_ms)?;

        let outcome = match req.target_kind {
            ChatTargetKind::Private => {
                self.dispatch(|resp| DriverCommand::SendPrivateMessage { req, resp })
                    .await?
            }
            ChatTargetKind::Group => {
                self.dispatch(|resp| DriverCommand::SendGroupMessage { req, resp })
                    .await?
            }
        };

        drop(_send_guard);

        if outcome.ok && outcome.onebot_message_id.is_some() {
            self.shared
                .emit_event(AdapterEvent::MessageSent(outcome.clone()));
        }
        let status = self.shared.status_snapshot();
        let report = humanizer::build_send_report(&outcome, &status);
        if let Err(err) = humanizer::write_latest_report(&report) {
            debug!(error = %err, "写入最新发送状态报告失败，已忽略非致命错误");
        }

        Ok(outcome)
    }
}

impl Drop for QqntDriver {
    fn drop(&mut self) {
        self.shared.signal_shutdown();
        self.cmd_queue.close_with_shutdown();
        if let Some(handle) = self.worker_join.lock().take() {
            let _ = handle.join();
        }
    }
}

#[async_trait]
impl BotDriver for QqntDriver {
    async fn status(&self) -> Result<AdapterStatus> {
        self.dispatch(DriverCommand::Status).await
    }

    fn cached_status(&self) -> Option<AdapterStatus> {
        Some(self.shared.status_snapshot())
    }

    async fn list_windows(&self) -> Result<Vec<WindowInfo>> {
        self.dispatch(DriverCommand::ListWindows).await
    }

    async fn ensure_qq_window(&self) -> Result<Vec<WindowInfo>> {
        self.dispatch(|resp| DriverCommand::EnsureQqWindow {
            force_wake: false,
            resp,
        })
        .await
    }

    async fn ensure_qq_window_ext(&self, force_wake: bool) -> Result<Vec<WindowInfo>> {
        self.dispatch(|resp| DriverCommand::EnsureQqWindow { force_wake, resp })
            .await
    }

    async fn diagnose_window(&self, hwnd: Option<isize>) -> Result<WindowDiagnosticReport> {
        self.dispatch(|resp| DriverCommand::DiagnoseWindow { hwnd, resp })
            .await
    }

    async fn inspect_tree(&self, hwnd: Option<isize>, max_depth: usize) -> Result<UiNodeSnapshot> {
        self.dispatch(|resp| DriverCommand::InspectTree {
            hwnd,
            max_depth,
            resp,
        })
        .await
    }

    async fn list_sessions(&self) -> Result<Vec<SessionSummary>> {
        self.dispatch(DriverCommand::ListSessions).await
    }

    async fn open_group(&self, spec: GroupTargetSpec) -> Result<AdapterStatus> {
        let status = self
            .dispatch(|resp| DriverCommand::OpenGroup { spec, resp })
            .await?;
        let report =
            humanizer::build_open_report(&status, "group opened and member guard verified");
        if let Err(err) = humanizer::write_latest_report(&report) {
            debug!(error = %err, "写入开群结果报告失败，已忽略非致命错误");
        }
        self.shared
            .emit_event(AdapterEvent::StatusChanged(status.clone()));
        Ok(status)
    }

    async fn open_private(&self, friend_name: &str) -> Result<AdapterStatus> {
        private_handlers::verify_private_whitelist(&self.shared, friend_name)?;
        let status = self
            .dispatch(|resp| DriverCommand::OpenPrivate {
                target: friend_name.to_string(),
                resp,
            })
            .await?;
        self.shared
            .emit_event(AdapterEvent::StatusChanged(status.clone()));
        Ok(status)
    }

    async fn poll_messages(&self, group: &str, limit: usize) -> Result<Vec<ChatMessage>> {
        let (all, new_incoming) = self
            .dispatch(|resp| DriverCommand::PollMessages {
                group: group.to_string(),
                limit,
                allow_unocclude: true,
                resp,
            })
            .await?;
        let selected = self.shared.select_incoming_events(new_incoming);
        let self_id = self.shared.resolve_login_info().user_id;
        for msg in selected {
            match msg.target_kind {
                ChatTargetKind::Private => {
                    self.shared
                        .emit_event(AdapterEvent::PrivateMessage(msg.clone()));
                }
                ChatTargetKind::Group => {
                    self.shared
                        .emit_event(AdapterEvent::GroupMessage(msg.clone()));
                }
            }
            poller::emit_inbound_file_upload_notices(&self.shared, &msg, self_id);
        }
        Ok(all)
    }

    async fn send_group_message(&self, mut req: SendRequest) -> Result<SendOutcome> {
        req.target_kind = ChatTargetKind::Group;
        self.execute_send(req).await
    }

    async fn send_private_message(&self, mut req: SendRequest) -> Result<SendOutcome> {
        req.target_kind = ChatTargetKind::Private;
        self.execute_send(req).await
    }

    fn subscribe_events(&self) -> broadcast::Receiver<AdapterEvent> {
        self.ensure_poller_started();
        self.shared.event_tx.subscribe()
    }

    async fn get_login_info(&self) -> Result<BotLoginInfo> {
        Ok(self.shared.resolve_login_info())
    }

    async fn get_group_list(&self, no_cache: bool) -> Result<Vec<GroupInfoSummary>> {
        self.dispatch(|resp| DriverCommand::GetGroupList { no_cache, resp })
            .await
    }

    async fn get_friend_list(&self, no_cache: bool) -> Result<Vec<FriendInfo>> {
        self.dispatch(|resp| DriverCommand::GetFriendList { no_cache, resp })
            .await
    }

    async fn get_group_info(
        &self,
        group_id: Option<i64>,
        group_name: Option<String>,
        no_cache: bool,
    ) -> Result<GroupInfoSummary> {
        self.dispatch(|resp| DriverCommand::GetGroupInfo {
            group_id,
            group_name,
            no_cache,
            resp,
        })
        .await
    }

    async fn get_group_member_list(
        &self,
        group_id: Option<i64>,
        group_name: Option<String>,
        no_cache: bool,
    ) -> Result<Vec<GroupMemberInfo>> {
        self.dispatch(|resp| DriverCommand::GetGroupMemberList {
            group_id,
            group_name,
            no_cache,
            resp,
        })
        .await
    }

    async fn get_message(&self, onebot_message_id: i64) -> Result<Option<StoredMessageRecord>> {
        self.shared.storage.get_message_by_id(onebot_message_id)
    }

    async fn get_recent_messages(
        &self,
        session_name: &str,
        limit: usize,
    ) -> Result<Vec<StoredMessageRecord>> {
        self.get_recent_messages_by_kind(session_name, None, limit)
            .await
    }

    async fn get_recent_messages_by_kind(
        &self,
        session_name: &str,
        chat_type: Option<ChatTargetKind>,
        limit: usize,
    ) -> Result<Vec<StoredMessageRecord>> {
        let trimmed = session_name.trim();
        let canon = runtime_state::canonical_storage_group_name(trimmed);
        let query_key = if canon.is_empty() {
            trimmed
        } else {
            canon.as_str()
        };
        let mut records = self
            .shared
            .storage
            .list_recent_messages_by_kind(query_key, chat_type, limit)?;
        if records.is_empty() && !trimmed.is_empty() && trimmed != query_key {
            records = self
                .shared
                .storage
                .list_recent_messages_by_kind(trimmed, chat_type, limit)?;
        }
        Ok(records)
    }

    async fn save_forward_nodes(&self, forward_id: &str, nodes_json: &str) -> Result<()> {
        let key = format!("forward:{}", forward_id.trim());
        self.shared.storage.set_meta(&key, nodes_json)
    }

    async fn get_forward_nodes(&self, forward_id: &str) -> Result<Option<String>> {
        let key = format!("forward:{}", forward_id.trim());
        self.shared.storage.get_meta(&key)
    }

    async fn get_stranger_info(&self, user_id: i64) -> Result<Option<FriendInfo>> {
        if let Some(friend) = self.shared.storage.get_friend_by_id(user_id)? {
            return Ok(Some(FriendInfo {
                user_id: friend.effective_user_id(),
                real_user_id: friend.real_user_id,
                nickname: friend.nickname,
                remark: friend.remark,
            }));
        }
        for group in self.shared.storage.list_groups()? {
            let eff_group_id = group.real_group_id.unwrap_or(group.group_id);
            let members = self
                .shared
                .storage
                .list_group_members(&group.group_name, eff_group_id)?;
            if let Some(member) = members
                .into_iter()
                .find(|item| item.user_id == user_id || item.real_user_id == Some(user_id))
            {
                let nickname = if !member.nickname.is_empty() {
                    member.nickname
                } else {
                    member.card.clone()
                };
                return Ok(Some(FriendInfo {
                    user_id: member.real_user_id.unwrap_or(member.user_id),
                    real_user_id: member.real_user_id,
                    nickname,
                    remark: member.card,
                }));
            }
        }
        Ok(None)
    }

    async fn resolve_group_name(&self, group_id: i64) -> Result<Option<String>> {
        Ok(self
            .shared
            .storage
            .get_group_by_id(group_id)?
            .map(|record| record.group_name))
    }

    async fn resolve_private_target(&self, user_id: i64) -> Result<Option<String>> {
        let Some(record) = self.shared.storage.get_friend_by_id(user_id)? else {
            return Ok(None);
        };
        let target = if !record.nickname.trim().is_empty() {
            record.nickname
        } else {
            record.remark
        };
        Ok((!target.trim().is_empty()).then_some(target))
    }

    async fn resolve_group_member_name(
        &self,
        group_name: &str,
        user_id: i64,
    ) -> Result<Option<String>> {
        let canon = runtime_state::canonical_storage_group_name(group_name);
        if canon.is_empty() {
            return Ok(None);
        }
        let group_id = self.shared.resolve_or_create_group_id(&canon, None);
        let Some(member) = self
            .shared
            .storage
            .get_group_member_by_id(&canon, user_id, group_id)?
        else {
            return Ok(None);
        };
        let display = if !member.card.trim().is_empty() {
            member.card
        } else {
            member.nickname
        };
        Ok((!display.trim().is_empty()).then_some(display))
    }

    async fn storage_stats(&self) -> Result<StorageStats> {
        self.shared.storage.stats()
    }

    async fn send_forward_messages(&self, req: ForwardSendRequest) -> Result<ForwardSendOutcome> {
        if req.target_kind == ChatTargetKind::Private {
            private_handlers::verify_private_whitelist(&self.shared, &req.target)?;
        }
        let _send_guard = self.shared.send_gate.lock().await;
        self.dispatch(DriverCommand::CheckSecurity).await?;
        self.wait_send_humanizer_gate().await?;
        let outcome = self
            .dispatch(|resp| DriverCommand::SendForwardMessages { req, resp })
            .await?;
        drop(_send_guard);

        if outcome.message_id > 0 {
            let summary = self
                .shared
                .storage
                .get_message_by_id(outcome.message_id)
                .ok()
                .flatten()
                .map(|rec| rec.raw_message)
                .filter(|raw| !raw.trim().is_empty())
                .unwrap_or_else(|| format!("[CQ:forward,id={}]", outcome.forward_id));
            self.shared
                .emit_event(AdapterEvent::MessageSent(SendOutcome {
                    ok: true,
                    group: outcome.target.clone(),
                    used_mode: common_types::InputMode::FocusRestoreUnicode,
                    foreground_touched: true,
                    mouse_touched: true,
                    verified_in_chat: true,
                    detail: summary,
                    onebot_message_id: Some(outcome.message_id),
                    group_id: outcome.group_id,
                    target_kind: outcome.target_kind,
                    user_id: outcome.user_id,
                    ..Default::default()
                }));
        }
        Ok(outcome)
    }

    async fn allocate_forward_nodes(&self, nodes_json: &str) -> Result<String> {
        self.shared.storage.allocate_forward_nodes(nodes_json)
    }

    async fn capture_screenshot(&self, hwnd: Option<isize>) -> Result<Vec<u8>> {
        self.dispatch(|resp| DriverCommand::CaptureScreenshot { hwnd, resp })
            .await
    }

    async fn probe_ui_action(
        &self,
        action: &str,
        target: Option<String>,
        query: Option<String>,
    ) -> Result<Value> {
        let _send_guard = self.shared.send_gate.lock().await;
        self.dispatch(|resp| DriverCommand::ProbeUiAction {
            action: action.to_string(),
            target,
            query,
            resp,
        })
        .await
    }

    async fn record_audit_event(&self, event_kind: &str, detail_json: &str) -> Result<()> {
        self.shared.storage.append_audit(event_kind, detail_json)
    }

    async fn delete_message(&self, onebot_message_id: i64) -> Result<()> {
        let _send_guard = self.shared.send_gate.lock().await;
        self.dispatch(DriverCommand::CheckSecurity).await?;
        self.wait_send_humanizer_gate().await?;
        self.dispatch(|resp| DriverCommand::DeleteMessage {
            onebot_message_id,
            resp,
        })
        .await
    }

    async fn scheduler_telemetry(&self) -> Result<SchedulerTelemetrySnapshot> {
        Ok(self.shared.scheduler_telemetry_snapshot())
    }

    async fn scheduler_config(&self) -> Result<SchedulerConfig> {
        Ok(self.shared.scheduler.lock().config())
    }

    async fn update_scheduler_config(&self, config: SchedulerConfig) -> Result<SchedulerConfig> {
        let mut scheduler = self.shared.scheduler.lock();
        scheduler.update_config(config, now_utc_ms());
        Ok(scheduler.config())
    }

    async fn mark_session_read(&self, target: &str, target_kind: ChatTargetKind) -> Result<()> {
        if target_kind == ChatTargetKind::Private {
            private_handlers::verify_private_whitelist(&self.shared, target)?;
        }
        let _send_guard = self.shared.send_gate.lock().await;
        self.dispatch(DriverCommand::CheckSecurity).await?;
        self.dispatch(|resp| DriverCommand::MarkSessionRead {
            target: target.to_string(),
            target_kind,
            resp,
        })
        .await
    }

    async fn ocr_image(
        &self,
        image: &str,
        language: Option<&str>,
    ) -> Result<common_types::OcrImageResponse> {
        onebot_queries::handle_ocr_image(image, language).await
    }

    fn record_outbound_deadline_abort(&self) {
        self.shared.record_outbound_deadline_abort();
    }

    async fn query_messages_history(
        &self,
        mut filter: MessageHistoryFilter,
    ) -> Result<Vec<StoredMessageRecord>> {
        let trimmed = filter.session_name.trim();
        if !trimmed.is_empty() {
            let canon = runtime_state::canonical_storage_group_name(trimmed);
            filter.session_name = if canon.is_empty() {
                trimmed.to_string()
            } else {
                canon
            };
        } else {
            filter.session_name.clear();
        }
        self.shared.storage.query_messages_filtered(&filter)
    }

    async fn list_audit_events(
        &self,
        event_kind: Option<&str>,
        limit: usize,
    ) -> Result<Vec<AuditEventRecord>> {
        self.shared.storage.list_audit_events(event_kind, limit)
    }

    async fn reset_security_breaker(&self) -> Result<AdapterStatus> {
        let _send_guard = self.shared.send_gate.lock().await;
        let status = self.dispatch(DriverCommand::ResetSecurityBreaker).await?;
        drop(_send_guard);
        self.ensure_poller_started();
        Ok(status)
    }

    async fn forward_single_message(&self, req: SingleForwardRequest) -> Result<SendOutcome> {
        self.shared
            .check_outbound_deadline(&req.target, req.deadline_unix_ms)?;
        if req.message_id <= 0 {
            return Err(AdapterError::InputRejected(
                "forward message_id must be a positive integer".to_string(),
            ));
        }
        if req.target_kind == ChatTargetKind::Private {
            private_handlers::verify_private_whitelist(&self.shared, &req.target)?;
        }
        let _send_guard = self.shared.send_gate.lock().await;
        self.shared
            .check_outbound_deadline(&req.target, req.deadline_unix_ms)?;
        self.dispatch(DriverCommand::CheckSecurity).await?;
        self.wait_send_humanizer_gate().await?;
        self.shared
            .check_outbound_deadline(&req.target, req.deadline_unix_ms)?;
        self.dispatch(|resp| DriverCommand::ForwardSingleMessage { req, resp })
            .await
    }
}
