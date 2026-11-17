//! 13 项行业标准微基准与全栈基准套件（覆盖 ID 散列、CQ 编解码、滑窗追踪、通知正则、HMAC、WIC 图像、SQLite WAL、HTTP RPC、多群洪峰调度、扩展协议回放及深分页/原生引用转发提取）

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use common_types::constants::DEFAULT_ONEBOT_ID_BOUND;
use common_types::{
    parse_cq_string_to_segments, segments_to_cq_string, stable_onebot_id, AdapterConfig,
    AdapterStatus, BotDriver, BotLoginInfo, ChatMessage, ChatTargetKind, GroupMemberInfo,
    GroupTargetSpec, MessageHistoryFilter, MessageSegment, SchedulerConfig, SendOutcome,
    SendRequest, ServerConfig, SessionSummary, UiNodeSnapshot, WindowInfo,
};
use parking_lot::Mutex;
use qqnt_driver::driver::enrich_parsed_messages_with_storage;
use qqnt_driver::parser::{
    build_and_enrich_notice_events, extract_rich_messages_from_tree,
    parse_notice_text_to_onebot_json, MessageTracker, NoticeContext, ParsedSystemNotice,
};
use qqnt_driver::scheduler::FloodScheduler;
use reqwest::Client;
use serde_json::{json, Value};
use storage::{RecordMessageInput, Storage, UpsertGroupInput};
use tokio::sync::broadcast;
use uia_core::{decode_image_bytes_to_bgra, encode_bgra_to_png_bytes, DecodedImageBgra};

use super::stats_engine::{run_async_benchmark, run_sync_benchmark, BenchmarkMeta};
use crate::constants::{BENCH_IMAGE_DIMENSION, HTTP_POOL_MAX_IDLE_PER_HOST};
use crate::models::BenchmarkResult;

/// 临时 SQLite WAL 测试数据库文件生命周期守卫（Drop 时自动清理主库与 -wal/-shm 附属文件）
struct BenchTempDbGuard {
    db_path: PathBuf,
}

impl BenchTempDbGuard {
    fn new(tag: &str) -> Self {
        let mut path = std::env::temp_dir();
        path.push(format!("qq_qa_bench_{tag}_{}.db", std::process::id()));
        Self::remove_files(&path);
        Self { db_path: path }
    }

    fn path_str(&self) -> String {
        self.db_path.to_string_lossy().into_owned()
    }

    fn remove_files(path: &std::path::Path) {
        let raw = path.to_string_lossy();
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(format!("{raw}-wal"));
        let _ = std::fs::remove_file(format!("{raw}-shm"));
    }
}

impl Drop for BenchTempDbGuard {
    fn drop(&mut self) {
        Self::remove_files(&self.db_path);
    }
}

/// 专用于基准闭环测量 HTTP/Axum 框架开销的极简零阻塞驱动桩
struct BenchLoopbackDriver {
    events: broadcast::Sender<common_types::AdapterEvent>,
}

impl BenchLoopbackDriver {
    fn new() -> Self {
        let (events, _) = broadcast::channel(64);
        Self { events }
    }
}

#[async_trait::async_trait]
impl BotDriver for BenchLoopbackDriver {
    async fn status(&self) -> common_types::Result<AdapterStatus> {
        Ok(AdapterStatus::default())
    }
    async fn list_windows(&self) -> common_types::Result<Vec<WindowInfo>> {
        Ok(Vec::new())
    }
    async fn inspect_tree(
        &self,
        _hwnd: Option<isize>,
        _max_depth: usize,
    ) -> common_types::Result<UiNodeSnapshot> {
        Ok(UiNodeSnapshot::default())
    }
    async fn list_sessions(&self) -> common_types::Result<Vec<SessionSummary>> {
        Ok(Vec::new())
    }
    async fn open_group(&self, _spec: GroupTargetSpec) -> common_types::Result<AdapterStatus> {
        Ok(AdapterStatus::default())
    }
    async fn poll_messages(
        &self,
        _group: &str,
        _limit: usize,
    ) -> common_types::Result<Vec<ChatMessage>> {
        Ok(Vec::new())
    }
    async fn send_group_message(&self, req: SendRequest) -> common_types::Result<SendOutcome> {
        Ok(SendOutcome {
            ok: true,
            group: req.group,
            ..Default::default()
        })
    }
    fn subscribe_events(&self) -> broadcast::Receiver<common_types::AdapterEvent> {
        self.events.subscribe()
    }
}

