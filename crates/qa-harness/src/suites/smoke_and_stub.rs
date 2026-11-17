//! L0 冒烟测试与 L1 打桩/故障注入测试套件（支持只读在线守护探活与全栈正反向 WS / Webhook 验证）

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Context};
use axum::body::Bytes;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::post;
use axum::{Json, Router};
use common_types::{AdapterEvent, ChatMessage, ChatTargetKind, MessageSegment};
use futures_util::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{accept_hdr_async, connect_async};

use super::stub_driver::StubFaultMode;
use super::{default_stub_and_config, ws_next_json, CategoryBuilder, SuiteExecContext};
use crate::models::{CategoryReport, QaTier};
use crate::telemetry::{snapshot_process, CpuUsageWindow};

/// 捕获本地模拟 Webhook 收到的请求头与原始签名字节载荷
#[derive(Debug, Clone)]
struct CapturedWebhookRequest {
    self_id: Option<String>,
    signature: Option<String>,
    body_bytes: Vec<u8>,
}

async fn webhook_capture_handler(
    State(store): State<Arc<Mutex<Vec<CapturedWebhookRequest>>>>,
    headers: HeaderMap,
    body: Bytes,
) -> Json<Value> {
    let self_id = headers
        .get("x-self-id")
        .and_then(|v| v.to_str().ok())
        .map(ToOwned::to_owned);
    let signature = headers
        .get("x-signature")
        .and_then(|v| v.to_str().ok())
        .map(ToOwned::to_owned);
    store.lock().push(CapturedWebhookRequest {
        self_id,
        signature,
        body_bytes: body.to_vec(),
    });
    Json(json!({
        "reply": "webhook自动回复",
        "auto_escape": false,
        "at_sender": true,
    }))
}

