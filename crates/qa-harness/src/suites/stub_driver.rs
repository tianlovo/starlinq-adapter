//! 可编程测试桩驱动与临时 SQLite 数据库生命周期守卫（Stub + RAII Guard + Observer 测试载体）

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
use base64::Engine as _;
use common_types::constants::{DEFAULT_ONEBOT_ID_BOUND, DEFAULT_TARGET_GROUP_NAME};
use common_types::{
    stable_onebot_id, AdapterError, AdapterEvent, AdapterStatus, AuditEventRecord, BotDriver,
    BotLoginInfo, BurstModeState, ChatMessage, ChatTargetKind, ForwardSendOutcome,
    ForwardSendRequest, FriendInfo, GroupInfoSummary, GroupMemberInfo, GroupTargetSpec, InputMode,
    MessageSegment, OcrImageResponse, Result, SchedulerConfig, SchedulerTelemetrySnapshot,
    SendOutcome, SendRequest, SessionSummary, SingleForwardRequest, StorageStats,
    StoredMessageRecord, UiNodeSnapshot, WindowInfo,
};
use parking_lot::Mutex;
use serde_json::{json, Value};
use tokio::sync::broadcast;

/// 可注入至 `ConfigurableStubDriver` 的故障场景枚举
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StubFaultMode {
    /// 正常健康状态
    Healthy,
    /// 模拟检测到 QQ 安全验证弹窗并触发风控熔断（HTTP 423 / retcode 1423）
    SecurityBreaker,
    /// 模拟目标群人数超过安全护栏上限（HTTP 403 / retcode 1403）
    MemberCountExceeded,
    /// 模拟底层 UIA 控件定位或输入失败（HTTP 502 / retcode 1502）
    UiaFailure,
}

/// 临时 SQLite WAL 文件守卫，离开作用域时同步清理主库及 `-wal`/`-shm` 附属文件（RAII Guard）
pub struct TempDbGuard {
    db_path: PathBuf,
}

impl TempDbGuard {
    /// 构建基于进程 PID 与唯一标签的隔离临时数据库路径
    pub fn new(tag: &str) -> Self {
        let mut path = std::env::temp_dir();
        path.push(format!("qq_qa_suite_{tag}_{}.db", std::process::id()));
        Self::cleanup_files(&path);
        Self { db_path: path }
    }

    /// 返回适用于 `Storage::open` 的 UTF-8 路径字符串
    pub fn path_str(&self) -> String {
        self.db_path.to_string_lossy().into_owned()
    }

    fn cleanup_files(path: &std::path::Path) {
        let raw = path.to_string_lossy();
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(format!("{raw}-wal"));
        let _ = std::fs::remove_file(format!("{raw}-shm"));
    }
}

impl Drop for TempDbGuard {
    fn drop(&mut self) {
        Self::cleanup_files(&self.db_path);
    }
}

/// 面向打桩、边界、并发、压力与性能 SLA 检验的全功能可编程模拟 `BotDriver` 实现
pub struct ConfigurableStubDriver {
    events: broadcast::Sender<AdapterEvent>,
    fault_mode: Mutex<StubFaultMode>,
    allowed_friends: Mutex<Vec<String>>,
    sent_records: Mutex<Vec<SendRequest>>,
    forward_nodes: Mutex<HashMap<String, String>>,
    stored_messages: Mutex<HashMap<i64, StoredMessageRecord>>,
    send_delay_ms: AtomicU64,
    active_sends: AtomicUsize,
    max_concurrent_sends: AtomicUsize,
    total_sent: AtomicU64,
    msg_seq: AtomicU64,
    strict_member_guard: AtomicBool,
    force_unconfirmed_send: AtomicBool,
    scheduler_cfg: Mutex<SchedulerConfig>,
    scheduler_tel: Mutex<SchedulerTelemetrySnapshot>,
    mark_read_calls: Mutex<Vec<(String, ChatTargetKind)>>,
    outbound_deadline_aborts: AtomicU64,
    audit_events: Mutex<Vec<AuditEventRecord>>,
}

impl ConfigurableStubDriver {
    /// 创建具备大缓冲广播通道与默认 `Demo测试群` 群元数据的模拟驱动
    pub fn new() -> Arc<Self> {
        let (events, _) = broadcast::channel(2048);
        Arc::new(Self {
            events,
            fault_mode: Mutex::new(StubFaultMode::Healthy),
            allowed_friends: Mutex::new(vec!["测试好友".to_string()]),
            sent_records: Mutex::new(Vec::new()),
            forward_nodes: Mutex::new(HashMap::new()),
            stored_messages: Mutex::new(HashMap::new()),
            send_delay_ms: AtomicU64::new(0),
            active_sends: AtomicUsize::new(0),
            max_concurrent_sends: AtomicUsize::new(0),
            total_sent: AtomicU64::new(0),
            msg_seq: AtomicU64::new(1000),
            strict_member_guard: AtomicBool::new(false),
            force_unconfirmed_send: AtomicBool::new(false),
            scheduler_cfg: Mutex::new(SchedulerConfig::default()),
            scheduler_tel: Mutex::new(SchedulerTelemetrySnapshot::default()),
            mark_read_calls: Mutex::new(Vec::new()),
            outbound_deadline_aborts: AtomicU64::new(0),
            audit_events: Mutex::new(Vec::new()),
        })
    }