macro_rules! push_sync_benches {
    (
        $results:expr,
        $(
            {
                id: $id:expr,
                title_zh: $zh:expr,
                title_en: $en:expr,
                subsystem: $sub:expr,
                bytes: $bytes:expr,
                run: |$seq:ident| $body:block
            }
        ),* $(,)?
    ) => {
        $(
            $results.push(run_sync_benchmark(
                BenchmarkMeta {
                    id: $id,
                    title_zh: $zh,
                    title_en: $en,
                    subsystem: $sub,
                    unit_bytes: $bytes,
                },
                |$seq| $body,
            ));
        )*
    };
}

fn build_synthetic_bgra_frame(dim: u32) -> DecodedImageBgra {
    let total = (dim as usize) * (dim as usize) * 4;
    let mut bgra = vec![0_u8; total];
    for y in 0..dim {
        for x in 0..dim {
            let offset = ((y * dim + x) * 4) as usize;
            bgra[offset] = ((x * 2) & 0xFF) as u8;
            bgra[offset + 1] = ((y * 2) & 0xFF) as u8;
            bgra[offset + 2] = (((x ^ y) * 3) & 0xFF) as u8;
            bgra[offset + 3] = 255;
        }
    }
    DecodedImageBgra {
        width: dim,
        height: dim,
        bgra_top_down: bgra,
    }
}

