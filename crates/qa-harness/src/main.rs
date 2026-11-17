//! 分级质量检验（L0-L4）与行业标准统计 Benchmark 命令行执行入口

mod bench;
mod constants;
mod models;
mod report;
mod suites;
mod telemetry;

use std::path::PathBuf;
use std::time::Instant;

use anyhow::Result;
use chrono::Utc;
use clap::Parser;

use constants::{DEFAULT_LIVE_URL, DEFAULT_REPORT_DIR};
use models::{QaFullReport, QaTier, SystemEnvironmentInfo};
use report::{known_defect_records, write_qa_reports};
use suites::{run_all_categories, SuiteExecContext};
use telemetry::{logical_cpu_cores, read_system_memory_mib};

/// `qq-qa-harness` 分级测试与统计基准命令行参数配置
#[derive(Debug, Parser)]
#[command(
    name = "qq-qa-harness",
    about = "qq-uia-adapter 分级 QA 测试（13 类测试）与行业标准 Benchmark（13 项基准）执行器"
)]
struct QaCliArgs {
    /// 指定执行的测试检验等级（L0 | L1 | L2 | L3 | L4 | All）
    #[arg(long, default_value = "All")]
    tier: String,
    /// 在线守护进程 HTTP 基地址（仅执行 L0 只读健康探针与风控隔离校验）
    #[arg(long, default_value = DEFAULT_LIVE_URL)]
    live_url: String,
    /// 可视化 HTML 与结构化 JSON 报告输出目录
    #[arg(long, default_value = DEFAULT_REPORT_DIR)]
    out_dir: PathBuf,
    /// 跳过对在线守护进程 127.0.0.1:18770 的只读探活检测
    #[arg(long, default_value_t = false)]
    skip_live: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = QaCliArgs::parse();
    let tier = QaTier::parse_flag(&args.tier).map_err(anyhow::Error::msg)?;
    let started = Instant::now();

    println!(
        "[qq-qa-harness] starting QA & Benchmark run (tier={}, skip_live={})",
        tier.as_str(),
        args.skip_live
    );

    let ctx = SuiteExecContext::new(args.live_url.clone(), args.skip_live)?;
    let categories = run_all_categories(tier, &ctx).await;
    let benchmarks = if tier.includes(QaTier::L4) {
        println!(
            "[qq-qa-harness] running L4 Criterion-style statistical benchmarks (13 suites)..."
        );
        bench::run_all_benchmarks().await?
    } else {
        Vec::new()
    };

    let (total_phys, avail_phys) = read_system_memory_mib().unwrap_or((0.0, 0.0));
    let live_telemetry = ctx.live_telemetry.lock().clone();
    let live_daemon_pid = live_telemetry.as_ref().map(|snap| snap.pid);

    let total_categories = categories.len();
    let passed_categories = categories.iter().filter(|cat| cat.passed).count();
    let total_cases: usize = categories.iter().map(|cat| cat.total_cases).sum();
    let passed_cases: usize = categories.iter().map(|cat| cat.passed_cases).sum();
    let skipped_cases: usize = categories.iter().map(|cat| cat.skipped_cases).sum();
    let failed_cases: usize = categories.iter().map(|cat| cat.failed_cases).sum();
    let overall_passed = categories.iter().all(|cat| cat.passed) && failed_cases == 0;

    let report = QaFullReport {
        overall_passed,
        total_duration_ms: started.elapsed().as_secs_f64() * 1000.0,
        total_categories,
        passed_categories,
        total_cases,
        passed_cases,
        skipped_cases,
        failed_cases,
        total_benchmarks: benchmarks.len(),
        verification_scope: "OFFLINE_MOCK_AND_TREE_REPLAY (NOT_LIVE_QQ_E2E)".to_string(),
        live_e2e_verified: false,
        environment: SystemEnvironmentInfo {
            generated_at_rfc3339: Utc::now().to_rfc3339(),
            tier_requested: tier.as_str().to_string(),
            build_profile: if cfg!(debug_assertions) {
                "debug".to_string()
            } else {
                "release".to_string()
            },
            logical_cores: logical_cpu_cores(),
            total_phys_memory_mib: total_phys,
            avail_phys_memory_mib: avail_phys,
            harness_pid: std::process::id(),
            live_daemon_detected: live_daemon_pid.is_some(),
            live_daemon_pid,
            live_telemetry,
        },
        categories,
        benchmarks,
        defects: known_defect_records(),
    };

    let (json_path, html_path) = write_qa_reports(&args.out_dir, &report)?;

    for cat in &report.categories {
        let tag = if cat.passed { "PASS" } else { "FAIL" };
        println!(
            "  [{tag}] [{}] {} ({}/{} passed, {} skipped, {:.1} ms) -> {}",
            cat.tier,
            cat.category_zh,
            cat.passed_cases,
            cat.total_cases,
            cat.skipped_cases,
            cat.duration_ms,
            cat.sla_actual
        );
        for case in &cat.cases {
            if case.skipped {
                println!("         SKIP case '{}': {}", case.name, case.detail);
            } else if !case.passed {
                println!("         FAIL case '{}': {}", case.name, case.detail);
            }
        }
    }

    for bm in &report.benchmarks {
        println!(
            "  [BENCH] {} {} | mean={:.1} ns CI95=[{:.1}, {:.1}] RME={:.2}% | {:.0} ops/s",
            bm.id,
            bm.title_zh,
            bm.mean_ns,
            bm.ci95_low_ns,
            bm.ci95_high_ns,
            bm.rme_percent,
            bm.ops_per_sec
        );
    }

    println!(
        "[qq-qa-harness] overall_passed={} | categories={}/{} | passed={}/{} | skipped={} | benchmarks={} | scope={} | duration={:.1} ms",
        report.overall_passed,
        report.passed_categories,
        report.total_categories,
        report.passed_cases,
        report.total_cases,
        report.skipped_cases,
        report.total_benchmarks,
        report.verification_scope,
        report.total_duration_ms
    );
    println!("[qq-qa-harness] JSON : {}", json_path.display());
    println!("[qq-qa-harness] HTML : {}", html_path.display());

    if !report.overall_passed {
        anyhow::bail!("QA suite finished with failed categories/cases");
    }
    Ok(())
}