    /// 动态切换故障注入模式
    pub fn set_fault_mode(&self, mode: StubFaultMode) {
        *self.fault_mode.lock() = mode;
    }

    /// 控制是否返回 `ok=false, onebot_message_id=None` 的未确认发送结果
    pub fn set_force_unconfirmed_send(&self, enabled: bool) {
        self.force_unconfirmed_send
            .store(enabled, Ordering::Release);
    }

    /// 设置每次发信人工延迟（毫秒），用于并发互斥与 WS 防阻塞验证
    pub fn set_send_delay_ms(&self, delay_ms: u64) {
        self.send_delay_ms.store(delay_ms, Ordering::Release);
    }

    /// 控制是否校验 `strict_below_members` 临界值
    pub fn set_strict_member_guard(&self, enabled: bool) {
        self.strict_member_guard.store(enabled, Ordering::Release);
    }

    /// 向已连接的 WS / Webhook 订阅者广播模拟适配事件
    pub fn emit_event(&self, event: AdapterEvent) {
        let _ = self.events.send(event);
    }

    /// 返回观察到的最大并发进入 `send_group_message` 数量（用于验证 `send_gate` 互斥有效）
    pub fn max_concurrent_sends(&self) -> usize {
        self.max_concurrent_sends.load(Ordering::Acquire)
    }

    /// 读取已成功执行的发送请求历史
    pub fn sent_requests(&self) -> Vec<SendRequest> {
        self.sent_records.lock().clone()
    }

    /// 向私聊白名单追加允许访问的模拟好友名称
    pub fn allow_private_friend(&self, friend_name: &str) {
        let mut guard = self.allowed_friends.lock();
        if !guard.iter().any(|existing| existing == friend_name) {
            guard.push(friend_name.to_string());
        }
    }

    /// 预置一条模拟历史消息至内存消息表（供 `get_msg` / `mark_msg_as_read` 测试使用）
    pub fn insert_stored_message(&self, record: StoredMessageRecord) {
        self.stored_messages
            .lock()
            .insert(record.onebot_message_id, record);
    }

    /// 注入自定义洪峰调度遥测快照并同步出站超时计数基线
    pub fn set_scheduler_telemetry(&self, telemetry: SchedulerTelemetrySnapshot) {
        self.outbound_deadline_aborts
            .store(telemetry.outbound_deadline_aborted_total, Ordering::Release);
        *self.scheduler_tel.lock() = telemetry;
    }

    /// 返回因响应截止时间过期而被拦截的出站回复累计次数
    pub fn outbound_deadline_abort_count(&self) -> u64 {
        self.outbound_deadline_aborts.load(Ordering::Acquire)
    }

    /// 返回已触发的已读清理调用记录列表
    pub fn mark_read_calls(&self) -> Vec<(String, ChatTargetKind)> {
        self.mark_read_calls.lock().clone()
    }

    fn check_fault(&self) -> Result<()> {
        match *self.fault_mode.lock() {
            StubFaultMode::Healthy => Ok(()),
            StubFaultMode::SecurityBreaker => Err(AdapterError::SecurityPromptDetected {
                title: "QQ安全验证".to_string(),
                detail: "stub security circuit breaker tripped".to_string(),
            }),
            StubFaultMode::MemberCountExceeded => Err(AdapterError::MemberCountExceeded {
                group: DEFAULT_TARGET_GROUP_NAME.to_string(),
                detected: Some(12),
                limit: 10,
            }),
            StubFaultMode::UiaFailure => Err(AdapterError::UiaFailure(
                "stub uia element missing".to_string(),
            )),
        }
    }

    fn record_outbound_message(
        &self,
        mut req: SendRequest,
        chat_type: ChatTargetKind,
        group_id: i64,
        user_id: i64,
    ) -> Result<(i64, String)> {
        req.segments = enrich_outbound_segments(&req.segments)?;
        let id = (self.msg_seq.fetch_add(1, Ordering::AcqRel) + 1) as i64;
        self.total_sent.fetch_add(1, Ordering::AcqRel);
        self.sent_records.lock().push(req.clone());
        let raw_message = if req.segments.is_empty() {
            req.text.clone()
        } else {
            common_types::segments_to_cq_string(&req.segments)
        };
        let group_name = req.group.clone();
        self.stored_messages.lock().insert(
            id,
            StoredMessageRecord {
                onebot_message_id: id,
                message_uid: format!("stub-{id}"),
                group_name: group_name.clone(),
                group_id,
                user_id,
                sender_name: "QA测试机器人".to_string(),
                raw_message,
                chat_type,
                segments: req.segments,
                is_self: true,
                timestamp_hint: Some("12:00".to_string()),
                captured_at_ms: 1_700_000_000_000,
            },
        );
        Ok((id, group_name))
    }

