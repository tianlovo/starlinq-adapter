//! L3 高压吞吐测试与极限暴力破坏性测试套件（混合 HTTP 风暴、WS 扇出、TCP 半开与 PRNG 模糊输入）

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::ensure;
use common_types::{
    parse_cq_string_to_segments, AdapterEvent, ChatMessage, ChatTargetKind, MessageSegment,
};
use futures_util::SinkExt;
use reqwest::header::CONTENT_TYPE;
use serde_json::{json, Value};
use storage::{RecordMessageInput, Storage};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

use super::stub_driver::TempDbGuard;
use super::{default_stub_and_config, ws_next_json, CategoryBuilder, SuiteExecContext};
use crate::bench::stats_engine::DeterministicRng;
use crate::constants::{DETERMINISTIC_PRNG_SEED, SLA_POST_CHAOS_HEALTH_MAX_MS, SLA_STRESS_MIN_RPS};
use crate::models::{CategoryReport, QaTier};

/// 执行第 7 类：压力测试（L3）
pub async fn run_stress_category(ctx: &SuiteExecContext) -> CategoryReport {
    let mut cat = CategoryBuilder::new(
        "CAT-07",
        "压力测试",
        "Stress Test",
        QaTier::L3,
        "4,000 混合并发 HTTP 0 次 5xx 且吞吐 >= 1,000 req/s；10 WS × 800 扇出 + 2,000 WAL 写入全绿",
    );

    let mut measured_rps = 0.0_f64;

    cat.run_case("stress_4000_mixed_http_calls_48_workers", async {
        let (driver, cfg) = default_stub_and_config();
        let (addr, handle) = adapter_server::spawn_server(driver, cfg).await?;
        let base = Arc::new(format!("http://{addr}"));
        let ok_counter = Arc::new(AtomicUsize::new(0));
        let send_payload = Arc::new(json!({"message": "stress-payload-keepalive"}));
        let empty_payload = Arc::new(json!({}));

        let total_reqs = 4_000_usize;
        let workers = 48_usize;
        let per_worker = total_reqs / workers;
        let remainder = total_reqs % workers;

        let t0 = Instant::now();
        let mut join_set = tokio::task::JoinSet::new();
        for wid in 0..workers {
            let count = per_worker + usize::from(wid < remainder);
            let client = ctx.http_client.clone();
            let base = Arc::clone(&base);
            let ok = Arc::clone(&ok_counter);
            let send_p = Arc::clone(&send_payload);
            let empty_p = Arc::clone(&empty_payload);
            join_set.spawn(async move {
                for step in 0..count {
                    let resp = match (wid + step) % 4 {
                        0 => client.get(format!("{base}/api/health")).send().await?,
                        1 => client.get(format!("{base}/api/status")).send().await?,
                        2 => {
                            client
                                .post(format!("{base}/onebot/v11/get_version_info"))
                                .json(&*empty_p)
                                .send()
                                .await?
                        }
                        _ => {
                            client
                                .post(format!("{base}/onebot/v11/send_group_msg"))
                                .json(&*send_p)
                                .send()
                                .await?
                        }
                    };
                    let code = resp.status().as_u16();
                    let _ = resp.bytes().await?;
                    ensure!(code == 200, "unexpected HTTP {code} during stress burst");
                    ok.fetch_add(1, Ordering::Relaxed);
                }
                Ok::<(), anyhow::Error>(())
            });
        }

        while let Some(res) = join_set.join_next().await {
            res??;
        }
        let elapsed_s = t0.elapsed().as_secs_f64().max(1e-6);
        let done = ok_counter.load(Ordering::Acquire);
        ensure!(done == total_reqs, "completed {done}/{total_reqs}");
        measured_rps = (done as f64) / elapsed_s;
        ensure!(
            measured_rps >= SLA_STRESS_MIN_RPS,
            "throughput {measured_rps:.1} req/s below SLA {SLA_STRESS_MIN_RPS:.1} req/s"
        );

        handle.abort();
        Ok(format!(
            "48 协程 4,000 次混合请求耗时 {:.2}s（实测 {:.1} req/s >= {:.0} req/s），0 失败",
            elapsed_s, measured_rps, SLA_STRESS_MIN_RPS
        ))
    })
    .await;

    cat.run_case("stress_800_ws_fanout_and_2000_wal_writes", async {
        let (driver, cfg) = default_stub_and_config();
        let (addr, handle) =
            adapter_server::spawn_server(Arc::clone(&driver) as Arc<_>, cfg).await?;
        let ws_url = format!("ws://{addr}/onebot/v11/ws");

        let mut subscribers = Vec::with_capacity(10);
        for _ in 0..10 {
            let (mut ws, _) = connect_async(&ws_url).await?;
            let _ = ws_next_json(&mut ws, Duration::from_secs(2)).await?;
            subscribers.push(ws);
        }

        for seq in 0..800_u64 {
            driver.emit_event(AdapterEvent::GroupMessage(ChatMessage {
                message_id: format!("stress-ws-{seq}"),
                group_name: "Demo测试群".to_string(),
                sender: "压测源".to_string(),
                content: format!("fanout #{seq}"),
                timestamp_hint: None,
                is_self: false,
                captured_at_ms: 1_700_000_000_000 + seq,
                onebot_message_id: Some(10_000 + seq as i64),
                group_id: Some(100_000_001),
                user_id: Some(20_001),
                target_kind: ChatTargetKind::Group,
                segments: vec![MessageSegment::Text {
                    text: format!("fanout #{seq}"),
                }],
                ..Default::default()
            }));
        }

        let mut ws_tasks = tokio::task::JoinSet::new();
        for mut ws in subscribers {
            ws_tasks.spawn(async move {
                let mut seen = 0_usize;
                let deadline = Instant::now() + Duration::from_secs(8);
                while seen < 800 && Instant::now() < deadline {
                    let evt = ws_next_json(&mut ws, Duration::from_secs(2)).await?;
                    if evt["post_type"] == "message" {
                        seen += 1;
                    }
                }
                ensure!(seen == 800, "stress WS client got {seen}/800 events");
                Ok::<(), anyhow::Error>(())
            });
        }
        while let Some(res) = ws_tasks.join_next().await {
            res??;
        }
        handle.abort();

        let guard = TempDbGuard::new("stress_wal_2k");
        let db = Storage::open(&guard.path_str())?;
        let t_db = Instant::now();
        for seq in 0..2_000_u64 {
            db.record_message(&RecordMessageInput {
                message_uid: format!("stress-wal-{seq}"),
                group_name: "StressWalGroup".to_string(),
                group_id: 100_000_001,
                user_id: 20_001,
                sender_name: "WalBench".to_string(),
                raw_message: format!("wal payload #{seq}"),
                captured_at_ms: 1_700_000_100_000 + seq,
                ..Default::default()
            })?;
        }
        let wal_ms = t_db.elapsed().as_secs_f64() * 1000.0;
        let stats = db.stats()?;
        ensure!(
            stats.message_count == 2_000,
            "expected 2000 WAL rows, got {}",
            stats.message_count
        );
        Ok(format!(
            "10 WS × 800 事件全部送达（8,000 帧）；2,000 条 WAL 持久化耗时 {wal_ms:.1} ms"
        ))
    })
    .await;

    super::flood_and_onebot::append_multi_group_flood_case(&mut cat).await;

    cat.set_metric("stress_throughput_rps", measured_rps);
    cat.set_sla(
        measured_rps >= SLA_STRESS_MIN_RPS,
        format!("{measured_rps:.1} req/s"),
    );
    cat.build()
}

