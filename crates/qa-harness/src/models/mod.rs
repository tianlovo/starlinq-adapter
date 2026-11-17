//! QA 数据模型门面：聚合分级策略枚举、12 类测试结果模型、统计基准结果与进程遥测载体

pub mod bench_models;
pub mod report_models;
pub mod tier_models;

pub use bench_models::{BenchmarkResult, DefectRecord, LatencyPercentiles};
pub use report_models::{ProcessTelemetrySnapshot, QaFullReport, SystemEnvironmentInfo};
pub use tier_models::{CaseOutcome, CategoryReport, QaTier};
