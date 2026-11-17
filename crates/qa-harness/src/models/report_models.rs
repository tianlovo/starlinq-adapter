//! 宿主机环境元数据、Win32 进程遥测快照与全量 QA 报告根模型定义

use serde::{Deserialize, Serialize};

use super::bench_models::{BenchmarkResult, DefectRecord};
use super::tier_models::CategoryReport;

/// 基于 Win32 内核接口采集的单进程内存与 CPU 累计时间快照
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProcessTelemetrySnapshot {
    pub pid: u32,
    pub label: String,
    pub working_set_mib: f64,
    pub peak_working_set_mib: f64,
    pub private_bytes_mib: f64,
    pub kernel_time_ms: f64,
    pub user_time_ms: f64,
    pub cpu_percent_window: Option<f64>,
}

/// 测试宿主机 CPU 逻辑核数、物理内存、构建模式与时间环境摘要
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemEnvironmentInfo {
    pub generated_at_rfc3339: String,
    pub tier_requested: String,
    pub build_profile: String,
    pub logical_cores: usize,
    pub total_phys_memory_mib: f64,
    pub avail_phys_memory_mib: f64,
    pub harness_pid: u32,
    pub live_daemon_detected: bool,
    pub live_daemon_pid: Option<u32>,
    pub live_telemetry: Option<ProcessTelemetrySnapshot>,
}

/// 输出至 JSON 与交互式 HTML 仪表盘的顶层完整测试与基准报告
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QaFullReport {
    pub overall_passed: bool,
    pub total_duration_ms: f64,
    pub total_categories: usize,
    pub passed_categories: usize,
    pub total_cases: usize,
    pub passed_cases: usize,
    pub skipped_cases: usize,
    pub failed_cases: usize,
    pub total_benchmarks: usize,
    pub verification_scope: String,
    pub live_e2e_verified: bool,
    pub environment: SystemEnvironmentInfo,
    pub categories: Vec<CategoryReport>,
    pub benchmarks: Vec<BenchmarkResult>,
    pub defects: Vec<DefectRecord>,
}
