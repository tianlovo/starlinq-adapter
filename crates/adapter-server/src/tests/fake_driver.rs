//! 测试模拟驱动桩模块（Mock / Fake Object）：模拟 QQNT 状态、群列表、成员、历史消息与安全熔断

use std::collections::HashMap;

use async_trait::async_trait;
use common_types::{
    canonical_group_names_match, segments_to_cq_string, AdapterError, AdapterEvent, AdapterStatus,
    AuditEventRecord, BotDriver, BotLoginInfo, ChatMessage, ChatTargetKind, ForwardSendOutcome,
    ForwardSendRequest, FriendInfo, GroupInfoSummary, GroupMemberInfo, GroupTargetSpec, InputMode,
    MessageSegment, OcrImageResponse, OcrPoint, OcrTextDetection, Result, SchedulerConfig,
    SchedulerTelemetrySnapshot, SendOutcome, SendRequest, SessionSummary, SingleForwardRequest,
    StorageStats, StoredMessageRecord, UiNodeSnapshot, WakeReport, WindowDiagnosticReport,
    WindowInfo,
};
use parking_lot::Mutex;
use serde_json::{json, Value};
use tokio::sync::broadcast;

const MOCK_GROUP_ID: i64 = 555_005;
const MOCK_GROUP_NAME: &str = "Demo测试群 (5)";
const MOCK_FRIEND_ID: i64 = 777_001;
const MOCK_FRIEND_NAME: &str = "测试好友";

/// 模拟驱动内部状态容器，集中记录已发送消息、诊断调用、白名单与存储消息列表
struct FakeDriverState {
    next_error: Option<AdapterError>,
    force_unconfirmed_send: bool,
    sent: Vec<SendRequest>,
    ensure_force_wakes: Vec<bool>,
    diag_requests: Vec<Option<isize>>,
    group_list_no_cache_calls: Vec<bool>,
    open_group_calls: usize,
    open_private_calls: Vec<String>,
    stored_messages: Vec<StoredMessageRecord>,
    forward_nodes: HashMap<String, String>,
    allowed_private_friends: Vec<String>,
    next_message_seq: i64,
    next_forward_seq: i64,
    audit_events: Vec<(String, String)>,
    scheduler_config: SchedulerConfig,
    scheduler_telemetry: SchedulerTelemetrySnapshot,
    mark_read_calls: Vec<(String, ChatTargetKind)>,
    outbound_deadline_aborted: u64,
    ocr_calls: Vec<(String, Option<String>)>,
    extra_sessions: Vec<SessionSummary>,
    extra_groups: Vec<GroupInfoSummary>,
    security_dialog_open: bool,
    forward_single_calls: Vec<SingleForwardRequest>,
}

/// 用于 HTTP 与 OneBot 集成测试的内存驱动桩实现
pub(super) struct FakeDriver {
    status: Mutex<AdapterStatus>,
    inner: Mutex<FakeDriverState>,
    event_sender: broadcast::Sender<AdapterEvent>,
}

impl FakeDriver {
    /// 构建包含默认群组、成员与初始状态的测试驱动桩
    pub(super) fn new() -> Self {
        let (event_sender, _) = broadcast::channel(64);
        let initial_record = StoredMessageRecord {
            onebot_message_id: 10_001,
            message_uid: "m-1".to_string(),
            group_name: MOCK_GROUP_NAME.to_string(),
            group_id: MOCK_GROUP_ID,
            user_id: 20_001,
            sender_name: "群友A".to_string(),
            raw_message: "hello".to_string(),
            chat_type: ChatTargetKind::Group,
            segments: vec![MessageSegment::Text {
                text: "hello".to_string(),
            }],
            is_self: false,
            timestamp_hint: Some("14:00".to_string()),
            captured_at_ms: 1_700_000_000_000,
        };
        Self {
            status: Mutex::new(AdapterStatus {
                desktop_name: "Default".to_string(),
                sandbox_isolated: false,
                qq_pids: vec![1234],
                resolved_qq_path: Some(r"C:\Program Files\Tencent\QQ\QQNT\QQ.exe".to_string()),
                input_mode: InputMode::Auto,
                messages_observed: 1,
                ..Default::default()
            }),
            inner: Mutex::new(FakeDriverState {
                next_error: None,
                force_unconfirmed_send: false,
                sent: Vec::new(),
                ensure_force_wakes: Vec::new(),
                diag_requests: Vec::new(),
                group_list_no_cache_calls: Vec::new(),
                open_group_calls: 0,
                open_private_calls: Vec::new(),
                stored_messages: vec![initial_record],
                forward_nodes: HashMap::new(),
                allowed_private_friends: Vec::new(),
                next_message_seq: 10_002,
                next_forward_seq: 10_000,
                audit_events: Vec::new(),
                scheduler_config: SchedulerConfig::default(),
                scheduler_telemetry: SchedulerTelemetrySnapshot::default(),
                mark_read_calls: Vec::new(),
                outbound_deadline_aborted: 0,
                ocr_calls: Vec::new(),
                extra_sessions: Vec::new(),
                extra_groups: Vec::new(),
                security_dialog_open: false,
                forward_single_calls: Vec::new(),
            }),
            event_sender,
        }
    }

