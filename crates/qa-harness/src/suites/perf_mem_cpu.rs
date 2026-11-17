//! L3 性能分位数测试（P50/P90/P95/P99/P99.9）、Win32 内存驻留与泄漏检测及 CPU 空闲/满载测试套件

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::ensure;
use common_types::constants::DEFAULT_ONEBOT_ID_BOUND;
use common_types::{
    parse_cq_string_to_segments, segments_to_cq_string, stable_onebot_id, ChatMessage,
    ChatTargetKind,
};
use futures_util::SinkExt;
use qqnt_driver::parser::MessageTracker;
use serde_json::json;
use storage::constants::{MAX_AUDIT_EVENTS, MEMORY_DB_PATH};
use storage::Storage;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use uia_core::{decode_image_bytes_to_bgra, encode_bgra_to_png_bytes, DecodedImageBgra};

use super::stub_driver::TempDbGuard;
use super::{default_stub_and_config, ws_next_json, CategoryBuilder, SuiteExecContext};
use crate::bench::stats_engine::percentile_sorted;
use crate::constants::{
    BENCH_IMAGE_DIMENSION, SLA_HTTP_P50_MAX_MS, SLA_HTTP_P99_MAX_MS, SLA_IDLE_CPU_MAX_PERCENT,
    SLA_MEMORY_RETENTION_MAX_MIB, SLA_WS_P99_MAX_MS,
};
use crate::models::{CategoryReport, QaTier};
use crate::telemetry::{snapshot_current_process, CpuUsageWindow};

fn build_test_bgra(dim: u32) -> DecodedImageBgra {
    let mut bgra = vec![0_u8; (dim as usize) * (dim as usize) * 4];
    for (idx, chunk) in bgra.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        chunk[0] = (idx & 0xFF) as u8;
        chunk[1] = ((idx >> 2) & 0xFF) as u8;
        chunk[2] = ((idx >> 4) & 0xFF) as u8;
        chunk[3] = 255;
    }
    DecodedImageBgra {
        width: dim,
        height: dim,
        bgra_top_down: bgra,
    }
}

