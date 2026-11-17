//! 交互节奏、机器人身份与本地 QQ 客户端启动配置结构体定义

use serde::{Deserialize, Serialize};

use super::InputMode;
use super::SendShortcut;
use crate::constants::{
    DEFAULT_CHAR_DELAY_MAX_MS, DEFAULT_CHAR_DELAY_MIN_MS, DEFAULT_MIN_SEND_INTERVAL_MS,
    DEFAULT_POLL_INTERVAL_MS, DEFAULT_QQ_EXECUTABLE_PATH, DEFAULT_QQ_LAUNCH_ARGS,
    DEFAULT_QQ_STARTUP_TIMEOUT_MS, DEFAULT_SELF_ID,
};

/// UIA 交互策略、轮询间隔与拟人化输入节流配置
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct InteractionConfig {
    pub default_mode: InputMode,
    pub allow_focus_fallback: bool,
    pub poll_interval_ms: u64,
    pub char_delay_min_ms: u64,
    pub char_delay_max_ms: u64,
    pub min_send_interval_ms: u64,
    pub send_shortcut: SendShortcut,
    pub extra_process_names: Vec<String>,
    pub extra_window_classes: Vec<String>,
}

impl Default for InteractionConfig {
    fn default() -> Self {
        Self {
            default_mode: InputMode::Auto,
            allow_focus_fallback: true,
            poll_interval_ms: DEFAULT_POLL_INTERVAL_MS,
            char_delay_min_ms: DEFAULT_CHAR_DELAY_MIN_MS,
            char_delay_max_ms: DEFAULT_CHAR_DELAY_MAX_MS,
            min_send_interval_ms: DEFAULT_MIN_SEND_INTERVAL_MS,
            send_shortcut: SendShortcut::Enter,
            extra_process_names: Vec::new(),
            extra_window_classes: Vec::new(),
        }
    }
}

/// 机器人账号标识与自动回复防环回配置
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BotConfig {
    pub self_id: i64,
    pub self_nicknames: Vec<String>,
    pub auto_ping_reply: bool,
}

impl Default for BotConfig {
    fn default() -> Self {
        Self {
            self_id: DEFAULT_SELF_ID,
            self_nicknames: Vec::new(),
            auto_ping_reply: false,
        }
    }
}

/// 本地 QQNT 桌面端定位、唤醒与无障碍启动参数配置
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct QqClientConfig {
    pub executable_path: Option<String>,
    pub auto_launch_or_wake: bool,
    pub launch_args: Vec<String>,
    pub startup_timeout_ms: u64,
}

impl Default for QqClientConfig {
    fn default() -> Self {
        Self {
            executable_path: Some(DEFAULT_QQ_EXECUTABLE_PATH.to_string()),
            auto_launch_or_wake: true,
            launch_args: DEFAULT_QQ_LAUNCH_ARGS
                .iter()
                .map(|arg| (*arg).to_string())
                .collect(),
            startup_timeout_ms: DEFAULT_QQ_STARTUP_TIMEOUT_MS,
        }
    }
}
