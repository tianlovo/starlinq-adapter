//! 质量检验与基准测试常量中心：统一收敛等级标识、SLA 阈值、基准统计参数与默认隔离配置

/// 默认实时守护进程 HTTP 基地址（仅允许 L0 只读健康探针）
pub const DEFAULT_LIVE_URL: &str = "http://127.0.0.1:18770";
/// 默认测试与基准可视化报告输出目录
pub const DEFAULT_REPORT_DIR: &str = ".runtime/qa-reports/latest";
/// 输出 HTML 可视化仪表盘文件名
pub const REPORT_HTML_FILE: &str = "index.html";
/// 输出 JSON 结构化遥测文件名
pub const REPORT_JSON_FILE: &str = "qa-report.json";

/// Criterion 风格统计基准预热时长（毫秒）
pub const BENCH_WARMUP_MS: u64 = 120;
/// 每个统计样本的目标采样时长（毫秒）
pub const BENCH_TARGET_SAMPLE_MS: u64 = 12;
/// 统计采样样本数
pub const BENCH_SAMPLE_COUNT: usize = 40;
/// 自助法（Bootstrap）95% 置信区间重采样次数
pub const BENCH_BOOTSTRAP_RESAMPLES: usize = 1000;
/// 确定性 Xorshift64 伪随机数种子（确保基准统计与模糊测试可复现）
pub const DETERMINISTIC_PRNG_SEED: u64 = 0x51A7_9E3D_C0B2_4F11;

/// L3 压力测试 SLA：4000 混合请求最低吞吐量阈值（请求/秒）
pub const SLA_STRESS_MIN_RPS: f64 = 1000.0;
/// L3 性能测试 SLA：HTTP P50 延迟上限（毫秒）
pub const SLA_HTTP_P50_MAX_MS: f64 = 3.0;
/// L3 性能测试 SLA：HTTP P99 延迟上限（毫秒）
pub const SLA_HTTP_P99_MAX_MS: f64 = 30.0;
/// L3 性能测试 SLA：WS RPC P99 延迟上限（毫秒）
pub const SLA_WS_P99_MAX_MS: f64 = 20.0;
/// L3 内存负载测试 SLA：高压负载及 GC 后私有内存净驻留增量上限（MiB）
pub const SLA_MEMORY_RETENTION_MAX_MIB: f64 = 64.0;
/// L3 CPU 负载测试 SLA：多连接空闲守护期间归一化 CPU 占用率上限（%）
pub const SLA_IDLE_CPU_MAX_PERCENT: f64 = 5.0;
/// 极端暴力风暴测试后服务端恢复探活延迟上限（毫秒）
pub const SLA_POST_CHAOS_HEALTH_MAX_MS: f64 = 50.0;

/// 用于基准与内存编解码验证的合成图像边长（像素）
pub const BENCH_IMAGE_DIMENSION: u32 = 128;
/// HTTP 客户端测试连接池单主机最大空闲连接数
pub const HTTP_POOL_MAX_IDLE_PER_HOST: usize = 64;