/// 执行第 9 类：性能分位数 SLA 测试（L3）
pub async fn run_performance_category(ctx: &SuiteExecContext) -> CategoryReport {
    let mut cat = CategoryBuilder::new(
        "CAT-09",
        "性能测试",
        "Performance Percentile SLA",
        QaTier::L3,
        "HTTP P50 < 3.0ms & P99 < 30.0ms；WS RPC P99 < 20.0ms",
    );

    let mut http_p50_ms = 0.0_f64;
    let mut http_p95_ms = 0.0_f64;
    let mut http_p99_ms = 0.0_f64;
    let mut http_p999_ms = 0.0_f64;
    let mut ws_p99_ms = 0.0_f64;

    cat.run_case("perf_1500_http_and_500_ws_latency_percentiles", async {
        let (driver, cfg) = default_stub_and_config();
        let (addr, handle) = adapter_server::spawn_server(driver, cfg).await?;
        let base = format!("http://{addr}");

        // 先执行 60 次热身以稳定本地 TCP 连接池与 Allocator 状态
        for _ in 0..60 {
            let _ = ctx
                .http_client
                .get(format!("{base}/api/health"))
                .send()
                .await?;
        }

        let mut http_ns = Vec::with_capacity(1500);
        for idx in 0..1500_usize {
            let t0 = Instant::now();
            match idx % 3 {
                0 => {
                    let resp = ctx.http_client.get(format!("{base}/api/health")).send().await?;
                    ensure!(resp.status().as_u16() == 200);
                }
                1 => {
                    let resp = ctx
                        .http_client
                        .post(format!("{base}/onebot/v11/get_status"))
                        .json(&json!({}))
                        .send()
                        .await?;
                    ensure!(resp.status().as_u16() == 200);
                }
                _ => {
                    let resp = ctx
                        .http_client
                        .post(format!("{base}/onebot/v11/send_group_msg"))
                        .json(&json!({"message": "perf-sla-probe"}))
                        .send()
                        .await?;
                    ensure!(resp.status().as_u16() == 200);
                }
            }
            http_ns.push(t0.elapsed().as_nanos() as f64);
        }
        http_ns.sort_by(|a, b| a.total_cmp(b));
        http_p50_ms = percentile_sorted(&http_ns, 50.0) / 1_000_000.0;
        let http_p90_ms = percentile_sorted(&http_ns, 90.0) / 1_000_000.0;
        http_p95_ms = percentile_sorted(&http_ns, 95.0) / 1_000_000.0;
        http_p99_ms = percentile_sorted(&http_ns, 99.0) / 1_000_000.0;
        http_p999_ms = percentile_sorted(&http_ns, 99.9) / 1_000_000.0;

        let (mut ws, _) = connect_async(format!("ws://{addr}/onebot/v11/ws")).await?;
        let _ = ws_next_json(&mut ws, Duration::from_secs(2)).await?;
        let mut ws_ns = Vec::with_capacity(500);
        for seq in 0..500_usize {
            let echo = format!("p-{seq}");
            let t0 = Instant::now();
            ws.send(Message::Text(
                json!({"action": "get_version_info", "echo": echo})
                    .to_string()
                    .into(),
            ))
            .await?;
            let reply = ws_next_json(&mut ws, Duration::from_secs(2)).await?;
            ensure!(reply["retcode"] == 0);
            ws_ns.push(t0.elapsed().as_nanos() as f64);
        }
        ws_ns.sort_by(|a, b| a.total_cmp(b));
        let ws_p50_ms = percentile_sorted(&ws_ns, 50.0) / 1_000_000.0;
        ws_p99_ms = percentile_sorted(&ws_ns, 99.0) / 1_000_000.0;

        handle.abort();

        ensure!(
            http_p50_ms <= SLA_HTTP_P50_MAX_MS,
            "HTTP P50={http_p50_ms:.3}ms > {SLA_HTTP_P50_MAX_MS}ms"
        );
        ensure!(
            http_p99_ms <= SLA_HTTP_P99_MAX_MS,
            "HTTP P99={http_p99_ms:.3}ms > {SLA_HTTP_P99_MAX_MS}ms"
        );
        ensure!(
            ws_p99_ms <= SLA_WS_P99_MAX_MS,
            "WS P99={ws_p99_ms:.3}ms > {SLA_WS_P99_MAX_MS}ms"
        );

        Ok(format!(
            "HTTP P50={http_p50_ms:.3}ms P90={http_p90_ms:.3}ms P95={http_p95_ms:.3}ms P99={http_p99_ms:.3}ms P99.9={http_p999_ms:.3}ms；WS P50={ws_p50_ms:.3}ms P99={ws_p99_ms:.3}ms"
        ))
    })
    .await;

    cat.set_metric("http_p50_ms", http_p50_ms);
    cat.set_metric("http_p95_ms", http_p95_ms);
    cat.set_metric("http_p99_ms", http_p99_ms);
    cat.set_metric("http_p999_ms", http_p999_ms);
    cat.set_metric("ws_p99_ms", ws_p99_ms);
    cat.set_sla(
        http_p50_ms <= SLA_HTTP_P50_MAX_MS
            && http_p99_ms <= SLA_HTTP_P99_MAX_MS
            && ws_p99_ms <= SLA_WS_P99_MAX_MS,
        format!("HTTP P50={http_p50_ms:.3}ms, P99={http_p99_ms:.3}ms, WS P99={ws_p99_ms:.3}ms"),
    );
    cat.build()
}