    fn finish_stub_send(
        &self,
        req: SendRequest,
        chat_type: ChatTargetKind,
        ids: (i64, i64),
        used_mode: InputMode,
        elapsed_ms: u64,
    ) -> Result<SendOutcome> {
        let (gid, uid) = ids;
        let is_priv = chat_type == ChatTargetKind::Private;
        let is_fwd = used_mode == InputMode::FocusRestoreUnicode;
        let group_id = (!is_priv || !is_fwd).then_some(gid);
        if self.force_unconfirmed_send.load(Ordering::Acquire) {
            return Ok(SendOutcome {
                ok: false,
                group: req.group,
                used_mode,
                foreground_touched: false,
                mouse_touched: false,
                verified_in_chat: false,
                elapsed_ms,
                detail: "stub forced unconfirmed send".to_string(),
                onebot_message_id: None,
                group_id,
                target_kind: chat_type,
                user_id: is_priv.then_some(uid),
                degraded_segments: Vec::new(),
            });
        }
        let (id, group) = self.record_outbound_message(req, chat_type, gid, uid)?;
        let out = SendOutcome {
            ok: true,
            group,
            used_mode,
            foreground_touched: is_fwd,
            mouse_touched: is_fwd,
            verified_in_chat: true,
            elapsed_ms,
            detail: "stub send ok".to_string(),
            onebot_message_id: Some(id),
            group_id,
            target_kind: chat_type,
            user_id: is_priv.then_some(uid),
            degraded_segments: Vec::new(),
        };
        let _ = self.events.send(AdapterEvent::MessageSent(out.clone()));
        Ok(out)
    }
}

/// 判断媒体引用是否为禁止的远端 HTTP(S) 地址
fn is_remote_http_source(raw: &str) -> bool {
    let lower = raw.trim().to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

/// 剥离 `file://` 前缀并兼容 Windows `/C:/...` 格式
fn strip_local_file_scheme(raw: &str) -> &str {
    let trimmed = raw.trim();
    let stripped = if trimmed
        .get(..7)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("file://"))
    {
        &trimmed[7..]
    } else {
        trimmed
    };
    if cfg!(windows) && stripped.starts_with('/') && stripped.as_bytes().get(2) == Some(&b':') {
        &stripped[1..]
    } else {
        stripped
    }
}

/// 解析 `base64://` 或 `data:*;base64,` 载荷并解码字节序列
fn try_decode_inline_base64(raw: &str) -> Result<Option<Vec<u8>>> {
    let trimmed = raw.trim();
    let encoded_opt = if trimmed
        .get(..9)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("base64://"))
    {
        Some(&trimmed[9..])
    } else if trimmed
        .get(..5)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("data:"))
    {
        let lower = trimmed.to_ascii_lowercase();
        let marker = lower.find(";base64,").ok_or_else(|| {
            AdapterError::InputRejected(
                "data: URI for stub media must contain ';base64,'".to_string(),
            )
        })?;
        Some(&trimmed[marker + 8..])
    } else {
        None
    };

    let Some(encoded) = encoded_opt else {
        return Ok(None);
    };
    let compact: String = encoded
        .chars()
        .filter(|ch| !ch.is_ascii_whitespace())
        .collect();
    if compact.is_empty() {
        return Err(AdapterError::InputRejected(
            "empty base64 payload".to_string(),
        ));
    }
    let decoded = STANDARD
        .decode(&compact)
        .or_else(|_| STANDARD_NO_PAD.decode(&compact))
        .or_else(|_| URL_SAFE.decode(&compact))
        .or_else(|_| URL_SAFE_NO_PAD.decode(&compact))
        .map_err(|err| AdapterError::InputRejected(format!("invalid base64 payload: {err}")))?;
    Ok(Some(decoded))
}

/// 安全定位本地或 `.runtime` 目录下的测试媒体文件
fn locate_stub_local_path(clean_ref: &str) -> Option<PathBuf> {
    let direct = PathBuf::from(clean_ref);
    if direct.is_file() {
        return Some(direct);
    }
    if clean_ref.contains('/') || clean_ref.contains('\\') {
        return None;
    }
    for root in [".runtime/media", ".runtime/staging_uploads"] {
        let candidate = PathBuf::from(root).join(clean_ref);
        if candidate.is_file() {
            return Some(candidate);
        }
        if let Ok(entries) = std::fs::read_dir(root) {
            for entry in entries.flatten() {
                let sub = entry.path().join(clean_ref);
                if entry.path().is_dir() && sub.is_file() {
                    return Some(sub);
                }
            }
        }
    }
    None
}

