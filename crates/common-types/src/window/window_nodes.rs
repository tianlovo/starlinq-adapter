//! 窗口信息、界面自动化节点快照与底层诊断窗口条目定义

use serde::{Deserialize, Serialize};

/// 枚举到的候选 Win32 窗口基础信息
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowInfo {
    pub hwnd: isize,
    pub pid: u32,
    pub process_name: String,
    pub class_name: String,
    pub title: String,
    pub is_visible: bool,
    pub is_iconic: bool,
    pub rect: [i32; 4],
    pub is_standalone_chat: bool,
}

/// 递归采集的 UI Automation 元素树节点可序列化快照
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct UiNodeSnapshot {
    pub control_type: String,
    pub localized_control_type: String,
    pub name: String,
    pub automation_id: String,
    pub class_name: String,
    pub rect: [i32; 4],
    pub is_enabled: bool,
    pub is_offscreen: bool,
    pub patterns: Vec<String>,
    pub value: Option<String>,
    pub children: Vec<UiNodeSnapshot>,
}

/// 在 UIA 与 MSAA 诊断期间采集的底层 Win32 窗口属性条目
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DiagWindowEntry {
    pub hwnd: isize,
    pub pid: u32,
    pub process_name: String,
    pub class_name: String,
    pub title: String,
    pub is_visible: bool,
    pub is_iconic: bool,
    pub dwm_cloaked: u32,
    pub ex_style: u32,
    pub rect: [i32; 4],
}
