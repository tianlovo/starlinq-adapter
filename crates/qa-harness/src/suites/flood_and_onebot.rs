//! L1/L2/L3 洪峰防雪崩调度、多超大群反饥饿抢占与 OneBot v11/截止期精细化集成测试套件

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use adapter_server::{build_router_with_state, spawn_server, AppState};
use anyhow::ensure;
use chrono::Utc;
use common_types::constants::{
    DEFAULT_ONEBOT_ID_BOUND, DEFAULT_TARGET_GROUP_NAME, MOCK_QQ_MAIN_CLASS,
};
use common_types::{
    stable_onebot_id, AdapterConfig, AdapterError, AdapterEvent, BotConfig, BotDriver,
    BurstModeState, ChatMessage, ChatTargetKind, GroupTargetSpec, InputMode, InteractionConfig,
    MessagePriorityTier, MessageSegment, SchedulerConfig, SchedulerTelemetrySnapshot,
    StorageConfig, StoredMessageRecord,
};
use mock_qq_win::MockQqServer;
use qqnt_driver::parser::canonical_group_name;
use qqnt_driver::profile::SECURITY_KEYWORDS;
use qqnt_driver::QqntDriver;
use serde_json::json;
use tokio::net::TcpListener;
use tokio_tungstenite::connect_async;
use uia_core::{enumerate_candidate_windows, find_windows_with_keywords};

use super::{
    default_stub_and_config, get_json_status, post_json_status, ws_next_json, CategoryBuilder,
    SuiteExecContext,
};

fn now_unix_ms() -> u64 {
    Utc::now().timestamp_millis().max(0) as u64
}