/// 判断路径或 URI 是否指向 UNC 网络共享或远程主机路径（放行本地盘符长路径与 `file://localhost/`）
fn is_unc_or_network_path(raw: &str) -> bool {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return false;
    }
    if trimmed.starts_with(r"\\") {
        if let Some(after_prefix) = trimmed.strip_prefix(r"\\?\") {
            let bytes = after_prefix.as_bytes();
            let is_local_drive = bytes.len() >= 3
                && bytes[0].is_ascii_alphabetic()
                && bytes[1] == b':'
                && (bytes[2] == b'\\' || bytes[2] == b'/');
            return !is_local_drive;
        }
        return true;
    }
    if trimmed.starts_with("//") {
        return true;
    }
    if trimmed
        .get(..7)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("file://"))
    {
        let after_scheme = &trimmed[7..];
        if after_scheme.starts_with("//") || after_scheme.starts_with(r"\\") {
            return true;
        }
        if let Some(after_slash) = after_scheme.strip_prefix('/') {
            return is_unc_or_network_path(after_slash);
        }
        if after_scheme.starts_with('\\') {
            return is_unc_or_network_path(after_scheme.trim_start_matches('\\'));
        }
        if let Some((host, rest)) = after_scheme.split_once(['/', '\\']) {
            let host_trimmed = host.trim();
            if !host_trimmed.is_empty() && !host_trimmed.eq_ignore_ascii_case("localhost") {
                return true;
            }
            return rest.starts_with('/') || rest.starts_with('\\') || is_unc_or_network_path(rest);
        }
        let host_only = after_scheme.trim();
        return !host_only.is_empty() && !host_only.eq_ignore_ascii_case("localhost");
    }
    false
}

/// 校验并解析 `ocr_image` 输入引用为图像字节，阻断目录穿越、UNC 网络路径与远端 HTTP
fn resolve_stub_ocr_bytes(raw_ref: &str) -> Result<Vec<u8>> {
    let trimmed = raw_ref.trim();
    if trimmed.is_empty() || trimmed.contains("..") || trimmed.contains('\0') {
        return Err(AdapterError::InputRejected(format!(
            "invalid image reference '{raw_ref}'"
        )));
    }
    if is_remote_http_source(trimmed) {
        return Err(AdapterError::UnsupportedAction {
            action: "ocr_image".to_string(),
            reason: "strict local OCR rejects remote http(s):// image URLs; use local path, file://, base64://, or data: URI".to_string(),
        });
    }
    if let Some(bytes) = try_decode_inline_base64(trimmed)? {
        return Ok(bytes);
    }
    if is_unc_or_network_path(trimmed) {
        return Err(AdapterError::InputRejected(
            "UNC or remote network share paths are forbidden in offline mode".to_string(),
        ));
    }
    let cleaned = strip_local_file_scheme(trimmed);
    if cleaned.is_empty() || cleaned.contains("..") || cleaned.contains('\0') {
        return Err(AdapterError::InputRejected(format!(
            "invalid local image path '{raw_ref}'"
        )));
    }
    if is_unc_or_network_path(cleaned) {
        return Err(AdapterError::InputRejected(
            "UNC or remote network share paths are forbidden in offline mode".to_string(),
        ));
    }
    let path = locate_stub_local_path(cleaned).ok_or_else(|| {
        AdapterError::InputRejected(format!("local image file not found: '{cleaned}'"))
    })?;
    std::fs::read(&path).map_err(|err| {
        AdapterError::InputRejected(format!(
            "failed to read local image '{}': {err}",
            path.display()
        ))
    })
}

/// 解析出站文件段长度（用于准确记录 `MessageSegment::File.size`）
fn resolve_outbound_file_size(raw_ref: &str) -> Option<u64> {
    if let Ok(Some(bytes)) = try_decode_inline_base64(raw_ref) {
        return Some(bytes.len() as u64);
    }
    let cleaned = strip_local_file_scheme(raw_ref);
    locate_stub_local_path(cleaned)
        .and_then(|path| std::fs::metadata(path).ok())
        .map(|meta| meta.len())
}