/// 执行第 1 类：冒烟测试（L0）
pub async fn run_smoke_category(ctx: &SuiteExecContext) -> CategoryReport {
    let mut cat = CategoryBuilder::new(
        "CAT-01",
        "冒烟测试",
        "Smoke Test",
        QaTier::L0,
        "隔离基础链路 100% 通过 + 在线守护只读健康且未熔断",
    );

    cat.run_case("smoke_isolated_rest_and_onebot_actions", async {
        let (driver, cfg) = default_stub_and_config();
        let (addr, handle) = adapter_server::spawn_server(driver, cfg).await?;
        let base = format!("http://{addr}");

        let health: Value = ctx
            .http_client
            .get(format!("{base}/api/health"))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        ensure!(health["ok"] == true, "health ok must be true: {health}");

        for action in [
            "get_status",
            "get_version_info",
            "get_login_info",
            "get_group_list",
        ] {
            let body: Value = ctx
                .http_client
                .post(format!("{base}/onebot/v11/{action}"))
                .json(&json!({}))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            ensure!(body["retcode"] == 0, "{action} retcode != 0: {body}");
        }
        handle.abort();
        Ok("隔离端口 /api/health 与 4 项核心 OneBot 动作响应正常".to_string())
    })
    .await;

    cat.run_case("smoke_isolated_forward_ws_lifecycle_and_ping", async {
        let (driver, cfg) = default_stub_and_config();
        let (addr, handle) = adapter_server::spawn_server(driver, cfg).await?;
        let (mut ws, _) = connect_async(format!("ws://{addr}/onebot/v11/ws")).await?;

        let lifecycle = ws_next_json(&mut ws, Duration::from_secs(2)).await?;
        ensure!(
            lifecycle["meta_event_type"] == "lifecycle" && lifecycle["sub_type"] == "connect",
            "unexpected first WS event: {lifecycle}"
        );

        ws.send(Message::Text(
            json!({"action": "get_version_info", "echo": "smoke-ws"})
                .to_string()
                .into(),
        ))
        .await?;
        let reply = ws_next_json(&mut ws, Duration::from_secs(2)).await?;
        ensure!(
            reply["echo"] == "smoke-ws" && reply["retcode"] == 0,
            "bad WS RPC reply: {reply}"
        );

        ws.send(Message::Ping(b"smk".to_vec().into())).await?;
        let pong = tokio::time::timeout(Duration::from_secs(2), async {
            while let Some(Ok(frame)) = ws.next().await {
                if let Message::Pong(p) = frame {
                    return Ok(p);
                }
            }
            bail!("WS closed before Pong")
        })
        .await??;
        ensure!(pong.as_ref() == b"smk", "Pong payload mismatch");
        handle.abort();
        Ok("正向 WS 生命周期握手、RPC 与 Ping/Pong 控制帧均通过".to_string())
    })
    .await;

    if ctx.skip_live {
        cat.push_skipped(
            "smoke_live_daemon_readonly",
            "命令行传入 --skip-live，跳过在线守护只读探活",
        );
    } else {
        cat.run_case("smoke_live_daemon_readonly_and_telemetry", async {
            let base = ctx.live_url.trim_end_matches('/');
            let health: Value = ctx
                .http_client
                .get(format!("{base}/api/health"))
                .send()
                .await
                .with_context(|| format!("连接在线守护 {base}/api/health 失败"))?
                .json()
                .await?;
            ensure!(health["ok"] == true, "live health not ok: {health}");
            let pid = health["pid"]
                .as_u64()
                .ok_or_else(|| anyhow::anyhow!("missing live daemon pid"))?
                as u32;
            let cpu_win = CpuUsageWindow::begin_pid(pid).ok();

            let status: Value = ctx
                .http_client
                .get(format!("{base}/api/status"))
                .send()
                .await?
                .json()
                .await?;
            ensure!(
                status["security_breaker_tripped"] == false,
                "在线守护进程处于安全熔断状态: {status}"
            );
            ensure!(
                status["sandbox_isolated"] == false,
                "在线守护进程处于沙箱隔离桌面: {status}"
            );

            let ob_status: Value = ctx
                .http_client
                .post(format!("{base}/onebot/v11/get_status"))
                .json(&json!({}))
                .send()
                .await?
                .json()
                .await?;
            ensure!(
                ob_status["retcode"] == 0,
                "live OneBot get_status failed: {ob_status}"
            );

            let _storage_stats: Value = ctx
                .http_client
                .get(format!("{base}/api/storage/stats"))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;

            let live_snap = cpu_win
                .and_then(|win| win.finish_and_snapshot("live-daemon").ok())
                .or_else(|| snapshot_process(pid, "live-daemon").ok());
            if let Some(snap) = live_snap {
                *ctx.live_telemetry.lock() = Some(snap.clone());
                return Ok(format!(
                    "在线守护 PID={} 只读健康正常，工作集 {:.2} MiB，群={:?}",
                    pid, snap.working_set_mib, status["active_group"]
                ));
            }
            Ok(format!("在线守护 PID={pid} 只读健康正常"))
        })
        .await;
    }

    let report = cat.build();
    let passed = report.passed;
    let mut final_report = report;
    final_report.sla_actual = if passed {
        if final_report.skipped_cases > 0 {
            format!(
                "{}/{} 冒烟探针通过（{} 跳过，不计入通过数）",
                final_report.passed_cases, final_report.total_cases, final_report.skipped_cases
            )
        } else {
            format!(
                "{}/{} 冒烟探针全绿",
                final_report.passed_cases, final_report.total_cases
            )
        }
    } else {
        "存在失败的冒烟探针".to_string()
    };
    final_report
}

