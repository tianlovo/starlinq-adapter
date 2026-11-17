//! L1 数据边界测试与条件边界测试套件（覆盖 UTF-8 切片、ReDoS、超大媒体、SQLite 饱和与权限/角色边界）

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::ensure;
use common_types::{
    parse_cq_string_to_segments, segments_to_cq_string, GroupTargetSpec, MessageSegment,
};
use futures_util::SinkExt;
use reqwest::header::CONTENT_TYPE;
use serde_json::{json, Value};
use storage::constants::MEMORY_DB_PATH;
use storage::{RecordMessageInput, Storage, UpsertGroupInput};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use uia_core::{decode_image_bytes_to_bgra, find_windows_with_keywords_ext};

use super::stub_driver::ConfigurableStubDriver;
use super::{default_stub_and_config, ws_next_json, CategoryBuilder, SuiteExecContext};
use crate::models::{CategoryReport, QaTier};

/// 执行第 3 类：数据边界测试（L1）
pub async fn run_data_boundary_category(ctx: &SuiteExecContext) -> CategoryReport {
    let mut cat = CategoryBuilder::new(
        "CAT-03",
        "数据边界测试",
        "Data Boundary Test",
        QaTier::L1,
        "零 UTF-8 切片崩溃、线性 ReDoS 防护、超限 413/400/OOM 保护与时间戳饱和",
    );

    cat.run_case(
        "data_boundary_empty_emoji_rtl_nul_and_64k_payloads",
        async {
            let (driver, cfg) = default_stub_and_config();
            let (addr, handle) = adapter_server::spawn_server(driver, cfg).await?;
            let base = format!("http://{addr}");

            // 空白字符串发送必须返回 400 BAD_REQUEST
            for blank in ["", "   \t\r\n  "] {
                let resp = ctx
                    .http_client
                    .post(format!("{base}/onebot/v11/send_group_msg"))
                    .json(&json!({"message": blank}))
                    .send()
                    .await?;
                ensure!(
                    resp.status().as_u16() == 400,
                    "blank message expected 400, got {}",
                    resp.status()
                );
            }

            // 4 字节 Emoji + RTL 反转符 + 64 KiB 超长文本必须无损透传
            let rtl_emoji = format!("边界\u{202E}测试😀🎉🦀_{}", "A".repeat(64 * 1024));
            let segs = parse_cq_string_to_segments(&rtl_emoji, true);
            let roundtrip = segments_to_cq_string(&segs);
            ensure!(roundtrip.contains("😀🎉🦀"), "emoji corrupted in roundtrip");

            let resp = ctx
                .http_client
                .post(format!("{base}/onebot/v11/send_group_msg"))
                .json(&json!({"message": rtl_emoji, "auto_escape": true}))
                .send()
                .await?;
            ensure!(
                resp.status().as_u16() == 200,
                "64 KiB UTF-8 payload failed: {}",
                resp.status()
            );

            // NUL 字符混排输入不得崩溃
            let nul_resp = ctx
                .http_client
                .post(format!("{base}/onebot/v11/send_group_msg"))
                .json(&json!({"message": "prefix\0suffix"}))
                .send()
                .await?;
            ensure!(nul_resp.status().as_u16() < 500, "NUL byte caused 5xx");

            handle.abort();
            Ok("空白串拒绝、RTL/Emoji/64KiB/NUL 极端文本安全处理通过".to_string())
        },
    )
    .await;

    cat.run_case(
        "data_boundary_multibyte_percent_form_and_utf8_auth_headers",
        async {
            let (driver, mut cfg) = default_stub_and_config();
            cfg.server.access_token = Some("qa+secret&1".to_string());
            let (addr, handle) = adapter_server::spawn_server(driver, cfg).await?;
            let base = format!("http://{addr}");

            // 验证 QA-BUG-001：中文多字节紧邻截断 %E6 表单数据不再引发 panic
            let resp = ctx
                .http_client
                .post(format!(
                    "{base}/onebot/v11/send_group_msg?access_token=qa%2Bsecret%261"
                ))
                .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body("message=中%E6%96%87测试%ZZ%E6")
                .send()
                .await?;
            ensure!(
                resp.status().as_u16() == 200,
                "multibyte form failed: {}",
                resp.status()
            );

            // 验证 QA-BUG-002：纯非 ASCII Authorization 头返回标准 401 而非索引 panic
            for hdr in ["中文中文密钥", "Bearer ", "Token 😀😀😀"] {
                let auth_resp = ctx
                    .http_client
                    .get(format!("{base}/api/health"))
                    .header("Authorization", hdr)
                    .send()
                    .await?;
                ensure!(
                    auth_resp.status().as_u16() == 401,
                    "expected 401 for '{hdr}'"
                );
            }

            handle.abort();
            Ok("QA-BUG-001/002 多字节百分号解码与非 ASCII 鉴权头边界零崩溃通过".to_string())
        },
    )
    .await;

    cat.run_case("data_boundary_pathological_cq_redos_and_whitespace_preservation", async {
        // 验证 QA-BUG-007：5,000 连逗号畸形 CQ 码在线性时间内结束（< 10 ms）
        let mut bomb = String::from("[CQ:face,id=1");
        for _ in 0..5000 {
            bomb.push_str(",k=v");
        }
        bomb.push(']');
        let t0 = Instant::now();
        let segs = parse_cq_string_to_segments(&bomb, false);
        let elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0;
        ensure!(!segs.is_empty() && elapsed_ms < 10.0, "CQ regex took too long: {elapsed_ms:.2} ms");

        let ws_segs = parse_cq_string_to_segments("[CQ:face,id=14,text=  空格保留  ]", false);
        let preserved = ws_segs.iter().any(|seg| matches!(seg, MessageSegment::Face { text: Some(t), .. } if t == "  空格保留  "));
        ensure!(preserved, "CQ param whitespace trimmed unexpectedly: {ws_segs:?}");
        Ok(format!("5,000 参数 CQ 探针仅耗时 {elapsed_ms:.3} ms（< 10ms）且保留参数空格"))
    })
    .await;

    cat.run_case("data_boundary_oversized_wic_image_and_prebody_413_and_sqlite_bounds", async {
        // 验证 QA-BUG-004：伪造 PNG IHDR 声明 16384x16384 像素被 WIC 前置拒绝
        let mut fake_png = Vec::from([137_u8, 80, 78, 71, 13, 10, 26, 10]);
        fake_png.extend_from_slice(&[0, 0, 0, 13]);
        fake_png.extend_from_slice(b"IHDR");
        fake_png.extend_from_slice(&16384_u32.to_be_bytes());
        fake_png.extend_from_slice(&16384_u32.to_be_bytes());
        fake_png.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
        ensure!(decode_image_bytes_to_bgra(&fake_png).is_err(), "oversized 16384x16384 PNG must fail");
        ensure!(decode_image_bytes_to_bgra(b"not-an-image").is_err(), "corrupt stream must fail");

        // 验证 QA-BUG-011：原始 TCP 声明 Content-Length > 8 MiB 在读体前即时短路 413
        let (driver, cfg) = default_stub_and_config();
        let (addr, handle) = adapter_server::spawn_server(driver, cfg).await?;
        let mut tcp = TcpStream::connect(addr).await?;
        let oversized = adapter_server::constants::HTTP_MAX_BODY_BYTES + 128;
        let head = format!(
            "POST /onebot/v11/send_group_msg HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {oversized}\r\n\r\n"
        );
        tcp.write_all(head.as_bytes()).await?;
        let mut buf = [0_u8; 256];
        let n = tokio::time::timeout(Duration::from_secs(2), tcp.read(&mut buf)).await??;
        let resp_head = String::from_utf8_lossy(&buf[..n]);
        ensure!(resp_head.contains("413"), "expected HTTP 413 for oversized Content-Length, got: {resp_head}");
        handle.abort();

        // 验证 QA-BUG-008：u64::MAX 时间戳饱和与空白群名写入拒绝
        let db = Storage::open(MEMORY_DB_PATH)?;
        ensure!(
            db.upsert_group(&UpsertGroupInput {
                group_name: "   ".to_string(),
                ..Default::default()
            })
            .is_err(),
            "empty group name must fail"
        );
        db.record_message(&RecordMessageInput {
            message_uid: "qa-max-ts".to_string(),
            group_name: "BoundaryGroup".to_string(),
            group_id: 100_000_001,
            user_id: 10001,
            sender_name: "QA".to_string(),
            raw_message: "max-ts".to_string(),
            captured_at_ms: u64::MAX,
            ..Default::default()
        })?;
        let recent = db.list_recent_messages("BoundaryGroup", 5)?;
        ensure!(recent.len() == 1 && recent[0].raw_message == "max-ts", "u64::MAX saturation failed");

        Ok("16384px 超大图像拒收、预检 413 秒级截断与 u64::MAX 饱和写入全部通过".to_string())
    })
    .await;

    cat.build()
}