/// 校验并补全出站 `Video`/`File` 消息段元数据，确保 `sent_records` 与历史消息准确回显
fn enrich_outbound_segments(segments: &[MessageSegment]) -> Result<Vec<MessageSegment>> {
    let mut enriched = Vec::with_capacity(segments.len());
    for seg in segments {
        match seg {
            MessageSegment::Video { file, url } => {
                let trimmed = file.trim();
                if trimmed.is_empty() || trimmed.contains("..") || trimmed.contains('\0') {
                    return Err(AdapterError::InputRejected(format!(
                        "invalid video reference '{file}'"
                    )));
                }
                if is_remote_http_source(trimmed)
                    || url.as_deref().is_some_and(is_remote_http_source)
                {
                    return Err(AdapterError::UnsupportedAction {
                        action: "send_video".to_string(),
                        reason: "strict local media policy rejects remote http(s):// video URLs"
                            .to_string(),
                    });
                }
                enriched.push(MessageSegment::Video {
                    file: trimmed.to_string(),
                    url: url.clone(),
                });
            }
            MessageSegment::File { file, name, size } => {
                let trimmed = file.trim();
                if trimmed.is_empty() || trimmed.contains("..") || trimmed.contains('\0') {
                    return Err(AdapterError::InputRejected(format!(
                        "invalid file reference '{file}'"
                    )));
                }
                if is_remote_http_source(trimmed) {
                    return Err(AdapterError::UnsupportedAction {
                        action: "send_file".to_string(),
                        reason: "strict local media policy rejects remote http(s):// file URLs"
                            .to_string(),
                    });
                }
                let resolved_size = size.or_else(|| resolve_outbound_file_size(trimmed));
                let resolved_name = name.clone().or_else(|| {
                    if trimmed
                        .get(..9)
                        .is_some_and(|p| p.eq_ignore_ascii_case("base64://"))
                        || trimmed
                            .get(..5)
                            .is_some_and(|p| p.eq_ignore_ascii_case("data:"))
                    {
                        None
                    } else {
                        std::path::Path::new(strip_local_file_scheme(trimmed))
                            .file_name()
                            .map(|fname| fname.to_string_lossy().into_owned())
                    }
                });
                enriched.push(MessageSegment::File {
                    file: trimmed.to_string(),
                    name: resolved_name,
                    size: resolved_size,
                });
            }
            other => enriched.push(other.clone()),
        }
    }
    Ok(enriched)
}

#[async_trait]
impl BotDriver for ConfigurableStubDriver {
    async fn status(&self) -> Result<AdapterStatus> {
        let fault = *self.fault_mode.lock();
        let (burst_mode_active, scheduler_pending) = {
            let tel = self.scheduler_tel.lock();
            (
                tel.burst_state == BurstModeState::BurstActive,
                tel.pending_total,
            )
        };
        Ok(AdapterStatus {
            desktop_name: "Default".to_string(),
            sandbox_isolated: false,
            qq_pids: vec![std::process::id()],
            resolved_qq_path: Some("MockQQ.exe".to_string()),
            attached_window: Some(WindowInfo {
                hwnd: 0x1024,
                pid: std::process::id(),
                process_name: "QQ.exe".to_string(),
                class_name: "MockQQNTWndClass".to_string(),
                title: "QQ".to_string(),
                is_visible: true,
                is_iconic: false,
                rect: [0, 0, 800, 640],
                is_standalone_chat: false,
            }),
            active_group: Some(DEFAULT_TARGET_GROUP_NAME.to_string()),
            active_chat_kind: Some(ChatTargetKind::Group),
            active_group_id: Some(100_000_001),
            active_real_group_id: Some(100_000_001),
            detected_member_count: Some(9),
            security_breaker_tripped: fault == StubFaultMode::SecurityBreaker,
            security_breaker_reason: (fault == StubFaultMode::SecurityBreaker)
                .then(|| "stub security breaker tripped".to_string()),
            input_mode: InputMode::BackgroundUia,
            messages_observed: 42,
            messages_sent: self.total_sent.load(Ordering::Acquire),
            burst_mode_active,
            scheduler_pending,
        })
    }

    async fn list_windows(&self) -> Result<Vec<WindowInfo>> {
        Ok(vec![WindowInfo {
            hwnd: 0x1024,
            pid: std::process::id(),
            process_name: "QQ.exe".to_string(),
            class_name: "MockQQNTWndClass".to_string(),
            title: "QQ".to_string(),
            is_visible: true,
            is_iconic: false,
            rect: [0, 0, 800, 640],
            is_standalone_chat: false,
        }])
    }

    async fn inspect_tree(
        &self,
        _hwnd: Option<isize>,
        _max_depth: usize,
    ) -> Result<UiNodeSnapshot> {
        Ok(UiNodeSnapshot::default())
    }

    async fn list_sessions(&self) -> Result<Vec<SessionSummary>> {
        Ok(vec![SessionSummary {
            title: DEFAULT_TARGET_GROUP_NAME.to_string(),
            raw_name: "Demo测试群 (9)".to_string(),
            preview: Some("stub session preview".to_string()),
            unread_count: Some(0),
            has_at_me_hint: false,
            has_at_all_hint: false,
            member_count_hint: Some(9),
            rect: [0, 0, 260, 64],
            is_group_hint: Some(true),
        }])
    }

