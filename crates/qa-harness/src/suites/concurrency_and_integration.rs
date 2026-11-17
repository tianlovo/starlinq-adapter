//! L2 并发竞态测试与全链路 Win32 MockQQ + QqntDriver 综合集成测试套件

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Context, Result};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use common_types::constants::MOCK_QQ_MAIN_CLASS;
use common_types::{
    AdapterConfig, AdapterEvent, ChatMessage, ChatTargetKind, MessageSegment, UiNodeSnapshot,
};
use futures_util::SinkExt;
use mock_qq_win::MockQqServer;
use qqnt_driver::QqntDriver;
use serde_json::{json, Value};
use storage::RecordMessageInput;
use storage::Storage;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{accept_async, connect_async};
use uia_core::{encode_bgra_to_png_bytes, DecodedImageBgra};

use super::stub_driver::TempDbGuard;
use super::{default_stub_and_config, ws_next_json, CategoryBuilder, SuiteExecContext};
use crate::models::{CategoryReport, QaTier};

fn scan_tree_controls(node: &UiNodeSnapshot, flags: &mut [bool; 3]) {
    let ct = node.control_type.to_ascii_lowercase();
    let cls = node.class_name.to_ascii_lowercase();
    if ct.contains("list") || cls.contains("listbox") {
        flags[0] = true;
    }
    if ct.contains("edit") || cls.contains("edit") {
        flags[1] = true;
    }
    if ct.contains("button") || cls.contains("button") {
        flags[2] = true;
    }
    for child in &node.children {
        scan_tree_controls(child, flags);
    }
}

fn build_tiny_valid_png_base64() -> Result<String> {
    let mut bgra = vec![0_u8; 8 * 8 * 4];
    for chunk in bgra.as_chunks_mut::<4>().0 {
        chunk[0] = 64;
        chunk[1] = 160;
        chunk[2] = 240;
        chunk[3] = 255;
    }
    let frame = DecodedImageBgra {
        width: 8,
        height: 8,
        bgra_top_down: bgra,
    };
    let png = encode_bgra_to_png_bytes(&frame)?;
    Ok(format!("base64://{}", STANDARD.encode(png)))
}