    pub(super) fn allow_private_friend(&self, friend_name: &str) {
        self.inner
            .lock()
            .allowed_private_friends
            .push(friend_name.to_string());
    }

    pub(super) fn set_next_error(&self, error: AdapterError) {
        self.inner.lock().next_error = Some(error);
    }

    pub(super) fn set_force_unconfirmed_send(&self, enabled: bool) {
        self.inner.lock().force_unconfirmed_send = enabled;
    }

    pub(super) fn sent_requests(&self) -> Vec<SendRequest> {
        self.inner.lock().sent.clone()
    }

    pub(super) fn ensure_force_wake_calls(&self) -> Vec<bool> {
        self.inner.lock().ensure_force_wakes.clone()
    }

    pub(super) fn diag_request_calls(&self) -> Vec<Option<isize>> {
        self.inner.lock().diag_requests.clone()
    }

    pub(super) fn group_list_no_cache_calls(&self) -> Vec<bool> {
        self.inner.lock().group_list_no_cache_calls.clone()
    }

    pub(super) fn open_group_call_count(&self) -> usize {
        self.inner.lock().open_group_calls
    }

    pub(super) fn open_private_calls(&self) -> Vec<String> {
        self.inner.lock().open_private_calls.clone()
    }

    pub(super) fn broadcast_event(&self, event: AdapterEvent) {
        let _ = self.event_sender.send(event);
    }

    pub(super) fn audit_events(&self) -> Vec<(String, String)> {
        self.inner.lock().audit_events.clone()
    }

    pub(super) fn stored_forward_count(&self) -> usize {
        self.inner.lock().forward_nodes.len()
    }

    pub(super) fn set_scheduler_telemetry(&self, telemetry: SchedulerTelemetrySnapshot) {
        self.inner.lock().scheduler_telemetry = telemetry;
    }

    pub(super) fn ocr_calls(&self) -> Vec<(String, Option<String>)> {
        self.inner.lock().ocr_calls.clone()
    }

    pub(super) fn insert_stored_message(&self, record: StoredMessageRecord) {
        self.inner.lock().stored_messages.push(record);
    }

    pub(super) fn mark_read_calls(&self) -> Vec<(String, ChatTargetKind)> {
        self.inner.lock().mark_read_calls.clone()
    }

    pub(super) fn outbound_deadline_abort_count(&self) -> u64 {
        self.inner.lock().outbound_deadline_aborted
    }

    pub(super) fn add_session(&self, session: SessionSummary) {
        self.inner.lock().extra_sessions.push(session);
    }

    pub(super) fn add_group_info(&self, group: GroupInfoSummary) {
        self.inner.lock().extra_groups.push(group);
    }

    pub(super) fn set_security_breaker_state(
        &self,
        tripped: bool,
        dialog_open: bool,
        reason: Option<&str>,
    ) {
        self.inner.lock().security_dialog_open = dialog_open;
        let mut st = self.status.lock();
        st.security_breaker_tripped = tripped;
        st.security_breaker_reason = reason.map(ToOwned::to_owned);
    }

    pub(super) fn forward_single_calls(&self) -> Vec<SingleForwardRequest> {
        self.inner.lock().forward_single_calls.clone()
    }

    fn take_error(&self) -> Result<()> {
        if let Some(error) = self.inner.lock().next_error.take() {
            return Err(error);
        }
        Ok(())
    }

    fn mock_group_summary() -> GroupInfoSummary {
        GroupInfoSummary {
            group_id: MOCK_GROUP_ID,
            real_group_id: Some(987_654_321),
            group_name: MOCK_GROUP_NAME.to_string(),
            member_count: 5,
            max_member_count: 500,
        }
    }