/// 执行第 8 类：极限暴力与混沌破坏性测试（L3）
pub async fn run_bruteforce_category(ctx: &SuiteExecContext) -> CategoryReport {
    let mut cat = CategoryBuilder::new(
        "CAT-08",
        "极限暴力测试",
        "Extreme Brute-Force Test",
        QaTier::L3,
        "TCP 半开断连、400 模糊脏包、80 WS 畸形脉冲与 500 async 风暴后 < 50ms 零崩自愈",
    );

    cat.run_case("bruteforce_50_abrupt_tcp_drops_and_truncated_headers", async {
        let (driver, cfg) = default_stub_and_config();
        let (addr, handle) = adapter_server::spawn_server(driver, cfg).await?;

        for idx in 0..50_usize {
            let mut tcp = TcpStream::connect(addr).await?;
            let payload: &[u8] = match idx % 4 {
                0 => b"POST /onebot/v11/send_group_msg HTTP/1.1\r\nHost: 127.0.0.1\r\n",
                1 => b"\xFF\x00\xFE\x99GARBAGE_PROTO_NO_CRLF",
                2 => b"GET /api/health HTTP/1.1\r\nContent-Length: 9999999999999999999999\r\n\r\n",
                _ => b"POST /onebot/v11/get_status HTTP/1.1\r\nHost: x\r\nContent-Length: 500\r\n\r\n{",
            };
            let _ = tcp.write_all(payload).await;
            let _ = tcp.shutdown().await;
            drop(tcp);
        }

        let health: Value = ctx
            .http_client
            .get(format!("http://{addr}/api/health"))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        ensure!(health["ok"] == true, "server unhealthy after 50 abrupt TCP resets");
        handle.abort();
        Ok("50 次原始 TCP 半开/截断请求头与二进制垃圾字节冲击后服务健康正常".to_string())
    })
    .await;

    cat.run_case(
        "bruteforce_400_prng_fuzzed_json_cq_and_form_payloads",
        async {
            let (driver, cfg) = default_stub_and_config();
            let (addr, handle) = adapter_server::spawn_server(driver, cfg).await?;
            let endpoint = format!("http://{addr}/onebot/v11/send_group_msg");
            let mut rng = DeterministicRng::new(DETERMINISTIC_PRNG_SEED);

            let fragments = [
                "中文",
                "😀",
                "\0",
                "\u{202E}",
                "%E6%",
                "%ZZ",
                "[CQ:face,id=",
                "[CQ:at,qq=100",
                "&amp;&#91;",
                "\",\"",
                "{[",
                "\r\n",
                "]]]",
                "===",
            ];

            for i in 0..400_usize {
                let mut fuzzed = String::with_capacity(128);
                for _ in 0..(4 + (rng.next_usize(12))) {
                    fuzzed.push_str(fragments[rng.next_usize(fragments.len())]);
                }
                let _ = parse_cq_string_to_segments(&fuzzed, (i & 1) == 0);

                let resp = match i % 3 {
                    0 => {
                        ctx.http_client
                            .post(&endpoint)
                            .json(&json!({"message": fuzzed, "group_id": Value::Null}))
                            .send()
                            .await?
                    }
                    1 => {
                        ctx.http_client
                            .post(&endpoint)
                            .header(CONTENT_TYPE, "application/json")
                            .body(format!("{{\"broken\":{i},\"raw\":\"{fuzzed}"))
                            .send()
                            .await?
                    }
                    _ => {
                        ctx.http_client
                            .post(&endpoint)
                            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
                            .body(format!(
                                "group_id=abc%FF&message={fuzzed}&auto_escape=maybe"
                            ))
                            .send()
                            .await?
                    }
                };
                let status = resp.status().as_u16();
                ensure!(
                    status < 500 || status == 501,
                    "unexpected 5xx response {status} on fuzz vector #{i}"
                );
            }

            handle.abort();
            Ok(
                "400 组确定性 PRNG 变异 JSON / CQ / 表单脏包触发 0 次 panic 与 0 次 500 内部异常"
                    .to_string(),
            )
        },
    )
    .await;

    let mut recovery_ms = 0.0_f64;
    cat.run_case("bruteforce_80_ws_chaos_500_async_flood_and_post_chaos_health", async {
        let (driver, cfg) = default_stub_and_config();
        let (addr, handle) =
            adapter_server::spawn_server(Arc::clone(&driver) as Arc<_>, cfg).await?;
        let base = format!("http://{addr}");
        let ws_url = format!("ws://{addr}/onebot/v11/ws");

        // 80 次高频 WS 畸形 JSON / 二进制帧 / 闪断风暴
        for round in 0..80_usize {
            let (mut ws, _) = connect_async(&ws_url).await?;
            let _ = ws.send(Message::Text(format!("{{malformed_{round}").into())).await;
            let _ = ws.send(Message::Binary(vec![0xFF, 0x00, round as u8].into())).await;
            let _ = ws.send(Message::Close(None)).await;
        }

        // 500 并发 _async 动作洪水，人工延迟 40ms 触发 MAX_INFLIGHT_ASYNC_ACTIONS=256 背压拒收
        driver.set_send_delay_ms(40);
        let mut flood = tokio::task::JoinSet::new();
        for i in 0..500_usize {
            let client = ctx.http_client.clone();
            let url = format!("{base}/onebot/v11/send_group_msg_async");
            flood.spawn(async move {
                let resp = client
                    .post(&url)
                    .json(&json!({"message": format!("async-flood-{i}")}))
                    .send()
                    .await?;
                let code = resp.status().as_u16();
                let body: Value = resp.json().await?;
                Ok::<(u16, i64), anyhow::Error>((code, body["retcode"].as_i64().unwrap_or(-1)))
            });
        }

        let mut accepted = 0_usize;
        let mut backpressured = 0_usize;
        while let Some(res) = flood.join_next().await {
            let (code, ret) = res??;
            ensure!(code == 200 || code == 400, "unexpected async flood code {code}");
            if ret == 1 {
                accepted += 1;
            } else if ret == 1400 {
                backpressured += 1;
            }
        }
        ensure!(accepted > 0 && accepted + backpressured == 500, "unaccounted async flood replies");
        driver.set_send_delay_ms(0);
        tokio::time::sleep(Duration::from_millis(120)).await;

        let t_heal = Instant::now();
        let health: Value = ctx
            .http_client
            .get(format!("{base}/api/health"))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        recovery_ms = t_heal.elapsed().as_secs_f64() * 1000.0;
        ensure!(health["ok"] == true);
        ensure!(
            recovery_ms <= SLA_POST_CHAOS_HEALTH_MAX_MS,
            "post-chaos health {recovery_ms:.2} ms exceeded SLA {SLA_POST_CHAOS_HEALTH_MAX_MS} ms"
        );

        handle.abort();
        Ok(format!(
            "80 WS 混沌脉冲 + 500 _async 洪水（受理={accepted}, 背压={backpressured}），自愈探活 {recovery_ms:.2} ms"
        ))
    })
    .await;

    cat.set_metric("post_chaos_recovery_ms", recovery_ms);
    cat.build()
}