async fn wait_for_clear_mock_desktop() {
    for _ in 0..80 {
        let has_mock = enumerate_candidate_windows(&[], &[MOCK_QQ_MAIN_CLASS.to_string()])
            .into_iter()
            .any(|win| win.class_name == MOCK_QQ_MAIN_CLASS);
        let has_security = !find_windows_with_keywords(&[], SECURITY_KEYWORDS).is_empty();
        if !has_mock && !has_security {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn build_flood_test_config(
    monitored_groups: Vec<String>,
    scheduler: SchedulerConfig,
) -> AdapterConfig {
    let primary = monitored_groups
        .first()
        .cloned()
        .unwrap_or_else(|| "大群Alpha".to_string());
    AdapterConfig {
        target: GroupTargetSpec {
            group_name: primary,
            strict_below_members: Some(10),
            require_member_count_verified: false,
            monitored_groups,
            allow_large_monitored_groups: true,
            ..Default::default()
        },
        interaction: InteractionConfig {
            default_mode: InputMode::BackgroundUia,
            allow_focus_fallback: false,
            poll_interval_ms: 80,
            char_delay_min_ms: 1,
            char_delay_max_ms: 3,
            min_send_interval_ms: 40,
            extra_window_classes: vec![MOCK_QQ_MAIN_CLASS.to_string()],
            ..Default::default()
        },
        storage: StorageConfig {
            db_path: ":memory:".to_string(),
        },
        bot: BotConfig {
            self_id: 2_888_001,
            self_nicknames: vec!["qq-bot".to_string(), "测试机器人".to_string()],
            auto_ping_reply: false,
        },
        scheduler,
        ..Default::default()
    }
}

async fn drain_initial_baseline(rx: &mut tokio::sync::broadcast::Receiver<AdapterEvent>) {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(900);
    while tokio::time::Instant::now() < deadline {
        if tokio::time::timeout(Duration::from_millis(220), rx.recv())
            .await
            .is_err()
        {
            break;
        }
    }
}

/// 向打桩分类追加 OneBot v11 协议完善、出站截止期守卫（408/1408）与调度策略接口校验用例
pub async fn append_onebot_v11_and_deadline_cases(
    cat: &mut CategoryBuilder,
    ctx: &SuiteExecContext,
) {
    cat.run_case(
        "stub_outbound_deadline_408_1408_guard_and_send_gate",
        async {
            let (driver, cfg) = default_stub_and_config();
            let ts_ms = now_unix_ms();
            driver.insert_stored_message(StoredMessageRecord {
                onebot_message_id: 77_101,
                message_uid: "stub-deadline-1".to_string(),
                group_name: DEFAULT_TARGET_GROUP_NAME.to_string(),
                group_id: 100_000_001,
                user_id: 20_001,
                sender_name: "群友甲".to_string(),
                raw_message: "触发回复".to_string(),
                chat_type: ChatTargetKind::Group,
                segments: vec![MessageSegment::Text {
                    text: "触发回复".to_string(),
                }],
                is_self: false,
                timestamp_hint: Some("12:00".to_string()),
                captured_at_ms: ts_ms,
            });

            let state = AppState::new(Arc::clone(&driver) as Arc<_>, cfg);
            let router = build_router_with_state(state.clone());
            let listener = TcpListener::bind("127.0.0.1:0").await?;
            let addr = listener.local_addr()?;
            let srv_handle = tokio::spawn(async move {
                let _ = axum::serve(listener, router).await;
            });
            let base = format!("http://{addr}");
            let expired = ts_ms.saturating_sub(5_000);

            // 校验 1：OneBot 过期 deadline_unix_ms 与零窗口拦截（0 次 UIA 发送）
            let (code, body) = post_json_status(
            &ctx.http_client,
            &format!("{base}/onebot/v11/send_group_msg"),
            json!({"group_id": 100_000_001, "message": "too late", "deadline_unix_ms": expired}),
        )
        .await?;
            ensure!(
                code == 408 && body["retcode"] == 1408 && body["code"] == "RESPONSE_WINDOW_EXPIRED"
            );
            ensure!(body["status"] == "failed");

            let (code, body) = post_json_status(
                &ctx.http_client,
                &format!("{base}/onebot/v11/send_group_msg"),
                json!({"group_id": 100_000_001, "message": "zero window", "response_window_ms": 0}),
            )
            .await?;
            ensure!(code == 408 && body["retcode"] == 1408);

            // 校验 2：原生 POST /api/send 过期时间戳拦截
            let (code, body) = post_json_status(
                &ctx.http_client,
                &format!("{base}/api/send"),
                json!({
                    "group": DEFAULT_TARGET_GROUP_NAME,
                    "text": "stale rest",
                    "target_kind": "group",
                    "deadline_unix_ms": expired,
                }),
            )
            .await?;
            ensure!(code == 408 && body["code"] == "RESPONSE_WINDOW_EXPIRED");

            // 校验 3：.handle_quick_operation 携带过期 response_deadline_ms 或陈旧 captured_at_ms 拦截
            let (code, body) = post_json_status(
                &ctx.http_client,
                &format!("{base}/onebot/v11/.handle_quick_operation"),
                json!({
                    "context": {
                        "message_type": "group",
                        "group_id": 100_000_001,
                        "message_id": 77_101,
                        "response_deadline_ms": expired,
                    },
                    "operation": {"reply": "expired quick reply"}
                }),
            )
            .await?;
            ensure!(code == 408 && body["retcode"] == 1408);

            let stale_capture = ts_ms.saturating_sub(120_000);
            let (code, body) = post_json_status(
                &ctx.http_client,
                &format!("{base}/onebot/v11/.handle_quick_operation"),
                json!({
                    "context": {
                        "message_type": "group",
                        "group_id": 100_000_001,
                        "message_id": 77_101,
                        "captured_at_ms": stale_capture,
                    },
                    "operation": {"reply": "stale captured quick reply"}
                }),
            )
            .await?;
            ensure!(code == 408 && body["retcode"] == 1408);
            ensure!(
                driver.sent_requests().is_empty(),
                "过期拦截阶段不应触发任何 UIA 发送"
            );
            ensure!(driver.outbound_deadline_abort_count() == 5);

            // 校验 4：持有 send_gate 锁等待超时后的锁后二次截止期拦截
            let gate_guard = state.send_gate.lock().await;
            let ob_url = format!("{base}/onebot/v11/send_group_msg");
            let rest_url = format!("{base}/api/send");
            let c1 = ctx.http_client.clone();
            let c2 = ctx.http_client.clone();
            let ob_fut = tokio::spawn(async move {
                post_json_status(
                &c1,
                &ob_url,
                json!({"group_id": 100_000_001, "message": "queued ob", "response_window_ms": 60}),
            )
            .await
            });
            let rest_fut = tokio::spawn(async move {
                post_json_status(
                    &c2,
                    &rest_url,
                    json!({
                        "group": DEFAULT_TARGET_GROUP_NAME,
                        "text": "queued rest",
                        "target_kind": "group",
                        "response_window_ms": 60,
                    }),
                )
                .await
            });
            tokio::time::sleep(Duration::from_millis(180)).await;
            drop(gate_guard);

            let (ob_code, ob_body) = ob_fut.await??;
            let (rest_code, rest_body) = rest_fut.await??;
            ensure!(ob_code == 408 && ob_body["retcode"] == 1408);
            ensure!(rest_code == 408 && rest_body["code"] == "RESPONSE_WINDOW_EXPIRED");
            ensure!(driver.sent_requests().is_empty());
            ensure!(driver.outbound_deadline_abort_count() == 7);

            let (sched_code, sched_body) =
                get_json_status(&ctx.http_client, &format!("{base}/api/scheduler")).await?;
            ensure!(sched_code == 200);
            ensure!(sched_body["telemetry"]["outbound_deadline_aborted_total"] == 7);

            // 校验 5：有效时间窗内的 POST /api/send 正常放行并透传 deadline_unix_ms
            let (ok_code, ok_body) = post_json_status(
                &ctx.http_client,
                &format!("{base}/api/send"),
                json!({
                    "group": DEFAULT_TARGET_GROUP_NAME,
                    "text": "fresh reply",
                    "target_kind": "group",
                    "response_window_ms": 10_000,
                }),
            )
            .await?;
            ensure!(ok_code == 200 && ok_body["ok"] == true);
            let sent = driver.sent_requests();
            ensure!(sent.len() == 1 && sent[0].deadline_unix_ms.is_some());

            srv_handle.abort();
            Ok(
                "OneBot/REST/QuickOp 过期时间窗与 send_gate 锁后超时均返回 408/1408 且 0 UIA 误发"
                    .to_string(),
            )
        },
    )
    .await;

    cat.run_case("stub_mark_read_slices_scheduler_policy_and_10004", async {
        let (driver, mut cfg) = default_stub_and_config();
        driver.allow_private_friend("测试好友");
        let friend_uid = stable_onebot_id("测试好友", DEFAULT_ONEBOT_ID_BOUND);
        let ts_ms = now_unix_ms();
        driver.insert_stored_message(StoredMessageRecord {
            onebot_message_id: 88_001,
            message_uid: "read-grp".to_string(),
            group_name: DEFAULT_TARGET_GROUP_NAME.to_string(),
            group_id: 100_000_001,
            user_id: 20_001,
            sender_name: "群友A".to_string(),
            raw_message: "hi group".to_string(),
            chat_type: ChatTargetKind::Group,
            segments: vec![MessageSegment::Text {
                text: "hi group".to_string(),
            }],
            is_self: false,
            timestamp_hint: Some("14:00".to_string()),
            captured_at_ms: ts_ms,
        });
        driver.insert_stored_message(StoredMessageRecord {
            onebot_message_id: 88_050,
            message_uid: "read-priv-ok".to_string(),
            group_name: "测试好友".to_string(),
            group_id: 0,
            user_id: friend_uid,
            sender_name: "测试好友".to_string(),
            raw_message: "hi friend".to_string(),
            chat_type: ChatTargetKind::Private,
            segments: vec![MessageSegment::Text {
                text: "hi friend".to_string(),
            }],
            is_self: false,
            timestamp_hint: Some("14:01".to_string()),
            captured_at_ms: ts_ms,
        });
        driver.insert_stored_message(StoredMessageRecord {
            onebot_message_id: 88_051,
            message_uid: "read-priv-deny".to_string(),
            group_name: "陌生人B".to_string(),
            group_id: 0,
            user_id: 777_999,
            sender_name: "陌生人B".to_string(),
            raw_message: "secret".to_string(),
            chat_type: ChatTargetKind::Private,
            segments: vec![MessageSegment::Text {
                text: "secret".to_string(),
            }],
            is_self: false,
            timestamp_hint: Some("14:02".to_string()),
            captured_at_ms: ts_ms,
        });
        driver.set_scheduler_telemetry(SchedulerTelemetrySnapshot {
            burst_state: BurstModeState::BurstActive,
            burst_enter_count: 3,
            selective_shed_p2_total: 19,
            active_monitored_groups: 2,
            ..Default::default()
        });
        cfg.target.monitored_groups =
            vec![DEFAULT_TARGET_GROUP_NAME.to_string(), "大群A".to_string()];
        cfg.target.allow_large_monitored_groups = true;
        cfg.target.strict_below_members = Some(10);

        let (addr, srv_handle) = spawn_server(Arc::clone(&driver) as Arc<_>, cfg).await?;
        let base = format!("http://{addr}");

        // 1. 已读清理与白名单校验（含 403/1403 拦截与 404/1404）
        let (code, body) = post_json_status(
            &ctx.http_client,
            &format!("{base}/onebot/v11/mark_msg_as_read"),
            json!({"message_id": 88_001}),
        )
        .await?;
        ensure!(
            code == 200
                && body["retcode"] == 0
                && body["data"]["target"] == DEFAULT_TARGET_GROUP_NAME
        );

        let (code, body) = post_json_status(
            &ctx.http_client,
            &format!("{base}/onebot/v11/mark_msg_as_read"),
            json!({"message_id": 88_051}),
        )
        .await?;
        ensure!(
            code == 403 && body["retcode"] == 1403 && body["code"] == "PRIVATE_TARGET_NOT_ALLOWED"
        );

        let (code, body) = post_json_status(
            &ctx.http_client,
            &format!("{base}/onebot/v11/mark_group_msg_as_read"),
            json!({"group_id": 100_000_001}),
        )
        .await?;
        ensure!(code == 200 && body["retcode"] == 0);

        let (code, body) = post_json_status(
            &ctx.http_client,
            &format!("{base}/onebot/v11/mark_group_msg_as_read"),
            json!({"group_id": 999_999}),
        )
        .await?;
        ensure!(code == 404 && body["retcode"] == 1404);

        let (code, body) = post_json_status(
            &ctx.http_client,
            &format!("{base}/onebot/v11/mark_private_msg_as_read"),
            json!({"user_id": friend_uid}),
        )
        .await?;
        ensure!(code == 200 && body["retcode"] == 0 && body["data"]["target"] == "测试好友");

        let (code, body) = post_json_status(
            &ctx.http_client,
            &format!("{base}/onebot/v11/mark_private_msg_as_read"),
            json!({"nickname": "陌生人B"}),
        )
        .await?;
        ensure!(code == 403 && body["retcode"] == 1403);

        let (code, body) = post_json_status(
            &ctx.http_client,
            &format!("{base}/api/sessions/read"),
            json!({"user_id": friend_uid}),
        )
        .await?;
        ensure!(code == 200 && body["ok"] == true && body["target_kind"] == "private");

        let (code, _) = post_json_status(
            &ctx.http_client,
            &format!("{base}/api/sessions/read"),
            json!({"target": "陌生人B", "target_kind": "private"}),
        )
        .await?;
        ensure!(code == 403);
        ensure!(driver.mark_read_calls().len() == 4);

        // 2. .get_word_slices 与 .get_scheduler_status + REST /api/scheduler(/policy)
        let (code, body) = post_json_status(
            &ctx.http_client,
            &format!("{base}/onebot/v11/.get_word_slices"),
            json!({"content": "Hello, QQ机器人123!"}),
        )
        .await?;
        ensure!(
            code == 200
                && body["retcode"] == 0
                && body["data"]["slices"]
                    == json!(["Hello", ",", "QQ", "机", "器", "人", "123", "!"])
        );

        let (code, body) = get_json_status(
            &ctx.http_client,
            &format!("{base}/onebot/v11/.get_scheduler_status"),
        )
        .await?;
        ensure!(
            code == 200
                && body["retcode"] == 0
                && body["data"]["telemetry"]["burst_state"] == "burst_active"
                && body["data"]["telemetry"]["selective_shed_p2_total"] == 19
        );

        let (code, body) =
            get_json_status(&ctx.http_client, &format!("{base}/api/scheduler")).await?;
        ensure!(
            code == 200
                && body["ok"] == true
                && body["allow_large_monitored_groups"] == true
                && body["monitored_groups"] == json!([DEFAULT_TARGET_GROUP_NAME, "大群A"])
        );

        let (code, body) = post_json_status(
            &ctx.http_client,
            &format!("{base}/api/scheduler/policy"),
            json!({
                "burst_window_ms": 4_500,
                "burst_enter_threshold": 24,
                "burst_exit_threshold": 9,
            }),
        )
        .await?;
        ensure!(code == 200 && body["config"]["burst_window_ms"] == 4_500);

        let (code, body) =
            get_json_status(&ctx.http_client, &format!("{base}/api/scheduler/policy")).await?;
        ensure!(code == 200 && body["config"]["burst_enter_threshold"] == 24);

        for invalid in [
            json!({"burst_window_ms": 0}),
            json!({"critical_response_window_ms": 0}),
            json!({"burst_enter_threshold": 5, "burst_exit_threshold": 6}),
            json!({"max_pending_events": 0}),
        ] {
            let (err_code, err_body) = post_json_status(
                &ctx.http_client,
                &format!("{base}/api/scheduler/policy"),
                invalid,
            )
            .await?;
            ensure!(err_code == 400 && err_body["code"] == "BAD_REQUEST");
        }

        // 3. 9 项 QQNT 无接口高危/云端动作必须稳定拒绝（HTTP 501 / retcode 10004）
        for action in [
            "get_group_honor_info",
            "get_group_system_msg",
            "get_group_at_all_remain",
            "get_essence_msg_list",
            "get_group_root_files",
            "download_file",
            "set_msg_emoji_like",
            "set_essence_msg",
            "set_qq_profile",
        ] {
            let (err_code, err_body) = post_json_status(
                &ctx.http_client,
                &format!("{base}/onebot/v11/{action}"),
                json!({}),
            )
            .await?;
            ensure!(
                err_code == 501
                    && err_body["retcode"] == 10004
                    && err_body["code"] == "UNSUPPORTED_ACTION",
                "action {action} unexpected: {err_code} {err_body}"
            );
        }

        srv_handle.abort();
        Ok(
            "mark_*_msg_as_read 白名单拦截、.get_word_slices、调度策略热更新及 9 项 10004 拒收全绿"
                .to_string(),
        )
    })
    .await;

    cat.run_case(
        "stub_upgraded_onebot_fields_get_msg_and_heartbeat_stat",
        async {
            let (driver, mut cfg) = default_stub_and_config();
            let ts_ms = now_unix_ms();
            let deadline_ms = ts_ms.saturating_add(15_000);
            driver.insert_stored_message(StoredMessageRecord {
                onebot_message_id: 99_001,
                message_uid: "stored-up-1".to_string(),
                group_name: DEFAULT_TARGET_GROUP_NAME.to_string(),
                group_id: 100_000_001,
                user_id: 20_001,
                sender_name: "群友甲".to_string(),
                raw_message: "stored msg".to_string(),
                chat_type: ChatTargetKind::Group,
                segments: vec![MessageSegment::Text {
                    text: "stored msg".to_string(),
                }],
                is_self: false,
                timestamp_hint: Some("15:00".to_string()),
                captured_at_ms: ts_ms,
            });
            driver.set_scheduler_telemetry(SchedulerTelemetrySnapshot {
                burst_state: BurstModeState::Normal,
                expired_window_total: 2,
                outbound_deadline_aborted_total: 1,
                ..Default::default()
            });
            cfg.onebot.heartbeat_interval_ms = 450;

            let (addr, srv_handle) = spawn_server(Arc::clone(&driver) as Arc<_>, cfg).await?;
            let base = format!("http://{addr}");
            let (mut ws, _) = connect_async(format!("ws://{addr}/onebot/v11/ws")).await?;
            let hello = ws_next_json(&mut ws, Duration::from_secs(2)).await?;
            ensure!(hello["meta_event_type"] == "lifecycle");

            // 验证 WS 推送事件升级字段
            driver.emit_event(AdapterEvent::GroupMessage(ChatMessage {
                message_id: "ws-up-1".to_string(),
                group_name: DEFAULT_TARGET_GROUP_NAME.to_string(),
                sender: "群管A".to_string(),
                content: "@qq-bot 状态".to_string(),
                timestamp_hint: Some("15:01".to_string()),
                is_self: false,
                captured_at_ms: ts_ms,
                onebot_message_id: Some(99_002),
                group_id: Some(100_000_001),
                user_id: Some(20_002),
                target_kind: ChatTargetKind::Group,
                segments: vec![MessageSegment::Text {
                    text: "@qq-bot 状态".to_string(),
                }],
                sender_role: Some("admin".to_string()),
                sender_card: Some("群管名片".to_string()),
                priority_tier: Some(MessagePriorityTier::CriticalBotTrigger),
                response_deadline_ms: Some(deadline_ms),
                ..Default::default()
            }));

            let mut evt_opt = None;
            let deadline = Instant::now() + Duration::from_secs(3);
            while Instant::now() < deadline {
                let frame = ws_next_json(&mut ws, Duration::from_secs(2)).await?;
                if frame["post_type"] == "message" {
                    evt_opt = Some(frame);
                    break;
                }
            }
            let evt = evt_opt.ok_or_else(|| anyhow::anyhow!("missing upgraded WS GroupMessage"))?;
            ensure!(
                evt["font"] == 0
                    && evt["message_seq"] == 99_002
                    && evt["real_id"] == 99_002
                    && evt["anonymous"].is_null()
                    && evt["sender"]["role"] == "admin"
                    && evt["sender"]["card"] == "群管名片"
                    && evt["priority_tier"] == "critical_bot_trigger"
                    && evt["response_deadline_ms"] == deadline_ms
            );

            // 验证 get_msg 升级字段
            let (code, get_msg_body) = get_json_status(
                &ctx.http_client,
                &format!("{base}/onebot/v11/get_msg?message_id=99001"),
            )
            .await?;
            ensure!(
                code == 200
                    && get_msg_body["data"]["font"] == 0
                    && get_msg_body["data"]["message_seq"] == 99_001
                    && get_msg_body["data"]["real_id"] == 99_001
                    && get_msg_body["data"]["sender"]["role"] == "member"
            );

            // 发送 1 条消息后核验 get_status.stat 与 WS 心跳附带 stat/scheduler
            let (code, _) = post_json_status(
                &ctx.http_client,
                &format!("{base}/onebot/v11/send_group_msg"),
                json!({"group_id": 100_000_001, "message": "heartbeat check"}),
            )
            .await?;
            ensure!(code == 200);

            let (code, status_body) =
                get_json_status(&ctx.http_client, &format!("{base}/onebot/v11/get_status")).await?;
            ensure!(code == 200);
            let stat = &status_body["data"]["stat"];
            ensure!(
                stat["message_received"] == 42
                    && stat["packet_received"] == 42
                    && stat["message_sent"] == 1
                    && stat["packet_sent"] == 1
                    && stat["packet_lost"] == 3
                    && stat["lost_times"] == 1
                    && stat["last_message_time"].as_i64().unwrap_or(0) > 0
                    && status_body["data"]["scheduler"]["expired_window_total"] == 2
            );

            let mut hb_opt = None;
            let hb_deadline = Instant::now() + Duration::from_secs(3);
            while Instant::now() < hb_deadline {
                let frame = ws_next_json(&mut ws, Duration::from_secs(2)).await?;
                if frame["meta_event_type"] == "heartbeat" {
                    hb_opt = Some(frame);
                    break;
                }
            }
            let hb = hb_opt.ok_or_else(|| anyhow::anyhow!("missing OneBot WS heartbeat frame"))?;
            ensure!(
                hb["status"]["stat"]["message_sent"] == 1
                    && hb["status"]["scheduler"]["outbound_deadline_aborted_total"] == 1
            );

            srv_handle.abort();
            Ok(
                "OneBot 事件/get_msg 新增字段、get_status stat 统计及 WS 心跳调度状态全部核验通过"
                    .to_string(),
            )
        },
    )
    .await;
}

/// 向 L2 集成分类追加 500 人大群常规无损与单群洪峰 P0 抢占恢复 Win32 Mock 集成用例
pub async fn append_single_group_flood_cases(cat: &mut CategoryBuilder) {
    cat.run_case("integration_large_group_500_normal_lossless", async {
        wait_for_clear_mock_desktop().await;
        let mock = MockQqServer::spawn_with_groups(vec![(
            "大群Alpha (500)".to_string(),
            vec!["群主 [11:59]: 初始基线".to_string()],
        )])?;
        let cfg = build_flood_test_config(
            vec!["大群Alpha".to_string()],
            SchedulerConfig {
                burst_window_ms: 3000,
                burst_enter_threshold: 16,
                burst_exit_threshold: 6,
                single_poll_burst_threshold: 12,
                ..Default::default()
            },
        );
        let driver = QqntDriver::new(cfg)?;
        let mut rx = driver.subscribe_events();
        driver
            .open_group(GroupTargetSpec {
                group_name: "大群Alpha".to_string(),
                monitored_groups: vec!["大群Alpha".to_string()],
                allow_large_monitored_groups: true,
                ..Default::default()
            })
            .await?;
        drain_initial_baseline(&mut rx).await;

        let batch: Vec<String> = (1..=8)
            .map(|i| format!("群友{i} [12:0{i}]: 常规低频消息 #{i}"))
            .collect();
        mock.inject_incoming_batch("大群Alpha (500)", &batch);

        let mut received = 0_usize;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(4);
        while received < 8 && tokio::time::Instant::now() < deadline {
            if let Ok(Ok(AdapterEvent::GroupMessage(_))) =
                tokio::time::timeout(Duration::from_millis(250), rx.recv()).await
            {
                received += 1;
            }
        }
        ensure!(
            received == 8,
            "正常流量 8 条期望 100% 投递，实际收到 {received}"
        );

        let tel = driver.scheduler_telemetry().await?;
        ensure!(tel.burst_state == BurstModeState::Normal && tel.selective_shed_p2_total == 0);

        let unmonitored = driver
            .open_group(GroupTargetSpec {
                group_name: "大群Alpha".to_string(),
                strict_below_members: Some(10),
                require_member_count_verified: false,
                ..Default::default()
            })
            .await;
        ensure!(
            matches!(unmonitored, Err(AdapterError::MemberCountExceeded { .. })),
            "未入白名单 500 人大群在显式 strict_below_members 下仍应拦截"
        );

        drop(driver);
        mock.shutdown();
        wait_for_clear_mock_desktop().await;
        Ok("500 人监控大群低频 8 条 100% 无损送达、保持 Normal 且未授权严格护栏生效".to_string())
    })
    .await;

    cat.run_case(
        "integration_large_group_500_burst_p0_priority_and_recovery",
        async {
            wait_for_clear_mock_desktop().await;
            let mock = MockQqServer::spawn_with_groups(vec![(
                "大群Alpha (500)".to_string(),
                vec!["群主 [11:59]: 初始基线".to_string()],
            )])?;
            let cfg = build_flood_test_config(
                vec!["大群Alpha".to_string()],
                SchedulerConfig {
                    burst_window_ms: 900,
                    burst_enter_threshold: 16,
                    burst_exit_threshold: 6,
                    single_poll_burst_threshold: 12,
                    burst_ordinary_sample_stride: 5,
                    hard_shed_queue_depth: 64,
                    ..Default::default()
                },
            );
            let driver = QqntDriver::new(cfg)?;
            let mut rx = driver.subscribe_events();
            driver
                .open_group(GroupTargetSpec {
                    group_name: "大群Alpha".to_string(),
                    monitored_groups: vec!["大群Alpha".to_string()],
                    allow_large_monitored_groups: true,
                    ..Default::default()
                })
                .await?;
            drain_initial_baseline(&mut rx).await;

            let mut flood_batch = Vec::with_capacity(104);
            for idx in 0..50 {
                flood_batch.push(format!("水友{idx} [12:01]: 洪峰刷屏 #{idx}"));
            }
            flood_batch.push("关键A [12:01]: @qq-bot 报数一".to_string());
            flood_batch.push("关键B [12:01]: /ping 查指令".to_string());
            for idx in 50..100 {
                flood_batch.push(format!("水友{idx} [12:02]: 洪峰刷屏 #{idx}"));
            }
            flood_batch.push("关键C [12:02]: @qq-bot 状态检查".to_string());
            flood_batch.push("关键D [12:02]: /ping 压测".to_string());
            mock.inject_incoming_batch("大群Alpha (500)", &flood_batch);

            let mut p0_contents = Vec::new();
            let mut p2_before_all_p0 = 0_usize;
            let deadline = tokio::time::Instant::now() + Duration::from_secs(6);
            while p0_contents.len() < 4 && tokio::time::Instant::now() < deadline {
                if let Ok(Ok(AdapterEvent::GroupMessage(msg))) =
                    tokio::time::timeout(Duration::from_millis(250), rx.recv()).await
                {
                    if msg.priority_tier == Some(MessagePriorityTier::CriticalBotTrigger) {
                        p0_contents.push(msg.content);
                    } else if p0_contents.len() < 4 {
                        p2_before_all_p0 += 1;
                    }
                }
            }
            ensure!(
                p0_contents.len() == 4,
                "100 闲聊 + 4 P0 洪峰下 4 条关键触发必须全保活，实际 {p0_contents:?}"
            );
            ensure!(
                p2_before_all_p0 == 0,
                "P0 必须在 P2 闲聊之前优先派发，实际提前出现 {p2_before_all_p0} 条非 P0 消息"
            );

            let mid_tel = driver.scheduler_telemetry().await?;
            ensure!(
                mid_tel.burst_state == BurstModeState::BurstActive
                    && mid_tel.selective_shed_p2_total > 0
                    && mid_tel.dispatched_p0_total == 4
            );

            tokio::time::sleep(Duration::from_millis(1050)).await;
            let recovered = driver.scheduler_telemetry().await?;
            ensure!(recovered.burst_state == BurstModeState::Normal);

            drop(driver);
            mock.shutdown();
            wait_for_clear_mock_desktop().await;
            Ok(format!(
            "500 人单群 104 并发洪峰下 4 条 P0 优先且 100% 投递、丢弃闲聊={}，静默后滞回 Normal",
            mid_tel.selective_shed_p2_total
        ))
        },
    )
    .await;
}

/// 向 L3 压力分类追加 3×600 人超大群并发洪峰与反饥饿抢占轮询测试
pub async fn append_multi_group_flood_case(cat: &mut CategoryBuilder) {
    cat.run_case(
        "stress_multi_large_group_3x600_concurrent_flood_rotation",
        async {
            wait_for_clear_mock_desktop().await;
            let groups = vec![
                (
                    "大群Alpha (600)".to_string(),
                    vec!["管理A [11:50]: Alpha基线".to_string()],
                ),
                (
                    "大群Beta (600)".to_string(),
                    vec!["管理B [11:51]: Beta基线".to_string()],
                ),
                (
                    "大群Gamma (600)".to_string(),
                    vec!["管理C [11:52]: Gamma基线".to_string()],
                ),
            ];
            let mock = MockQqServer::spawn_with_groups(groups)?;
            let monitored = vec![
                "大群Alpha".to_string(),
                "大群Beta".to_string(),
                "大群Gamma".to_string(),
            ];
            let cfg = build_flood_test_config(
                monitored.clone(),
                SchedulerConfig {
                    burst_window_ms: 3000,
                    burst_enter_threshold: 16,
                    burst_exit_threshold: 6,
                    single_poll_burst_threshold: 12,
                    burst_ordinary_sample_stride: 4,
                    urgent_preempt_dwell_ms: 250,
                    session_dwell_min_ms: 600,
                    patrol_interval_ms: 1000,
                    ..Default::default()
                },
            );
            let driver = QqntDriver::new(cfg)?;
            let mut rx = driver.subscribe_events();
            driver
                .open_group(GroupTargetSpec {
                    group_name: "大群Alpha".to_string(),
                    monitored_groups: monitored,
                    allow_large_monitored_groups: true,
                    ..Default::default()
                })
                .await?;
            drain_initial_baseline(&mut rx).await;

            let mut alpha_msgs: Vec<String> = (0..18)
                .map(|i| format!("A路人{i} [12:05]: Alpha洪峰 #{i}"))
                .collect();
            alpha_msgs.push("A触发 [12:05]: @qq-bot Alpha紧急汇报".to_string());
            let mut beta_msgs: Vec<String> = (0..20)
                .map(|i| format!("B路人{i} [12:05]: Beta洪峰 #{i}"))
                .collect();
            beta_msgs.push("B触发 [12:05]: @qq-bot Beta紧急求助".to_string());
            let mut gamma_msgs: Vec<String> = (0..20)
                .map(|i| format!("C路人{i} [12:05]: Gamma洪峰 #{i}"))
                .collect();
            gamma_msgs.push("C触发 [12:05]: @qq-bot Gamma立即排查".to_string());

            mock.inject_incoming_batch("大群Beta (600)", &beta_msgs);
            mock.inject_incoming_batch("大群Gamma (600)", &gamma_msgs);
            mock.inject_incoming_batch("大群Alpha (600)", &alpha_msgs);
            ensure!(
                mock.unread_for("大群Beta (600)") > 0 && mock.unread_for("大群Gamma (600)") > 0
            );

            let mut p0_groups = HashSet::new();
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            while p0_groups.len() < 3 && tokio::time::Instant::now() < deadline {
                if let Ok(Ok(AdapterEvent::GroupMessage(msg))) =
                    tokio::time::timeout(Duration::from_millis(250), rx.recv()).await
                {
                    if msg.priority_tier == Some(MessagePriorityTier::CriticalBotTrigger) {
                        if let Some(resp_dl) = msg.response_deadline_ms {
                            ensure!(
                                msg.captured_at_ms <= resp_dl && now_unix_ms() <= resp_dl,
                                "P0 消息超出响应截止窗口: captured={}, deadline={resp_dl}",
                                msg.captured_at_ms
                            );
                        } else {
                            anyhow::bail!("P0 消息缺少 response_deadline_ms 字段");
                        }
                        p0_groups.insert(canonical_group_name(&msg.group_name));
                    }
                }
            }
            ensure!(
                p0_groups.contains("大群Alpha")
                    && p0_groups.contains("大群Beta")
                    && p0_groups.contains("大群Gamma"),
                "3×600 人并发洪峰必须收割全三群 P0，实际命中: {p0_groups:?}"
            );

            let tel = driver.scheduler_telemetry().await?;
            ensure!(
                tel.multi_group_switches_total >= 2,
                "多群抢占切换次数应 >= 2，实际 {}",
                tel.multi_group_switches_total
            );
            driver
                .mark_session_read("大群Beta", ChatTargetKind::Group)
                .await?;

            drop(driver);
            mock.shutdown();
            wait_for_clear_mock_desktop().await;
            Ok(format!(
                "3×600 人超大群并发洪峰收割全三群 @qq-bot P0（窗口内送达、群切换={} 次）",
                tel.multi_group_switches_total
            ))
        },
    )
    .await;
}