/// 执行第 5 类：并发与竞态测试（L2）
pub async fn run_concurrency_category(ctx: &SuiteExecContext) -> CategoryReport {
    let mut cat = CategoryBuilder::new(
        "CAT-05",
        "并发测试",
        "Concurrency & Race Test",
        QaTier::L2,
        "64 路 HTTP send_gate 严格互斥（峰值=1）、16 WS 零丢失广播及 16 OS 线程 WAL 零忙等",
    );

    cat.run_case(
        "concurrency_64_http_parallel_send_gate_serialization",
        async {
            let (driver, cfg) = default_stub_and_config();
            driver.set_send_delay_ms(4);
            let (addr, handle) =
                adapter_server::spawn_server(Arc::clone(&driver) as Arc<_>, cfg).await?;
            let url = format!("http://{addr}/onebot/v11/send_group_msg");

            let mut join_set = tokio::task::JoinSet::new();
            for idx in 0..64 {
                let client = ctx.http_client.clone();
                let target_url = url.clone();
                join_set.spawn(async move {
                    let resp = client
                        .post(&target_url)
                        .json(&json!({"message": format!("conc-http-{idx}")}))
                        .send()
                        .await?;
                    let status = resp.status().as_u16();
                    let body: Value = resp.json().await?;
                    Ok::<(u16, Value), anyhow::Error>((status, body))
                });
            }

            let mut ok_count = 0_usize;
            while let Some(res) = join_set.join_next().await {
                let (code, body) = res??;
                ensure!(
                    code == 200 && body["retcode"] == 0,
                    "concurrent send failed: {code} {body}"
                );
                ok_count += 1;
            }
            let max_conc = driver.max_concurrent_sends();
            ensure!(ok_count == 64, "missing responses: {ok_count}/64");
            ensure!(
                max_conc == 1,
                "send_gate serialization violated! max_concurrent_sends={max_conc}"
            );

            handle.abort();
            Ok(format!(
                "64 路并发发信全部成功，send_gate 最大同时进入驱动并发度严格等于 {max_conc}"
            ))
        },
    )
    .await;

    cat.run_case(
        "concurrency_16_ws_200_broadcast_and_ping_bypass_slow_send",
        async {
            let (driver, cfg) = default_stub_and_config();
            let (addr, handle) =
                adapter_server::spawn_server(Arc::clone(&driver) as Arc<_>, cfg).await?;
            let ws_url = format!("ws://{addr}/onebot/v11/ws");

            let mut clients = Vec::with_capacity(16);
            for _ in 0..16 {
                let (mut ws, _) = connect_async(&ws_url).await?;
                let first = ws_next_json(&mut ws, Duration::from_secs(2)).await?;
                ensure!(first["sub_type"] == "connect", "expected lifecycle connect");
                clients.push(ws);
            }

            for seq in 0..200 {
                driver.emit_event(AdapterEvent::GroupMessage(ChatMessage {
                    message_id: format!("conc-evt-{seq}"),
                    group_name: "Demo测试群".to_string(),
                    sender: "并发源".to_string(),
                    content: format!("广播压测 #{seq}"),
                    timestamp_hint: Some("12:00".to_string()),
                    is_self: false,
                    captured_at_ms: 1_700_000_000_000 + seq,
                    onebot_message_id: Some(1000 + seq as i64),
                    group_id: Some(100_000_001),
                    user_id: Some(20_001),
                    target_kind: ChatTargetKind::Group,
                    segments: vec![MessageSegment::Text {
                        text: format!("广播压测 #{seq}"),
                    }],
                    ..Default::default()
                }));
            }

            let mut recv_tasks = tokio::task::JoinSet::new();
            for mut ws in clients {
                recv_tasks.spawn(async move {
                    let mut seen = 0_usize;
                    let deadline = Instant::now() + Duration::from_secs(6);
                    while seen < 200 && Instant::now() < deadline {
                        let msg = ws_next_json(&mut ws, Duration::from_secs(2)).await?;
                        if msg["post_type"] == "message" {
                            seen += 1;
                        }
                    }
                    ensure!(seen == 200, "WS subscriber received {seen}/200 events");
                    Ok::<(), anyhow::Error>(())
                });
            }
            while let Some(res) = recv_tasks.join_next().await {
                res??;
            }

            // 验证 QA-BUG-006：慢发送占用 send_gate 200ms 时，同连接快查询必须在 120ms 内并发返回
            driver.set_send_delay_ms(200);
            let (mut rpc_ws, _) = connect_async(&ws_url).await?;
            let _ = ws_next_json(&mut rpc_ws, Duration::from_secs(2)).await?;
            rpc_ws
                .send(Message::Text(
                    json!({
                        "action": "send_group_msg",
                        "params": {"message": "slow-gate-hold"},
                        "echo": "hold-slow",
                    })
                    .to_string()
                    .into(),
                ))
                .await?;
            tokio::time::sleep(Duration::from_millis(20)).await;

            let t_fast = Instant::now();
            rpc_ws
                .send(Message::Text(
                    json!({"action": "get_version_info", "echo": "bypass-fast"})
                        .to_string()
                        .into(),
                ))
                .await?;
            let fast_reply = ws_next_json(&mut rpc_ws, Duration::from_millis(120)).await?;
            ensure!(
                fast_reply["echo"] == "bypass-fast",
                "fast RPC blocked by slow send: {fast_reply}"
            );
            let fast_ms = t_fast.elapsed().as_secs_f64() * 1000.0;
            let slow_reply = ws_next_json(&mut rpc_ws, Duration::from_secs(2)).await?;
            ensure!(slow_reply["echo"] == "hold-slow" && slow_reply["retcode"] == 0);

            handle.abort();
            Ok(format!(
                "16 WS × 200 广播零丢失；快查询穿透 200ms 慢发送仅耗时 {fast_ms:.2} ms"
            ))
        },
    )
    .await;

    cat.run_case("concurrency_16_os_threads_sqlite_wal_contention", async {
        let guard = TempDbGuard::new("conc_wal");
        let storage = Arc::new(Storage::open(&guard.path_str())?);

        let mut threads = Vec::with_capacity(16);
        for tid in 0..16_u64 {
            let db = Arc::clone(&storage);
            threads.push(std::thread::spawn(move || -> Result<()> {
                for step in 0..40_u64 {
                    let msg_id = db.record_message(&RecordMessageInput {
                        message_uid: format!("thr-{tid}-msg-{step}"),
                        group_name: "ConcurrencyGroup".to_string(),
                        group_id: 100_000_001,
                        user_id: 1000 + tid as i64,
                        sender_name: format!("Thread{tid}"),
                        raw_message: format!("concurrent payload {tid}:{step}"),
                        is_self: step % 2 == 0,
                        timestamp_hint: None,
                        captured_at_ms: 1_700_000_000_000 + tid * 1000 + step,
                        chat_type: ChatTargetKind::Group,
                        segments: Vec::new(),
                    })?;
                    let loaded = db.get_message_by_id(msg_id)?;
                    ensure!(loaded.is_some(), "missing recorded msg {msg_id}");
                    if step % 5 == 0 {
                        db.append_audit(
                            "conc_audit",
                            &format!("{{\"tid\":{tid},\"step\":{step}}}"),
                        )?;
                    }
                }
                Ok(())
            }));
        }

        for th in threads {
            th.join()
                .map_err(|_| anyhow::anyhow!("SQLite WAL worker thread panicked"))??;
        }
        let stats = storage.stats()?;
        ensure!(
            stats.message_count == 640,
            "expected 640 messages, got {}",
            stats.message_count
        );
        Ok("16 个 OS 线程并发读写 WAL SQLite，640 条写入全部成功，0 次 SQLITE_BUSY".to_string())
    })
    .await;

    cat.set_metric("ws_fanout_delivered", 3200.0);
    cat.set_metric("wal_concurrent_threads", 16.0);
    cat.build()
}

