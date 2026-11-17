//! 行业标准统计基准分位数、置信区间与缺陷审计记录结构体定义

use serde::{Deserialize, Serialize};

/// 延迟分布全分位数统计指标（单位：纳秒）
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LatencyPercentiles {
    pub min_ns: f64,
    pub p50_ns: f64,
    pub p90_ns: f64,
    pub p95_ns: f64,
    pub p99_ns: f64,
    pub p999_ns: f64,
    pub max_ns: f64,
}

/// 单项行业标准 Benchmark 统计结果（含 95% Bootstrap 置信区间、MAD、RME 与 Tukey 离群点）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkResult {
    pub id: String,
    pub title_zh: String,
    pub title_en: String,
    pub subsystem: String,
    pub unit_bytes: usize,
    pub samples: usize,
    pub iters_per_sample: u64,
    pub total_ops: u64,
    pub mean_ns: f64,
    pub ci95_low_ns: f64,
    pub ci95_high_ns: f64,
    pub median_ns: f64,
    pub std_dev_ns: f64,
    pub mad_ns: f64,
    pub rme_percent: f64,
    pub outliers_low: usize,
    pub outliers_high: usize,
    pub ops_per_sec: f64,
    pub mib_per_sec: Option<f64>,
    pub percentiles: LatencyPercentiles,
    pub sample_means_ns: Vec<f64>,
}

/// QA 全维度测试期间发现并闭环修复的缺陷审计记录
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DefectRecord {
    pub id: String,
    pub severity: String,
    pub status: String,
    pub category: String,
    pub component: String,
    pub summary: String,
    pub root_cause: String,
    pub resolution: String,
    pub regression_test: String,
}
