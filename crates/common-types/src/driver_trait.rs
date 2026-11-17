//! 抽象驱动契约模块：定义由 QqntDriver 实现并供 API 与 CLI 消费的异步接口

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::config::SchedulerConfig;
use crate::error::Result;
use crate::messaging::{
    AdapterEvent, AdapterStatus, AuditEventRecord, ChatMessage, GroupTargetSpec,
    MessageHistoryFilter, SchedulerTelemetrySnapshot, SendOutcome, SendRequest, SessionSummary,
    SingleForwardRequest,
};
use crate::onebot::{
    BotLoginInfo, ChatTargetKind, FriendInfo, GroupInfoSummary, GroupMemberInfo, OcrImageResponse,
    StorageStats, StoredMessageRecord,
};
use crate::window::{UiNodeSnapshot, WindowDiagnosticReport, WindowInfo};

/// 向目标群聊或私聊会话执行桌面多选合并转发的请求载荷
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ForwardSendRequest {
    pub target: String,
    pub target_kind: ChatTargetKind,
    pub message_ids: Vec<i64>,
}

/// 真实桌面多选合并转发执行及历史气泡回显校验结果
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ForwardSendOutcome {
    pub message_id: i64,
    pub forward_id: String,
    pub target: String,
    pub target_kind: ChatTargetKind,
    pub group_id: Option<i64>,
    pub user_id: Option<i64>,
}