    async fn open_group(&self, spec: GroupTargetSpec) -> Result<AdapterStatus> {
        self.check_fault()?;
        if self.strict_member_guard.load(Ordering::Acquire) {
            if let Some(limit) = spec.strict_below_members {
                if 9 >= limit {
                    return Err(AdapterError::MemberCountExceeded {
                        group: spec.group_name,
                        detected: Some(9),
                        limit,
                    });
                }
            }
        }
        self.status().await
    }

    async fn open_private(&self, friend_name: &str) -> Result<AdapterStatus> {
        self.check_fault()?;
        if !self
            .allowed_friends
            .lock()
            .iter()
            .any(|name| name == friend_name)
        {
            return Err(AdapterError::InputRejected(format!(
                "private friend '{friend_name}' not allowed in stub"
            )));
        }
        self.status().await
    }

    async fn poll_messages(&self, _group: &str, _limit: usize) -> Result<Vec<ChatMessage>> {
        self.check_fault()?;
        Ok(Vec::new())
    }

    async fn send_group_message(&self, req: SendRequest) -> Result<SendOutcome> {
        let inflight = self.active_sends.fetch_add(1, Ordering::AcqRel) + 1;
        let _ = self
            .max_concurrent_sends
            .fetch_max(inflight, Ordering::AcqRel);

        let delay = self.send_delay_ms.load(Ordering::Acquire);
        if delay > 0 {
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }
        let fault_res = self.check_fault();
        self.active_sends.fetch_sub(1, Ordering::AcqRel);
        fault_res?;

        self.finish_stub_send(
            req,
            ChatTargetKind::Group,
            (100_000_001, 10_001),
            InputMode::BackgroundUia,
            delay,
        )
    }

    async fn send_private_message(&self, req: SendRequest) -> Result<SendOutcome> {
        self.check_fault()?;
        if !self
            .allowed_friends
            .lock()
            .iter()
            .any(|name| name == &req.group)
        {
            return Err(AdapterError::InputRejected(format!(
                "private friend '{}' not allowed in stub",
                req.group
            )));
        }
        let peer_id = stable_onebot_id(&req.group, DEFAULT_ONEBOT_ID_BOUND);
        self.finish_stub_send(
            req,
            ChatTargetKind::Private,
            (peer_id, peer_id),
            InputMode::BackgroundUia,
            1,
        )
    }

    fn subscribe_events(&self) -> broadcast::Receiver<AdapterEvent> {
        self.events.subscribe()
    }

    async fn get_login_info(&self) -> Result<BotLoginInfo> {
        Ok(BotLoginInfo {
            user_id: 10_001,
            nickname: "QA测试机器人".to_string(),
            real_qq_id: Some(10_001),
        })
    }

    async fn get_group_list(&self, _no_cache: bool) -> Result<Vec<GroupInfoSummary>> {
        Ok(vec![GroupInfoSummary {
            group_id: 100_000_001,
            real_group_id: Some(100_000_001),
            group_name: DEFAULT_TARGET_GROUP_NAME.to_string(),
            member_count: 9,
            max_member_count: 200,
        }])
    }

    async fn get_friend_list(&self, _no_cache: bool) -> Result<Vec<FriendInfo>> {
        Ok(vec![FriendInfo {
            user_id: 200_000_002,
            real_user_id: Some(200_000_002),
            nickname: "测试好友".to_string(),
            remark: "测试好友".to_string(),
        }])
    }

    async fn get_group_info(
        &self,
        _group_id: Option<i64>,
        _group_name: Option<String>,
        _no_cache: bool,
    ) -> Result<GroupInfoSummary> {
        Ok(GroupInfoSummary {
            group_id: 100_000_001,
            real_group_id: Some(100_000_001),
            group_name: DEFAULT_TARGET_GROUP_NAME.to_string(),
            member_count: 9,
            max_member_count: 200,
        })
    }

    async fn get_group_member_list(
        &self,
        _group_id: Option<i64>,
        _group_name: Option<String>,
        _no_cache: bool,
    ) -> Result<Vec<GroupMemberInfo>> {
        Ok(vec![GroupMemberInfo {
            group_id: 100_000_001,
            user_id: 10_001,
            nickname: "QA群友".to_string(),
            card: "QA群友".to_string(),
            role: "owner".to_string(),
            ..Default::default()
        }])
    }

    async fn get_message(&self, onebot_message_id: i64) -> Result<Option<StoredMessageRecord>> {
        self.check_fault()?;
        Ok(self.stored_messages.lock().get(&onebot_message_id).cloned())
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
        self.check_fault()?;
        let trimmed = session_name.trim();
        let mut records: Vec<StoredMessageRecord> = self
            .stored_messages
            .lock()
            .values()
            .filter(|item| {
                (trimmed.is_empty()
                    || common_types::canonical_group_names_match(&item.group_name, trimmed))
                    && chat_type.is_none_or(|kind| item.chat_type == kind)
            })
            .cloned()
            .collect();
        records.sort_by_key(|item| item.onebot_message_id);
        if records.len() > limit {
            records = records.split_off(records.len() - limit);
        }
        Ok(records)
    }

