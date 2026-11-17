//! Wave v6 存储层专项测试：覆盖深分页过滤、LIKE 字面量转义、审计流水检索与入站合并转发 UID 幂等淘汰

use common_types::constants::DEFAULT_TARGET_GROUP_NAME;
use common_types::{ChatTargetKind, MessageHistoryFilter};

use super::*;
use crate::constants::{MAX_STORED_FORWARD_BUNDLES, MEMORY_DB_PATH};

fn make_record(
    uid: &str,
    session_name: &str,
    chat_type: ChatTargetKind,
    sender: &str,
    raw_message: &str,
    captured_at_ms: u64,
) -> RecordMessageInput {
    RecordMessageInput {
        message_uid: uid.to_string(),
        group_name: session_name.to_string(),
        group_id: if chat_type == ChatTargetKind::Group {
            100_000_001
        } else {
            0
        },
        user_id: 222,
        sender_name: sender.to_string(),
        raw_message: raw_message.to_string(),
        is_self: false,
        timestamp_hint: None,
        captured_at_ms,
        chat_type,
        segments: Vec::new(),
    }
}

#[test]
fn deep_pagination_cursors_and_reverse_order_work_beyond_250_messages() {
    let storage = Storage::open(MEMORY_DB_PATH).expect("open memory storage");
    let inputs: Vec<RecordMessageInput> = (1..=280)
        .map(|idx| {
            make_record(
                &format!("deep-uid-{idx}"),
                DEFAULT_TARGET_GROUP_NAME,
                ChatTargetKind::Group,
                "Alice",
                &format!("msg-{idx}"),
                1_700_000_000_000 + idx,
            )
        })
        .collect();
    let ids = storage
        .record_messages_batch(&inputs)
        .expect("batch insert 280 messages");
    assert_eq!(ids.len(), 280);
    assert_eq!(ids[0], 10_001);
    assert_eq!(ids[279], 10_280);

    // 默认查询取最新 50 条并按从旧到新顺序返回
    let latest = storage
        .query_messages_filtered(&MessageHistoryFilter {
            session_name: DEFAULT_TARGET_GROUP_NAME.to_string(),
            limit: 50,
            ..Default::default()
        })
        .expect("query latest 50");
    assert_eq!(latest.len(), 50);
    assert_eq!(latest.first().unwrap().onebot_message_id, 10_231);
    assert_eq!(latest.last().unwrap().onebot_message_id, 10_280);

    // 深分页向早先历史翻页：跨越 200 条阈值定位 10_031..=10_060
    let deep_page = storage
        .query_messages_filtered(&MessageHistoryFilter {
            session_name: format!("{DEFAULT_TARGET_GROUP_NAME} (9)"),
            chat_type: Some(ChatTargetKind::Group),
            before_or_at_id: Some(10_060),
            limit: 30,
            ..Default::default()
        })
        .expect("query deep page");
    assert_eq!(deep_page.len(), 30);
    assert_eq!(deep_page.first().unwrap().onebot_message_id, 10_031);
    assert_eq!(deep_page.last().unwrap().onebot_message_id, 10_060);

    // 显式要求 reverse_order 时应按编号从大到小倒序返回
    let deep_reversed = storage
        .query_messages_filtered(&MessageHistoryFilter {
            session_name: DEFAULT_TARGET_GROUP_NAME.to_string(),
            before_or_at_id: Some(10_060),
            limit: 5,
            reverse_order: true,
            ..Default::default()
        })
        .expect("query deep reversed");
    assert_eq!(
        deep_reversed
            .iter()
            .map(|r| r.onebot_message_id)
            .collect::<Vec<_>>(),
        vec![10_060, 10_059, 10_058, 10_057, 10_056]
    );

    // 仅传 after_id 时取紧邻该游标之后最早的 limit 条增量消息
    let after_inc = storage
        .query_messages_filtered(&MessageHistoryFilter {
            session_name: DEFAULT_TARGET_GROUP_NAME.to_string(),
            after_id: Some(10_020),
            limit: 25,
            reverse_order: false,
            ..Default::default()
        })
        .expect("query after_id asc");
    assert_eq!(after_inc.len(), 25);
    assert_eq!(after_inc.first().unwrap().onebot_message_id, 10_021);
    assert_eq!(after_inc.last().unwrap().onebot_message_id, 10_045);

    let after_rev = storage
        .query_messages_filtered(&MessageHistoryFilter {
            session_name: DEFAULT_TARGET_GROUP_NAME.to_string(),
            after_id: Some(10_020),
            limit: 4,
            reverse_order: true,
            ..Default::default()
        })
        .expect("query after_id rev");
    assert_eq!(
        after_rev
            .iter()
            .map(|r| r.onebot_message_id)
            .collect::<Vec<_>>(),
        vec![10_024, 10_023, 10_022, 10_021]
    );

    // 同时指定上下界时取区间内最新记录并校验 limit 的 [1, 200] 夹紧边界
    let clamped_min = storage
        .query_messages_filtered(&MessageHistoryFilter {
            session_name: DEFAULT_TARGET_GROUP_NAME.to_string(),
            after_id: Some(10_020),
            before_or_at_id: Some(10_100),
            limit: 0,
            ..Default::default()
        })
        .expect("query clamped min");
    assert_eq!(clamped_min.len(), 1);
    assert_eq!(clamped_min[0].onebot_message_id, 10_100);

    let clamped_max = storage
        .query_messages_filtered(&MessageHistoryFilter {
            session_name: DEFAULT_TARGET_GROUP_NAME.to_string(),
            limit: 500,
            ..Default::default()
        })
        .expect("query clamped max");
    assert_eq!(clamped_max.len(), 200);
    assert_eq!(clamped_max.first().unwrap().onebot_message_id, 10_081);
    assert_eq!(clamped_max.last().unwrap().onebot_message_id, 10_280);
}