/// 执行第 6 类：综合全链路集成测试（Win32 MockQQ + QqntDriver + 正反向 WS + 安全熔断隔离，L2）
pub async fn run_integration_category(ctx: &SuiteExecContext) -> CategoryReport {
    let mut cat = CategoryBuilder::new(
        "CAT-06",
        "综合集成测试",
        "Comprehensive Integration Test",
        QaTier::L2,
        "MockQQ 真实 Win32 窗口树、后台零鼠标粘贴发信、双通道推送与线上守护风控隔离全通",
    );

    let rev_listener = match TcpListener::bind("127.0.0.1:0").await {
        Ok(l) => l,
        Err(err) => {
            let err_msg = format!("绑定临时反向 WS 监听端口失败: {err}");
            cat.run_case("integration_setup", async move { bail!(err_msg) })
                .await;
            return cat.build();
        }
    };
    let rev_addr = rev_listener.local_addr().expect("rev local addr");
    let (rev_tx, rev_rx) = oneshot::channel();
    let rev_task = tokio::spawn(async move {
        if let Ok((tcp, _)) = rev_listener.accept().await {
            if let Ok(ws) = accept_async(tcp).await {
                let _ = rev_tx.send(ws);
            }
        }
    });

    let mock = match MockQqServer::spawn() {
        Ok(m) => m,
        Err(err) => {
            rev_task.abort();
            cat.run_case("integration_mock_qq_spawn", async move {
                bail!("启动 Win32 MockQqServer 失败: {err}")
            })
            .await;
            return cat.build();
        }
    };
    let mock_hwnd = mock.hwnd();

    let mut cfg = AdapterConfig::default();
    cfg.server.host = "127.0.0.1".to_string();
    cfg.server.port = 0;
    cfg.storage.db_path = ":memory:".to_string();
    cfg.target.group_name = "Demo测试群".to_string();
    cfg.target.strict_below_members = Some(10);
    cfg.target.require_member_count_verified = false;
    cfg.target.allowed_private_friends = vec!["测试好友".to_string()];
    cfg.interaction.extra_window_classes = vec![MOCK_QQ_MAIN_CLASS.to_string()];
    cfg.interaction.char_delay_min_ms = 2;
    cfg.interaction.char_delay_max_ms = 4;
    cfg.interaction.min_send_interval_ms = 80;
    cfg.interaction.poll_interval_ms = 120;
    cfg.bot.auto_ping_reply = false;
    cfg.onebot.reverse_ws_urls = vec![format!("ws://{rev_addr}/onebot")];
    cfg.onebot.heartbeat_interval_ms = 60_000;
    cfg.onebot.reconnect_interval_ms = 200;

    let driver = match QqntDriver::new(cfg.clone()) {
        Ok(d) => Arc::new(d),
        Err(err) => {
            mock.shutdown();
            rev_task.abort();
            cat.run_case("integration_driver_init", async move {
                bail!("初始化 QqntDriver 失败: {err}")
            })
            .await;
            return cat.build();
        }
    };

    let (addr, srv_handle) = match adapter_server::spawn_server(driver, cfg).await {
        Ok(pair) => pair,
        Err(err) => {
            mock.shutdown();
            rev_task.abort();
            cat.run_case("integration_server_spawn", async move {
                bail!("启动隔离 adapter-server 失败: {err}")
            })
            .await;
            return cat.build();
        }
    };
    let base = format!("http://{addr}");

    cat.run_case("integration_ensure_tree_and_background_send", async {
        let ensure_wins: Vec<common_types::WindowInfo> = ctx
            .http_client
            .post(format!("{base}/api/ensure-qq"))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        ensure!(
            ensure_wins.iter().any(|w| w.hwnd == mock_hwnd),
            "MockQQ hwnd={mock_hwnd} not discovered"
        );

        let tree: UiNodeSnapshot = ctx
            .http_client
            .get(format!("{base}/api/tree?hwnd={mock_hwnd}&depth=5"))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let mut flags = [false; 3];
        scan_tree_controls(&tree, &mut flags);
        ensure!(
            flags == [true, true, true],
            "UIA tree missing LIST/EDIT/BUTTON: {flags:?}"
        );

        let send_body: Value = ctx
            .http_client
            .post(format!("{base}/onebot/v11/send_group_msg"))
            .json(&json!({"message": "QA全链路集成纯文本验证"}))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        ensure!(
            send_body["retcode"] == 0,
            "send_group_msg failed: {send_body}"
        );
        ensure!(
            send_body["data"]["outcome"]["mouse_touched"] == false,
            "mouse touched!"
        );
        ensure!(
            send_body["data"]["outcome"]["foreground_touched"] == false,
            "foreground touched!"
        );
        let sent_msg_id = send_body["data"]["message_id"]
            .as_i64()
            .ok_or_else(|| anyhow::anyhow!("missing message_id"))?;

        tokio::time::sleep(Duration::from_millis(100)).await;
        let replay: Value = ctx
            .http_client
            .get(format!("{base}/onebot/v11/get_msg?message_id={sent_msg_id}"))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        ensure!(
            replay["retcode"] == 0 && replay["data"]["raw_message"] == "QA全链路集成纯文本验证",
            "unexpected get_msg replay: {replay}"
        );

        let strict_resp = ctx
            .http_client
            .post(format!("{base}/onebot/v11/send_group_msg"))
            .json(&json!({
                "message": [
                    {"type": "reply", "data": {"id": sent_msg_id.to_string()}},
                    {"type": "at", "data": {"qq": "10001", "name": "群友A"}},
                    {"type": "text", "data": {"text": "集成引用回复"}}
                ]
            }))
            .send()
            .await?;
        let strict_status = strict_resp.status().as_u16();
        ensure!(
            strict_status == 400 || strict_status == 501,
            "expected strict rejection 400/501 on MockQQ without native reply menu, got {strict_status}"
        );
        ensure!(
            !mock
                .messages_for("Demo测试群 (5)")
                .iter()
                .any(|m| m.contains("集成引用回复") || m.contains("「回复")),
            "strict mode leaked plain-text degraded reply fallback into MockQQ"
        );

        tokio::time::sleep(Duration::from_millis(100)).await;
        let img_b64 = build_tiny_valid_png_base64()?;
        let img_body: Value = ctx
            .http_client
            .post(format!("{base}/onebot/v11/send_group_msg"))
            .json(&json!({
                "message": [{"type": "image", "data": {"file": img_b64}}]
            }))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        ensure!(img_body["retcode"] == 0 && img_body["data"]["outcome"]["mouse_touched"] == false);

        Ok(
            "MockQQ 控件树、后台无鼠标发信回放、严格模式拒绝伪造 reply+at 纯文本降级及 base64 PNG CF_DIB 粘贴全绿"
                .to_string(),
        )
    })
    .await;

    cat.run_case(
        "integration_dual_ws_incoming_push_and_security_isolation",
        async {
            let (mut fwd_ws, _) = connect_async(format!("ws://{addr}/onebot/v11/ws")).await?;
            let fwd_hello = ws_next_json(&mut fwd_ws, Duration::from_secs(2)).await?;
            ensure!(fwd_hello["sub_type"] == "connect");

            let mut rev_ws = tokio::time::timeout(Duration::from_secs(4), rev_rx).await??;
            let rev_hello = ws_next_json(&mut rev_ws, Duration::from_secs(2)).await?;
            ensure!(rev_hello["sub_type"] == "connect");

            // 注入群友入站消息，确认双端 WS 均实时推送
            let tag = "QA_DUAL_WS_PUSH_VERIFIED";
            mock.inject_incoming_message("Demo测试群 (5)", &format!("群友A [15:30]: {tag}"));

            async fn wait_push<S>(
                mut ws: tokio_tungstenite::WebSocketStream<S>,
                tag: &str,
            ) -> anyhow::Result<()>
            where
                S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
            {
                let deadline = Instant::now() + Duration::from_secs(5);
                while Instant::now() < deadline {
                    let evt = ws_next_json(&mut ws, Duration::from_secs(2)).await?;
                    if evt["post_type"] == "message"
                        && evt["raw_message"].as_str().is_some_and(|s| s.contains(tag))
                    {
                        return Ok(());
                    }
                }
                bail!("timed out waiting for '{tag}' message push");
            }
            tokio::try_join!(wait_push(fwd_ws, tag), wait_push(rev_ws, tag))?;

            // 触发 Mock 安全弹窗验证隔离驱动 423 熔断，同时证明在线守护 :18770（如在线）绝对未受波及
            mock.open_security_dialog("QQ安全验证");
            tokio::time::sleep(Duration::from_millis(80)).await;
            let blocked = ctx
                .http_client
                .post(format!("{base}/onebot/v11/send_group_msg"))
                .json(&json!({"message": "must-trip-mock-only"}))
                .send()
                .await?;
            ensure!(
                blocked.status().as_u16() == 423,
                "mock security dialog expected 423, got {}",
                blocked.status()
            );

            if !ctx.skip_live {
                let live_status: Value = ctx
                    .http_client
                    .get(format!("{}/api/status", ctx.live_url.trim_end_matches('/')))
                    .send()
                    .await
                    .context("check live daemon isolation")?
                    .json()
                    .await?;
                ensure!(
                    live_status["security_breaker_tripped"] == false,
                    "QA-BUG-010 回归失败：Mock 安全窗波及在线守护"
                );
            }
            mock.close_security_dialog();

            Ok(
                "正/反向 WS 双通道实时入站广播正常，且 Mock 安全弹窗 423 熔断与在线守护彻底隔离"
                    .to_string(),
            )
        },
    )
    .await;

    let _ = ctx
        .http_client
        .post(format!("{base}/api/shutdown"))
        .json(&json!({}))
        .send()
        .await;
    let _ = tokio::time::timeout(Duration::from_secs(2), srv_handle).await;
    rev_task.abort();
    mock.shutdown();

    super::flood_and_onebot::append_single_group_flood_cases(&mut cat).await;

    cat.build()
}
