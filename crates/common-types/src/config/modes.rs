//! 输入模式与发送快捷键枚举定义：借助 strum 自动实现格式化与别名解析

use serde::{Deserialize, Serialize};

/// 在聊天输入框内键入并提交消息的策略模式
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Default,
    Serialize,
    Deserialize,
    strum::Display,
    strum::EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(ascii_case_insensitive)]
pub enum InputMode {
    /// 纯后台 UIA ValuePattern 与 PostMessage 投递，不抢占前台焦点与鼠标
    #[strum(
        to_string = "background_uia",
        serialize = "background_uia",
        serialize = "background-uia",
        serialize = "background"
    )]
    BackgroundUia,
    /// 短暂切入前台窗口执行 Unicode SendInput 输入，并依赖 RAII 守卫恢复原焦点
    #[strum(
        to_string = "focus_restore_unicode",
        serialize = "focus_restore_unicode",
        serialize = "focus-restore-unicode",
        serialize = "focus"
    )]
    FocusRestoreUnicode,
    /// 优先尝试后台投递，仅在显式允许时回退至焦点恢复模式
    #[default]
    #[strum(to_string = "auto", serialize = "auto")]
    Auto,
}

/// 提交已编辑聊天内容时使用的快捷键组合
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Default,
    Serialize,
    Deserialize,
    strum::Display,
    strum::EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(ascii_case_insensitive)]
pub enum SendShortcut {
    /// 直接按 Enter 键发送消息
    #[default]
    #[strum(to_string = "enter", serialize = "enter")]
    Enter,
    /// 使用 Ctrl+Enter 组合键发送消息
    #[strum(
        to_string = "ctrl_enter",
        serialize = "ctrl_enter",
        serialize = "ctrl-enter"
    )]
    CtrlEnter,
}
