//! Wave v6 契约验收套件（CAT-13）：覆盖深分页游标/倒序、原生单条转发、最近联系人/审计脱敏、子目录全清与熔断受控恢复

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use adapter_server::spawn_server;
use anyhow::{ensure, Context, Result};
use common_types::constants::DEFAULT_TARGET_GROUP_NAME;
use common_types::{ChatTargetKind, MessageHistoryFilter, MessageSegment, StoredMessageRecord};
use serde_json::json;
use storage::{RecordMessageInput, Storage};
use tokio_tungstenite::connect_async;

use super::stub_driver::StubFaultMode;
use super::{
    default_stub_and_config, get_json_status, post_json_status, ws_next_json, CategoryBuilder,
    SuiteExecContext,
};
use crate::models::{CategoryReport, QaTier};

const STUB_GROUP_ID: i64 = 100_000_001;

/// 临时 `.runtime` 缓存测试目录守卫（Drop 时仅清理本用例创建目录，绝不误删外部运行态文件）
struct Wave6CacheCleanupGuard {
    remove_runtime: bool,
    remove_media: bool,
    remove_staging: bool,
    media_file: PathBuf,
    staging_subdir: PathBuf,
    staging_direct_file: PathBuf,
}

impl Drop for Wave6CacheCleanupGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.media_file);
        let _ = std::fs::remove_dir_all(&self.staging_subdir);
        let _ = std::fs::remove_file(&self.staging_direct_file);
        if self.remove_media {
            let _ = std::fs::remove_dir_all(".runtime/media");
        }
        if self.remove_staging {
            let _ = std::fs::remove_dir_all(".runtime/staging_uploads");
        }
        if self.remove_runtime {
            let _ = std::fs::remove_dir_all(".runtime");
        }
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|dur| dur.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or(1_700_000_000_000)
}

fn make_stub_msg(
    id: i64,
    session_name: &str,
    chat_type: ChatTargetKind,
    sender_name: &str,
    text: &str,
    captured_at_ms: u64,
) -> StoredMessageRecord {
    StoredMessageRecord {
        onebot_message_id: id,
        message_uid: format!("w6-qa-{id}"),
        group_name: session_name.to_string(),
        group_id: if chat_type == ChatTargetKind::Private {
            0
        } else {
            STUB_GROUP_ID
        },
        user_id: if chat_type == ChatTargetKind::Private {
            200_000_002
        } else {
            20_001
        },
        sender_name: sender_name.to_string(),
        raw_message: text.to_string(),
        chat_type,
        segments: vec![MessageSegment::Text {
            text: text.to_string(),
        }],
        is_self: false,
        timestamp_hint: Some("14:00".to_string()),
        captured_at_ms,
    }
}

