//! Criterion.rs / Google Benchmark 统计学微基准引擎：预热、自适应校准、95% Bootstrap 置信区间与离群分析

use std::future::Future;
use std::hint::black_box;
use std::time::{Duration, Instant};

use crate::constants::{
    BENCH_BOOTSTRAP_RESAMPLES, BENCH_SAMPLE_COUNT, BENCH_TARGET_SAMPLE_MS, BENCH_WARMUP_MS,
    DETERMINISTIC_PRNG_SEED,
};
use crate::models::{BenchmarkResult, LatencyPercentiles};

/// 单项基准测试的元信息描述符（Parameter Object）
#[derive(Debug, Clone)]
pub struct BenchmarkMeta {
    pub id: &'static str,
    pub title_zh: &'static str,
    pub title_en: &'static str,
    pub subsystem: &'static str,
    pub unit_bytes: usize,
}

/// 确定性 Xorshift64 伪随机数生成器，供自助法重采样及模糊输入测试稳定复现
pub struct DeterministicRng {
    state: u64,
}

impl DeterministicRng {
    /// 使用给定非零种子初始化生成器
    pub fn new(seed: u64) -> Self {
        Self {
            state: if seed == 0 {
                DETERMINISTIC_PRNG_SEED
            } else {
                seed
            },
        }
    }

    /// 生成下一个 64 位无符号随机整数
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }

    /// 生成 `[0, bound)` 范围内的均匀随机索引
    pub fn next_usize(&mut self, bound: usize) -> usize {
        if bound == 0 {
            return 0;
        }
        (self.next_u64() as usize) % bound
    }
}

/// 按线性插值从升序数组中计算 `[0, 100]` 百分位值
pub fn percentile_sorted(sorted: &[f64], pct: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    if sorted.len() == 1 {
        return sorted[0];
    }
    let clamped = pct.clamp(0.0, 100.0);
    let rank = (clamped / 100.0) * ((sorted.len() - 1) as f64);
    let lo = rank.floor() as usize;
    let hi = rank.ceil() as usize;
    let frac = rank - (lo as f64);
    sorted[lo] + (sorted[hi] - sorted[lo]) * frac
}

/// 对一组纳秒样本均值执行全套 Criterion 级统计学汇总并封装结果
pub fn summarize_benchmark_samples(
    meta: &BenchmarkMeta,
    iters_per_sample: u64,
    sample_means_ns: Vec<f64>,
) -> BenchmarkResult {
    let mut sorted = sample_means_ns.clone();
    sorted.sort_by(|a, b| a.total_cmp(b));

    let n = sorted.len().max(1);
    let mean_ns = sorted.iter().sum::<f64>() / (n as f64);
    let median_ns = percentile_sorted(&sorted, 50.0);

    let variance = if n > 1 {
        sorted
            .iter()
            .map(|&v| {
                let diff = v - mean_ns;
                diff * diff
            })
            .sum::<f64>()
            / ((n - 1) as f64)
    } else {
        0.0
    };
    let std_dev_ns = variance.sqrt();

    let mut abs_devs: Vec<f64> = sorted.iter().map(|&v| (v - median_ns).abs()).collect();
    abs_devs.sort_by(|a, b| a.total_cmp(b));
    let raw_mad = percentile_sorted(&abs_devs, 50.0);
    // 按正态分布一致性因子 1.4826 将中位绝对偏差缩放为等价稳健标准差
    let mad_ns = raw_mad * 1.4826;

    // 执行 1,000 次确定性 Bootstrap 重采样以获得 95% 均值置信区间
    let mut rng = DeterministicRng::new(DETERMINISTIC_PRNG_SEED ^ (iters_per_sample + 1));
    let mut boot_means = Vec::with_capacity(BENCH_BOOTSTRAP_RESAMPLES);
    for _ in 0..BENCH_BOOTSTRAP_RESAMPLES {
        let mut sum = 0.0;
        for _ in 0..n {
            sum += sample_means_ns[rng.next_usize(n)];
        }
        boot_means.push(sum / (n as f64));
    }
    boot_means.sort_by(|a, b| a.total_cmp(b));
    let ci95_low_ns = percentile_sorted(&boot_means, 2.5);
    let ci95_high_ns = percentile_sorted(&boot_means, 97.5);
    let half_width = ((ci95_high_ns - ci95_low_ns) * 0.5).max(0.0);
    let rme_percent = if mean_ns > 0.0 {
        (half_width / mean_ns) * 100.0
    } else {
        0.0
    };

    // Tukey 1.5 * IQR 四分位距围栏离群点判定
    let q1 = percentile_sorted(&sorted, 25.0);
    let q3 = percentile_sorted(&sorted, 75.0);
    let iqr = (q3 - q1).max(0.0);
    let low_fence = q1 - 1.5 * iqr;
    let high_fence = q3 + 1.5 * iqr;
    let outliers_low = sorted.iter().filter(|&&v| v < low_fence).count();
    let outliers_high = sorted.iter().filter(|&&v| v > high_fence).count();

    let percentiles = LatencyPercentiles {
        min_ns: sorted[0],
        p50_ns: median_ns,
        p90_ns: percentile_sorted(&sorted, 90.0),
        p95_ns: percentile_sorted(&sorted, 95.0),
        p99_ns: percentile_sorted(&sorted, 99.0),
        p999_ns: percentile_sorted(&sorted, 99.9),
        max_ns: sorted[n - 1],
    };

    let safe_mean = mean_ns.max(1e-3);
    let ops_per_sec = 1_000_000_000.0 / safe_mean;
    let mib_per_sec =
        (meta.unit_bytes > 0).then(|| ((meta.unit_bytes as f64) * ops_per_sec) / (1024.0 * 1024.0));

    BenchmarkResult {
        id: meta.id.to_string(),
        title_zh: meta.title_zh.to_string(),
        title_en: meta.title_en.to_string(),
        subsystem: meta.subsystem.to_string(),
        unit_bytes: meta.unit_bytes,
        samples: sample_means_ns.len(),
        iters_per_sample,
        total_ops: iters_per_sample * (sample_means_ns.len() as u64),
        mean_ns,
        ci95_low_ns,
        ci95_high_ns,
        median_ns,
        std_dev_ns,
        mad_ns,
        rme_percent,
        outliers_low,
        outliers_high,
        ops_per_sec,
        mib_per_sec,
        percentiles,
        sample_means_ns,
    }
}