/// 执行第 2 类：打桩与故障注入测试（L1）
pub async fn run_stub_category(ctx: &SuiteExecContext) -> CategoryReport {
    let mut cat = CategoryBuilder::new(
        "CAT-02",
        "打桩测试",
        "Stub & Fault-Injection Test",
        QaTier::L1,
        "全部可编程故障状态码、HMAC Webhook 签名与反向 WS 握手头 100% 精确",
    );

    cat.run_case("stub_fault_injection_423_403_500_and_10004_matrix", async {
        let (driver, cfg) = default_stub_and_config();
        let (addr, handle) = adapter_server::spawn_server(Arc::clone(&driver) as Arc<_>, cfg).await?;
        let base = format!("http://{addr}");

        driver.set_fault_mode(StubFaultMode::SecurityBreaker);
        let resp = ctx
            .http_client
            .post(format!("{base}/onebot/v11/send_group_msg"))
            .json(&json!({"message": "fault-sec"}))
            .send()
            .await?;
        ensure!(resp.status().as_u16() == 423, "expected 423, got {}", resp.status());
        let body: Value = resp.json().await?;
        ensure!(body["retcode"] == 1423, "expected retcode 1423, got {body}");

        driver.set_fault_mode(StubFaultMode::MemberCountExceeded);
        let resp = ctx
            .http_client
            .post(format!("{base}/onebot/v11/send_group_msg"))
            .json(&json!({"message": "fault-member"}))
            .send()
            .await?;
        ensure!(resp.status().as_u16() == 403, "expected 403, got {}", resp.status());
        let body: Value = resp.json().await?;
        ensure!(body["retcode"] == 1403, "expected retcode 1403, got {body}");

        driver.set_fault_mode(StubFaultMode::UiaFailure);
        let resp = ctx
            .http_client
            .post(format!("{base}/onebot/v11/send_group_msg"))
            .json(&json!({"message": "fault-uia"}))
            .send()
            .await?;
        ensure!(resp.status().as_u16() == 500, "expected 500, got {}", resp.status());
        let body: Value = resp.json().await?;
        ensure!(
            body["retcode"] == 1500 && body["code"] == "INTERNAL_ERROR",
            "expected retcode 1500 / INTERNAL_ERROR, got {body}"
        );

        driver.set_fault_mode(StubFaultMode::Healthy);
        for unsupported in ["set_group_kick", "get_cookies", "send_like"] {
            let u_resp = ctx
                .http_client
                .post(format!("{base}/onebot/v11/{unsupported}"))
                .json(&json!({}))
                .send()
                .await?;
            ensure!(u_resp.status().as_u16() == 501, "{unsupported} expected 501");
            let u_body: Value = u_resp.json().await?;
            ensure!(u_body["retcode"] == 10004, "{unsupported} expected 10004: {u_body}");
        }

        handle.abort();
        Ok("SecurityBreaker(423/1423)、MemberExceeded(403/1403)、UiaFailure(500/1500) 与 10004 矩阵精确命中".to_string())
    })
    .await;

    cat.run_case("stub_http_webhook_hmac_sha1_and_quick_operation", async {
        let captured = Arc::new(Mutex::new(Vec::<CapturedWebhookRequest>::new()));
        let hook_app = Router::new()
            .route("/hook", post(webhook_capture_handler))
            .with_state(Arc::clone(&captured));
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let hook_addr = listener.local_addr()?;
        let hook_task = tokio::spawn(async move {
            let _ = axum::serve(listener, hook_app).await;
        });

        let (driver, mut cfg) = default_stub_and_config();
        cfg.bot.self_id = 10_001;
        cfg.onebot.http_post_urls = vec![format!("http://{hook_addr}/hook")];
        cfg.onebot.secret = Some("qa-hook-secret".to_string());
        let (_addr, srv_handle) =
            adapter_server::spawn_server(Arc::clone(&driver) as Arc<_>, cfg).await?;

        tokio::time::sleep(Duration::from_millis(80)).await;
        driver.emit_event(AdapterEvent::GroupMessage(ChatMessage {
            message_id: "stub-hook-1".to_string(),
            group_name: "Demo测试群".to_string(),
            sender: "群友Stub".to_string(),
            content: "触发 Webhook".to_string(),
            timestamp_hint: Some("10:00".to_string()),
            is_self: false,
            captured_at_ms: chrono::Utc::now().timestamp_millis().max(0) as u64,
            onebot_message_id: Some(77_001),
            group_id: Some(100_000_001),
            user_id: Some(20_001),
            target_kind: ChatTargetKind::Group,
            segments: vec![MessageSegment::Text {
                text: "触发 Webhook".to_string(),
            }],
            ..Default::default()
        }));

        let deadline = Instant::now() + Duration::from_secs(3);
        while (captured.lock().is_empty() || driver.sent_requests().is_empty())
            && Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        {
            let guard = captured.lock();
            ensure!(guard.len() == 1, "webhook call count != 1: {}", guard.len());
            ensure!(
                guard[0].self_id.as_deref() == Some("10001"),
                "X-Self-ID mismatch"
            );
            let expected = format!(
                "sha1={}",
                adapter_server::onebot::hmac_sha1_hex(b"qa-hook-secret", &guard[0].body_bytes)
            );
            ensure!(
                guard[0].signature.as_deref() == Some(expected.as_str()),
                "HMAC signature mismatch: {:?} != {expected}",
                guard[0].signature
            );
        }
        let sent = driver.sent_requests();
        ensure!(
            !sent.is_empty() && sent[0].text.contains("webhook自动回复"),
            "quick_operation auto reply missing"
        );

        srv_handle.abort();
        hook_task.abort();
        Ok("HMAC-SHA1 X-Signature 签名校验与 quick_operation 自动回信验证通过".to_string())
    })
    .await;

    cat.run_case(
        "stub_reverse_ws_handshake_headers_and_action_dispatch",
        async {
            let rev_listener = TcpListener::bind("127.0.0.1:0").await?;
            let rev_addr = rev_listener.local_addr()?;
            let (tx, rx) = oneshot::channel();
            let rev_accept = tokio::spawn(async move {
                if let Some(captured) = capture_rev_connection(rev_listener).await {
                    let _ = tx.send(captured);
                }
            });

            let (driver, mut cfg) = default_stub_and_config();
            cfg.bot.self_id = 10_001;
            cfg.onebot.reverse_ws_urls = vec![format!("ws://{rev_addr}/onebot")];
            let (_addr, srv_handle) = adapter_server::spawn_server(driver, cfg).await?;

            let (hdr_self, hdr_role, mut rev_ws) =
                tokio::time::timeout(Duration::from_secs(3), rx).await??;
            ensure!(
                hdr_self.as_deref() == Some("10001"),
                "unexpected X-Self-ID: {hdr_self:?}"
            );
            ensure!(
                hdr_role.as_deref() == Some("Universal"),
                "unexpected X-Client-Role: {hdr_role:?}"
            );

            let lifecycle = ws_next_json(&mut rev_ws, Duration::from_secs(2)).await?;
            ensure!(
                lifecycle["sub_type"] == "connect",
                "missing reverse WS connect event"
            );

            rev_ws
                .send(Message::Text(
                    json!({"action": "get_login_info", "echo": "stub-rev-1"})
                        .to_string()
                        .into(),
                ))
                .await?;
            let reply = ws_next_json(&mut rev_ws, Duration::from_secs(2)).await?;
            ensure!(
                reply["echo"] == "stub-rev-1" && reply["retcode"] == 0,
                "reverse action failed"
            );

            srv_handle.abort();
            rev_accept.abort();
            Ok("反向 WS 主动建连、X-Self-ID/X-Client-Role 握手头及动作回执验证通过".to_string())
        },
    )
    .await;

    cat.run_case(
        "stub_truthful_unconfirmed_send_1500_and_media_record_10004",
        async {
            let (driver, cfg) = default_stub_and_config();
            let (addr, handle) =
                adapter_server::spawn_server(Arc::clone(&driver) as Arc<_>, cfg).await?;
            let base = format!("http://{addr}");

            driver.set_force_unconfirmed_send(true);
            let unconfirmed_resp = ctx
                .http_client
                .post(format!("{base}/onebot/v11/send_group_msg"))
                .json(&json!({ "message": "stub-unconfirmed" }))
                .send()
                .await?;
            ensure!(
                unconfirmed_resp.status().as_u16() == 500,
                "expected HTTP 500 on unconfirmed send, got {}",
                unconfirmed_resp.status()
            );
            let unconfirmed_body: Value = unconfirmed_resp.json().await?;
            ensure!(
                unconfirmed_body["retcode"] == 1500,
                "expected retcode=1500 on unconfirmed send, got {unconfirmed_body}"
            );

            driver.set_force_unconfirmed_send(false);
            let rec_resp = ctx
                .http_client
                .post(format!("{base}/onebot/v11/send_group_msg"))
                .json(&json!({
                    "message": [{ "type": "record", "data": { "file": "voice.amr" } }]
                }))
                .send()
                .await?;
            ensure!(
                rec_resp.status().as_u16() == 501,
                "expected HTTP 501 on unsupported record, got {}",
                rec_resp.status()
            );
            let rec_body: Value = rec_resp.json().await?;
            ensure!(
                rec_body["retcode"] == 10004,
                "expected retcode=10004 on unsupported record, got {rec_body}"
            );

            handle.abort();
            Ok(
                "未确认发送(ok=false)->1500 与默认禁降级语音段(record)->10004 门禁断言通过"
                    .to_string(),
            )
        },
    )
    .await;

    cat.run_case("stub_poke_and_recall_actions", async {
        let (driver, cfg) = default_stub_and_config();
        let (addr, handle) =
            adapter_server::spawn_server(Arc::clone(&driver) as Arc<_>, cfg).await?;
        let base = format!("http://{addr}");

        let send_resp = ctx
            .http_client
            .post(format!("{base}/onebot/v11/send_group_msg"))
            .json(&json!({ "message": "stub-recall-target" }))
            .send()
            .await?;
        ensure!(send_resp.status().is_success(), "stub send failed");
        let send_body: Value = send_resp.json().await?;
        let msg_id = send_body["data"]["message_id"]
            .as_i64()
            .ok_or_else(|| anyhow::anyhow!("missing stub message_id: {send_body}"))?;

        let get_ok = ctx
            .http_client
            .post(format!("{base}/onebot/v11/get_msg"))
            .json(&json!({ "message_id": msg_id }))
            .send()
            .await?;
        ensure!(get_ok.status().is_success(), "get_msg before recall failed");

        let del_ok = ctx
            .http_client
            .post(format!("{base}/onebot/v11/delete_msg"))
            .json(&json!({ "message_id": msg_id }))
            .send()
            .await?;
        ensure!(
            del_ok.status().is_success(),
            "delete_msg expected 200, got {}",
            del_ok.status()
        );

        let get_gone = ctx
            .http_client
            .post(format!("{base}/onebot/v11/get_msg"))
            .json(&json!({ "message_id": msg_id }))
            .send()
            .await?;
        ensure!(
            get_gone.status().as_u16() == 400,
            "get_msg after recall expected 400, got {}",
            get_gone.status()
        );

        let del_missing = ctx
            .http_client
            .post(format!("{base}/onebot/v11/delete_msg"))
            .json(&json!({ "message_id": 999_999 }))
            .send()
            .await?;
        ensure!(
            del_missing.status().as_u16() == 400,
            "delete_msg unknown id expected 400, got {}",
            del_missing.status()
        );

        let poke_group = ctx
            .http_client
            .post(format!("{base}/onebot/v11/send_poke"))
            .json(&json!({ "group_id": 100_000_001_i64, "user_id": 20_001_i64 }))
            .send()
            .await?;
        ensure!(
            poke_group.status().as_u16() == 501,
            "group send_poke expected 501, got {}",
            poke_group.status()
        );
        let poke_group_body: Value = poke_group.json().await?;
        ensure!(
            poke_group_body["retcode"] == 10004,
            "group send_poke expected retcode=10004, got {poke_group_body}"
        );

        let poke_priv = ctx
            .http_client
            .post(format!("{base}/onebot/v11/friend_poke"))
            .json(&json!({ "user_id": 200_000_002_i64 }))
            .send()
            .await?;
        ensure!(
            poke_priv.status().as_u16() == 501,
            "friend_poke expected 501, got {}",
            poke_priv.status()
        );
        let poke_priv_body: Value = poke_priv.json().await?;
        ensure!(
            poke_priv_body["retcode"] == 10004,
            "friend_poke expected retcode=10004, got {poke_priv_body}"
        );

        let poke_cq = ctx
            .http_client
            .post(format!("{base}/onebot/v11/send_group_msg"))
            .json(&json!({ "group_id": 100_000_001_i64, "message": "[CQ:poke,qq=20001]" }))
            .send()
            .await?;
        ensure!(
            poke_cq.status().as_u16() == 501,
            "[CQ:poke] send_group_msg expected 501, got {}",
            poke_cq.status()
        );
        let poke_cq_body: Value = poke_cq.json().await?;
        ensure!(
            poke_cq_body["retcode"] == 10004,
            "[CQ:poke] expected retcode=10004, got {poke_cq_body}"
        );

        let poke_count = driver
            .sent_requests()
            .iter()
            .filter(|req| {
                req.segments
                    .iter()
                    .any(|seg| matches!(seg, common_types::MessageSegment::Poke { .. }))
                    || req.text.contains("戳一戳")
                    || req.text.contains("CQ:poke")
            })
            .count();
        ensure!(
            poke_count == 0,
            "expected 0 outbound poke or degraded text requests recorded, got {poke_count}"
        );

        handle.abort();
        Ok(
            "Stub 通道 delete_msg(含get_msg联动与400缺失校验) 及出站戳一戳 501/10004 严格门禁断言通过"
                .to_string(),
        )
    })
    .await;

    cat.run_case("stub_rest_open_private_delete_msg_and_history", async {
        let (driver, cfg) = default_stub_and_config();
        let (addr, handle) =
            adapter_server::spawn_server(Arc::clone(&driver) as Arc<_>, cfg).await?;
        let base = format!("http://{addr}");

        // 1. 原生 /api/open-private 允许白名单好友并拒绝空白或未授权目标
        let open_empty = ctx
            .http_client
            .post(format!("{base}/api/open-private"))
            .json(&json!({ "target": "  " }))
            .send()
            .await?;
        ensure!(
            open_empty.status().as_u16() == 400,
            "empty /api/open-private expected 400, got {}",
            open_empty.status()
        );

        let open_denied = ctx
            .http_client
            .post(format!("{base}/api/open-private"))
            .json(&json!({ "target": "未授权好友" }))
            .send()
            .await?;
        ensure!(
            open_denied.status().is_client_error(),
            "denied /api/open-private expected 4xx, got {}",
            open_denied.status()
        );

        let open_ok = ctx
            .http_client
            .post(format!("{base}/api/open-private"))
            .json(&json!({ "target": "测试好友" }))
            .send()
            .await?;
        ensure!(
            open_ok.status().is_success(),
            "whitelisted /api/open-private expected 200, got {}",
            open_ok.status()
        );
        let open_body: Value = open_ok.json().await?;
        ensure!(
            open_body["attached_window"]["hwnd"] == 0x1024,
            "unexpected /api/open-private status body: {open_body}"
        );

        // 2. 先经由 /api/send 写入打桩存储，再验证 /api/messages/history 只读查询与 /api/delete-msg 撤回
        let send_resp = ctx
            .http_client
            .post(format!("{base}/api/send"))
            .json(&json!({ "group": "Demo测试群", "text": "rest-stub-history-entry" }))
            .send()
            .await?;
        ensure!(send_resp.status().is_success(), "stub /api/send failed");
        let send_body: Value = send_resp.json().await?;
        let msg_id = send_body["onebot_message_id"]
            .as_i64()
            .ok_or_else(|| anyhow::anyhow!("missing onebot_message_id: {send_body}"))?;

        let history_before = ctx
            .http_client
            .get(format!("{base}/api/messages/history"))
            .query(&[("group", "Demo测试群"), ("limit", "20")])
            .send()
            .await?;
        ensure!(
            history_before.status().is_success(),
            "/api/messages/history expected 200, got {}",
            history_before.status()
        );
        let history_items: Vec<Value> = history_before.json().await?;
        ensure!(
            history_items
                .iter()
                .any(|item| item["onebot_message_id"] == msg_id && item["raw_message"] == "rest-stub-history-entry"),
            "expected sent message {msg_id} in /api/messages/history, got {history_items:?}"
        );

        let del_bad = ctx
            .http_client
            .post(format!("{base}/api/delete-msg"))
            .json(&json!({ "message_id": 0 }))
            .send()
            .await?;
        ensure!(
            del_bad.status().as_u16() == 400,
            "non-positive /api/delete-msg expected 400, got {}",
            del_bad.status()
        );

        let del_ok = ctx
            .http_client
            .post(format!("{base}/api/delete-msg"))
            .json(&json!({ "message_id": msg_id }))
            .send()
            .await?;
        ensure!(
            del_ok.status().is_success(),
            "valid /api/delete-msg expected 200, got {}",
            del_ok.status()
        );
        let del_body: Value = del_ok.json().await?;
        ensure!(
            del_body["ok"] == true && del_body["message_id"] == msg_id,
            "unexpected /api/delete-msg response body: {del_body}"
        );

        let history_after = ctx
            .http_client
            .get(format!("{base}/api/messages/history"))
            .query(&[("group", "Demo测试群"), ("limit", "20")])
            .send()
            .await?;
        ensure!(history_after.status().is_success());
        let after_items: Vec<Value> = history_after.json().await?;
        ensure!(
            after_items.iter().all(|item| item["onebot_message_id"] != msg_id),
            "recalled message {msg_id} unexpectedly persisted in history: {after_items:?}"
        );

        handle.abort();
        Ok(
            "Stub 原生 REST /api/open-private、/api/messages/history 与 /api/delete-msg 联动断言通过"
                .to_string(),
        )
    })
    .await;

    super::flood_and_onebot::append_onebot_v11_and_deadline_cases(&mut cat, ctx).await;

    cat.set_metric("fault_modes_verified", 5.0);
    cat.build()
}

#[allow(clippy::result_large_err)]
async fn capture_rev_connection(
    rev_listener: TcpListener,
) -> Option<(
    Option<String>,
    Option<String>,
    tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
)> {
    let Ok((tcp, _)) = rev_listener.accept().await else {
        return None;
    };
    let mut hdr_self = None;
    let mut hdr_role = None;
    let ws = accept_hdr_async(tcp, |req: &Request, resp: Response| {
        extract_reverse_ws_headers(req, &mut hdr_self, &mut hdr_role);
        Ok(resp)
    })
    .await
    .ok()?;
    Some((hdr_self, hdr_role, ws))
}

fn extract_reverse_ws_headers(
    req: &Request,
    hdr_self: &mut Option<String>,
    hdr_role: &mut Option<String>,
) {
    *hdr_self = req
        .headers()
        .get("x-self-id")
        .and_then(|v| v.to_str().ok())
        .map(ToOwned::to_owned);
    *hdr_role = req
        .headers()
        .get("x-client-role")
        .and_then(|v| v.to_str().ok())
        .map(ToOwned::to_owned);
}