/// 等待其他并发单元测试中的 `qa-v4-*` 临时暂存目录释放，避免跨套件清理误伤
async fn wait_for_other_staging_tests_idle() {
    let staging_root = Path::new(".runtime/staging_uploads");
    for _ in 0..160 {
        let has_v4_dir = std::fs::read_dir(staging_root)
            .ok()
            .into_iter()
            .flatten()
            .flatten()
            .any(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with("qa-v4-"))
            });
        if !has_v4_dir {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn verify_deep_history_contracts(ctx: &SuiteExecContext) -> Result<String> {
    // 1. 校验 SQLite Storage::query_messages_filtered 越过 200 条深度分页与 LIKE 通配符字面转义
    let mem_db = Storage::open(":memory:").context("open memory storage for deep history")?;
    let mut target_escape_id = 0_i64;
    for idx in 1..=250_i64 {
        let sender = if idx % 2 == 0 { "Alice" } else { "Bob" };
        let body = if idx == 42 {
            "literal_100%_needle".to_string()
        } else {
            format!("sqlite-deep-{idx}")
        };
        let inserted_id = mem_db.record_message(&RecordMessageInput {
            message_uid: format!("w6-db-{idx}"),
            group_name: DEFAULT_TARGET_GROUP_NAME.to_string(),
            group_id: STUB_GROUP_ID,
            user_id: 20_001,
            sender_name: sender.to_string(),
            raw_message: body,
            is_self: false,
            timestamp_hint: Some("14:05".to_string()),
            captured_at_ms: 1_700_000_000_000 + idx as u64,
            chat_type: ChatTargetKind::Group,
            segments: Vec::new(),
        })?;
        if idx == 42 {
            target_escape_id = inserted_id;
        }
    }
    let escaped_hits = mem_db.query_messages_filtered(&MessageHistoryFilter {
        session_name: format!("{DEFAULT_TARGET_GROUP_NAME} (9)"),
        chat_type: Some(ChatTargetKind::Group),
        keyword: Some("100%_needle".to_string()),
        limit: 10,
        ..Default::default()
    })?;
    ensure!(
        escaped_hits.len() == 1 && escaped_hits[0].onebot_message_id == target_escape_id,
        "LIKE wildcard escaping failed: {escaped_hits:?}"
    );

    // 2. 校验 OneBot get_group/friend_msg_history 与 REST /api/messages/history 深分页和 message_seq=0
    let (driver, cfg) = default_stub_and_config();
    for idx in 1..=260_i64 {
        let sender = if idx % 2 == 0 { "Alice" } else { "Bob" };
        driver.insert_stored_message(make_stub_msg(
            20_000 + idx,
            DEFAULT_TARGET_GROUP_NAME,
            ChatTargetKind::Group,
            sender,
            &format!("w6-deep-msg-{idx}"),
            1_700_000_010_000 + idx as u64,
        ));
    }
    for idx in 1..=25_i64 {
        driver.insert_stored_message(make_stub_msg(
            21_000 + idx,
            "测试好友",
            ChatTargetKind::Private,
            "测试好友",
            &format!("w6-priv-msg-{idx}"),
            1_700_000_020_000 + idx as u64,
        ));
    }

    let (addr, srv_handle) = spawn_server(Arc::clone(&driver) as Arc<_>, cfg).await?;
    let base = format!("http://{addr}");

    let (latest_code, latest_body) = post_json_status(
        &ctx.http_client,
        &format!("{base}/onebot/v11/get_group_msg_history"),
        json!({ "group_id": STUB_GROUP_ID, "message_seq": 0, "count": 5 }),
    )
    .await?;
    ensure!(latest_code == 200, "get_group_msg_history seq=0 failed");
    let latest_ids: Vec<i64> = latest_body["data"]["messages"]
        .as_array()
        .context("missing messages array")?
        .iter()
        .filter_map(|v| v["message_id"].as_i64())
        .collect();
    ensure!(
        latest_ids == vec![20_256, 20_257, 20_258, 20_259, 20_260],
        "message_seq=0 did not return latest 5: {latest_ids:?}"
    );

    let (rev_code, rev_body) = post_json_status(
        &ctx.http_client,
        &format!("{base}/onebot/v11/get_group_msg_history"),
        json!({
            "group_id": STUB_GROUP_ID,
            "message_seq": 20_030,
            "count": 4,
            "reverseOrder": true,
        }),
    )
    .await?;
    ensure!(rev_code == 200, "deep reverseOrder failed");
    let rev_ids: Vec<i64> = rev_body["data"]["messages"]
        .as_array()
        .context("missing reverse messages")?
        .iter()
        .filter_map(|v| v["message_id"].as_i64())
        .collect();
    ensure!(
        rev_ids == vec![20_030, 20_029, 20_028, 20_027],
        "deep reverseOrder mismatch: {rev_ids:?}"
    );

    let (priv_code, priv_body) = post_json_status(
        &ctx.http_client,
        &format!("{base}/onebot/v11/get_friend_msg_history"),
        json!({ "user_id": 200_000_002, "message_seq": 0, "count": 3, "reverse_order": true }),
    )
    .await?;
    ensure!(priv_code == 200, "get_friend_msg_history failed");
    let priv_ids: Vec<i64> = priv_body["data"]["messages"]
        .as_array()
        .context("missing private messages")?
        .iter()
        .filter_map(|v| v["message_id"].as_i64())
        .collect();
    ensure!(
        priv_ids == vec![21_025, 21_024, 21_023],
        "private reverse_order mismatch: {priv_ids:?}"
    );

    let (rest_code, rest_body) = get_json_status(
        &ctx.http_client,
        &format!(
            "{base}/api/messages/history?session=Demo%E6%B5%8B%E8%AF%95%E7%BE%A4&after_id=20020&before_id=20030&sender=alice&keyword=w6-deep-msg-2&limit=3&reverse_order=true"
        ),
    )
    .await?;
    ensure!(rest_code == 200, "REST history query failed: {rest_body}");
    let rest_ids: Vec<i64> = rest_body
        .as_array()
        .context("REST history must return array")?
        .iter()
        .filter_map(|v| v["onebot_message_id"].as_i64())
        .collect();
    ensure!(
        rest_ids == vec![20_028, 20_026, 20_024],
        "REST history cursor/filter mismatch: {rest_ids:?}"
    );

    srv_handle.abort();
    Ok(
        "260 条深分页、message_seq=0 兜底、reverseOrder 倒序、LIKE 转义及 REST 多条件过滤全通过"
            .to_string(),
    )
}

async fn verify_single_forward_contracts(ctx: &SuiteExecContext) -> Result<String> {
    let (driver, mut cfg) = default_stub_and_config();
    cfg.onebot.post_message_sent = true;
    driver.insert_stored_message(make_stub_msg(
        96_001,
        DEFAULT_TARGET_GROUP_NAME,
        ChatTargetKind::Group,
        "群友Alpha",
        "w6-single-forward-anchor",
        now_unix_ms(),
    ));
    driver.insert_stored_message(make_stub_msg(
        96_003,
        "陌生路人",
        ChatTargetKind::Private,
        "陌生路人",
        "w6-stranger-secret",
        now_unix_ms(),
    ));

    let (addr, srv_handle) = spawn_server(Arc::clone(&driver) as Arc<_>, cfg).await?;
    let base = format!("http://{addr}");

    let (mut ws, _) = connect_async(format!("ws://{addr}/onebot/v11/ws/event")).await?;
    let _lifecycle = ws_next_json(&mut ws, Duration::from_secs(2)).await?;

    let (grp_code, grp_body) = post_json_status(
        &ctx.http_client,
        &format!("{base}/onebot/v11/forward_group_single_msg"),
        json!({ "message_id": 96_001, "group_id": STUB_GROUP_ID }),
    )
    .await?;
    ensure!(
        grp_code == 200 && grp_body["retcode"] == 0,
        "forward_group_single_msg failed: {grp_code} {grp_body}"
    );
    let fwd_group_id = grp_body["data"]["message_id"]
        .as_i64()
        .context("missing forwarded group message_id")?;
    ensure!(
        fwd_group_id > 0 && fwd_group_id != 96_001,
        "forwarded message_id invalid: {fwd_group_id}"
    );

    let ws_sent = ws_next_json(&mut ws, Duration::from_secs(2)).await?;
    ensure!(
        ws_sent["post_type"] == "message_sent" && ws_sent["message_id"] == fwd_group_id,
        "single forward must emit enriched message_sent event: {ws_sent}"
    );

    let (priv_code, priv_body) = post_json_status(
        &ctx.http_client,
        &format!("{base}/onebot/v11/forward_friend_single_msg"),
        json!({ "message_id": 96_001, "nickname": "测试好友" }),
    )
    .await?;
    ensure!(
        priv_code == 200 && priv_body["retcode"] == 0,
        "forward_friend_single_msg failed: {priv_code} {priv_body}"
    );

    let (rest_code, rest_body) = post_json_status(
        &ctx.http_client,
        &format!("{base}/api/forward-msg"),
        json!({ "message_id": 96_001, "group": DEFAULT_TARGET_GROUP_NAME }),
    )
    .await?;
    ensure!(
        rest_code == 200 && rest_body["ok"] == true && rest_body["onebot_message_id"].is_i64(),
        "POST /api/forward-msg failed: {rest_code} {rest_body}"
    );

    // 非白名单私聊目标与非白名单源消息均应返回 403/1403
    let (deny_tgt_code, deny_tgt_body) = post_json_status(
        &ctx.http_client,
        &format!("{base}/onebot/v11/forward_friend_single_msg"),
        json!({ "message_id": 96_001, "nickname": "未授权路人" }),
    )
    .await?;
    ensure!(
        deny_tgt_code == 403 && deny_tgt_body["retcode"] == 1403,
        "unauthorized forward target must return 403/1403: {deny_tgt_code} {deny_tgt_body}"
    );

    let (deny_src_code, deny_src_body) = post_json_status(
        &ctx.http_client,
        &format!("{base}/onebot/v11/forward_group_single_msg"),
        json!({ "message_id": 96_003, "group_id": STUB_GROUP_ID }),
    )
    .await?;
    ensure!(
        deny_src_code == 403 && deny_src_body["retcode"] == 1403,
        "unauthorized private source must return 403/1403: {deny_src_code} {deny_src_body}"
    );

    // 过期截止期返回 408/1408；未知消息返 400/1400；未确认返回 500/1500；群成员超限返 403/1403
    let (exp_code, exp_body) = post_json_status(
        &ctx.http_client,
        &format!("{base}/api/forward-msg"),
        json!({
            "message_id": 96_001,
            "group": DEFAULT_TARGET_GROUP_NAME,
            "deadline_unix_ms": now_unix_ms().saturating_sub(2_000),
        }),
    )
    .await?;
    ensure!(
        exp_code == 408
            && exp_body["retcode"] == 1408
            && exp_body["code"] == "RESPONSE_WINDOW_EXPIRED",
        "expired forward deadline must return 408/1408: {exp_code} {exp_body}"
    );

    let (miss_code, miss_body) = post_json_status(
        &ctx.http_client,
        &format!("{base}/onebot/v11/forward_group_single_msg"),
        json!({ "message_id": 999_999_999, "group_id": STUB_GROUP_ID }),
    )
    .await?;
    ensure!(
        miss_code == 400 && miss_body["retcode"] == 1400,
        "missing forward source message must return 400/1400: {miss_code} {miss_body}"
    );

    driver.set_force_unconfirmed_send(true);
    let (unconf_code, unconf_body) = post_json_status(
        &ctx.http_client,
        &format!("{base}/onebot/v11/forward_group_single_msg"),
        json!({ "message_id": 96_001, "group_id": STUB_GROUP_ID }),
    )
    .await?;
    driver.set_force_unconfirmed_send(false);
    ensure!(
        unconf_code == 500 && unconf_body["retcode"] == 1500,
        "unconfirmed single forward must return 500/1500: {unconf_code} {unconf_body}"
    );

    driver.set_strict_member_guard(true);
    let (guard_code, guard_body) = post_json_status(
        &ctx.http_client,
        &format!("{base}/onebot/v11/forward_group_single_msg"),
        json!({ "message_id": 96_001, "group_id": STUB_GROUP_ID }),
    )
    .await?;
    driver.set_strict_member_guard(false);
    ensure!(
        guard_code == 403 && guard_body["retcode"] == 1403,
        "strict group member guard must return 403/1403: {guard_code} {guard_body}"
    );

    srv_handle.abort();
    Ok(
        "单条转发群/私聊/REST、WS MessageSent 派发、私聊白名单、群护栏及 408/400/500 守卫全通过"
            .to_string(),
    )
}

async fn verify_recent_contact_audit_and_cache_clean(ctx: &SuiteExecContext) -> Result<String> {
    let (driver, mut cfg) = default_stub_and_config();
    cfg.target.strict_below_members = Some(10);
    let base_ts = now_unix_ms();

    driver.insert_stored_message(make_stub_msg(
        96_101,
        "测试好友",
        ChatTargetKind::Private,
        "测试好友",
        "w6-allowed-friend-latest",
        base_ts + 200,
    ));
    driver.insert_stored_message(make_stub_msg(
        96_102,
        "陌生路人",
        ChatTargetKind::Private,
        "陌生路人",
        "w6-stranger-leak",
        base_ts + 300,
    ));
    driver.insert_stored_message(make_stub_msg(
        96_103,
        DEFAULT_TARGET_GROUP_NAME,
        ChatTargetKind::Group,
        "群友Beta",
        "w6-hoyo-latest",
        base_ts + 100,
    ));
    driver.insert_stored_message(make_stub_msg(
        96_104,
        "超大禁入群 (250)",
        ChatTargetKind::Group,
        "路人",
        "w6-big-group-leak",
        base_ts + 400,
    ));

    use common_types::BotDriver as _;
    driver
        .record_audit_event("private_whitelist_abort", r#"{"target":"秘密路人X"}"#)
        .await?;
    driver
        .record_audit_event(
            "message_recalled",
            r#"{"chat_type":"private","target":"秘密路人Y","message_id":96999}"#,
        )
        .await?;
    driver
        .record_audit_event(
            "async_action_failed",
            r#"{"action":"send_private_msg","error":"private chat target '秘密路人Z' is not allowed"}"#,
        )
        .await?;
    driver
        .record_audit_event(
            "message_recalled",
            r#"{"chat_type":"private","target":"测试好友","message_id":96101}"#,
        )
        .await?;
    driver
        .record_audit_event(
            "message_recalled",
            r#"{"chat_type":"group","target":"Demo测试群","message_id":96103}"#,
        )
        .await?;

    let (addr, srv_handle) = spawn_server(Arc::clone(&driver) as Arc<_>, cfg).await?;
    let base = format!("http://{addr}");

    let (rc_code, rc_body) = get_json_status(
        &ctx.http_client,
        &format!("{base}/onebot/v11/get_recent_contact?count=20"),
    )
    .await?;
    ensure!(rc_code == 200, "get_recent_contact failed: {rc_body}");
    let peers: Vec<&str> = rc_body["data"]
        .as_array()
        .context("recent contact data must be array")?
        .iter()
        .filter_map(|item| item["peer_name"].as_str())
        .collect();
    ensure!(
        peers == vec!["测试好友", DEFAULT_TARGET_GROUP_NAME],
        "recent contacts leaked or misordered: {peers:?}"
    );

    let (ob_audit_code, ob_audit_body) = get_json_status(
        &ctx.http_client,
        &format!("{base}/onebot/v11/.get_audit_events?limit=50"),
    )
    .await?;
    ensure!(ob_audit_code == 200, ".get_audit_events failed");
    let ob_audit_text = ob_audit_body["data"].to_string();
    ensure!(
        !ob_audit_text.contains("秘密路人")
            && ob_audit_text.contains("测试好友")
            && ob_audit_text.contains("Demo测试群"),
        "OneBot audit events leaked unauthorized private peer: {ob_audit_text}"
    );

    let (rest_audit_code, rest_audit_body) = get_json_status(
        &ctx.http_client,
        &format!("{base}/api/audit-events?event_kind=message_recalled&limit=10"),
    )
    .await?;
    ensure!(rest_audit_code == 200, "GET /api/audit-events failed");
    let rest_arr = rest_audit_body
        .as_array()
        .context("audit events response must be array")?;
    ensure!(
        rest_arr.len() == 2 && !rest_audit_body.to_string().contains("秘密路人"),
        "REST audit events filter/redaction mismatch: {rest_audit_body}"
    );

    // 使用进程独有文件名验证 `.runtime/media` 与 `.runtime/staging_uploads` 子目录全清且不伤及并发用例
    wait_for_other_staging_tests_idle().await;
    let pid = std::process::id();
    let media_dir = Path::new(".runtime/media");
    let staging_root = Path::new(".runtime/staging_uploads");
    let remove_runtime = !Path::new(".runtime").exists();
    let remove_media = !media_dir.exists();
    let remove_staging = !staging_root.exists();
    let media_file = media_dir.join(format!("wave6_qa_media_{pid}.png"));
    let staging_subdir = staging_root.join(format!("wave6-qa-{pid}"));
    let staging_direct_file = staging_root.join(format!("wave6_qa_direct_{pid}.bin"));
    let _cleanup_guard = Wave6CacheCleanupGuard {
        remove_runtime,
        remove_media,
        remove_staging,
        media_file: media_file.clone(),
        staging_subdir: staging_subdir.clone(),
        staging_direct_file: staging_direct_file.clone(),
    };

    std::fs::create_dir_all(media_dir)?;
    std::fs::create_dir_all(&staging_subdir)?;
    let nested_clip = staging_subdir.join(format!("wave6_qa_clip_{pid}.mp4"));
    std::fs::write(&media_file, b"w6png")?;
    std::fs::write(&nested_clip, b"w6videoclip")?;
    std::fs::write(&staging_direct_file, b"bin")?;

    let (clean_code, clean_body) = post_json_status(
        &ctx.http_client,
        &format!("{base}/api/cache/clean"),
        json!({}),
    )
    .await?;
    ensure!(
        clean_code == 200
            && clean_body["cleaned"] == true
            && clean_body["media_removed"].as_u64().unwrap_or(0) >= 1
            && clean_body["staging_removed"].as_u64().unwrap_or(0) >= 2
            && clean_body["freed_bytes"].as_u64().unwrap_or(0) >= 19,
        "POST /api/cache/clean summary mismatch: {clean_code} {clean_body}"
    );
    ensure!(
        media_dir.is_dir() && staging_root.is_dir(),
        "cache root dirs must remain after clean"
    );
    ensure!(
        !media_file.exists() && !staging_subdir.exists() && !staging_direct_file.exists(),
        "unique wave6 cache files or staging subdir still exist after clean"
    );

    let (ob_clean_code, ob_clean_body) = post_json_status(
        &ctx.http_client,
        &format!("{base}/onebot/v11/clean_cache"),
        json!({}),
    )
    .await?;
    ensure!(
        ob_clean_code == 200
            && ob_clean_body["retcode"] == 0
            && ob_clean_body["data"]["cleaned"] == true,
        "OneBot clean_cache failed: {ob_clean_code} {ob_clean_body}"
    );

    srv_handle.abort();
    Ok("get_recent_contact 越权过滤、审计私聊脱敏及 .runtime/media + staging_uploads 子目录全清通过".to_string())
}

async fn verify_security_breaker_reset_contracts(ctx: &SuiteExecContext) -> Result<String> {
    let (driver, cfg) = default_stub_and_config();
    let (addr, srv_handle) = spawn_server(Arc::clone(&driver) as Arc<_>, cfg).await?;
    let base = format!("http://{addr}");

    // 1. 风控弹窗存在时：get_status.good=false，且 POST /api/security/reset 与 .reset_security_breaker 均返 423/1423
    driver.set_fault_mode(StubFaultMode::SecurityBreaker);
    let (st_code, st_body) =
        get_json_status(&ctx.http_client, &format!("{base}/onebot/v11/get_status")).await?;
    ensure!(
        st_code == 200 && st_body["data"]["good"] == false,
        "expected good=false while tripped: {st_body}"
    );

    let (rest_deny_code, rest_deny_body) = post_json_status(
        &ctx.http_client,
        &format!("{base}/api/security/reset"),
        json!({}),
    )
    .await?;
    ensure!(
        rest_deny_code == 423
            && rest_deny_body["retcode"] == 1423
            && rest_deny_body["code"] == "SECURITY_PROMPT_DETECTED",
        "expected 423/1423 on REST security reset: {rest_deny_code} {rest_deny_body}"
    );

    let (ob_deny_code, ob_deny_body) = post_json_status(
        &ctx.http_client,
        &format!("{base}/onebot/v11/.reset_security_breaker"),
        json!({}),
    )
    .await?;
    ensure!(
        ob_deny_code == 423 && ob_deny_body["retcode"] == 1423,
        "expected 423/1423 on OneBot .reset_security_breaker: {ob_deny_code} {ob_deny_body}"
    );

    // 2. 关闭风控弹窗后：复位返回 200、security_breaker_tripped=false，审计记录同时含 denied 与 reset，发信恢复
    driver.set_fault_mode(StubFaultMode::Healthy);
    let (ok_code, ok_body) = post_json_status(
        &ctx.http_client,
        &format!("{base}/onebot/v11/.reset_security_breaker"),
        json!({}),
    )
    .await?;
    ensure!(
        ok_code == 200
            && ok_body["retcode"] == 0
            && ok_body["data"]["security_breaker_tripped"] == false,
        "expected 200 reset success: {ok_code} {ok_body}"
    );

    let (rest_ok_code, rest_ok_body) = post_json_status(
        &ctx.http_client,
        &format!("{base}/api/security/reset"),
        json!({}),
    )
    .await?;
    ensure!(
        rest_ok_code == 200 && rest_ok_body["security_breaker_tripped"] == false,
        "expected REST security reset 200: {rest_ok_code} {rest_ok_body}"
    );

    let (audit_code, audit_body) = get_json_status(
        &ctx.http_client,
        &format!("{base}/api/audit-events?limit=20"),
    )
    .await?;
    ensure!(audit_code == 200, "list audit events after reset failed");
    let kinds: Vec<&str> = audit_body
        .as_array()
        .context("audit events must be array")?
        .iter()
        .filter_map(|ev| ev["event_kind"].as_str())
        .collect();
    ensure!(
        kinds.contains(&"security_breaker_reset_denied")
            && kinds.contains(&"security_breaker_reset"),
        "expected denied and reset audit events: {kinds:?}"
    );

    let (send_code, send_body) = post_json_status(
        &ctx.http_client,
        &format!("{base}/onebot/v11/send_group_msg"),
        json!({ "group_id": STUB_GROUP_ID, "message": "w6-after-breaker-reset" }),
    )
    .await?;
    ensure!(
        send_code == 200 && send_body["retcode"] == 0,
        "send_group_msg after breaker reset failed: {send_code} {send_body}"
    );

    srv_handle.abort();
    Ok(
        "安全弹窗 423 拒收复位 -> 关窗 200 复位 -> denied/reset 审计留痕与发信恢复全通过"
            .to_string(),
    )
}

/// 执行第 13 类：Wave v6 协议与运维能力契约验证（L1）
pub async fn run_wave6_category(ctx: &SuiteExecContext) -> CategoryReport {
    let mut cat = CategoryBuilder::new(
        "CAT-13",
        "Wave v6 协议与运维契约验证",
        "Wave v6 Contracts (History / Single Forward / Contacts / Audit / Cache / Breaker Reset)",
        QaTier::L1,
        "深分页 seq=0/倒序、单条转发守卫、最近联系人/审计脱敏、子目录清缓存及 423->200 熔断复位全契约 100% 达标",
    );

    cat.run_case("deep_history_seq0_and_reverse_order_contracts", async {
        verify_deep_history_contracts(ctx).await
    })
    .await;

    cat.run_case(
        "single_forward_group_private_and_deadline_contracts",
        async { verify_single_forward_contracts(ctx).await },
    )
    .await;

    cat.run_case(
        "recent_contact_audit_and_full_cache_clean_contracts",
        async { verify_recent_contact_audit_and_cache_clean(ctx).await },
    )
    .await;

    cat.run_case(
        "security_breaker_reset_denied_and_recovery_contracts",
        async { verify_security_breaker_reset_contracts(ctx).await },
    )
    .await;

    cat.set_metric("wave6_contract_cases_checked", 4.0);
    cat.build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn wave6_contract_suite_passes_all_cases() {
        let ctx = SuiteExecContext::new("http://127.0.0.1:18770".to_string(), true)
            .expect("build suite context");
        let report = run_wave6_category(&ctx).await;
        assert!(
            report.passed && report.failed_cases == 0 && report.passed_cases == 4,
            "wave6_suite failed: {report:#?}"
        );
    }
}