/// 运行同步纯内存或本地 I/O 微基准（自动预热、迭代次数自适应校准并收集 40 个独立统计样本）
pub fn run_sync_benchmark<T, F>(meta: BenchmarkMeta, mut op: F) -> BenchmarkResult
where
    F: FnMut(u64) -> T,
{
    let warmup_budget = Duration::from_millis(BENCH_WARMUP_MS);
    let warmup_start = Instant::now();
    let mut seq = 0_u64;
    while warmup_start.elapsed() < warmup_budget {
        black_box(op(black_box(seq)));
        seq = seq.wrapping_add(1);
    }

    // 通过指数探针测算单次调用真实耗时，为每个统计样本匹配约 BENCH_TARGET_SAMPLE_MS 的平滑迭代数
    let mut probe_iters = 1_u64;
    let probe_ns = loop {
        let t0 = Instant::now();
        for _ in 0..probe_iters {
            black_box(op(black_box(seq)));
            seq = seq.wrapping_add(1);
        }
        let elapsed_ns = t0.elapsed().as_nanos() as f64;
        if elapsed_ns >= 500_000.0 || probe_iters >= 65_536 {
            break elapsed_ns / (probe_iters as f64);
        }
        probe_iters *= 4;
    };

    let target_ns = (BENCH_TARGET_SAMPLE_MS as f64) * 1_000_000.0;
    let iters_per_sample = ((target_ns / probe_ns.max(1.0)).round() as u64).clamp(1, 200_000);

    let mut samples = Vec::with_capacity(BENCH_SAMPLE_COUNT);
    for _ in 0..BENCH_SAMPLE_COUNT {
        let t0 = Instant::now();
        for _ in 0..iters_per_sample {
            black_box(op(black_box(seq)));
            seq = seq.wrapping_add(1);
        }
        let elapsed_ns = (t0.elapsed().as_nanos() as f64).max(1.0);
        samples.push(elapsed_ns / (iters_per_sample as f64));
    }

    summarize_benchmark_samples(&meta, iters_per_sample, samples)
}

/// 运行异步网络/全栈基准（与同步引擎采用相同的预热、自适应校准与 Bootstrap 统计学分析）
pub async fn run_async_benchmark<T, Fut, F>(meta: BenchmarkMeta, mut op: F) -> BenchmarkResult
where
    Fut: Future<Output = T>,
    F: FnMut(u64) -> Fut,
{
    let warmup_budget = Duration::from_millis(BENCH_WARMUP_MS);
    let warmup_start = Instant::now();
    let mut seq = 0_u64;
    while warmup_start.elapsed() < warmup_budget {
        black_box(op(black_box(seq)).await);
        seq = seq.wrapping_add(1);
    }

    let probe_count = 16_u64;
    let t_probe = Instant::now();
    for _ in 0..probe_count {
        black_box(op(black_box(seq)).await);
        seq = seq.wrapping_add(1);
    }
    let avg_probe_ns = (t_probe.elapsed().as_nanos() as f64) / (probe_count as f64);
    let target_ns = (BENCH_TARGET_SAMPLE_MS as f64) * 1_000_000.0;
    let iters_per_sample = ((target_ns / avg_probe_ns.max(1.0)).round() as u64).clamp(8, 2_000);

    let mut samples = Vec::with_capacity(BENCH_SAMPLE_COUNT);
    for _ in 0..BENCH_SAMPLE_COUNT {
        let t0 = Instant::now();
        for _ in 0..iters_per_sample {
            black_box(op(black_box(seq)).await);
            seq = seq.wrapping_add(1);
        }
        let elapsed_ns = (t0.elapsed().as_nanos() as f64).max(1.0);
        samples.push(elapsed_ns / (iters_per_sample as f64));
    }

    summarize_benchmark_samples(&meta, iters_per_sample, samples)
}