/// 由 `QqntDriver` 与测试桩共同实现的异步机器人驱动接口（Adapter 抽象边界）
#[async_trait]
pub trait BotDriver: Send + Sync + 'static {
    async fn status(&self) -> Result<AdapterStatus>;

    /// 非阻塞读取驱动缓存的最近一次状态快照，供 WebSocket 心跳上报使用
    fn cached_status(&self) -> Option<AdapterStatus> {
        None
    }

    async fn list_windows(&self) -> Result<Vec<WindowInfo>>;

    async fn ensure_qq_window(&self) -> Result<Vec<WindowInfo>> {
        self.list_windows().await
    }

    async fn ensure_qq_window_ext(&self, _force_wake: bool) -> Result<Vec<WindowInfo>> {
        self.ensure_qq_window().await
    }

    async fn diagnose_window(&self, _hwnd: Option<isize>) -> Result<WindowDiagnosticReport> {
        Ok(WindowDiagnosticReport::default())
    }

    async fn inspect_tree(&self, hwnd: Option<isize>, max_depth: usize) -> Result<UiNodeSnapshot>;
    async fn list_sessions(&self) -> Result<Vec<SessionSummary>>;
    async fn open_group(&self, spec: GroupTargetSpec) -> Result<AdapterStatus>;

    async fn open_private(&self, _friend_name: &str) -> Result<AdapterStatus> {
        Err(crate::error::AdapterError::InputRejected(
            "open_private is not implemented for this driver".to_string(),
        ))
    }

    async fn poll_messages(&self, group: &str, limit: usize) -> Result<Vec<ChatMessage>>;
    async fn send_group_message(&self, req: SendRequest) -> Result<SendOutcome>;

    async fn send_private_message(&self, _req: SendRequest) -> Result<SendOutcome> {
        Err(crate::error::AdapterError::InputRejected(
            "send_private_message is not implemented for this driver".to_string(),
        ))
    }

    fn subscribe_events(&self) -> broadcast::Receiver<AdapterEvent>;

    async fn get_login_info(&self) -> Result<BotLoginInfo> {
        Ok(BotLoginInfo::default())
    }

    async fn get_group_list(&self, _no_cache: bool) -> Result<Vec<GroupInfoSummary>> {
        Ok(Vec::new())
    }

    async fn get_friend_list(&self, _no_cache: bool) -> Result<Vec<FriendInfo>> {
        Ok(Vec::new())
    }

    async fn get_group_info(
        &self,
        _group_id: Option<i64>,
        _group_name: Option<String>,
        _no_cache: bool,
    ) -> Result<GroupInfoSummary> {
        Ok(GroupInfoSummary::default())
    }

    async fn get_group_member_list(
        &self,
        _group_id: Option<i64>,
        _group_name: Option<String>,
        _no_cache: bool,
    ) -> Result<Vec<GroupMemberInfo>> {
        Ok(Vec::new())
    }

    async fn get_message(&self, _onebot_message_id: i64) -> Result<Option<StoredMessageRecord>> {
        Ok(None)
    }

    async fn get_recent_messages(
        &self,
        _session_name: &str,
        _limit: usize,
    ) -> Result<Vec<StoredMessageRecord>> {
        Ok(Vec::new())
    }

    /// 按可选聊天类型（群聊或私聊）过滤查询指定会话最近 `limit` 条缓存消息
    async fn get_recent_messages_by_kind(
        &self,
        session_name: &str,
        chat_type: Option<ChatTargetKind>,
        limit: usize,
    ) -> Result<Vec<StoredMessageRecord>> {
        let mut records = self.get_recent_messages(session_name, limit).await?;
        if let Some(kind) = chat_type {
            records.retain(|rec| rec.chat_type == kind);
        }
        Ok(records)
    }

    async fn save_forward_nodes(&self, _forward_id: &str, _nodes_json: &str) -> Result<()> {
        Ok(())
    }

    async fn get_forward_nodes(&self, _forward_id: &str) -> Result<Option<String>> {
        Ok(None)
    }

    async fn get_stranger_info(&self, _user_id: i64) -> Result<Option<FriendInfo>> {
        Ok(None)
    }

    async fn resolve_group_name(&self, _group_id: i64) -> Result<Option<String>> {
        Ok(None)
    }

    async fn resolve_private_target(&self, _user_id: i64) -> Result<Option<String>> {
        Ok(None)
    }

    async fn resolve_group_member_name(
        &self,
        _group_name: &str,
        _user_id: i64,
    ) -> Result<Option<String>> {
        Ok(None)
    }

    async fn storage_stats(&self) -> Result<StorageStats> {
        Ok(StorageStats::default())
    }

    /// 执行基于桌面 UIA 多选气泡与合并转发目标选取的真实合并转发发送
    async fn send_forward_messages(&self, _req: ForwardSendRequest) -> Result<ForwardSendOutcome> {
        Err(crate::error::AdapterError::UnsupportedAction {
            action: "send_forward_msg".to_string(),
            reason: "send_forward_messages is not implemented for this driver".to_string(),
        })
    }

    /// 原子分配单调递增的 `fwd-*` 编号并持久化合并转发节点载体
    async fn allocate_forward_nodes(&self, _nodes_json: &str) -> Result<String> {
        Err(crate::error::AdapterError::UnsupportedAction {
            action: "allocate_forward_nodes".to_string(),
            reason: "allocate_forward_nodes is not implemented for this driver".to_string(),
        })
    }

    /// 抓取当前或指定 QQ 窗口的无边框 `PrintWindow` PNG 截图字节流
    async fn capture_screenshot(&self, _hwnd: Option<isize>) -> Result<Vec<u8>> {
        Err(crate::error::AdapterError::UnsupportedAction {
            action: "capture_screenshot".to_string(),
            reason: "capture_screenshot is not implemented for this driver".to_string(),
        })
    }

    /// 对当前会话界面执行只读弹窗探针（如表情面板、`@` 列表、气泡右键菜单、多选栏）并采集节点树摘要
    async fn probe_ui_action(
        &self,
        _action: &str,
        _target: Option<String>,
        _query: Option<String>,
    ) -> Result<serde_json::Value> {
        Err(crate::error::AdapterError::UnsupportedAction {
            action: "probe_ui_action".to_string(),
            reason: "probe_ui_action is not implemented for this driver".to_string(),
        })
    }

    /// 撤回指定 OneBot 消息编号对应的客户端已发送消息（不可撤回或超时时返回错误）
    async fn delete_message(&self, _onebot_message_id: i64) -> Result<()> {
        Err(crate::error::AdapterError::UnsupportedAction {
            action: "delete_msg".to_string(),
            reason: "delete_msg is not implemented for this driver".to_string(),
        })
    }

    /// 向持久化审计流水表写入异步动作失败或风控异常事件
    async fn record_audit_event(&self, _event_kind: &str, _detail_json: &str) -> Result<()> {
        Ok(())
    }

    /// 获取洪峰调度器与响应窗口选择器的运行遥测快照
    async fn scheduler_telemetry(&self) -> Result<SchedulerTelemetrySnapshot> {
        Ok(SchedulerTelemetrySnapshot::default())
    }

    /// 获取当前生效的洪峰调度与响应窗口策略配置
    async fn scheduler_config(&self) -> Result<SchedulerConfig> {
        Ok(SchedulerConfig::default())
    }

    /// 动态更新当前运行时的洪峰调度与响应窗口策略配置
    async fn update_scheduler_config(&self, _config: SchedulerConfig) -> Result<SchedulerConfig> {
        Err(crate::error::AdapterError::UnsupportedAction {
            action: "update_scheduler_config".to_string(),
            reason: "update_scheduler_config is not implemented for this driver".to_string(),
        })
    }

    /// 将指定群聊或私聊会话的未读红点提示同步标记为已读
    async fn mark_session_read(&self, _target: &str, _target_kind: ChatTargetKind) -> Result<()> {
        Ok(())
    }

    /// 记录因超出响应窗口而中止的出站消息发送事件（供遥测指标统计）
    fn record_outbound_deadline_abort(&self) {}

    /// 对指定图片源执行本地 OCR 文字识别（默认返回 `UnsupportedAction`）
    async fn ocr_image(&self, _image: &str, _language: Option<&str>) -> Result<OcrImageResponse> {
        Err(crate::error::AdapterError::UnsupportedAction {
            action: "ocr_image".to_string(),
            reason: "ocr_image is not implemented for this driver".to_string(),
        })
    }

    /// 按会话、游标范围、发送者与关键词深度分页查询历史消息（默认回退至内存态过滤）
    async fn query_messages_history(
        &self,
        filter: MessageHistoryFilter,
    ) -> Result<Vec<StoredMessageRecord>> {
        let mut records = self
            .get_recent_messages_by_kind(&filter.session_name, filter.chat_type, usize::MAX)
            .await?;
        let sender_needle = filter
            .sender
            .as_deref()
            .map(str::trim)
            .filter(|val| !val.is_empty())
            .map(str::to_lowercase);
        let keyword_needle = filter
            .keyword
            .as_deref()
            .map(str::trim)
            .filter(|val| !val.is_empty())
            .map(str::to_lowercase);

        records.retain(|rec| {
            if let Some(max_id) = filter.before_or_at_id {
                if rec.onebot_message_id > max_id {
                    return false;
                }
            }
            if let Some(min_id) = filter.after_id {
                if rec.onebot_message_id <= min_id {
                    return false;
                }
            }
            if let Some(ref needle) = sender_needle {
                if !rec.sender_name.to_lowercase().contains(needle) {
                    return false;
                }
            }
            if let Some(ref needle) = keyword_needle {
                if !rec.raw_message.to_lowercase().contains(needle) {
                    return false;
                }
            }
            true
        });

        let clamped_limit = filter.limit.clamp(1, 200);
        if filter.after_id.is_some() && filter.before_or_at_id.is_none() {
            // 仅指定下界游标时按编号升序截取最早增量，再按需反转
            records.sort_by_key(|rec| rec.onebot_message_id);
            records.truncate(clamped_limit);
            if filter.reverse_order {
                records.reverse();
            }
        } else {
            // 默认取最新记录，未指定 reverse_order 时反转为从旧到新排列
            records.sort_by_key(|rec| std::cmp::Reverse(rec.onebot_message_id));
            records.truncate(clamped_limit);
            if !filter.reverse_order {
                records.reverse();
            }
        }
        Ok(records)
    }

    /// 按可选事件类型过滤查询最近持久化的审计流水记录
    async fn list_audit_events(
        &self,
        _event_kind: Option<&str>,
        _limit: usize,
    ) -> Result<Vec<AuditEventRecord>> {
        Ok(Vec::new())
    }

    /// 手动重置桌面安全验证熔断器并恢复正常调度状态（默认返回 `UnsupportedAction`）
    async fn reset_security_breaker(&self) -> Result<AdapterStatus> {
        Err(crate::error::AdapterError::UnsupportedAction {
            action: "reset_security_breaker".to_string(),
            reason: "reset_security_breaker is not implemented for this driver".to_string(),
        })
    }

    /// 将单条历史消息原生转发至目标群聊或好友私聊会话（默认返回 `UnsupportedAction`）
    async fn forward_single_message(&self, _req: SingleForwardRequest) -> Result<SendOutcome> {
        Err(crate::error::AdapterError::UnsupportedAction {
            action: "forward_single_message".to_string(),
            reason: "forward_single_message is not implemented for this driver".to_string(),
        })
    }
}
