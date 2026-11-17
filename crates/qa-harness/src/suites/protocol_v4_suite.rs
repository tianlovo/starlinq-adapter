//! OneBot v11 协议第 4 阶段（OCR / 原生视频 / 文件传输 / 11 项明确 10004）专项契约验证套件

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use adapter_server::spawn_server;
use anyhow::{ensure, Context, Result};
use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use common_types::constants::{DEFAULT_ONEBOT_ID_BOUND, DEFAULT_TARGET_GROUP_NAME};
use common_types::{
    stable_onebot_id, AdapterEvent, ChatTargetKind, MessageSegment, OcrImageResponse, SendOutcome,
    StoredMessageRecord,
};
use serde_json::json;
use tokio_tungstenite::connect_async;
use uia_core::{encode_bgra_to_png_bytes, DecodedImageBgra};
use windows::core::PCWSTR;
use windows::Win32::Foundation::COLORREF;
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, CreateFontW, DeleteDC, DeleteObject, GdiFlush,
    SelectObject, SetBkMode, SetTextColor, TextOutW, ANTIALIASED_QUALITY, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DIB_RGB_COLORS, HGDIOBJ,
    OUT_DEFAULT_PRECIS, TRANSPARENT,
};

use super::{
    default_stub_and_config, get_json_status, post_json_status, ws_next_json, CategoryBuilder,
    SuiteExecContext,
};

const STUB_GROUP_ID: i64 = 100_000_001;

static NEXT_STAGE_SEQ: AtomicUsize = AtomicUsize::new(1);

/// 临时 `.runtime/staging_uploads` 测试目录守卫（Drop 时自动清理测试夹具）
struct StagingDirGuard {
    dir: PathBuf,
}

impl StagingDirGuard {
    /// 创建进程及序号隔离的临时暂存测试目录
    fn create() -> Result<Self> {
        let seq = NEXT_STAGE_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = PathBuf::from(".runtime")
            .join("staging_uploads")
            .join(format!("qa-v4-{}-{seq}", std::process::id()));
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("create staging test dir '{}'", dir.display()))?;
        Ok(Self { dir })
    }

    /// 写入测试夹具文件并返回路径
    fn write_file(&self, name: &str, bytes: &[u8]) -> Result<PathBuf> {
        let path = self.dir.join(name);
        std::fs::write(&path, bytes)
            .with_context(|| format!("write staging fixture '{}'", path.display()))?;
        Ok(path)
    }

    /// 返回相对暂存根目录的沙箱子路径引用
    fn relative_ref(&self, name: &str) -> String {
        let folder = self
            .dir
            .file_name()
            .map(|seg| seg.to_string_lossy().into_owned())
            .unwrap_or_default();
        format!("{folder}/{name}")
    }
}