    fn is_friend_allowed(&self, target: &str) -> bool {
        let guard = self.inner.lock();
        guard
            .allowed_private_friends
            .iter()
            .any(|item| item == target)
    }

    fn push_self_stored_message(
        guard: &mut FakeDriverState,
        uid_prefix: &str,
        target: &str,
        target_kind: ChatTargetKind,
        raw_message: String,
        segments: Vec<MessageSegment>,
    ) -> i64 {
        let is_private = target_kind == ChatTargetKind::Private;
        let allocated = guard.next_message_seq;
        guard.next_message_seq += 1;
        guard.stored_messages.push(StoredMessageRecord {
            onebot_message_id: allocated,
            message_uid: format!("{uid_prefix}-{allocated}"),
            group_name: target.to_string(),
            group_id: if is_private {
                MOCK_FRIEND_ID
            } else {
                MOCK_GROUP_ID
            },
            user_id: if is_private { MOCK_FRIEND_ID } else { 10_001 },
            sender_name: "BotSelf".to_string(),
            raw_message,
            chat_type: target_kind,
            segments,
            is_self: true,
            timestamp_hint: Some("14:05".to_string()),
            captured_at_ms: 1_700_000_005_000 + allocated as u64,
        });
        allocated
    }

    fn record_sent_message(&self, request: SendRequest) -> SendOutcome {
        if self.inner.lock().force_unconfirmed_send {
            return SendOutcome {
                ok: false,
                group: request.group,
                verified_in_chat: false,
                detail: "forced unconfirmed send in test".to_string(),
                onebot_message_id: None,
                target_kind: request.target_kind,
                ..Default::default()
            };
        }
        let is_private = request.target_kind == ChatTargetKind::Private;
        let stored_raw = if request.segments.is_empty() {
            request.text.clone()
        } else {
            segments_to_cq_string(&request.segments)
        };
        let allocated_id = {
            let mut inner_guard = self.inner.lock();
            inner_guard.sent.push(request.clone());
            Self::push_self_stored_message(
                &mut inner_guard,
                "sent",
                &request.group,
                request.target_kind,
                stored_raw,
                request.segments.clone(),
            )
        };

        self.status.lock().messages_sent += 1;
        let outcome = SendOutcome {
            ok: true,
            group: request.group,
            used_mode: InputMode::BackgroundUia,
            foreground_touched: false,
            mouse_touched: false,
            verified_in_chat: true,
            elapsed_ms: 25,
            detail: "sent".to_string(),
            onebot_message_id: Some(allocated_id),
            group_id: (!is_private).then_some(MOCK_GROUP_ID),
            user_id: is_private.then_some(MOCK_FRIEND_ID),
            target_kind: request.target_kind,
            degraded_segments: Vec::new(),
        };
        let _ = self
            .event_sender
            .send(AdapterEvent::MessageSent(outcome.clone()));
        outcome
    }

    fn mock_members() -> Vec<GroupMemberInfo> {
        vec![
            GroupMemberInfo {
                group_id: MOCK_GROUP_ID,
                user_id: 10_001,
                real_user_id: Some(111_222_333),
                nickname: "BotSelf".to_string(),
                card: "机器人".to_string(),
                role: "owner".to_string(),
            },
            GroupMemberInfo {
                group_id: MOCK_GROUP_ID,
                user_id: 20_001,
                real_user_id: Some(444_555_666),
                nickname: "群友A".to_string(),
                card: "测试群友".to_string(),
                role: "member".to_string(),
            },
        ]
    }
}

#[async_trait]
impl BotDriver for FakeDriver {
    async fn status(&self) -> Result<AdapterStatus> {
        Ok(self.status.lock().clone())
    }

    fn cached_status(&self) -> Option<AdapterStatus> {
        Some(self.status.lock().clone())
    }

    async fn list_windows(&self) -> Result<Vec<WindowInfo>> {
        self.take_error()?;
        Ok(vec![WindowInfo {
            hwnd: 42,
            pid: 1234,
            process_name: "QQ.exe".to_string(),
            class_name: "MockQQNTWndClass".to_string(),
            title: "QQ".to_string(),
            is_visible: true,
            is_iconic: false,
            rect: [0, 0, 800, 640],
            is_standalone_chat: false,
        }])
    }