#[test]
fn like_escape_literals_case_folding_and_chat_type_isolation_work() {
    let storage = Storage::open(MEMORY_DB_PATH).expect("open memory storage");
    let fixtures = vec![
        make_record(
            "esc-1",
            "SharedName",
            ChatTargetKind::Group,
            "Alice_Dev",
            "progress 100% done",
            1_700_000_000_001,
        ),
        make_record(
            "esc-2",
            "SharedName",
            ChatTargetKind::Group,
            "AliceXDev",
            "progress 1000 done",
            1_700_000_000_002,
        ),
        make_record(
            "esc-3",
            "SharedName",
            ChatTargetKind::Group,
            "Bob",
            r"C:\path\to\file_name",
            1_700_000_000_003,
        ),
        make_record(
            "esc-4",
            "SharedName",
            ChatTargetKind::Group,
            "Bob",
            "C:/path/to/fileXname",
            1_700_000_000_004,
        ),
        make_record(
            "esc-5",
            "SharedName",
            ChatTargetKind::Group,
            "Élodie",
            "CAFÉ_MENU",
            1_700_000_000_005,
        ),
        make_record(
            "esc-6",
            "SharedName",
            ChatTargetKind::Private,
            "Alice_Dev",
            "progress 100% done",
            1_700_000_000_006,
        ),
    ];
    storage
        .record_messages_batch(&fixtures)
        .expect("insert escape fixtures");

    // 校验百分号按字面量匹配而非通配符，且隔离同名群聊与私聊
    let percent_hits = storage
        .query_messages_filtered(&MessageHistoryFilter {
            session_name: "SharedName".to_string(),
            chat_type: Some(ChatTargetKind::Group),
            sender: Some("alice_d".to_string()),
            keyword: Some("100%".to_string()),
            limit: 20,
            ..Default::default()
        })
        .expect("query literal percent");
    assert_eq!(percent_hits.len(), 1);
    assert_eq!(percent_hits[0].message_uid, "esc-1");

    // 校验反斜杠与下划线均按字面量转义匹配
    let slash_hits = storage
        .query_messages_filtered(&MessageHistoryFilter {
            session_name: "SharedName".to_string(),
            keyword: Some(r"\to\file_n".to_string()),
            limit: 20,
            ..Default::default()
        })
        .expect("query literal backslash and underscore");
    assert_eq!(slash_hits.len(), 1);
    assert_eq!(slash_hits[0].message_uid, "esc-3");

    // 校验非 ASCII 带大小写重音字符与跨会话空 session_name 过滤
    let unicode_hits = storage
        .query_messages_filtered(&MessageHistoryFilter {
            session_name: String::new(),
            sender: Some("élodie".to_string()),
            keyword: Some("café_".to_string()),
            limit: 20,
            ..Default::default()
        })
        .expect("query non-ascii case folding");
    assert_eq!(unicode_hits.len(), 1);
    assert_eq!(unicode_hits[0].message_uid, "esc-5");
}