impl Drop for StagingDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// 通过 Win32 GDI 平滑字体渲染白底黑字 `HOYO 2026` 测试 PNG 字节流供真实 WinRT OCR 验证
fn build_synthetic_hoyo_png() -> Result<Vec<u8>> {
    let width = 480_u32;
    let height = 140_u32;
    let byte_len = (width as usize) * (height as usize) * 4;
    let mut bgra = vec![255_u8; byte_len];

    unsafe {
        let hdc = CreateCompatibleDC(None);
        ensure!(!hdc.is_invalid(), "CreateCompatibleDC failed");

        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width as i32,
                biHeight: -(height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };

        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let Ok(hbmp) = CreateDIBSection(Some(hdc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0) else {
            let _ = DeleteDC(hdc);
            anyhow::bail!("CreateDIBSection failed");
        };
        if bits.is_null() {
            let _ = DeleteObject(HGDIOBJ(hbmp.0));
            let _ = DeleteDC(hdc);
            anyhow::bail!("CreateDIBSection returned null pixel pointer");
        }

        std::ptr::write_bytes(bits.cast::<u8>(), 255, byte_len);
        let old_bmp = SelectObject(hdc, HGDIOBJ(hbmp.0));
        let face_wide: Vec<u16> = "Tahoma\0".encode_utf16().collect();
        let hfont = CreateFontW(
            -36,
            0,
            0,
            0,
            700,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            ANTIALIASED_QUALITY,
            0,
            PCWSTR(face_wide.as_ptr()),
        );
        if hfont.is_invalid() {
            let _ = SelectObject(hdc, old_bmp);
            let _ = DeleteObject(HGDIOBJ(hbmp.0));
            let _ = DeleteDC(hdc);
            anyhow::bail!("CreateFontW failed");
        }
        let old_font = SelectObject(hdc, HGDIOBJ(hfont.0));

        let _ = SetBkMode(hdc, TRANSPARENT);
        let _ = SetTextColor(hdc, COLORREF(0x0000_0000));
        let wide: Vec<u16> = "HOYO 2026".encode_utf16().collect();
        let drawn = TextOutW(hdc, 48, 36, &wide).as_bool();
        let _ = GdiFlush();

        if drawn {
            let slice = std::slice::from_raw_parts(bits.cast::<u8>(), byte_len);
            bgra.copy_from_slice(slice);
            for pixel in bgra.as_chunks_mut::<4>().0 {
                pixel[3] = 255;
            }
        }

        let _ = SelectObject(hdc, old_font);
        let _ = SelectObject(hdc, old_bmp);
        let _ = DeleteObject(HGDIOBJ(hfont.0));
        let _ = DeleteObject(HGDIOBJ(hbmp.0));
        let _ = DeleteDC(hdc);
        ensure!(drawn, "TextOutW failed");
    }

    encode_bgra_to_png_bytes(&DecodedImageBgra {
        width,
        height,
        bgra_top_down: bgra,
    })
    .map_err(Into::into)
}

fn assert_ocr_contains_hoyo_2026(resp: &OcrImageResponse, source: &str) -> Result<()> {
    ensure!(
        !resp.language.trim().is_empty(),
        "{source}: OCR language must not be empty"
    );
    let combined = resp
        .texts
        .iter()
        .map(|item| item.text.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    ensure!(
        combined.contains("HOYO") && combined.contains("2026"),
        "{source}: OCR text mismatch, got '{combined}' ({resp:?})"
    );
    for item in &resp.texts {
        ensure!(
            item.coordinates.len() == 4,
            "{source}: expected 4 coordinates, got {:?}",
            item.coordinates
        );
    }
    Ok(())
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|dur| dur.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or(1_700_000_000_000)
}

fn make_stub_record(
    id: i64,
    session_name: &str,
    group_id: i64,
    chat_type: ChatTargetKind,
    text: &str,
) -> StoredMessageRecord {
    StoredMessageRecord {
        onebot_message_id: id,
        message_uid: format!("biz-v5-{id}"),
        group_name: session_name.to_string(),
        group_id,
        user_id: if chat_type == ChatTargetKind::Private {
            200_000_002
        } else {
            20_001
        },
        sender_name: "QA测试成员".to_string(),
        raw_message: text.to_string(),
        chat_type,
        segments: vec![MessageSegment::Text {
            text: text.to_string(),
        }],
        is_self: false,
        timestamp_hint: Some("12:00".to_string()),
        captured_at_ms: now_unix_ms(),
    }
}

/// 校验 Wave v5 业务风险防线（私聊越权、同名会话隔离、快捷操作原子/截止期、UNC/BASE64 OCR 与策略非零校验）
async fn verify_biz_risk_rest_and_quick_ops(ctx: &SuiteExecContext) -> Result<()> {
    let (driver, cfg) = default_stub_and_config();
    driver.insert_stored_message(make_stub_record(
        95_100,
        "测试好友",
        0,
        ChatTargetKind::Private,
        "allowed private msg",
    ));
    driver.insert_stored_message(make_stub_record(
        95_101,
        "陌生路人",
        0,
        ChatTargetKind::Private,
        "secret private msg",
    ));
    driver.insert_stored_message(make_stub_record(
        95_201,
        "测试好友",
        STUB_GROUP_ID,
        ChatTargetKind::Group,
        "same-name-group-1",
    ));
    driver.insert_stored_message(make_stub_record(
        95_202,
        "测试好友",
        STUB_GROUP_ID,
        ChatTargetKind::Group,
        "same-name-group-2",
    ));
    for idx in 1..=4 {
        driver.insert_stored_message(make_stub_record(
            95_210 + idx,
            "测试好友",
            0,
            ChatTargetKind::Private,
            &format!("same-name-priv-{idx}"),
        ));
    }
    driver.insert_stored_message(make_stub_record(
        95_301,
        DEFAULT_TARGET_GROUP_NAME,
        STUB_GROUP_ID,
        ChatTargetKind::Group,
        "quick-op target",
    ));
    driver.insert_stored_message(make_stub_record(
        95_302,
        "其他测试群",
        666_001,
        ChatTargetKind::Group,
        "cross-group target",
    ));

    let (addr, srv_handle) = spawn_server(Arc::clone(&driver) as Arc<_>, cfg).await?;
    let base = format!("http://{addr}");

    // 1. 非白名单私聊 get_msg 与 /api/messages/history 强制返回 403/1403
    let (deny_msg_code, deny_msg_body) = get_json_status(
        &ctx.http_client,
        &format!("{base}/onebot/v11/get_msg?message_id=95101"),
    )
    .await?;
    ensure!(
        deny_msg_code == 403
            && deny_msg_body["retcode"] == 1403
            && deny_msg_body["code"] == "PRIVATE_TARGET_NOT_ALLOWED",
        "expected 403/1403 for unwhitelisted private get_msg, got {deny_msg_code} {deny_msg_body}"
    );
    let (allow_msg_code, allow_msg_body) = get_json_status(
        &ctx.http_client,
        &format!("{base}/onebot/v11/get_msg?message_id=95100"),
    )
    .await?;
    ensure!(
        allow_msg_code == 200 && allow_msg_body["data"]["raw_message"] == "allowed private msg",
        "expected 200 for allowed private get_msg, got {allow_msg_code} {allow_msg_body}"
    );
    let (deny_hist_code, deny_hist_body) = get_json_status(
        &ctx.http_client,
        &format!("{base}/api/messages/history?session=%E9%99%8C%E7%94%9F%E8%B7%AF%E4%BA%BA&chat_type=private"),
    )
    .await?;
    ensure!(
        deny_hist_code == 403
            && deny_hist_body["retcode"] == 1403
            && deny_hist_body["code"] == "PRIVATE_TARGET_NOT_ALLOWED",
        "expected 403/1403 for unwhitelisted private history, got {deny_hist_code} {deny_hist_body}"
    );

    // 2. 同名 Group vs Private 会话严格按 chat_type 隔离，不被尾部截断互相挤占
    let (grp_hist_code, grp_hist_body) = post_json_status(
        &ctx.http_client,
        &format!("{base}/onebot/v11/get_group_msg_history"),
        json!({ "group_name": "测试好友", "count": 2 }),
    )
    .await?;
    ensure!(
        grp_hist_code == 200,
        "get_group_msg_history failed: {grp_hist_body}"
    );
    let grp_msgs = grp_hist_body["data"]["messages"]
        .as_array()
        .context("missing group history messages array")?;
    ensure!(
        grp_msgs.len() == 2
            && grp_msgs[0]["message_type"] == "group"
            && grp_msgs[0]["raw_message"] == "same-name-group-1"
            && grp_msgs[1]["raw_message"] == "same-name-group-2",
        "same-name group history polluted or truncated: {grp_hist_body}"
    );

    let (priv_hist_code, priv_hist_body) = post_json_status(
        &ctx.http_client,
        &format!("{base}/onebot/v11/get_friend_msg_history"),
        json!({ "nickname": "测试好友", "count": 2 }),
    )
    .await?;
    ensure!(
        priv_hist_code == 200,
        "get_friend_msg_history failed: {priv_hist_body}"
    );
    let priv_msgs = priv_hist_body["data"]["messages"]
        .as_array()
        .context("missing friend history messages array")?;
    ensure!(
        priv_msgs.len() == 2
            && priv_msgs[0]["message_type"] == "private"
            && priv_msgs[0]["raw_message"] == "same-name-priv-3"
            && priv_msgs[1]["raw_message"] == "same-name-priv-4",
        "same-name private history polluted or truncated: {priv_hist_body}"
    );

    // 3. .handle_quick_operation 组合不支持动作原子拒绝、过期撤回 408/1408 与独立媒体规范
    let (send_code, send_body) = post_json_status(
        &ctx.http_client,
        &format!("{base}/onebot/v11/send_group_msg"),
        json!({ "group_id": STUB_GROUP_ID, "message": "self-quick-delete-target" }),
    )
    .await?;
    ensure!(send_code == 200, "seed self message failed: {send_body}");
    let self_msg_id = send_body["data"]["message_id"]
        .as_i64()
        .context("missing seeded message_id")?;
    let baseline_sent = driver.sent_requests().len();

    let (combo_code, combo_body) = post_json_status(
        &ctx.http_client,
        &format!("{base}/onebot/v11/.handle_quick_operation"),
        json!({
            "context": {
                "post_type": "message",
                "message_type": "group",
                "group_id": STUB_GROUP_ID,
                "user_id": 20_001,
                "message_id": self_msg_id,
            },
            "operation": { "reply": "should-not-send", "ban": true },
        }),
    )
    .await?;
    ensure!(
        combo_code == 501
            && combo_body["retcode"] == 10004
            && combo_body["code"] == "UNSUPPORTED_ACTION"
            && driver.sent_requests().len() == baseline_sent,
        "combo quick operation must return 501/10004 with 0 sends: {combo_code} {combo_body}"
    );

    let (exp_code, exp_body) = post_json_status(
        &ctx.http_client,
        &format!("{base}/onebot/v11/.handle_quick_operation"),
        json!({
            "context": {
                "message_type": "group",
                "group_id": STUB_GROUP_ID,
                "message_id": self_msg_id,
                "response_deadline_ms": now_unix_ms().saturating_sub(1_000),
            },
            "operation": { "delete": true },
        }),
    )
    .await?;
    ensure!(
        exp_code == 408
            && exp_body["retcode"] == 1408
            && exp_body["code"] == "RESPONSE_WINDOW_EXPIRED"
            && driver.outbound_deadline_abort_count() >= 1,
        "expired quick delete must return 408/1408: {exp_code} {exp_body}"
    );
    let (still_code, _) = get_json_status(
        &ctx.http_client,
        &format!("{base}/onebot/v11/get_msg?message_id={self_msg_id}"),
    )
    .await?;
    ensure!(
        still_code == 200,
        "expired quick delete unexpectedly removed message"
    );

    let (ok_del_code, _) = post_json_status(
        &ctx.http_client,
        &format!("{base}/onebot/v11/.handle_quick_operation"),
        json!({
            "context": {
                "message_type": "group",
                "group_id": STUB_GROUP_ID,
                "message_id": self_msg_id,
                "response_deadline_ms": now_unix_ms().saturating_add(5_000),
            },
            "operation": { "delete": true },
        }),
    )
    .await?;
    ensure!(
        ok_del_code == 200,
        "non-expired quick delete should succeed"
    );

    let (vid_code, vid_body) = post_json_status(
        &ctx.http_client,
        &format!("{base}/onebot/v11/.handle_quick_operation"),
        json!({
            "context": {
                "post_type": "message",
                "message_type": "group",
                "group_id": STUB_GROUP_ID,
                "user_id": 20_001,
                "message_id": 95_301,
            },
            "operation": { "reply": "[CQ:video,file=clip.mp4]", "at_sender": true },
        }),
    )
    .await?;
    ensure!(
        vid_code == 200 && vid_body["data"]["applied"] == true,
        "standalone quick video reply failed: {vid_code} {vid_body}"
    );
    let latest_sent = driver
        .sent_requests()
        .pop()
        .context("missing quick video sent request")?;
    ensure!(
        matches!(&latest_sent.segments[..], [MessageSegment::Video { .. }]),
        "standalone quick reply must not prepend Reply or At: {:?}",
        latest_sent.segments
    );

    let (cross_code, _) = post_json_status(
        &ctx.http_client,
        &format!("{base}/onebot/v11/.handle_quick_operation"),
        json!({
            "context": {
                "post_type": "message",
                "message_type": "group",
                "group_id": STUB_GROUP_ID,
                "user_id": 20_001,
                "message_id": 95_302,
            },
            "operation": { "reply": "no-cross-reply", "at_sender": false },
        }),
    )
    .await?;
    ensure!(cross_code == 200, "cross-session quick reply failed");
    let cross_sent = driver
        .sent_requests()
        .pop()
        .context("missing cross-session quick reply")?;
    ensure!(
        !cross_sent
            .segments
            .iter()
            .any(|seg| matches!(seg, MessageSegment::Reply { .. })),
        "cross-group quick reply must not inject Reply segment: {:?}",
        cross_sent.segments
    );

    // 4. POST /api/ocr 拒绝 UNC 网络共享路径 (400)，并放行大写 BASE64:// 载荷
    for unc in [r"\\127.0.0.1\share\x.png", "//127.0.0.1/share/x.png"] {
        let (unc_code, unc_body) = post_json_status(
            &ctx.http_client,
            &format!("{base}/api/ocr"),
            json!({ "image": unc }),
        )
        .await?;
        ensure!(
            unc_code == 400 && unc_body["retcode"] == 1400 && unc_body["code"] == "BAD_REQUEST",
            "expected 400/1400 for UNC path '{unc}', got {unc_code} {unc_body}"
        );
    }
    let png_bytes = build_synthetic_hoyo_png()?;
    let upper_b64 = format!("BASE64://{}", STANDARD.encode(&png_bytes));
    let (upper_code, upper_body) = post_json_status(
        &ctx.http_client,
        &format!("{base}/api/ocr"),
        json!({ "image": upper_b64 }),
    )
    .await?;
    ensure!(
        upper_code == 200,
        "uppercase BASE64:// OCR failed: {upper_code} {upper_body}"
    );
    let parsed_ocr: OcrImageResponse =
        serde_json::from_value(upper_body).context("decode uppercase BASE64:// OCR response")?;
    assert_ocr_contains_hoyo_2026(&parsed_ocr, "POST /api/ocr BASE64://")?;

    // 5. POST /api/scheduler/policy 5 项非零字段传 0 拒绝返回 400 / BAD_REQUEST / 1400
    for field in [
        "contextual_response_window_ms",
        "ordinary_response_window_ms",
        "single_poll_burst_threshold",
        "hard_shed_queue_depth",
        "group_quantum_messages",
    ] {
        let (pol_code, pol_body) = post_json_status(
            &ctx.http_client,
            &format!("{base}/api/scheduler/policy"),
            json!({ field: 0 }),
        )
        .await?;
        ensure!(
            pol_code == 400 && pol_body["retcode"] == 1400 && pol_body["code"] == "BAD_REQUEST",
            "expected 400/1400 for zero {field}, got {pol_code} {pol_body}"
        );
    }

    srv_handle.abort();
    Ok(())
}

/// 校验 `post_message_sent = true` 回填真实存储正文，且失败发送不发射 `message_sent`
async fn verify_biz_risk_message_sent_ws(ctx: &SuiteExecContext) -> Result<()> {
    let (driver, mut cfg) = default_stub_and_config();
    cfg.onebot.post_message_sent = true;
    let (addr, srv_handle) = spawn_server(Arc::clone(&driver) as Arc<_>, cfg).await?;
    let base = format!("http://{addr}");
    tokio::time::sleep(Duration::from_millis(40)).await;

    let (mut ws, _) = connect_async(format!("ws://{addr}/onebot/v11/ws/event")).await?;
    let lifecycle = ws_next_json(&mut ws, Duration::from_secs(2)).await?;
    ensure!(
        lifecycle["meta_event_type"] == "lifecycle",
        "expected WS lifecycle event, got {lifecycle}"
    );

    let (ok_code, ok_body) = post_json_status(
        &ctx.http_client,
        &format!("{base}/onebot/v11/send_group_msg"),
        json!({
            "group_id": STUB_GROUP_ID,
            "message": "[CQ:at,qq=20001] 真实回执正文",
        }),
    )
    .await?;
    ensure!(ok_code == 200, "send_group_msg failed: {ok_body}");
    let sent_msg_id = ok_body["data"]["message_id"]
        .as_i64()
        .context("missing sent message_id")?;

    let ws_sent = ws_next_json(&mut ws, Duration::from_secs(2)).await?;
    ensure!(
        ws_sent["post_type"] == "message_sent"
            && ws_sent["message_id"] == sent_msg_id
            && ws_sent["raw_message"] == "[CQ:at,qq=20001] 真实回执正文"
            && ws_sent["sender"]["nickname"] == "QA测试机器人"
            && ws_sent["message"]
                == json!([
                    { "type": "at", "data": { "qq": "20001" } },
                    { "type": "text", "data": { "text": " 真实回执正文" } }
                ]),
        "message_sent did not enrich real stored payload: {ws_sent}"
    );

    driver.set_force_unconfirmed_send(true);
    let (fail_code, fail_body) = post_json_status(
        &ctx.http_client,
        &format!("{base}/onebot/v11/send_group_msg"),
        json!({ "group_id": STUB_GROUP_ID, "message": "unconfirmed-should-not-emit" }),
    )
    .await?;
    ensure!(
        fail_code == 500 && fail_body["retcode"] == 1500,
        "expected 500/1500 for unconfirmed send, got {fail_code} {fail_body}"
    );
    driver.set_force_unconfirmed_send(false);

    driver.emit_event(AdapterEvent::MessageSent(SendOutcome {
        ok: false,
        group: DEFAULT_TARGET_GROUP_NAME.to_string(),
        detail: "failed".to_string(),
        onebot_message_id: Some(99_999),
        target_kind: ChatTargetKind::Group,
        ..Default::default()
    }));
    driver.emit_event(AdapterEvent::Notice(json!({
        "post_type": "notice",
        "notice_type": "group_recall",
        "group_id": STUB_GROUP_ID,
        "user_id": 20_001,
        "operator_id": 10_001,
        "message_id": 95_301,
    })));

    let marker_ev = ws_next_json(&mut ws, Duration::from_secs(2)).await?;
    ensure!(
        marker_ev["post_type"] == "notice",
        "failed send unexpectedly emitted message_sent before notice marker: {marker_ev}"
    );

    srv_handle.abort();
    Ok(())
}

/// 执行 `CAT-12` OneBot v11/OCR/文件视频新契约全量校验并输出分类报告
pub async fn run_protocol_v4_category(ctx: &SuiteExecContext) -> crate::models::CategoryReport {
    let mut cat = CategoryBuilder::new(
        "CAT-12",
        "OneBot v11 扩展契约与 OCR/文件视频验证",
        "OneBot v11 Extended Contracts: WinRT OCR, Video & File Actions",
        crate::models::QaTier::L1,
        "WinRT 离线 OCR、原生视频/文件边界及 11 项明确 10004 拒收全部达标",
    );

    cat.run_case("winrt_offline_ocr_actions_and_boundaries", async {
        let (driver, cfg) = default_stub_and_config();
        let (addr, srv_handle) = spawn_server(Arc::clone(&driver) as Arc<_>, cfg).await?;
        let base = format!("http://{addr}");

        let png_bytes = build_synthetic_hoyo_png()?;
        let b64_uri = format!("base64://{}", STANDARD.encode(&png_bytes));

        for action in ["ocr_image", ".ocr_image"] {
            let (code, body) = post_json_status(
                &ctx.http_client,
                &format!("{base}/onebot/v11/{action}"),
                json!({ "image": b64_uri }),
            )
            .await?;
            ensure!(
                code == 200 && body["retcode"] == 0 && body["status"] == "ok",
                "{action} failed: {code} {body}"
            );
            let parsed: OcrImageResponse = serde_json::from_value(body["data"].clone())
                .with_context(|| format!("decode {action} data"))?;
            assert_ocr_contains_hoyo_2026(&parsed, action)?;
        }

        let (rest_code, rest_body) = post_json_status(
            &ctx.http_client,
            &format!("{base}/api/ocr"),
            json!({ "image": b64_uri }),
        )
        .await?;
        ensure!(
            rest_code == 200,
            "POST /api/ocr failed: {rest_code} {rest_body}"
        );
        let rest_parsed: OcrImageResponse =
            serde_json::from_value(rest_body).context("decode POST /api/ocr response")?;
        assert_ocr_contains_hoyo_2026(&rest_parsed, "POST /api/ocr")?;

        let (http_code, http_body) = post_json_status(
            &ctx.http_client,
            &format!("{base}/onebot/v11/ocr_image"),
            json!({ "image": "https://example.com/remote.png" }),
        )
        .await?;
        ensure!(
            http_code == 501
                && http_body["retcode"] == 10004
                && http_body["code"] == "UNSUPPORTED_ACTION",
            "expected 501/10004 for remote OCR URL, got {http_code} {http_body}"
        );

        let (rest_http_code, rest_http_body) = post_json_status(
            &ctx.http_client,
            &format!("{base}/api/ocr"),
            json!({ "image": "http://example.com/remote.png" }),
        )
        .await?;
        ensure!(
            rest_http_code == 501 && rest_http_body["code"] == "UNSUPPORTED_ACTION",
            "expected 501 UNSUPPORTED_ACTION on /api/ocr, got {rest_http_code} {rest_http_body}"
        );

        let (trav_code, trav_body) = post_json_status(
            &ctx.http_client,
            &format!("{base}/onebot/v11/ocr_image"),
            json!({ "image": "../outside.png" }),
        )
        .await?;
        ensure!(
            trav_code == 400 && trav_body["retcode"] == 1400 && trav_body["code"] == "BAD_REQUEST",
            "expected 400/1400 for '..' traversal, got {trav_code} {trav_body}"
        );

        srv_handle.abort();
        Ok(
            "ocr_image / .ocr_image / POST /api/ocr 真实 WinRT OCR 识别与 http/.. 边界全通过"
                .to_string(),
        )
    })
    .await;

    cat.run_case("video_capability_local_send_and_mixed_remote_reject", async {
        let staging = StagingDirGuard::create()?;
        let mp4_path = staging.write_file("qa_clip.mp4", b"FTYP_MOCK_MP4_PAYLOAD")?;
        let mp4_str = mp4_path.to_string_lossy().replace('\\', "/");

        let (driver, cfg) = default_stub_and_config();
        let (addr, srv_handle) = spawn_server(Arc::clone(&driver) as Arc<_>, cfg).await?;
        let base = format!("http://{addr}");

        let (get_cap_code, get_cap_body) =
            get_json_status(&ctx.http_client, &format!("{base}/onebot/v11/can_send_video")).await?;
        ensure!(
            get_cap_code == 200
                && get_cap_body["retcode"] == 0
                && get_cap_body["data"]["yes"] == true,
            "GET can_send_video unexpected: {get_cap_code} {get_cap_body}"
        );

        let (post_cap_code, post_cap_body) = post_json_status(
            &ctx.http_client,
            &format!("{base}/onebot/v11/can_send_video"),
            json!({}),
        )
        .await?;
        ensure!(
            post_cap_code == 200
                && post_cap_body["retcode"] == 0
                && post_cap_body["data"]["yes"] == true,
            "POST can_send_video unexpected: {post_cap_code} {post_cap_body}"
        );

        let (local_code, local_body) = post_json_status(
            &ctx.http_client,
            &format!("{base}/onebot/v11/send_group_msg"),
            json!({
                "group_id": STUB_GROUP_ID,
                "message": format!("[CQ:video,file={mp4_str}]"),
            }),
        )
        .await?;
        ensure!(
            local_code == 200
                && local_body["retcode"] == 0
                && local_body["data"]["message_id"].as_i64().unwrap_or(0) > 0,
            "local [CQ:video] send failed: {local_code} {local_body}"
        );
        let msg_id_local = local_body["data"]["message_id"].as_i64().unwrap_or(0);

        let b64_video = format!("base64://{}", STANDARD.encode(b"MOCK_VIDEO_B64"));
        let (b64_code, b64_body) = post_json_status(
            &ctx.http_client,
            &format!("{base}/onebot/v11/send_group_msg"),
            json!({
                "group_id": STUB_GROUP_ID,
                "message": format!("[CQ:video,file={b64_video}]"),
            }),
        )
        .await?;
        ensure!(
            b64_code == 200 && b64_body["retcode"] == 0,
            "base64:// [CQ:video] send failed: {b64_code} {b64_body}"
        );

        let sent = driver.sent_requests();
        ensure!(sent.len() == 2, "expected 2 recorded video sends, got {}", sent.len());
        ensure!(
            matches!(&sent[0].segments[..], [MessageSegment::Video { .. }])
                && matches!(&sent[1].segments[..], [MessageSegment::Video { .. }]),
            "StubDriver did not accurately record Video segments: {sent:?}"
        );

        let (get_msg_code, get_msg_body) = post_json_status(
            &ctx.http_client,
            &format!("{base}/onebot/v11/get_msg"),
            json!({ "message_id": msg_id_local }),
        )
        .await?;
        ensure!(
            get_msg_code == 200
                && get_msg_body["data"]["raw_message"]
                    .as_str()
                    .unwrap_or("")
                    .contains("[CQ:video"),
            "get_msg did not echo Video segment: {get_msg_code} {get_msg_body}"
        );

        let (http_code, http_body) = post_json_status(
            &ctx.http_client,
            &format!("{base}/onebot/v11/send_group_msg"),
            json!({
                "group_id": STUB_GROUP_ID,
                "message": "[CQ:video,file=http://example.com/demo.mp4]",
            }),
        )
        .await?;
        ensure!(
            http_code == 501
                && http_body["retcode"] == 10004
                && http_body["code"] == "UNSUPPORTED_ACTION",
            "expected 501/10004 for remote http:// video, got {http_code} {http_body}"
        );

        let (mixed_code, mixed_body) = post_json_status(
            &ctx.http_client,
            &format!("{base}/onebot/v11/send_group_msg"),
            json!({
                "group_id": STUB_GROUP_ID,
                "message": [
                    { "type": "text", "data": { "text": "prefix" } },
                    { "type": "video", "data": { "file": mp4_str } }
                ],
            }),
        )
        .await?;
        ensure!(
            mixed_code == 501
                && mixed_body["retcode"] == 10004
                && mixed_body["code"] == "UNSUPPORTED_ACTION",
            "expected 501/10004 for mixed text+video, got {mixed_code} {mixed_body}"
        );

        srv_handle.abort();
        Ok("can_send_video=true、本地/base64 [CQ:video] 发送入库与 http/图文混排 501/10004 拒收全通过".to_string())
    })
    .await;

    cat.run_case("upload_file_and_get_file_sandbox_contracts", async {
        let staging = StagingDirGuard::create()?;
        let payload_bytes = b"HOYO-STAGE3-FILE-OK";
        let txt_path = staging.write_file("sample_qa.txt", payload_bytes)?;
        let txt_str = txt_path.to_string_lossy().replace('\\', "/");
        let sandbox_rel = staging.relative_ref("sample_qa.txt");

        let (driver, cfg) = default_stub_and_config();
        let (addr, srv_handle) = spawn_server(Arc::clone(&driver) as Arc<_>, cfg).await?;
        let base = format!("http://{addr}");

        let (grp_code, grp_body) = post_json_status(
            &ctx.http_client,
            &format!("{base}/onebot/v11/upload_group_file"),
            json!({
                "group_id": STUB_GROUP_ID,
                "file": txt_str,
                "name": "sample_qa.txt",
            }),
        )
        .await?;
        ensure!(
            grp_code == 200
                && grp_body["retcode"] == 0
                && grp_body["data"]["message_id"].as_i64().unwrap_or(0) > 0,
            "upload_group_file failed: {grp_code} {grp_body}"
        );

        let friend_uid = stable_onebot_id("测试好友", DEFAULT_ONEBOT_ID_BOUND);
        let b64_file = format!("base64://{}", STANDARD.encode(b"PRIVATE-DOC"));
        let (priv_code, priv_body) = post_json_status(
            &ctx.http_client,
            &format!("{base}/onebot/v11/upload_private_file"),
            json!({
                "user_id": friend_uid,
                "file": b64_file,
                "name": "priv.txt",
            }),
        )
        .await?;
        ensure!(
            priv_code == 200 && priv_body["retcode"] == 0,
            "upload_private_file failed: {priv_code} {priv_body}"
        );

        let sent = driver.sent_requests();
        ensure!(sent.len() == 2, "expected 2 recorded file uploads");
        ensure!(
            matches!(
                &sent[0].segments[..],
                [MessageSegment::File { name: Some(n), size: Some(sz), .. }]
                    if n == "sample_qa.txt" && *sz == payload_bytes.len() as u64
            ) && matches!(
                &sent[1].segments[..],
                [MessageSegment::File { name: Some(n), size: Some(11), .. }]
                    if n == "priv.txt"
            ),
            "StubDriver did not enrich/record outbound File segments accurately: {sent:?}"
        );

        let (deny_priv_code, deny_priv_body) = post_json_status(
            &ctx.http_client,
            &format!("{base}/onebot/v11/upload_private_file"),
            json!({
                "nickname": "陌生人B",
                "file": txt_str,
            }),
        )
        .await?;
        ensure!(
            deny_priv_code == 403 && deny_priv_body["retcode"] == 1403,
            "expected 403/1403 for non-whitelisted upload_private_file, got {deny_priv_code} {deny_priv_body}"
        );

        let (folder_code, folder_body) = post_json_status(
            &ctx.http_client,
            &format!("{base}/onebot/v11/upload_group_file"),
            json!({
                "group_id": STUB_GROUP_ID,
                "file": txt_str,
                "folder": "/sub-folder",
            }),
        )
        .await?;
        ensure!(
            folder_code == 501
                && folder_body["retcode"] == 10004
                && folder_body["code"] == "UNSUPPORTED_ACTION",
            "expected 501/10004 for non-root folder, got {folder_code} {folder_body}"
        );

        let (gf_ok_code, gf_ok_body) = post_json_status(
            &ctx.http_client,
            &format!("{base}/onebot/v11/get_file"),
            json!({ "file_id": sandbox_rel }),
        )
        .await?;
        ensure!(
            gf_ok_code == 200
                && gf_ok_body["retcode"] == 0
                && gf_ok_body["data"]["file_name"] == "sample_qa.txt"
                && gf_ok_body["data"]["file_size"] == payload_bytes.len() as u64
                && gf_ok_body["data"]["base64"] == STANDARD.encode(payload_bytes),
            "get_file sandbox read unexpected: {gf_ok_code} {gf_ok_body}"
        );

        let (gf_trav_code, gf_trav_body) = post_json_status(
            &ctx.http_client,
            &format!("{base}/onebot/v11/get_file"),
            json!({ "file_id": "../secret.txt" }),
        )
        .await?;
        ensure!(
            gf_trav_code == 400
                && gf_trav_body["retcode"] == 1400
                && gf_trav_body["code"] == "BAD_REQUEST",
            "expected 400/1400 for get_file '../' traversal, got {gf_trav_code} {gf_trav_body}"
        );

        let (gf_http_code, gf_http_body) = post_json_status(
            &ctx.http_client,
            &format!("{base}/onebot/v11/get_file"),
            json!({ "file": "https://example.com/a.txt" }),
        )
        .await?;
        ensure!(
            gf_http_code == 501
                && gf_http_body["retcode"] == 10004
                && gf_http_body["code"] == "UNSUPPORTED_ACTION",
            "expected 501/10004 for get_file https URL, got {gf_http_code} {gf_http_body}"
        );

        srv_handle.abort();
        Ok("upload_group_file / upload_private_file / 非根目录 10004 / get_file 沙箱与穿越拦截全通过".to_string())
    })
    .await;

    cat.run_case(
        "all_11_new_explicit_10004_actions_reject_not_10002",
        async {
            let (driver, cfg) = default_stub_and_config();
            let (addr, srv_handle) = spawn_server(Arc::clone(&driver) as Arc<_>, cfg).await?;
            let base = format!("http://{addr}");

            let explicit_10004_actions = [
                "set_msg_emoji_like",
                "fetch_emoji_like",
                "set_essence_msg",
                "delete_essence_msg",
                "_send_group_notice",
                "_get_group_notice",
                "delete_friend",
                "set_qq_avatar",
                "get_group_file_system_info",
                "get_group_files_by_folder",
                "get_group_file_url",
            ];

            for action in explicit_10004_actions {
                let (code, body) = post_json_status(
                    &ctx.http_client,
                    &format!("{base}/onebot/v11/{action}"),
                    json!({}),
                )
                .await?;
                ensure!(
                    code == 501
                        && body["retcode"] == 10004
                        && body["retcode"] != 10002
                        && body["retcode"] != 1404
                        && body["status"] == "failed"
                        && body["code"] == "UNSUPPORTED_ACTION",
                    "action {action} did not return explicit 501/10004: {code} {body}"
                );
            }

            let (unk_code, unk_body) = post_json_status(
                &ctx.http_client,
                &format!("{base}/onebot/v11/unknown_action_not_in_registry"),
                json!({}),
            )
            .await?;
            ensure!(
                unk_code == 404
                    && unk_body["retcode"] == 1404
                    && unk_body["code"] == "ACTION_NOT_FOUND",
                "unknown action should return 404/1404 ACTION_NOT_FOUND, got {unk_code} {unk_body}"
            );

            srv_handle.abort();
            Ok(
                "11 项新增显式 10004 动作全部返回 HTTP 501 / retcode 10004（非 1404/10002）"
                    .to_string(),
            )
        },
    )
    .await;

    cat.run_case("business_logic_risk_guards_v5", async {
        verify_biz_risk_rest_and_quick_ops(ctx).await?;
        verify_biz_risk_message_sent_ws(ctx).await?;
        Ok(
            "Wave v5 越权私聊 403、同名会话隔离、快捷操作原子/截止期、UNC/BASE64 OCR、策略非零与真实 message_sent 回执全通过"
                .to_string(),
        )
    })
    .await;

    cat.set_metric("new_explicit_10004_actions_checked", 11.0);
    cat.build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn protocol_v4_contract_suite_passes_all_cases() {
        let ctx = SuiteExecContext::new("http://127.0.0.1:18770".to_string(), true)
            .expect("build suite context");
        let report = run_protocol_v4_category(&ctx).await;
        assert!(
            report.passed && report.failed_cases == 0 && report.passed_cases == 5,
            "protocol_v4_suite failed: {report:#?}"
        );
    }
}