    async fn ensure_qq_window_ext(&self, force_wake: bool) -> Result<Vec<WindowInfo>> {
        self.inner.lock().ensure_force_wakes.push(force_wake);
        self.list_windows().await
    }

    async fn diagnose_window(&self, hwnd: Option<isize>) -> Result<WindowDiagnosticReport> {
        self.inner.lock().diag_requests.push(hwnd);
        self.take_error()?;
        Ok(WindowDiagnosticReport {
            target_hwnd: hwnd.unwrap_or(42),
            wake_report: WakeReport {
                visited_msaa_nodes: 7,
                ..Default::default()
            },
            uia_control_count: 11,
            uia_semantic_count: 5,
            uia_raw_count: 13,
            uia_find_all_count: 9,
            sample_semantic_nodes: vec!["d=1 Document name=QQ".to_string()],
            ..Default::default()
        })
    }

    async fn inspect_tree(
        &self,
        _hwnd: Option<isize>,
        _max_depth: usize,
    ) -> Result<UiNodeSnapshot> {
        self.take_error()?;
        Ok(UiNodeSnapshot {
            control_type: "Window".to_string(),
            name: "QQ".to_string(),
            class_name: "MockQQNTWndClass".to_string(),
            ..Default::default()
        })
    }

    async fn list_sessions(&self) -> Result<Vec<SessionSummary>> {
        self.take_error()?;
        let mut list = vec![SessionSummary {
            title: MOCK_GROUP_NAME.to_string(),
            raw_name: MOCK_GROUP_NAME.to_string(),
            member_count_hint: Some(5),
            ..Default::default()
        }];
        list.extend(self.inner.lock().extra_sessions.clone());
        Ok(list)
    }

    async fn open_group(&self, spec: GroupTargetSpec) -> Result<AdapterStatus> {
        self.take_error()?;
        self.inner.lock().open_group_calls += 1;
        if let Some(limit) = spec.strict_below_members {
            if limit <= 5 {
                return Err(AdapterError::MemberCountExceeded {
                    group: spec.group_name,
                    detected: Some(5),
                    limit,
                });
            }
        }
        let mut status_guard = self.status.lock();
        status_guard.attached_window = Some(WindowInfo {
            hwnd: 42,
            pid: 1234,
            process_name: "QQ.exe".to_string(),
            class_name: "MockQQNTWndClass".to_string(),
            title: "QQ".to_string(),
            is_visible: true,
            is_iconic: false,
            rect: [0, 0, 800, 640],
            is_standalone_chat: false,
        });
        status_guard.active_chat_kind = Some(ChatTargetKind::Group);
        status_guard.active_group = Some(format!("{} (5)", spec.group_name));
        status_guard.detected_member_count = Some(5);
        Ok(status_guard.clone())
    }

    async fn open_private(&self, friend_name: &str) -> Result<AdapterStatus> {
        self.take_error()?;
        self.inner
            .lock()
            .open_private_calls
            .push(friend_name.to_string());
        if !self.is_friend_allowed(friend_name) {
            return Err(AdapterError::PrivateTargetNotAllowed {
                target: friend_name.to_string(),
            });
        }
        let mut status_guard = self.status.lock();
        status_guard.attached_window = Some(WindowInfo {
            hwnd: 42,
            pid: 1234,
            process_name: "QQ.exe".to_string(),
            class_name: "MockQQNTWndClass".to_string(),
            title: "QQ".to_string(),
            is_visible: true,
            is_iconic: false,
            rect: [0, 0, 800, 640],
            is_standalone_chat: false,
        });
        status_guard.active_chat_kind = Some(ChatTargetKind::Private);
        status_guard.active_group = Some(friend_name.to_string());
        status_guard.active_group_id = Some(MOCK_FRIEND_ID);
        status_guard.active_real_group_id = None;
        status_guard.detected_member_count = None;
        Ok(status_guard.clone())
    }

    async fn poll_messages(&self, group: &str, _limit: usize) -> Result<Vec<ChatMessage>> {
        self.take_error()?;
        Ok(vec![ChatMessage {
            message_id: "m-1".to_string(),
            group_name: group.to_string(),
            sender: "群友A".to_string(),
            content: "hello".to_string(),
            timestamp_hint: Some("14:00".to_string()),
            is_self: false,
            captured_at_ms: 1_700_000_000_000,
            onebot_message_id: Some(10_001),
            group_id: Some(MOCK_GROUP_ID),
            user_id: Some(20_001),
            target_kind: ChatTargetKind::Group,
            segments: vec![MessageSegment::Text {
                text: "hello".to_string(),
            }],
            ..Default::default()
        }])
    }