/// 按 Criterion 统计学方法执行全套 13 项核心子系统基准测试并返回精确测量指标（Macro Codegen）
pub async fn run_all_benchmarks() -> Result<Vec<BenchmarkResult>> {
    let mut results = Vec::with_capacity(13);

    let cq_sample = concat!(
        "基准负载[CQ:at,qq=10001,name=群友Alpha] ",
        "[CQ:face,id=178] [CQ:reply,id=4096] ",
        "实体转义验证 &amp;&#91;ok&#93; [CQ:image,file=bench_frame.png]"
    );
    let segments_sample = parse_cq_string_to_segments(cq_sample, false);
    let serialize_bytes = cq_sample.len();

    let poll_window: Vec<ChatMessage> = (0..50)
        .map(|idx| ChatMessage {
            message_id: format!("bm-poll-msg-{idx:04}"),
            group_name: "Demo测试群".to_string(),
            sender: format!("群友{:02}", idx % 8),
            content: format!("滑窗消息基准样本 #{idx}"),
            timestamp_hint: Some("14:20".to_string()),
            is_self: idx == 49,
            captured_at_ms: 1_700_000_000_000 + idx,
            target_kind: ChatTargetKind::Group,
            ..Default::default()
        })
        .collect();
    let tracker = Arc::new(Mutex::new(MessageTracker::new(vec![
        "测试机器人".to_string()
    ])));

    let members_ctx = [GroupMemberInfo {
        group_id: 100_000_001,
        user_id: 10_001,
        nickname: "群友Alpha".to_string(),
        card: "群友Alpha".to_string(),
        role: "member".to_string(),
        ..Default::default()
    }];
    let notice_ctx = NoticeContext {
        self_id: 10001,
        chat_kind: ChatTargetKind::Group,
        session_name: "Demo测试群",
        group_id: Some(100_000_001),
        peer_user_id: None,
        members: &members_ctx,
    };
    let notice_templates = [
        "\"群友Alpha\" 撤回了一条消息",
        "\"群友Alpha\" 拍了拍 \"你\"",
        "\"新成员Beta\" 加入了群聊",
        "管理员禁言了 \"群友Alpha\" 10分钟",
    ];

    let webhook_payload = json!({
        "time": 1_700_000_123_i64,
        "self_id": 10001_i64,
        "post_type": "message",
        "message_type": "group",
        "sub_type": "normal",
        "message_id": 88421_i64,
        "group_id": 100_000_001_i64,
        "user_id": 200_000_002_i64,
        "raw_message": "X".repeat(768),
        "sender": { "user_id": 200_000_002_i64, "nickname": "测试好友A", "role": "member" }
    })
    .to_string();

    let bgra_frame = build_synthetic_bgra_frame(BENCH_IMAGE_DIMENSION);
    let bgra_frame_bytes = bgra_frame.bgra_top_down.len();

    let wal_db_guard = BenchTempDbGuard::new("wal_roundtrip");
    let wal_storage = Storage::open(&wal_db_guard.path_str())?;
    let batch_db_guard = BenchTempDbGuard::new("wal_batch");
    let batch_storage = Storage::open(&batch_db_guard.path_str())?;
    let group_batch_template: Vec<UpsertGroupInput> = (0..20)
        .map(|idx| UpsertGroupInput {
            group_name: format!("基准测试群-{idx:02}"),
            real_group_id: Some(684_000_000 + idx),
            member_count: Some(9),
            max_member_count: Some(200),
            last_preview: Some("最近一条群聊预览文本".to_string()),
            unread_count: idx as u32 % 4,
        })
        .collect();

    push_sync_benches! {
        results,
        {
            id: "BM-01",
            title_zh: "FNV-1a OneBot 确定性 ID 散列",
            title_en: "stable_onebot_id FNV-1a Hasher",
            subsystem: "common-types",
            bytes: 64,
            run: |seq| {
                let key = format!("group:Demo测试群:member:user-{:016x}:slot", seq);
                stable_onebot_id(&key, DEFAULT_ONEBOT_ID_BOUND)
            }
        },
        {
            id: "BM-02",
            title_zh: "CQ 字符串到结构化消息段解析",
            title_en: "CQ String -> MessageSegment Parser",
            subsystem: "common-types",
            bytes: cq_sample.len(),
            run: |_seq| {
                parse_cq_string_to_segments(cq_sample, false).len()
            }
        },
        {
            id: "BM-03",
            title_zh: "结构化消息段到 CQ 实体转义序列化",
            title_en: "MessageSegment -> CQ String Serializer",
            subsystem: "common-types",
            bytes: serialize_bytes,
            run: |_seq| {
                segments_to_cq_string(&segments_sample).len()
            }
        },
        {
            id: "BM-04",
            title_zh: "50 条消息滑窗去重与自发回显对齐",
            title_en: "MessageTracker::observe_poll (50-row window)",
            subsystem: "qqnt-driver",
            bytes: 50 * 64,
            run: |seq| {
                let mut batch = poll_window.clone();
                if let Some(last) = batch.last_mut() {
                    last.message_id = format!("bm-poll-dynamic-{seq}");
                }
                let mut guard = tracker.lock();
                let (all, incoming) = guard.observe_poll(batch);
                all.len() + incoming.len()
            }
        },
        {
            id: "BM-05",
            title_zh: "系统提示横幅正则 Notice 语义引擎",
            title_en: "parse_notice_text_to_onebot_json Regex Engine",
            subsystem: "qqnt-driver",
            bytes: 48,
            run: |seq| {
                let text = notice_templates[(seq as usize) % notice_templates.len()];
                parse_notice_text_to_onebot_json(&notice_ctx, text, 1_700_000_000_i64 + (seq as i64))
                    .to_string()
                    .len()
            }
        },
        {
            id: "BM-06",
            title_zh: "Webhook RFC2104 HMAC-SHA1 签名计算",
            title_en: "hmac_sha1_hex Webhook Signature (1 KiB)",
            subsystem: "adapter-server",
            bytes: webhook_payload.len(),
            run: |_seq| {
                adapter_server::onebot::hmac_sha1_hex(b"qa-webhook-bench-secret", webhook_payload.as_bytes())
                    .len()
            }
        },
        {
            id: "BM-07",
            title_zh: "原生 WIC 128x128 BGRA <-> PNG 硬件双向编解码",
            title_en: "WIC Native PNG Encode + Decode (128x128 BGRA)",
            subsystem: "uia-core",
            bytes: bgra_frame_bytes,
            run: |_seq| {
                let png = encode_bgra_to_png_bytes(&bgra_frame).expect("encode benchmark png");
                let decoded = decode_image_bytes_to_bgra(&png).expect("decode benchmark png");
                decoded.bgra_top_down.len()
            }
        },
        {
            id: "BM-08",
            title_zh: "SQLite WAL 单条消息写入与按 ID 读取往返",
            title_en: "SQLite WAL record_message + get_message_by_id",
            subsystem: "storage",
            bytes: 128,
            run: |seq| {
                let msg_id = wal_storage
                    .record_message(&RecordMessageInput {
                        message_uid: format!("bm-uid-{seq}"),
                        group_name: "Demo测试群".to_string(),
                        group_id: 100_000_001,
                        user_id: 200_000_002,
                        sender_name: "测试好友A".to_string(),
                        raw_message: "SQLite WAL 单条读写往返基准测试载荷".to_string(),
                        is_self: false,
                        timestamp_hint: Some("14:25".to_string()),
                        captured_at_ms: 1_700_000_000_000 + seq,
                        chat_type: ChatTargetKind::Group,
                        segments: vec![MessageSegment::Text {
                            text: "SQLite WAL 读写基准".to_string(),
                        }],
                    })
                    .expect("record wal message");
                wal_storage
                    .get_message_by_id(msg_id)
                    .expect("read wal message")
                    .map(|rec| rec.onebot_message_id)
                    .unwrap_or(0)
            }
        },
        {
            id: "BM-09",
            title_zh: "SQLite WAL 20 群组事务批量 Upsert 吞吐",
            title_en: "SQLite WAL upsert_groups_batch (20 groups/tx)",
            subsystem: "storage",
            bytes: 20 * 64,
            run: |seq| {
                let mut items = group_batch_template.clone();
                if let Some(first) = items.first_mut() {
                    first.unread_count = (seq % 100) as u32;
                }
                batch_storage
                    .upsert_groups_batch(&items)
                    .expect("batch upsert groups");
                items.len()
            }
        },
    }

    // BM-10：独立隔离回环端口测量全栈 Axum HTTP OneBot JSON RPC 延迟与吞吐
    let bench_driver = Arc::new(BenchLoopbackDriver::new());
    let bench_cfg = AdapterConfig {
        server: ServerConfig {
            host: "127.0.0.1".to_string(),
            port: 0,
            access_token: None,
        },
        ..Default::default()
    };
    let (addr, server_handle) = adapter_server::spawn_server(bench_driver, bench_cfg).await?;
    let client = Client::builder()
        .no_proxy()
        .pool_max_idle_per_host(HTTP_POOL_MAX_IDLE_PER_HOST)
        .timeout(Duration::from_secs(5))
        .build()?;
    let rpc_url = format!("http://{addr}/onebot/v11/get_version_info");

    let bm10 = run_async_benchmark(
        BenchmarkMeta {
            id: "BM-10",
            title_zh: "Axum HTTP 环回 OneBot get_version_info 全栈 RPC",
            title_en: "Axum HTTP Loopback /onebot/v11/get_version_info",
            subsystem: "adapter-server",
            unit_bytes: 160,
        },
        |_| {
            let client = client.clone();
            let rpc_url = rpc_url.clone();
            async move {
                let resp = client
                    .post(&rpc_url)
                    .json(&json!({}))
                    .send()
                    .await
                    .expect("benchmark loopback http post");
                let status = resp.status();
                let payload: Value = resp.json().await.expect("parse version json");
                (status.as_u16(), payload["retcode"].as_i64())
            }
        },
    )
    .await;
    results.push(bm10);
    server_handle.abort();

    // BM-11：三超大群并发洪峰下 FloodScheduler::ingest_and_select 分级限流与多群响应窗选择器统计基准
    let bench_groups = ["大群Alpha", "大群Beta", "大群Gamma"];
    let flood_batch_template: Vec<ChatMessage> = (0..90_usize)
        .map(|idx| {
            let group_name = bench_groups[idx % bench_groups.len()].to_string();
            let (content, segments) = if idx % 15 == 0 {
                let text = format!("@qq-bot 紧急指令 #{idx}");
                (
                    text,
                    vec![
                        MessageSegment::At {
                            qq: "2888001".to_string(),
                            name: Some("qq-bot".to_string()),
                        },
                        MessageSegment::Text {
                            text: format!(" 紧急指令 #{idx}"),
                        },
                    ],
                )
            } else if idx % 15 == 5 {
                let text = format!("@全体成员 公告轮播 #{idx}");
                (text.clone(), vec![MessageSegment::Text { text }])
            } else {
                let text = format!("多群并发水群刷屏负载消息 #{idx}");
                (text.clone(), vec![MessageSegment::Text { text }])
            };
            ChatMessage {
                message_id: format!("bm11-msg-{idx:04}"),
                group_name,
                sender: format!("群友{:02}", idx % 12),
                content,
                timestamp_hint: Some("12:05".to_string()),
                is_self: false,
                captured_at_ms: 1_700_000_000_000,
                group_id: Some(684_289_000 + (idx % 3) as i64),
                user_id: Some(20_000 + (idx % 12) as i64),
                target_kind: ChatTargetKind::Group,
                segments,
                ..Default::default()
            }
        })
        .collect();
    let flood_batch_bytes: usize = flood_batch_template
        .iter()
        .map(|m| m.content.len() + m.group_name.len() + m.sender.len())
        .sum();
    let self_nicknames = vec!["qq-bot".to_string(), "测试机器人".to_string()];
    let flood_scheduler = Arc::new(Mutex::new(FloodScheduler::new(SchedulerConfig::default())));

    let bm11 = run_sync_benchmark(
        BenchmarkMeta {
            id: "BM-11",
            title_zh: "FloodScheduler 多群并发洪峰分级与响应窗口选择",
            title_en: "FloodScheduler::ingest_and_select Multi-Group Burst",
            subsystem: "qqnt-driver",
            unit_bytes: flood_batch_bytes,
        },
        |seq| {
            let now_ms = 1_700_000_000_000 + (seq * 120);
            let mut batch = flood_batch_template.clone();
            for item in &mut batch {
                item.captured_at_ms = now_ms;
            }
            flood_scheduler
                .lock()
                .ingest_and_select(now_ms, batch, 2_888_001, &self_nicknames)
                .len()
        },
    );
    results.push(bm11);

    // BM-12：第 4 阶段扩展通知分类 + 折行气泡回放与被动撤回 UID 差分基准
    let bm12_tree_raw =
        include_str!("../../../../crates/qqnt-driver/tests/fixtures/public/04_tree.json");
    let bm12_tree: UiNodeSnapshot = serde_json::from_str(bm12_tree_raw)?;
    let bm12_members = [
        GroupMemberInfo {
            group_id: 999_888,
            user_id: 222_001,
            real_user_id: Some(222_001),
            nickname: "群友甲".to_string(),
            card: String::new(),
            role: "member".to_string(),
        },
        GroupMemberInfo {
            group_id: 999_888,
            user_id: 222_002,
            real_user_id: Some(222_002),
            nickname: "群主乙".to_string(),
            card: String::new(),
            role: "owner".to_string(),
        },
    ];
    let bm12_group_ctx = NoticeContext {
        self_id: 10_001,
        chat_kind: ChatTargetKind::Group,
        session_name: "Demo测试群",
        group_id: Some(999_888),
        peer_user_id: None,
        members: &bm12_members,
    };
    let bm12_priv_ctx = NoticeContext {
        self_id: 10_001,
        chat_kind: ChatTargetKind::Private,
        session_name: "新好友",
        group_id: None,
        peer_user_id: Some(778_899),
        members: &[],
    };
    let bm12_group_notices = [
        "群友甲 回应了你的 消息:",
        "群友甲 被设为管理员",
        "群友甲 被取消管理员",
        "群主乙 将 群友甲 的消息设为精华消息",
        "群主乙 将一条消息移出了精华消息",
        "群友甲 领了 群主乙 的红包，获得了手气王",
        "群主乙 开启了全员禁言",
        "群主乙 关闭了全员禁言",
    ];
    let bm12_friend_notice = "你们已经成为好友，现在可以开始聊天了";
    let bm12_unit_bytes = bm12_tree_raw.len()
        + bm12_group_notices.iter().map(|s| s.len()).sum::<usize>()
        + bm12_friend_notice.len();
    let bm12_storage = Storage::open(":memory:")?;
    let bm12_tracker = Arc::new(Mutex::new(MessageTracker::new(vec![
        "测试机器人".to_string()
    ])));
    let bm12_uids_full = vec![
        "uid-bm12-1".to_string(),
        "uid-bm12-2".to_string(),
        "uid-bm12-3".to_string(),
    ];
    let bm12_uids_recalled = vec!["uid-bm12-1".to_string(), "uid-bm12-3".to_string()];
    let bm12_recall_input = vec![ParsedSystemNotice {
        notice_id: "bm12-recall".to_string(),
        text: "群友甲 撤回了一条消息".to_string(),
    }];

    let bm12 = run_sync_benchmark(
        BenchmarkMeta {
            id: "BM-12",
            title_zh: "扩展通知分类、折行文件回放与被动撤回 UID 差分",
            title_en: "Expanded Notices, Wrapped File Replay & Passive Recall UID Diff",
            subsystem: "qqnt-driver",
            unit_bytes: bm12_unit_bytes,
        },
        |seq| {
            let mut count = 0usize;
            for raw_notice in bm12_group_notices {
                let parsed = parse_notice_text_to_onebot_json(
                    &bm12_group_ctx,
                    raw_notice,
                    1_700_000_000 + seq as i64,
                );
                if parsed.get("notice_type").is_some() {
                    count += 1;
                }
            }
            let friend_ev = parse_notice_text_to_onebot_json(
                &bm12_priv_ctx,
                bm12_friend_notice,
                1_700_000_000 + seq as i64,
            );
            if friend_ev.get("notice_type").is_some() {
                count += 1;
            }
            let rich =
                extract_rich_messages_from_tree(&bm12_tree, bm12_tree.rect, false, "Demo测试群");
            count += rich.len();
            let current_uids = if seq % 2 == 0 {
                &bm12_uids_full
            } else {
                &bm12_uids_recalled
            };
            let disappeared = bm12_tracker
                .lock()
                .diff_visible_uids("Demo测试群", current_uids);
            let enriched = build_and_enrich_notice_events(
                &bm12_storage,
                &bm12_group_ctx,
                &bm12_recall_input,
                &disappeared,
                1_700_000_000 + seq as i64,
            );
            count + disappeared.len() + enriched.len()
        },
    );
    results.push(bm12);

    // BM-13：深分页过滤检索 + 03_tree/06_tree 原生引用与合并转发卡提取 + get_or_allocate_forward_for_uid 幂等查找
    let bm13_tree03_raw =
        include_str!("../../../../crates/qqnt-driver/tests/fixtures/public/03_tree.json");
    let bm13_tree06_raw =
        include_str!("../../../../crates/qqnt-driver/tests/fixtures/public/06_tree.json");
    let bm13_tree03: UiNodeSnapshot = serde_json::from_str(bm13_tree03_raw)?;
    let bm13_tree06: UiNodeSnapshot = serde_json::from_str(bm13_tree06_raw)?;
    let bm13_storage = Storage::open(":memory:")?;
    for idx in 1..=240_i64 {
        let raw_message = if idx == 120 {
            "@测试好友A [LIVE-02-FACE-AT]".to_string()
        } else {
            format!("bm13-deep-msg-{idx}")
        };
        bm13_storage.record_message(&RecordMessageInput {
            message_uid: format!("bm13-seed-{idx}"),
            group_name: "Demo测试群".to_string(),
            group_id: 100_000_001,
            user_id: 200_000_003,
            sender_name: "测试机器人".to_string(),
            raw_message,
            is_self: true,
            timestamp_hint: Some("14:30".to_string()),
            captured_at_ms: 1_700_000_000_000 + idx as u64,
            chat_type: ChatTargetKind::Group,
            segments: Vec::new(),
        })?;
    }
    let bm13_login = BotLoginInfo {
        user_id: 200_000_003,
        nickname: "测试机器人".to_string(),
        real_qq_id: Some(200_000_003),
    };
    let bm13_fwd_json = r#"[{"type":"node","data":{"nickname":"测试机器人","content":[{"type":"text","data":{"text":"bm13-fwd"}}]}}]"#;
    let bm13_unit_bytes = bm13_tree03_raw.len() + bm13_tree06_raw.len() + 256;

    let bm13 = run_sync_benchmark(
        BenchmarkMeta {
            id: "BM-13",
            title_zh: "深分页过滤检索、03/06_tree 原生引用转发提取与幂等查表",
            title_en:
                "Deep History Filter, 03/06_tree Quote/Forward Extraction & Idempotent Lookup",
            subsystem: "storage + qqnt-driver",
            unit_bytes: bm13_unit_bytes,
        },
        |seq| {
            let page = bm13_storage
                .query_messages_filtered(&MessageHistoryFilter {
                    session_name: "Demo测试群".to_string(),
                    chat_type: Some(ChatTargetKind::Group),
                    before_or_at_id: Some(10_080 + (seq as i64 % 160)),
                    keyword: Some("bm13-deep-msg-".to_string()),
                    limit: 20,
                    reverse_order: true,
                    ..Default::default()
                })
                .expect("bm13 query_messages_filtered");
            let mut rich03 = extract_rich_messages_from_tree(
                &bm13_tree03,
                bm13_tree03.rect,
                false,
                "Demo测试群",
            );
            enrich_parsed_messages_with_storage(
                &bm13_storage,
                "Demo测试群",
                ChatTargetKind::Group,
                &bm13_login,
                &mut rich03,
            );
            let mut rich06 = extract_rich_messages_from_tree(
                &bm13_tree06,
                bm13_tree06.rect,
                false,
                "Demo测试群",
            );
            enrich_parsed_messages_with_storage(
                &bm13_storage,
                "Demo测试群",
                ChatTargetKind::Group,
                &bm13_login,
                &mut rich06,
            );
            let fwd_id = bm13_storage
                .get_or_allocate_forward_for_uid("bm13-idempotent-uid", bm13_fwd_json)
                .expect("bm13 idempotent forward lookup");
            page.len() + rich03.len() + rich06.len() + fwd_id.len()
        },
    );
    results.push(bm13);

    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bench_suite_executes_all_13_benchmarks_including_bm13() {
        // 验证 BM-01..BM-13 全套基准入口、04_tree 折行文件、03/06_tree 原生引用转发与 6 类通知解析均无异常
        let tree: UiNodeSnapshot = serde_json::from_str(include_str!(
            "../../../../crates/qqnt-driver/tests/fixtures/public/04_tree.json"
        ))
        .expect("parse 04_tree.json");
        let rich = extract_rich_messages_from_tree(&tree, tree.rect, false, "Demo测试群");
        assert!(
            rich.iter()
                .any(|item| item.message.content.contains("sample_live_verify.txt")),
            "BM-12 fixture must merge wrapped filename"
        );

        let all = run_all_benchmarks()
            .await
            .expect("run_all_benchmarks must succeed");
        assert_eq!(all.len(), 13);
        assert_eq!(all[11].id, "BM-12");
        assert_eq!(all[12].id, "BM-13");
        assert!(all[12].ops_per_sec > 0.0);
    }
}