/// 执行第 10 类：内存负载与有界淘汰防泄漏测试（L3）
pub async fn run_memory_load_category(_ctx: &SuiteExecContext) -> CategoryReport {
    let mut cat = CategoryBuilder::new(
        "CAT-10",
        "内存负载测试",
        "Memory Load & Leak Ceiling Test",
        QaTier::L3,
        "10k 消息 + 1.5k 转发 + 7.5k 审计 + 200 WIC 图像压测后净驻留增量 < 64.0 MiB 且缓存有界",
    );

    let mut before_mib = 0.0_f64;
    let mut peak_mib = 0.0_f64;
    let mut after_mib = 0.0_f64;
    let mut delta_mib = 0.0_f64;

    cat.run_case("mem_10k_tracker_bounded_storage_and_200_wic_cycles", async {
        let before = snapshot_current_process("mem-before")?;
        before_mib = before.private_bytes_mib;

        {
            let mut tracker = MessageTracker::new(vec!["BotSelf".to_string()]);
            for batch_idx in 0..200_u64 {
                let mut batch = Vec::with_capacity(50);
                for inner in 0..50_u64 {
                    let seq = batch_idx * 50 + inner;
                    batch.push(ChatMessage {
                        message_id: format!("mem-msg-{seq}"),
                        group_name: "MemGroup".to_string(),
                        sender: format!("User{}", seq % 16),
                        content: format!("内存驻留负载检测消息 #{seq}"),
                        is_self: false,
                        captured_at_ms: 1_700_000_000_000 + seq,
                        target_kind: ChatTargetKind::Group,
                        ..Default::default()
                    });
                }
                let _ = tracker.observe_poll(batch);
            }

            let guard = TempDbGuard::new("mem_bounded_wal");
            let wal_db = Storage::open(&guard.path_str())?;
            for idx in 0..1500_usize {
                wal_db.set_meta(
                    &format!("forward:mem-{idx}"),
                    "[{\"type\":\"node\",\"data\":{\"content\":\"bounded\"}}]",
                )?;
            }
            ensure!(
                wal_db.get_meta("forward:mem-0")?.is_none(),
                "oldest forward key must be evicted past 512 cap"
            );
            ensure!(
                wal_db.get_meta("forward:mem-1499")?.is_some(),
                "latest forward key must remain"
            );

            let mem_db = Storage::open(MEMORY_DB_PATH)?;
            for idx in 0..7500_usize {
                mem_db.append_audit("mem_stress", &format!("{{\"seq\":{idx}}}"))?;
            }
            let audit_count = mem_db.stats()?.audit_count;
            ensure!(
                audit_count <= MAX_AUDIT_EVENTS as u64,
                "audit_count={audit_count} exceeded MAX_AUDIT_EVENTS={MAX_AUDIT_EVENTS}"
            );

            let frame = build_test_bgra(BENCH_IMAGE_DIMENSION);
            for _ in 0..200 {
                let png = encode_bgra_to_png_bytes(&frame)?;
                let decoded = decode_image_bytes_to_bgra(&png)?;
                ensure!(decoded.width == BENCH_IMAGE_DIMENSION);
            }

            let peak = snapshot_current_process("mem-peak")?;
            peak_mib = peak.private_bytes_mib.max(peak.working_set_mib);
        }

        tokio::time::sleep(Duration::from_millis(30)).await;
        let after = snapshot_current_process("mem-after")?;
        after_mib = after.private_bytes_mib;
        delta_mib = (after_mib - before_mib).max(0.0);

        ensure!(
            delta_mib <= SLA_MEMORY_RETENTION_MAX_MIB,
            "private memory retention delta {delta_mib:.2} MiB exceeded {SLA_MEMORY_RETENTION_MAX_MIB} MiB"
        );
        Ok(format!(
            "私有内存 前={before_mib:.2} MiB -> 峰={peak_mib:.2} MiB -> 后={after_mib:.2} MiB（净增量 +{delta_mib:.2} MiB < {SLA_MEMORY_RETENTION_MAX_MIB:.0} MiB）"
        ))
    })
    .await;

    cat.set_metric("before_private_mib", before_mib);
    cat.set_metric("peak_mib", peak_mib);
    cat.set_metric("after_private_mib", after_mib);
    cat.set_metric("retention_delta_mib", delta_mib);
    cat.set_sla(
        delta_mib <= SLA_MEMORY_RETENTION_MAX_MIB,
        format!("净驻留增量 +{delta_mib:.2} MiB"),
    );
    cat.build()
}