    async fn send_group_message(&self, request: SendRequest) -> Result<SendOutcome> {
        self.take_error()?;
        Ok(self.record_sent_message(request))
    }

    async fn send_private_message(&self, mut request: SendRequest) -> Result<SendOutcome> {
        self.take_error()?;
        if !self.is_friend_allowed(&request.group) {
            return Err(AdapterError::PrivateTargetNotAllowed {
                target: request.group,
            });
        }
        request.target_kind = ChatTargetKind::Private;
        Ok(self.record_sent_message(request))
    }

    fn subscribe_events(&self) -> broadcast::Receiver<AdapterEvent> {
        self.event_sender.subscribe()
    }

    async fn get_login_info(&self) -> Result<BotLoginInfo> {
        self.take_error()?;
        Ok(BotLoginInfo {
            user_id: 10_001,
            nickname: "BotSelf".to_string(),
            real_qq_id: Some(111_222_333),
        })
    }

    async fn get_group_list(&self, no_cache: bool) -> Result<Vec<GroupInfoSummary>> {
        let extra = {
            let mut guard = self.inner.lock();
            guard.group_list_no_cache_calls.push(no_cache);
            guard.extra_groups.clone()
        };
        self.take_error()?;
        let mut groups = vec![Self::mock_group_summary()];
        groups.extend(extra);
        Ok(groups)
    }

    async fn get_friend_list(&self, _no_cache: bool) -> Result<Vec<FriendInfo>> {
        self.take_error()?;
        Ok(vec![FriendInfo {
            user_id: MOCK_FRIEND_ID,
            real_user_id: Some(888_777_666),
            nickname: MOCK_FRIEND_NAME.to_string(),
            remark: "测试备注".to_string(),
        }])
    }

    async fn get_group_info(
        &self,
        group_id: Option<i64>,
        group_name: Option<String>,
        _no_cache: bool,
    ) -> Result<GroupInfoSummary> {
        self.take_error()?;
        let summary = Self::mock_group_summary();
        let id_ok =
            group_id.is_none_or(|id| id == summary.group_id || summary.real_group_id == Some(id));
        let name_ok = group_name
            .as_deref()
            .is_none_or(|name| summary.group_name.contains(name));
        if id_ok && name_ok {
            Ok(summary)
        } else {
            Err(AdapterError::GroupNotFound {
                group: group_name.unwrap_or_else(|| format!("{group_id:?}")),
            })
        }
    }

    async fn get_group_member_list(
        &self,
        _group_id: Option<i64>,
        _group_name: Option<String>,
        _no_cache: bool,
    ) -> Result<Vec<GroupMemberInfo>> {
        self.take_error()?;
        Ok(Self::mock_members())
    }

    async fn get_message(&self, onebot_message_id: i64) -> Result<Option<StoredMessageRecord>> {
        self.take_error()?;
        Ok(self
            .inner
            .lock()
            .stored_messages
            .iter()
            .find(|item| item.onebot_message_id == onebot_message_id)
            .cloned())
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
        self.take_error()?;
        let trimmed = session_name.trim();
        let mut matched: Vec<StoredMessageRecord> = self
            .inner
            .lock()
            .stored_messages
            .iter()
            .filter(|item| {
                (trimmed.is_empty() || canonical_group_names_match(&item.group_name, trimmed))
                    && chat_type.is_none_or(|kind| item.chat_type == kind)
            })
            .cloned()
            .collect();
        if matched.len() > limit {
            let start = matched.len() - limit;
            matched = matched.split_off(start);
        }
        Ok(matched)
    }

    async fn save_forward_nodes(&self, forward_id: &str, nodes_json: &str) -> Result<()> {
        self.take_error()?;
        let mut guard = self.inner.lock();
        if guard.forward_nodes.len() >= 512 && !guard.forward_nodes.contains_key(forward_id) {
            if let Some(evict_key) = guard.forward_nodes.keys().min().cloned() {
                guard.forward_nodes.remove(&evict_key);
            }
        }
        guard
            .forward_nodes
            .insert(forward_id.to_string(), nodes_json.to_string());
        Ok(())
    }