#[test]
fn list_audit_events_filters_orders_and_clamps_limit() {
    let storage = Storage::open(MEMORY_DB_PATH).expect("open memory storage");
    for idx in 1..=220 {
        let kind = if idx % 2 == 0 {
            "security_breaker"
        } else {
            "session_pollution_purged"
        };
        storage
            .append_audit(kind, &format!("{{\"seq\":{idx}}}"))
            .expect("append audit event");
    }

    let all_clamped = storage
        .list_audit_events(None, 500)
        .expect("list all clamped");
    assert_eq!(all_clamped.len(), 200);
    assert!(all_clamped[0].id > all_clamped[1].id);
    assert_eq!(all_clamped[0].detail_json, r#"{"seq":220}"#);

    let filtered = storage
        .list_audit_events(Some("security_breaker"), 5)
        .expect("list filtered security_breaker");
    assert_eq!(filtered.len(), 5);
    assert!(filtered.iter().all(|e| e.event_kind == "security_breaker"));
    assert_eq!(filtered[0].detail_json, r#"{"seq":220}"#);
    assert_eq!(filtered[4].detail_json, r#"{"seq":212}"#);

    let min_clamped = storage
        .list_audit_events(Some("session_pollution_purged"), 0)
        .expect("list min clamped");
    assert_eq!(min_clamped.len(), 1);
    assert_eq!(min_clamped[0].detail_json, r#"{"seq":219}"#);
}

#[test]
fn get_or_allocate_forward_for_uid_is_idempotent_and_bounded() {
    let storage = Storage::open(MEMORY_DB_PATH).expect("open memory storage");

    // 同一 UID 重复调用应直接复用已有 fwd-* 编号，不空耗自增序列
    let first_id = storage
        .get_or_allocate_forward_for_uid("msg-uid-alpha", r#"[{"id":"n1"}]"#)
        .expect("allocate first forward");
    assert_eq!(first_id, "fwd-10001");

    let reused_id = storage
        .get_or_allocate_forward_for_uid("msg-uid-alpha", r#"[{"id":"n1-new"}]"#)
        .expect("reuse existing forward");
    assert_eq!(reused_id, first_id);
    assert_eq!(
        storage.get_meta("forward_seq").expect("read forward_seq"),
        Some("10001".to_string())
    );

    let second_id = storage
        .get_or_allocate_forward_for_uid("msg-uid-beta", r#"[{"id":"n2"}]"#)
        .expect("allocate second forward");
    assert_eq!(second_id, "fwd-10002");

    // 写入超过 MAX_STORED_FORWARD_BUNDLES 条，验证 forward:* 与 forward_uid:* 均被严格淘汰在上限内
    let total = MAX_STORED_FORWARD_BUNDLES as usize + 30;
    for idx in 0..total {
        storage
            .get_or_allocate_forward_for_uid(
                &format!("bulk-uid-{idx}"),
                &format!("{{\"idx\":{idx}}}"),
            )
            .expect("allocate bulk forward");
    }

    let (fwd_count, uid_count) = {
        let conn = storage.conn.lock();
        let fwd: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM kv_meta WHERE key LIKE 'forward:%'",
                [],
                |r| r.get(0),
            )
            .expect("count forward keys");
        let uid: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM kv_meta WHERE key LIKE 'forward\\_uid:%' ESCAPE '\\'",
                [],
                |r| r.get(0),
            )
            .expect("count forward_uid keys");
        (fwd, uid)
    };
    assert_eq!(fwd_count, MAX_STORED_FORWARD_BUNDLES);
    assert_eq!(uid_count, MAX_STORED_FORWARD_BUNDLES);

    assert!(
        storage
            .get_meta(&format!("forward:{first_id}"))
            .expect("oldest forward evicted")
            .is_none(),
        "oldest forward bundle should be evicted"
    );
    assert!(
        storage
            .get_meta("forward_uid:msg-uid-alpha")
            .expect("oldest forward_uid evicted")
            .is_none(),
        "oldest forward_uid mapping should be evicted"
    );
    let newest_uid_key = format!("forward_uid:bulk-uid-{}", total - 1);
    let newest_fwd_id = storage
        .get_meta(&newest_uid_key)
        .expect("newest forward_uid lookup")
        .expect("newest forward_uid retained");
    assert!(storage
        .get_meta(&format!("forward:{newest_fwd_id}"))
        .expect("newest forward lookup")
        .is_some());

    // 当先前对应的 forward:{fwd_id} 已被淘汰时，再次请求同 UID 应重新分配有效编号而非返回悬空键
    let realloc_alpha = storage
        .get_or_allocate_forward_for_uid("msg-uid-alpha", r#"[{"id":"n1-reborn"}]"#)
        .expect("reallocate evicted forward uid");
    assert_ne!(realloc_alpha, first_id);
    assert!(storage
        .get_meta(&format!("forward:{realloc_alpha}"))
        .expect("reallocated forward exists")
        .is_some());

    // 若仅因非 UID 调用挤占导致 forward:{fwd_id} 被淘汰而 forward_uid 仍在，亦应自动重建有效转发包
    let orphan_candidate = storage
        .get_or_allocate_forward_for_uid("orphan-uid-check", r#"[{"id":"orphan"}]"#)
        .expect("allocate orphan candidate");
    for idx in 0..MAX_STORED_FORWARD_BUNDLES {
        storage
            .allocate_forward_nodes(&format!("{{\"plain\":{idx}}}"))
            .expect("evict forward bundle via plain allocate");
    }
    assert!(storage
        .get_meta(&format!("forward:{orphan_candidate}"))
        .expect("candidate bundle evicted")
        .is_none());
    let healed_id = storage
        .get_or_allocate_forward_for_uid("orphan-uid-check", r#"[{"id":"healed"}]"#)
        .expect("heal dangling forward_uid");
    assert_ne!(healed_id, orphan_candidate);
    assert_eq!(
        storage
            .get_meta(&format!("forward:{healed_id}"))
            .expect("healed forward exists")
            .as_deref(),
        Some(r#"[{"id":"healed"}]"#)
    );
}

#[test]
fn timestamp_anchor_blocks_reverse_merge_and_outbound_always_allocates_new_id() {
    // 1. 验证 record_message：先落库带 HH:MM 时间头锚点，再插入同群同发送者同正文的 None 新消息不得倒灌合并
    let storage = Storage::open(MEMORY_DB_PATH).expect("open memory storage");
    let mut anchor = make_record(
        "uid-anchor-1357",
        DEFAULT_TARGET_GROUP_NAME,
        ChatTargetKind::Group,
        "BotSelf",
        "LIVE-BUG-V6-002-MSG",
        1_700_000_000_000,
    );
    anchor.is_self = true;
    anchor.timestamp_hint = Some("13:57".to_string());
    let anchor_id = storage.record_message(&anchor).expect("insert anchor");

    let mut fresh_none = make_record(
        "uid-new-none-1",
        DEFAULT_TARGET_GROUP_NAME,
        ChatTargetKind::Group,
        "BotSelf",
        "LIVE-BUG-V6-002-MSG",
        1_700_000_010_000,
    );
    fresh_none.is_self = true;
    fresh_none.timestamp_hint = None;
    let new_id = storage
        .record_message(&fresh_none)
        .expect("insert none-ts message");
    assert_ne!(new_id, anchor_id);

    // 模拟随后 SyncAndPoll 批量对账，总行数必须严格保持 2 行且零重复
    let batch_ids = storage
        .record_messages_batch(&[anchor.clone(), fresh_none])
        .expect("sync and poll reconcile");
    assert_eq!(batch_ids, vec![anchor_id, new_id]);
    assert_eq!(storage.stats().expect("stats").message_count, 2);

    // 2. 验证 record_confirmed_outbound_message：无碰撞 UID 与碰撞 occurrence=0 UID 均分配全新编号
    let storage_distinct = Storage::open(MEMORY_DB_PATH).expect("open distinct outbound db");
    let anchor_id_d = storage_distinct
        .record_message(&anchor)
        .expect("insert anchor distinct");
    let mut fwd_distinct = make_record(
        "uid-fwd-distinct",
        DEFAULT_TARGET_GROUP_NAME,
        ChatTargetKind::Group,
        "BotSelf",
        "LIVE-BUG-V6-002-MSG",
        1_700_000_020_000,
    );
    fwd_distinct.is_self = true;
    let (fwd_id_d, stored_uid_d) = storage_distinct
        .record_confirmed_outbound_message(&fwd_distinct)
        .expect("record outbound distinct");
    assert_ne!(fwd_id_d, anchor_id_d);
    assert_eq!(stored_uid_d, "uid-fwd-distinct");
    let polled_distinct = storage_distinct
        .record_messages_batch(&[anchor.clone(), fwd_distinct])
        .expect("poll reconcile distinct");
    assert_eq!(polled_distinct, vec![anchor_id_d, fwd_id_d]);
    assert_eq!(storage_distinct.stats().expect("stats").message_count, 2);

    // 模拟旧气泡滚出视口导致 occurrence=0 UI UID 与历史锚点完全碰撞的场景
    let storage_collide = Storage::open(MEMORY_DB_PATH).expect("open colliding outbound db");
    let anchor_id_c = storage_collide
        .record_message(&anchor)
        .expect("insert anchor collide");
    let mut colliding_fwd = anchor.clone();
    colliding_fwd.timestamp_hint = None;
    colliding_fwd.captured_at_ms = 1_700_000_030_000;
    let (salted_id, salted_uid) = storage_collide
        .record_confirmed_outbound_message(&colliding_fwd)
        .expect("record outbound colliding uid");
    assert_ne!(salted_id, anchor_id_c);
    assert!(
        salted_uid.starts_with("uid-anchor-1357#out:1700000030000:"),
        "expected salted uid, got {salted_uid}"
    );
    let mut polled_salted = colliding_fwd.clone();
    polled_salted.message_uid = salted_uid;
    let polled_collide = storage_collide
        .record_messages_batch(&[anchor, polled_salted])
        .expect("poll reconcile salted");
    assert_eq!(polled_collide, vec![anchor_id_c, salted_id]);
    assert_eq!(storage_collide.stats().expect("stats").message_count, 2);
}

#[test]
fn none_to_hhmm_timestamp_split_still_merges_and_reuses_id() {
    let storage = Storage::open(MEMORY_DB_PATH).expect("open memory storage");
    let mut plain = make_record(
        "uid-plain-before-ts",
        DEFAULT_TARGET_GROUP_NAME,
        ChatTargetKind::Group,
        "BotSelf",
        "SPLIT-FORWARD-CHECK",
        1_700_000_000_000,
    );
    plain.is_self = true;
    plain.timestamp_hint = None;
    let orig_id = storage.record_message(&plain).expect("insert plain");

    let mut with_ts = plain;
    with_ts.message_uid = "uid-plain-with-1357".to_string();
    with_ts.timestamp_hint = Some("13:57".to_string());
    with_ts.captured_at_ms = 1_700_000_005_000;
    let merged_id = storage
        .record_message(&with_ts)
        .expect("merge timestamp completion");

    assert_eq!(merged_id, orig_id);
    assert_eq!(storage.stats().expect("stats").message_count, 1);
    let stored = storage
        .get_message_by_id(orig_id)
        .expect("query merged")
        .expect("merged row exists");
    assert_eq!(stored.timestamp_hint.as_deref(), Some("13:57"));
    assert_eq!(stored.message_uid, "uid-plain-with-1357");
}