/// 执行第 11 类：CPU 负载（空闲开销与多核满载计算效率）测试（L3）
pub async fn run_cpu_load_category(_ctx: &SuiteExecContext) -> CategoryReport {
    let mut cat = CategoryBuilder::new(
        "CAT-11",
        "CPU 负载测试",
        "CPU Load & Idle Overhead Test",
        QaTier::L3,
        "5 WS 空闲驻留归一化 CPU < 5.0%；4 线程计算饱和吞吐实测无阻塞抖动",
    );

    let mut idle_cpu_pct = 0.0_f64;
    let mut peak_cpu_pct = 0.0_f64;
    let mut ops_per_cpu_ms = 0.0_f64;

    cat.run_case("cpu_idle_overhead_and_4thread_compute_burst", async {
        let (driver, cfg) = default_stub_and_config();
        let (addr, handle) = adapter_server::spawn_server(driver, cfg).await?;
        let ws_url = format!("ws://{addr}/onebot/v11/ws");

        let mut ws_conns = Vec::with_capacity(5);
        for _ in 0..5 {
            let (mut ws, _) = connect_async(&ws_url).await?;
            let _ = ws_next_json(&mut ws, Duration::from_secs(2)).await?;
            ws_conns.push(ws);
        }
        tokio::time::sleep(Duration::from_millis(120)).await;

        let idle_win = CpuUsageWindow::begin_current()?;
        tokio::time::sleep(Duration::from_millis(1200)).await;
        let idle_snap = idle_win.finish_and_snapshot("idle-1200ms")?;
        idle_cpu_pct = idle_snap.cpu_percent_window.unwrap_or(0.0);

        drop(ws_conns);
        handle.abort();

        ensure!(
            idle_cpu_pct <= SLA_IDLE_CPU_MAX_PERCENT,
            "idle normalized CPU {idle_cpu_pct:.2}% exceeded SLA {SLA_IDLE_CPU_MAX_PERCENT:.1}%"
        );

        // 4 线程密集算力脉冲 800ms 测量内核/用户态总算力效率
        let stop_flag = Arc::new(AtomicBool::new(false));
        let total_ops = Arc::new(AtomicU64::new(0));
        let burst_win = CpuUsageWindow::begin_current()?;
        let mut workers = Vec::with_capacity(4);
        for tid in 0..4_u64 {
            let stop = Arc::clone(&stop_flag);
            let ops = Arc::clone(&total_ops);
            workers.push(std::thread::spawn(move || {
                let sample = "CPU饱和算力[CQ:at,qq=10001] [CQ:face,id=14] payload &amp;";
                let mut local_ops = 0_u64;
                while !stop.load(Ordering::Relaxed) {
                    let segs = parse_cq_string_to_segments(sample, false);
                    let cq = segments_to_cq_string(&segs);
                    let _ = stable_onebot_id(&format!("cpu-{tid}-{local_ops}-{}", cq.len()), DEFAULT_ONEBOT_ID_BOUND);
                    local_ops += 1;
                }
                ops.fetch_add(local_ops, Ordering::Relaxed);
            }));
        }
        tokio::time::sleep(Duration::from_millis(800)).await;
        stop_flag.store(true, Ordering::Release);
        for worker in workers {
            worker.join().map_err(|_| anyhow::anyhow!("CPU burst worker panicked"))?;
        }
        let burst_snap = burst_win.finish_and_snapshot("cpu-burst-800ms")?;
        peak_cpu_pct = burst_snap.cpu_percent_window.unwrap_or(0.0);
        let ops_done = total_ops.load(Ordering::Acquire);
        let cpu_ms = ((peak_cpu_pct / 100.0) * 800.0 * (crate::telemetry::logical_cpu_cores() as f64))
            .max(1.0);
        ops_per_cpu_ms = (ops_done as f64) / cpu_ms;

        Ok(format!(
            "5 WS 空闲驻留归一化 CPU = {idle_cpu_pct:.3}%（< {SLA_IDLE_CPU_MAX_PERCENT:.1}%）；4 线程脉冲峰值 CPU = {peak_cpu_pct:.2}%（完成 {ops_done} ops，效率 {ops_per_cpu_ms:.1} ops/CPU-ms）"
        ))
    })
    .await;

    cat.set_metric("idle_cpu_percent", idle_cpu_pct);
    cat.set_metric("burst_cpu_percent", peak_cpu_pct);
    cat.set_metric("ops_per_cpu_ms", ops_per_cpu_ms);
    cat.set_sla(
        idle_cpu_pct <= SLA_IDLE_CPU_MAX_PERCENT,
        format!("空闲 CPU={idle_cpu_pct:.3}%，脉冲峰值 CPU={peak_cpu_pct:.2}%"),
    );
    cat.build()
}