    async fn get_forward_nodes(&self, forward_id: &str) -> Result<Option<String>> {
        self.take_error()?;
        Ok(self.inner.lock().forward_nodes.get(forward_id).cloned())
    }

    async fn get_stranger_info(&self, user_id: i64) -> Result<Option<FriendInfo>> {
        self.take_error()?;
        if user_id == MOCK_FRIEND_ID || user_id == 888_777_666 {
            return Ok(Some(FriendInfo {
                user_id: MOCK_FRIEND_ID,
                real_user_id: Some(888_777_666),
                nickname: MOCK_FRIEND_NAME.to_string(),
                remark: "测试备注".to_string(),
            }));
        }
        Ok(Self::mock_members()
            .into_iter()
            .find(|member| member.user_id == user_id || member.real_user_id == Some(user_id))
            .map(|member| FriendInfo {
                user_id: member.user_id,
                real_user_id: member.real_user_id,
                nickname: member.nickname,
                remark: member.card,
            }))
    }

    async fn resolve_group_name(&self, group_id: i64) -> Result<Option<String>> {
        self.take_error()?;
        if group_id == MOCK_GROUP_ID || group_id == 987_654_321 {
            Ok(Some(MOCK_GROUP_NAME.to_string()))
        } else {
            Ok(None)
        }
    }

    async fn resolve_private_target(&self, user_id: i64) -> Result<Option<String>> {
        self.take_error()?;
        if user_id == MOCK_FRIEND_ID || user_id == 888_777_666 {
            Ok(Some(MOCK_FRIEND_NAME.to_string()))
        } else {
            Ok(None)
        }
    }

    async fn resolve_group_member_name(
        &self,
        _group_name: &str,
        user_id: i64,
    ) -> Result<Option<String>> {
        self.take_error()?;
        Ok(Self::mock_members()
            .into_iter()
            .find(|item| item.user_id == user_id || item.real_user_id == Some(user_id))
            .map(|item| item.nickname))
    }

    async fn storage_stats(&self) -> Result<StorageStats> {
        self.take_error()?;
        let guard = self.inner.lock();
        Ok(StorageStats {
            db_path: ":memory:".to_string(),
            group_count: 1,
            friend_count: 1,
            member_count: 2,
            message_count: guard.stored_messages.len() as u64,
            audit_count: 1 + guard.audit_events.len() as u64,
        })
    }

    async fn allocate_forward_nodes(&self, nodes_json: &str) -> Result<String> {
        self.take_error()?;
        let mut guard = self.inner.lock();
        guard.next_forward_seq += 1;
        let forward_id = format!("fwd-{}", guard.next_forward_seq);
        guard
            .forward_nodes
            .insert(forward_id.clone(), nodes_json.to_string());
        Ok(forward_id)
    }

    async fn send_forward_messages(&self, req: ForwardSendRequest) -> Result<ForwardSendOutcome> {
        self.take_error()?;
        let mut guard = self.inner.lock();
        let mut nodes_json = Vec::with_capacity(req.message_ids.len());
        for mid in &req.message_ids {
            let rec = guard
                .stored_messages
                .iter()
                .find(|m| m.onebot_message_id == *mid)
                .ok_or_else(|| {
                    AdapterError::InputRejected(format!(
                        "referenced forward message_id={mid} not found"
                    ))
                })?;
            nodes_json.push(json!({
                "type": "node",
                "data": {
                    "id": mid.to_string(),
                    "user_id": rec.user_id.to_string(),
                    "nickname": rec.sender_name,
                    "content": rec.raw_message,
                }
            }));
        }
        guard.next_forward_seq += 1;
        let forward_id = format!("fwd-{}", guard.next_forward_seq);
        let serialized = serde_json::to_string(&nodes_json).unwrap_or_else(|_| "[]".to_string());
        guard.forward_nodes.insert(forward_id.clone(), serialized);
        let allocated_id = Self::push_self_stored_message(
            &mut guard,
            "fwd-msg",
            &req.target,
            req.target_kind,
            format!("[合并转发:{forward_id}]"),
            vec![MessageSegment::Forward {
                id: forward_id.clone(),
            }],
        );
        let is_private = req.target_kind == ChatTargetKind::Private;
        Ok(ForwardSendOutcome {
            message_id: allocated_id,
            forward_id,
            target: req.target,
            target_kind: req.target_kind,
            group_id: (!is_private).then_some(MOCK_GROUP_ID),
            user_id: is_private.then_some(MOCK_FRIEND_ID),
        })
    }