    async fn save_forward_nodes(&self, forward_id: &str, nodes_json: &str) -> Result<()> {
        self.forward_nodes
            .lock()
            .insert(forward_id.to_string(), nodes_json.to_string());
        Ok(())
    }

    async fn get_forward_nodes(&self, forward_id: &str) -> Result<Option<String>> {
        Ok(self.forward_nodes.lock().get(forward_id).cloned())
    }

    async fn resolve_group_name(&self, group_id: i64) -> Result<Option<String>> {
        Ok((group_id == 100_000_001).then(|| DEFAULT_TARGET_GROUP_NAME.to_string()))
    }

    async fn resolve_private_target(&self, user_id: i64) -> Result<Option<String>> {
        let target_id = stable_onebot_id("测试好友", DEFAULT_ONEBOT_ID_BOUND);
        Ok((user_id == target_id || user_id == 200_000_002).then(|| "测试好友".to_string()))
    }

    async fn storage_stats(&self) -> Result<StorageStats> {
        Ok(StorageStats {
            db_path: ":memory:".to_string(),
            group_count: 1,
            member_count: 1,
            message_count: self.total_sent.load(Ordering::Acquire),
            audit_count: self.audit_events.lock().len() as u64,
            friend_count: 1,
        })
    }

    async fn allocate_forward_nodes(&self, nodes_json: &str) -> Result<String> {
        self.check_fault()?;
        let seq = self.msg_seq.fetch_add(1, Ordering::AcqRel) + 1;
        let forward_id = format!("fwd-{seq}");
        self.forward_nodes
            .lock()
            .insert(forward_id.clone(), nodes_json.to_string());
        Ok(forward_id)
    }

    async fn send_forward_messages(&self, req: ForwardSendRequest) -> Result<ForwardSendOutcome> {
        self.check_fault()?;
        let id = (self.msg_seq.fetch_add(1, Ordering::AcqRel) + 1) as i64;
        let forward_id = format!("fwd-{id}");
        let nodes = serde_json::to_string(&req.message_ids).unwrap_or_else(|_| "[]".to_string());
        self.forward_nodes.lock().insert(forward_id.clone(), nodes);
        let is_private = req.target_kind == ChatTargetKind::Private;
        Ok(ForwardSendOutcome {
            message_id: id,
            forward_id,
            target: req.target.clone(),
            target_kind: req.target_kind,
            group_id: (!is_private).then_some(100_000_001),
            user_id: is_private.then_some(stable_onebot_id(&req.target, DEFAULT_ONEBOT_ID_BOUND)),
        })
    }

