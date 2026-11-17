//! 聊天消息、富媒体发送请求与发送执行结果载体定义

use serde::{Deserialize, Serialize};

use super::scheduler_types::MessagePriorityTier;
use crate::config::InputMode;
use crate::onebot::{ChatTargetKind, MessageSegment};

/// 从当前聊天历史面板解析出的单条群聊或私聊消息记录
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ChatMessage {
    pub message_id: String,
    pub group_name: String,
    pub sender: String,
    pub content: String,
    pub timestamp_hint: Option<String>,
    pub is_self: bool,
    pub captured_at_ms: u64,
    #[serde(default)]
    pub onebot_message_id: Option<i64>,
    #[serde(default)]
    pub group_id: Option<i64>,
    #[serde(default)]
    pub user_id: Option<i64>,
    #[serde(default)]
    pub target_kind: ChatTargetKind,
    #[serde(default)]
    pub segments: Vec<MessageSegment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to_message_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender_role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender_card: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority_tier: Option<MessagePriorityTier>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_deadline_ms: Option<u64>,
    #[serde(default)]
    pub reply_targets_self: bool,
}

/// 向目标群聊或私聊好友发送纯文本或富媒体消息段的请求载荷
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SendRequest {
    pub group: String,
    pub text: String,
    #[serde(default)]
    pub mode: Option<InputMode>,
    #[serde(default)]
    pub target_kind: ChatTargetKind,
    #[serde(default)]
    pub segments: Vec<MessageSegment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_unix_ms: Option<u64>,
}

/// 一次消息发送执行与历史回显校验的结果报告
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SendOutcome {
    pub ok: bool,
    pub group: String,
    pub used_mode: InputMode,
    pub foreground_touched: bool,
    pub mouse_touched: bool,
    pub verified_in_chat: bool,
    pub elapsed_ms: u64,
    pub detail: String,
    #[serde(default)]
    pub onebot_message_id: Option<i64>,
    #[serde(default)]
    pub group_id: Option<i64>,
    #[serde(default)]
    pub target_kind: ChatTargetKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<i64>,
    #[serde(default)]
    pub degraded_segments: Vec<String>,
}
