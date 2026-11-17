//! Chromium 无障碍唤醒遥测与综合窗口诊断报告模型定义

use serde::{Deserialize, Serialize};

use super::DiagWindowEntry;

/// 唤醒 Chromium 无障碍树过程中抽样记录的 MSAA 节点摘要
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct MsaaNodeSample {
    pub depth: usize,
    pub role: i32,
    pub name: String,
    pub child_count: i32,
}

/// 唤醒 Electron 与 Chromium 无障碍接口时的遥测统计
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct WakeReport {
    pub target_hwnds_before: Vec<isize>,
    pub target_hwnds_after: Vec<isize>,
    pub screen_reader_flag_before: bool,
    pub pulsed_screen_reader: bool,
    pub visited_msaa_nodes: usize,
    pub max_msaa_depth: usize,
    pub ia2_query_service_ok: usize,
    pub ia2_first_hr: i32,
    pub acc_hit_test_ok: usize,
    pub acc_from_point_ok: usize,
    pub msaa_samples: Vec<MsaaNodeSample>,
}

/// 结合 Win32、MSAA 与 UIA 视图生成的目标窗口综合诊断报告
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct WindowDiagnosticReport {
    pub target_hwnd: isize,
    pub all_qq_windows: Vec<DiagWindowEntry>,
    pub child_windows_before: Vec<DiagWindowEntry>,
    pub child_windows_after: Vec<DiagWindowEntry>,
    pub wake_report: WakeReport,
    pub uia_control_count: usize,
    pub uia_semantic_count: usize,
    pub uia_raw_count: usize,
    pub uia_find_all_count: usize,
    pub sample_semantic_nodes: Vec<String>,
}