    async fn capture_screenshot(&self, _hwnd: Option<isize>) -> Result<Vec<u8>> {
        self.check_fault()?;
        Ok(vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A])
    }

    async fn probe_ui_action(
        &self,
        action: &str,
        target: Option<String>,
        query: Option<String>,
    ) -> Result<Value> {
        self.check_fault()?;
        Ok(json!({
            "action": action,
            "target": target,
            "query": query,
            "stub": true,
        }))
    }

    async fn delete_message(&self, onebot_message_id: i64) -> Result<()> {
        self.check_fault()?;
        if onebot_message_id <= 0 {
            return Err(AdapterError::InputRejected(
                "message_id must be a positive integer".to_string(),
            ));
        }

        let removed = self
            .stored_messages
            .lock()
            .remove(&onebot_message_id)
            .ok_or_else(|| {
                AdapterError::InputRejected(format!("message_id={onebot_message_id} not found"))
            })?;

        let notice = match removed.chat_type {
            ChatTargetKind::Group => json!({
                "time": 1_700_000_010_i64,
                "self_id": 10_001_i64,
                "post_type": "notice",
                "notice_type": "group_recall",
                "group_id": removed.group_id,
                "user_id": removed.user_id,
                "operator_id": 10_001_i64,
                "message_id": onebot_message_id,
            }),
            ChatTargetKind::Private => json!({
                "time": 1_700_000_010_i64,
                "self_id": 10_001_i64,
                "post_type": "notice",
                "notice_type": "friend_recall",
                "user_id": removed.user_id,
                "message_id": onebot_message_id,
            }),
        };
        let _ = self.events.send(AdapterEvent::Notice(notice));
        Ok(())
    }

    async fn scheduler_telemetry(&self) -> Result<SchedulerTelemetrySnapshot> {
        let mut tel = self.scheduler_tel.lock().clone();
        tel.outbound_deadline_aborted_total = self.outbound_deadline_aborts.load(Ordering::Acquire);
        Ok(tel)
    }

    async fn scheduler_config(&self) -> Result<SchedulerConfig> {
        Ok(self.scheduler_cfg.lock().clone())
    }

    async fn update_scheduler_config(&self, config: SchedulerConfig) -> Result<SchedulerConfig> {
        *self.scheduler_cfg.lock() = config.clone();
        Ok(config)
    }

    async fn mark_session_read(&self, target: &str, target_kind: ChatTargetKind) -> Result<()> {
        self.check_fault()?;
        let trimmed = target.trim();
        if trimmed.is_empty() {
            return Err(AdapterError::InputRejected(
                "session target must not be empty".to_string(),
            ));
        }
        self.mark_read_calls
            .lock()
            .push((trimmed.to_string(), target_kind));
        Ok(())
    }

    fn record_outbound_deadline_abort(&self) {
        self.outbound_deadline_aborts.fetch_add(1, Ordering::AcqRel);
    }

    async fn ocr_image(&self, image: &str, language: Option<&str>) -> Result<OcrImageResponse> {
        self.check_fault()?;
        let bytes = resolve_stub_ocr_bytes(image)?;
        let lang_owned = language
            .map(str::trim)
            .filter(|lang| !lang.is_empty())
            .map(ToOwned::to_owned);
        tokio::task::spawn_blocking(move || {
            uia_core::recognize_image_bytes(&bytes, lang_owned.as_deref())
        })
        .await
        .map_err(|err| AdapterError::UiaFailure(format!("stub ocr task join failed: {err}")))?
    }

    async fn record_audit_event(&self, event_kind: &str, detail_json: &str) -> Result<()> {
        let mut guard = self.audit_events.lock();
        let id = (guard.len() + 1) as i64;
        guard.push(AuditEventRecord {
            id,
            event_kind: event_kind.to_string(),
            detail_json: detail_json.to_string(),
            created_at_ms: 1_700_000_000_000 + id as u64,
        });
        Ok(())
    }

    async fn list_audit_events(
        &self,
        event_kind: Option<&str>,
        limit: usize,
    ) -> Result<Vec<AuditEventRecord>> {
        let clean = event_kind.map(str::trim).filter(|val| !val.is_empty());
        let guard = self.audit_events.lock();
        Ok(guard
            .iter()
            .rev()
            .filter(|ev| clean.is_none_or(|kind| ev.event_kind == kind))
            .take(limit.clamp(1, 200))
            .cloned()
            .collect())
    }

    async fn reset_security_breaker(&self) -> Result<AdapterStatus> {
        if *self.fault_mode.lock() == StubFaultMode::SecurityBreaker {
            let _ = self
                .record_audit_event(
                    "security_breaker_reset_denied",
                    r#"{"reason":"dialog_open"}"#,
                )
                .await;
        }
        self.check_fault()?;
        self.record_audit_event("security_breaker_reset", r#"{"reset":true}"#)
            .await?;
        let status = self.status().await?;
        let _ = self
            .events
            .send(AdapterEvent::StatusChanged(status.clone()));
        Ok(status)
    }

    async fn forward_single_message(&self, req: SingleForwardRequest) -> Result<SendOutcome> {
        self.check_fault()?;
        if let Some(deadline) = req.deadline_unix_ms {
            let now_ms = chrono::Utc::now().timestamp_millis().max(0) as u64;
            if now_ms > deadline {
                self.record_outbound_deadline_abort();
                return Err(AdapterError::ResponseWindowExpired {
                    target: req.target,
                    elapsed_ms: now_ms - deadline,
                    window_ms: 0,
                });
            }
        }
        let src = self
            .stored_messages
            .lock()
            .get(&req.message_id)
            .cloned()
            .ok_or_else(|| {
                AdapterError::InputRejected(format!("message_id={} not found", req.message_id))
            })?;
        let is_priv = req.target_kind == ChatTargetKind::Private;
        let allowed = self.allowed_friends.lock().clone();
        if (src.chat_type == ChatTargetKind::Private
            && !allowed.iter().any(|n| n == &src.group_name))
            || (is_priv && !allowed.iter().any(|n| n == &req.target))
        {
            return Err(AdapterError::PrivateTargetNotAllowed { target: req.target });
        }
        if !is_priv && self.strict_member_guard.load(Ordering::Acquire) {
            return Err(AdapterError::MemberCountExceeded {
                group: req.target,
                detected: Some(9),
                limit: 9,
            });
        }
        if !is_priv
            && !common_types::canonical_group_names_match(&req.target, DEFAULT_TARGET_GROUP_NAME)
        {
            return Err(AdapterError::GroupNotFound { group: req.target });
        }
        let (gid, uid) = if is_priv {
            (0, stable_onebot_id(&req.target, DEFAULT_ONEBOT_ID_BOUND))
        } else {
            (100_000_001, 10_001)
        };
        self.finish_stub_send(
            SendRequest {
                group: req.target,
                text: src.raw_message,
                target_kind: req.target_kind,
                segments: src.segments,
                ..Default::default()
            },
            req.target_kind,
            (gid, uid),
            InputMode::FocusRestoreUnicode,
            1,
        )
    }
}
