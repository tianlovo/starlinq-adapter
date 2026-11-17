//! 驱动运行状态快照与异步广播事件枚举定义

use serde::{Deserialize, Serialize};

use super::chat_payloads::{ChatMessage, SendOutcome};
use crate::config::InputMode;
use crate::onebot::ChatTargetKind;
use crate::window::WindowInfo;

/// QQNT UIA 驱动当前的桌面环境、进程绑定与运行状态快照
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AdapterStatus {
    pub desktop_name: String,
    pub sandbox_isolated: bool,
    pub qq_pids: Vec<u32>,
    #[serde(default)]
    pub resolved_qq_path: Option<String>,
    pub attached_window: Option<WindowInfo>,
    pub active_group: Option<String>,
    #[serde(default)]
    pub active_chat_kind: Option<ChatTargetKind>,
    #[serde(default)]
    pub active_group_id: Option<i64>,
    #[serde(default)]
    pub active_real_group_id: Option<i64>,
    pub detected_member_count: Option<u32>,
    pub security_breaker_tripped: bool,
    pub security_breaker_reason: Option<String>,
    pub input_mode: InputMode,
    pub messages_observed: u64,
    pub messages_sent: u64,
    #[serde(default)]
    pub burst_mode_active: bool,
    #[serde(default)]
    pub scheduler_pending: usize,
}

/// 观察者与发布订阅模式（Observer / Pub-Sub）广播的异步适配事件
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event_type", content = "data", rename_all = "snake_case")]
pub enum AdapterEvent {
    GroupMessage(ChatMessage),
    PrivateMessage(ChatMessage),
    MessageSent(SendOutcome),
    Notice(serde_json::Value),
    Request(serde_json::Value),
    SecurityAlert { title: String, detail: String },
    StatusChanged(AdapterStatus),
}