    async fn capture_screenshot(&self, _hwnd: Option<isize>) -> Result<Vec<u8>> {
        self.take_error()?;
        Ok(vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A])
    }

    async fn probe_ui_action(
        &self,
        action: &str,
        target: Option<String>,
        query: Option<String>,
    ) -> Result<Value> {
        self.take_error()?;
        Ok(json!({
            "action": action,
            "target": target,
            "query": query,
            "mock": true,
        }))
    }

    async fn record_audit_event(&self, event_kind: &str, detail_json: &str) -> Result<()> {
        self.inner
            .lock()
            .audit_events
            .push((event_kind.to_string(), detail_json.to_string()));
        Ok(())
    }

    async fn delete_message(&self, onebot_message_id: i64) -> Result<()> {
        self.take_error()?;
        if onebot_message_id <= 0 {
            return Err(AdapterError::InputRejected(
                "message_id must be a positive integer".to_string(),
            ));
        }

        let removed = {
            let mut inner_guard = self.inner.lock();
            let pos = inner_guard
                .stored_messages
                .iter()
                .position(|item| item.onebot_message_id == onebot_message_id)
                .ok_or_else(|| {
                    AdapterError::InputRejected(format!("message_id={onebot_message_id} not found"))
                })?;
            if !inner_guard.stored_messages[pos].is_self {
                return Err(AdapterError::UnsupportedAction {
                    action: "delete_msg".to_string(),
                    reason: format!(
                        "message_id={onebot_message_id} is not self-sent or cannot be recalled"
                    ),
                });
            }
            let record = inner_guard.stored_messages.remove(pos);
            inner_guard.audit_events.push((
                "message_recalled".to_string(),
                json!({
                    "message_id": onebot_message_id,
                    "chat_type": record.chat_type,
                    "target": record.group_name,
                })
                .to_string(),
            ));
            record
        };

        let self_id = self.get_login_info().await?.user_id;
        let notice = match removed.chat_type {
            ChatTargetKind::Group => json!({
                "time": 1_700_000_010_i64,
                "self_id": self_id,
                "post_type": "notice",
                "notice_type": "group_recall",
                "group_id": removed.group_id,
                "user_id": removed.user_id,
                "operator_id": self_id,
                "message_id": onebot_message_id,
            }),
            ChatTargetKind::Private => json!({
                "time": 1_700_000_010_i64,
                "self_id": self_id,
                "post_type": "notice",
                "notice_type": "friend_recall",
                "user_id": removed.user_id,
                "message_id": onebot_message_id,
            }),
        };
        let _ = self.event_sender.send(AdapterEvent::Notice(notice));
        Ok(())
    }

    async fn scheduler_telemetry(&self) -> Result<SchedulerTelemetrySnapshot> {
        self.take_error()?;
        let guard = self.inner.lock();
        Ok(guard.scheduler_telemetry.clone())
    }

    async fn scheduler_config(&self) -> Result<SchedulerConfig> {
        self.take_error()?;
        Ok(self.inner.lock().scheduler_config.clone())
    }

    async fn update_scheduler_config(&self, config: SchedulerConfig) -> Result<SchedulerConfig> {
        self.take_error()?;
        self.inner.lock().scheduler_config = config.clone();
        Ok(config)
    }

    async fn mark_session_read(&self, target: &str, target_kind: ChatTargetKind) -> Result<()> {
        self.take_error()?;
        self.inner
            .lock()
            .mark_read_calls
            .push((target.to_string(), target_kind));
        Ok(())
    }

    fn record_outbound_deadline_abort(&self) {
        let mut guard = self.inner.lock();
        guard.outbound_deadline_aborted += 1;
        guard.scheduler_telemetry.outbound_deadline_aborted_total += 1;
    }

    async fn ocr_image(&self, image: &str, language: Option<&str>) -> Result<OcrImageResponse> {
        self.take_error()?;
        let lang = language
            .map(str::trim)
            .filter(|val| !val.is_empty())
            .unwrap_or("zh-Hans-CN")
            .to_string();
        self.inner
            .lock()
            .ocr_calls
            .push((image.to_string(), language.map(str::to_string)));
        Ok(OcrImageResponse {
            texts: vec![OcrTextDetection {
                text: "MOCK OCR 2026".to_string(),
                confidence: 95,
                coordinates: vec![
                    OcrPoint { x: 10, y: 12 },
                    OcrPoint { x: 120, y: 12 },
                    OcrPoint { x: 120, y: 36 },
                    OcrPoint { x: 10, y: 36 },
                ],
            }],
            language: lang,
        })
    }

    async fn list_audit_events(
        &self,
        event_kind: Option<&str>,
        limit: usize,
    ) -> Result<Vec<AuditEventRecord>> {
        self.take_error()?;
        let clamped = limit.clamp(1, 200);
        let clean_kind = event_kind.map(str::trim).filter(|val| !val.is_empty());
        let guard = self.inner.lock();
        Ok(guard
            .audit_events
            .iter()
            .enumerate()
            .rev()
            .filter(|(_, (kind, _))| clean_kind.is_none_or(|expected| kind == expected))
            .take(clamped)
            .map(|(idx, (kind, detail))| AuditEventRecord {
                id: (idx + 1) as i64,
                event_kind: kind.clone(),
                detail_json: detail.clone(),
                created_at_ms: 1_700_000_000_000 + idx as u64,
            })
            .collect())
    }

    async fn reset_security_breaker(&self) -> Result<AdapterStatus> {
        self.take_error()?;
        if self.inner.lock().security_dialog_open {
            self.inner.lock().audit_events.push((
                "security_breaker_reset_denied".to_string(),
                r#"{"reason":"security prompt dialog still open"}"#.to_string(),
            ));
            return Err(AdapterError::SecurityPromptDetected {
                title: "QQ安全验证".to_string(),
                detail: "security prompt dialog is still visible on desktop".to_string(),
            });
        }
        let updated = {
            let mut st = self.status.lock();
            st.security_breaker_tripped = false;
            st.security_breaker_reason = None;
            st.clone()
        };
        self.inner.lock().audit_events.push((
            "security_breaker_reset".to_string(),
            r#"{"reset":true}"#.to_string(),
        ));
        let _ = self
            .event_sender
            .send(AdapterEvent::StatusChanged(updated.clone()));
        Ok(updated)
    }

    async fn forward_single_message(&self, req: SingleForwardRequest) -> Result<SendOutcome> {
        self.take_error()?;
        if req.message_id <= 0 {
            return Err(AdapterError::InputRejected(
                "message_id must be a positive integer".to_string(),
            ));
        }
        if req.target_kind == ChatTargetKind::Private && !self.is_friend_allowed(&req.target) {
            return Err(AdapterError::PrivateTargetNotAllowed { target: req.target });
        }
        let source_rec = self
            .inner
            .lock()
            .stored_messages
            .iter()
            .find(|item| item.onebot_message_id == req.message_id)
            .cloned()
            .ok_or_else(|| {
                AdapterError::InputRejected(format!("message_id={} not found", req.message_id))
            })?;
        if source_rec.chat_type == ChatTargetKind::Private
            && !self.is_friend_allowed(&source_rec.group_name)
        {
            return Err(AdapterError::PrivateTargetNotAllowed {
                target: source_rec.group_name,
            });
        }
        if self.inner.lock().force_unconfirmed_send {
            return Ok(SendOutcome {
                ok: false,
                group: req.target,
                detail: "forced unconfirmed single forward in test".to_string(),
                target_kind: req.target_kind,
                ..Default::default()
            });
        }

        let is_private = req.target_kind == ChatTargetKind::Private;
        let allocated_id = {
            let mut guard = self.inner.lock();
            guard.forward_single_calls.push(req.clone());
            Self::push_self_stored_message(
                &mut guard,
                "fwd-single",
                &req.target,
                req.target_kind,
                source_rec.raw_message,
                source_rec.segments,
            )
        };
        self.status.lock().messages_sent += 1;
        let outcome = SendOutcome {
            ok: true,
            group: req.target,
            used_mode: InputMode::FocusRestoreUnicode,
            foreground_touched: true,
            mouse_touched: true,
            verified_in_chat: true,
            elapsed_ms: 30,
            detail: "forwarded".to_string(),
            onebot_message_id: Some(allocated_id),
            group_id: (!is_private).then_some(MOCK_GROUP_ID),
            user_id: is_private.then_some(MOCK_FRIEND_ID),
            target_kind: req.target_kind,
            degraded_segments: Vec::new(),
        };
        let _ = self
            .event_sender
            .send(AdapterEvent::MessageSent(outcome.clone()));
        Ok(outcome)
    }
}