/// 执行第 4 类：条件边界测试（L1）
pub async fn run_condition_boundary_category(ctx: &SuiteExecContext) -> CategoryReport {
    let mut cat = CategoryBuilder::new(
        "CAT-04",
        "条件边界测试",
        "Condition Boundary Test",
        QaTier::L1,
        "群人数上限 9/10 临界、私聊白名单、多端鉴权、WS 三角色权限及安全弹窗隔离 100% 精准",
    );

    cat.run_case("condition_member_count_threshold_9_vs_10_and_whitelist", async {
        let driver = ConfigurableStubDriver::new();
        driver.set_strict_member_guard(true);
        let mut cfg = default_stub_and_config().1;
        cfg.target.strict_below_members = Some(10);
        let (addr, handle) = adapter_server::spawn_server(driver, cfg).await?;
        let base = format!("http://{addr}");

        // 群成员数=9，below=10 允许打开；below=9 则正好卡在 `9 >= 9` 边界拒收 403
        let ok_resp = ctx
            .http_client
            .post(format!("{base}/api/open-group"))
            .json(&json!({"group_name": "Demo测试群", "strict_below_members": 10}))
            .send()
            .await?;
        ensure!(ok_resp.status().as_u16() == 200, "strict_below_members=10 must allow 9 members");

        let reject_resp = ctx
            .http_client
            .post(format!("{base}/api/open-group"))
            .json(&json!({"group_name": "Demo测试群", "strict_below_members": 9}))
            .send()
            .await?;
        ensure!(reject_resp.status().as_u16() == 403, "strict_below_members=9 must reject 9 members");

        // 私聊白名单边界 + 未授权合并转发禁止写入孤儿键（QA-BUG-009）
        let spec = GroupTargetSpec {
            allowed_private_friends: vec!["测试好友A".to_string()],
            ..Default::default()
        };
        ensure!(!spec.is_private_friend_allowed(""), "empty friend disallowed");
        ensure!(spec.is_private_friend_allowed("测试好友A"), "exact whitelist friend allowed");

        let fwd_forbid = ctx
            .http_client
            .post(format!("{base}/onebot/v11/send_private_forward_msg"))
            .json(&json!({
                "nickname": "未授权路人",
                "messages": [{"type": "node", "data": {"nickname": "A", "content": "x"}}]
            }))
            .send()
            .await?;
        ensure!(fwd_forbid.status().as_u16() == 403, "unauthorized forward must return 403");

        let over_nodes: Vec<Value> = (0..=adapter_server::constants::MAX_FORWARD_NODES)
            .map(|i| json!({"type": "node", "data": {"nickname": "A", "content": format!("n-{i}")}}))
            .collect();
        let fwd_cap = ctx
            .http_client
            .post(format!("{base}/onebot/v11/send_private_forward_msg"))
            .json(&json!({"nickname": "测试好友", "messages": over_nodes}))
            .send()
            .await?;
        ensure!(fwd_cap.status().as_u16() == 400, "over MAX_FORWARD_NODES must return 400");

        handle.abort();
        Ok("人数阈值 9<10 / 9>=9 临界、私聊白名单与合并转发 200 节点上限校验通过".to_string())
    })
    .await;

    cat.run_case(
        "condition_auth_matrix_ws_roles_async_and_allow_mock_isolation",
        async {
            let (driver, mut cfg) = default_stub_and_config();
            cfg.server.access_token = Some("cond-tok+9".to_string());
            let (addr, handle) =
                adapter_server::spawn_server(Arc::clone(&driver) as Arc<_>, cfg).await?;
            let base = format!("http://{addr}");

            // 1. Bearer / Token / Query 三种鉴权方式均可通过，错误密钥稳定 401
            ensure!(
                ctx.http_client
                    .get(format!("{base}/api/health"))
                    .send()
                    .await?
                    .status()
                    .as_u16()
                    == 401
            );
            ensure!(
                ctx.http_client
                    .get(format!("{base}/api/health"))
                    .header("Authorization", "Bearer cond-tok+9")
                    .send()
                    .await?
                    .status()
                    .as_u16()
                    == 200
            );
            ensure!(
                ctx.http_client
                    .get(format!("{base}/api/health"))
                    .header("Authorization", "Token cond-tok+9")
                    .send()
                    .await?
                    .status()
                    .as_u16()
                    == 200
            );
            ensure!(
                ctx.http_client
                    .get(format!("{base}/api/health?access_token=cond-tok%2B9"))
                    .send()
                    .await?
                    .status()
                    .as_u16()
                    == 200
            );

            // 2. 正向 Event-only WS 拒绝 API 动作（10004）；API-only WS 允许动作调用
            let (mut ev_ws, _) = connect_async(format!(
                "ws://{addr}/onebot/v11/ws/event?access_token=cond-tok%2B9"
            ))
            .await?;
            let _lifecycle = ws_next_json(&mut ev_ws, Duration::from_secs(2)).await?;
            ev_ws
                .send(Message::Text(
                    json!({"action": "get_status", "echo": "cond-ev-deny"})
                        .to_string()
                        .into(),
                ))
                .await?;
            let ev_reply = ws_next_json(&mut ev_ws, Duration::from_secs(2)).await?;
            ensure!(
                ev_reply["retcode"] == 10004,
                "Event-only WS must reject API call: {ev_reply}"
            );

            let (mut api_ws, _) = connect_async(format!(
                "ws://{addr}/onebot/v11/ws/api?access_token=cond-tok%2B9"
            ))
            .await?;
            api_ws
                .send(Message::Text(
                    json!({"action": "get_version_info", "echo": "cond-api-ok"})
                        .to_string()
                        .into(),
                ))
                .await?;
            let api_reply = ws_next_json(&mut api_ws, Duration::from_secs(2)).await?;
            ensure!(
                api_reply["retcode"] == 0,
                "API-only WS action failed: {api_reply}"
            );

            // 3. 验证 _async 动作返回 retcode=1
            let async_body: Value = ctx
                .http_client
                .post(format!(
                    "{base}/onebot/v11/send_group_msg_async?access_token=cond-tok%2B9"
                ))
                .json(&json!({"message": "async-cond"}))
                .send()
                .await?
                .json()
                .await?;
            ensure!(async_body["status"] == "async" && async_body["retcode"] == 1);

            // 4. 验证 QA-BUG-010：allow_mock=false 严禁命中 Mock 安全窗体
            let hits_prod = find_windows_with_keywords_ext(&[], &["安全验证"], false);
            ensure!(
                hits_prod.is_empty(),
                "allow_mock=false unexpectedly matched security windows"
            );

            handle.abort();
            Ok(
                "三重鉴权矩阵、WS API/Event 角色隔离、_async 修饰符及 allow_mock 隔离验证全绿"
                    .to_string(),
            )
        },
    )
    .await;

    cat.build()
}
