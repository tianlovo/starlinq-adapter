//! 驱动层 Wave 2 扩展单元测试：覆盖新增查询接口、全量消息段出站规划、富化规则与系统通知识别

use std::sync::Arc;

use common_types::{
    AdapterConfig, BotConfig, BotDriver, ChatMessage, ChatTargetKind, MessageSegment,
    StorageConfig, UiNodeSnapshot,
};
use storage::constants::MEMORY_DB_PATH;
use storage::{
    RecordMessageInput, Storage, UpsertFriendInput, UpsertGroupInput, UpsertMemberInput,
};
use tokio::sync::broadcast;

use super::outbound_media::{build_outbound_send_plan, OutboundSendPlan};
use super::runtime_state::{purge_stale_media_crop_files_in_dir, SharedInner};
use super::segment_enricher::{enrich_parsed_messages, EnrichContext};
use super::QqntDriver;
use crate::parser::{
    extract_system_notices_from_tree, parse_notice_text_to_onebot_json, MessageTracker,
    NoticeContext, OutboundStubBinding, ParsedTreeMessage,
};

fn build_memory_driver() -> QqntDriver {
    let config = AdapterConfig {
        storage: StorageConfig {
            db_path: MEMORY_DB_PATH.to_string(),
        },
        bot: BotConfig::default(),
        ..Default::default()
    };
    QqntDriver::new(config).expect("create memory driver")
}

#[tokio::test]
async fn bot_driver_recent_messages_forwards_and_stranger_lookup() {
    let driver = build_memory_driver();

    let mut msgs = vec![
        ChatMessage {
            message_id: "uid-hist-1".to_string(),
            group_name: "Demo测试群 (5)".to_string(),
            sender: "群友A".to_string(),
            content: "第一条历史".to_string(),
            is_self: false,
            captured_at_ms: 1_700_000_001_000,
            target_kind: ChatTargetKind::Group,
            ..Default::default()
        },
        ChatMessage {
            message_id: "uid-hist-2".to_string(),
            group_name: "Demo测试群 (5)".to_string(),
            sender: "群友B".to_string(),
            content: "第二条历史".to_string(),
            is_self: false,
            captured_at_ms: 1_700_000_002_000,
            target_kind: ChatTargetKind::Group,
            ..Default::default()
        },
    ];
    driver
        .shared
        .persist_messages_for_group("Demo测试群 (5)", Some(5), &mut msgs);

    let recent = driver
        .get_recent_messages("Demo测试群", 10)
        .await
        .expect("recent messages");
    assert_eq!(recent.len(), 2);
    assert!(recent.iter().any(|item| item.raw_message == "第一条历史"));
    assert!(recent.iter().any(|item| item.raw_message == "第二条历史"));

    driver
        .save_forward_nodes("fwd-100", "[{\"type\":\"node\"}]")
        .await
        .expect("save forward");
    assert_eq!(
        driver
            .get_forward_nodes("fwd-100")
            .await
            .expect("get forward"),
        Some("[{\"type\":\"node\"}]".to_string())
    );
    assert_eq!(
        driver
            .get_forward_nodes("missing")
            .await
            .expect("missing forward"),
        None
    );

    let friend = driver
        .shared
        .storage
        .upsert_friend(&UpsertFriendInput {
            nickname: "测试好友A".to_string(),
            remark: "测试好友".to_string(),
            real_user_id: Some(10086001),
            last_preview: None,
        })
        .expect("upsert friend");
    let found_friend = driver
        .get_stranger_info(friend.effective_user_id())
        .await
        .expect("stranger friend")
        .expect("friend should exist");
    assert_eq!(found_friend.nickname, "测试好友A");
    assert_eq!(found_friend.user_id, 10086001);

    let group = driver
        .shared
        .storage
        .upsert_group(&UpsertGroupInput {
            group_name: "Demo测试群".to_string(),
            real_group_id: Some(90001234),
            member_count: Some(5),
            max_member_count: None,
            last_preview: None,
            unread_count: 0,
        })
        .expect("upsert group");
    let members = vec![UpsertMemberInput {
        nickname: "群成员乙".to_string(),
        real_user_id: Some(888777666),
        card: "乙名片".to_string(),
        role: "member".to_string(),
    }];
    driver
        .shared
        .storage
        .upsert_group_members(&group.group_name, group.effective_group_id(), &members)
        .expect("upsert group members");

    let found_member = driver
        .get_stranger_info(888777666)
        .await
        .expect("stranger member")
        .expect("member should exist");
    assert_eq!(found_member.user_id, 888777666);
    assert_eq!(found_member.nickname, "群成员乙");
    assert_eq!(found_member.remark, "乙名片");
}

#[test]
fn outbound_send_plan_handles_all_twenty_one_segments() {
    let storage = Arc::new(Storage::open(MEMORY_DB_PATH).expect("memory storage"));
    let segments = vec![
        MessageSegment::Text {
            text: "你好".to_string(),
        },
        MessageSegment::Face {
            id: "14".to_string(),
            text: None,
        },
        MessageSegment::Rps {},
        MessageSegment::Dice {},
        MessageSegment::Shake {},
        MessageSegment::Poke {
            poke_type: "1".to_string(),
            id: "123".to_string(),
            name: Some("戳一戳".to_string()),
        },
        MessageSegment::Anonymous {
            ignore: Some("true".to_string()),
        },
        MessageSegment::Share {
            url: "https://example.com".to_string(),
            title: "示例分享".to_string(),
            content: None,
            image: None,
        },
        MessageSegment::Contact {
            contact_type: "qq".to_string(),
            id: "10001".to_string(),
        },
        MessageSegment::Location {
            lat: "31.23".to_string(),
            lon: "121.47".to_string(),
            title: Some("上海".to_string()),
            content: None,
        },
        MessageSegment::Music {
            music_type: "qq".to_string(),
            id: Some("99".to_string()),
            url: None,
            audio: None,
            title: Some("晴天".to_string()),
            content: None,
            image: None,
        },
        MessageSegment::Forward {
            id: "fwd-9".to_string(),
        },
        MessageSegment::Node {
            id: None,
            user_id: Some("10001".to_string()),
            nickname: Some("甲".to_string()),
            content: None,
        },
        MessageSegment::Xml {
            data: "<msg/>".to_string(),
        },
        MessageSegment::Json {
            data: "{\"app\":\"card\"}".to_string(),
        },
        MessageSegment::File {
            file: "report.pdf".to_string(),
            name: None,
            size: None,
        },
        MessageSegment::Record {
            file: "voice.amr".to_string(),
            magic: None,
            url: None,
        },
        MessageSegment::Video {
            file: "clip.mp4".to_string(),
            url: None,
        },
    ];

    let plan = build_outbound_send_plan(
        &storage,
        &[],
        ("Demo测试群", ChatTargetKind::Group),
        &segments,
    )
    .expect("build plan");

    assert_eq!(plan.canonical_segments.len(), segments.len());
    assert!(plan.combined_text.contains("你好"));
    assert!(plan.combined_text.contains("示例分享"));
    assert!(plan.combined_text.contains("晴天"));
    assert!(plan.combined_text.contains("report.pdf"));
    assert!(plan.combined_text.contains("[语音]"));
    assert!(plan.combined_text.contains("[视频]"));
    assert!(plan.degraded_segments.contains(&"anonymous".to_string()));
}

#[test]
fn segment_enricher_handles_media_forward_and_face_tokens() {
    let storage = Storage::open(MEMORY_DB_PATH).expect("memory storage");
    let bot_login = common_types::BotLoginInfo {
        user_id: 10001,
        nickname: "Bot".to_string(),
        real_qq_id: None,
    };
    let ctx = EnrichContext {
        storage: &storage,
        session_key: "Demo测试群",
        chat_kind: ChatTargetKind::Group,
        hwnd: 0,
        members: &[],
        bot_login: &bot_login,
    };

    let mut parsed = vec![ParsedTreeMessage {
        message: ChatMessage {
            message_id: "m-rich-1".to_string(),
            group_name: "Demo测试群".to_string(),
            sender: "群友A".to_string(),
            content: "[语音] [视频] [合并转发] [微笑] [测试]".to_string(),
            target_kind: ChatTargetKind::Group,
            ..Default::default()
        },
        image_rects: Vec::new(),
        ..Default::default()
    }];

    enrich_parsed_messages(&ctx, &mut parsed);
    let segs = &parsed[0].message.segments;
    assert!(segs
        .iter()
        .any(|seg| matches!(seg, MessageSegment::Record { .. })));
    assert!(segs
        .iter()
        .any(|seg| matches!(seg, MessageSegment::Video { .. })));
    assert!(segs
        .iter()
        .any(|seg| matches!(seg, MessageSegment::Forward { .. })));
    assert!(segs
        .iter()
        .any(|seg| matches!(seg, MessageSegment::Face { id, .. } if id == "14")));
    assert!(segs
        .iter()
        .any(|seg| matches!(seg, MessageSegment::Text { text } if text.contains("[测试]"))));
}

#[test]
fn notice_parser_and_tracker_deduplicate_and_classify() {
    let avatar_chat_row = UiNodeSnapshot {
        control_type: "ListItem".to_string(),
        rect: [400, 120, 880, 180],
        is_enabled: true,
        children: vec![
            UiNodeSnapshot {
                control_type: "Group".to_string(),
                name: "群友A的头像".to_string(),
                rect: [410, 125, 450, 165],
                is_enabled: true,
                ..Default::default()
            },
            UiNodeSnapshot {
                control_type: "Text".to_string(),
                name: "刚才谁撤回了一条消息哈哈".to_string(),
                rect: [460, 128, 700, 160],
                is_enabled: true,
                ..Default::default()
            },
        ],
        ..Default::default()
    };

    let banner_nodes = [
        "群友A 撤回了一条消息",
        "群友B 拍了拍 你 的头像",
        "管理员 邀请 新人 加入了群聊。",
        "群友C 退出了群聊。",
        "群友D 被禁言 10分钟",
    ]
    .into_iter()
    .enumerate()
    .map(|(idx, text)| {
        let top = 200 + (idx as i32) * 35;
        UiNodeSnapshot {
            control_type: "ListItem".to_string(),
            rect: [420, top, 820, top + 28],
            is_enabled: true,
            children: vec![UiNodeSnapshot {
                control_type: "Text".to_string(),
                name: text.to_string(),
                rect: [430, top + 2, 800, top + 24],
                is_enabled: true,
                ..Default::default()
            }],
            ..Default::default()
        }
    })
    .collect::<Vec<_>>();

    let mut list_children = vec![avatar_chat_row];
    list_children.extend(banner_nodes);

    let root = UiNodeSnapshot {
        control_type: "Window".to_string(),
        rect: [0, 0, 1000, 700],
        is_enabled: true,
        children: vec![UiNodeSnapshot {
            control_type: "List".to_string(),
            name: "消息列表".to_string(),
            rect: [340, 80, 960, 620],
            is_enabled: true,
            children: list_children,
            ..Default::default()
        }],
        ..Default::default()
    };

    let extracted = extract_system_notices_from_tree(&root, [0, 0, 1000, 700], false, "Demo测试群");
    assert_eq!(extracted.len(), 5);
    assert!(!extracted
        .iter()
        .any(|item| item.text.contains("谁撤回了一条消息哈哈")));

    let mut tracker = MessageTracker::new(Vec::new());
    tracker.seed_notice_baseline(&extracted[..4]);
    let fresh = tracker.observe_notices(extracted.clone());
    assert_eq!(fresh.len(), 1);
    assert!(fresh[0].text.contains("被禁言"));

    let ctx = NoticeContext {
        self_id: 10001,
        chat_kind: ChatTargetKind::Group,
        session_name: "Demo测试群",
        group_id: Some(555_005),
        peer_user_id: None,
        members: &[],
    };
    let recall = parse_notice_text_to_onebot_json(&ctx, &extracted[0].text, 1_700_000_000);
    assert_eq!(recall["notice_type"], "group_recall");
    assert_eq!(recall["group_id"], 555_005);

    let poke = parse_notice_text_to_onebot_json(&ctx, &extracted[1].text, 1_700_000_000);
    assert_eq!(poke["notice_type"], "notify");
    assert_eq!(poke["sub_type"], "poke");
    assert_eq!(poke["target_id"], 10001);

    let join = parse_notice_text_to_onebot_json(&ctx, &extracted[2].text, 1_700_000_000);
    assert_eq!(join["notice_type"], "group_increase");
    assert_eq!(join["sub_type"], "invite");

    let leave = parse_notice_text_to_onebot_json(&ctx, &extracted[3].text, 1_700_000_000);
    assert_eq!(leave["notice_type"], "group_decrease");
    assert_eq!(leave["sub_type"], "leave");

    let ban = parse_notice_text_to_onebot_json(&ctx, &extracted[4].text, 1_700_000_000);
    assert_eq!(ban["notice_type"], "group_ban");
    assert_eq!(ban["sub_type"], "ban");
    assert_eq!(ban["duration"], 600);

    let priv_ctx = NoticeContext {
        self_id: 10001,
        chat_kind: ChatTargetKind::Private,
        session_name: "测试好友A",
        group_id: None,
        peer_user_id: Some(777_001),
        members: &[],
    };
    let priv_recall =
        parse_notice_text_to_onebot_json(&priv_ctx, "测试好友A 撤回了一条消息", 1_700_000_000);
    assert_eq!(priv_recall["notice_type"], "friend_recall");
    assert_eq!(priv_recall["user_id"], 777_001);
}

#[test]
fn outbound_media_rejects_multibyte_and_file_urls_without_utf8_panic() {
    let storage = Arc::new(Storage::open(MEMORY_DB_PATH).expect("memory storage"));
    for bad_path in [
        "😀😀😀",
        "中文图片测试路径.png",
        "FILE:///C:/__nonexistent_qa_path__/demo.png",
    ] {
        let segments = vec![MessageSegment::Image {
            file: bad_path.to_string(),
            url: None,
            summary: None,
            image_type: None,
        }];
        let err = build_outbound_send_plan(
            &storage,
            &[],
            ("Demo测试群", ChatTargetKind::Group),
            &segments,
        )
        .err()
        .expect("invalid image source must return InputRejected cleanly");
        assert!(matches!(err, common_types::AdapterError::InputRejected(_)));
    }
}

#[test]
fn outbound_send_plan_reply_is_non_trailing_and_tracks_degraded_segments() {
    let storage = Arc::new(Storage::open(MEMORY_DB_PATH).expect("memory storage"));
    let target_id = storage
        .record_message(&RecordMessageInput {
            message_uid: "reply-seed-uid".to_string(),
            group_name: "Demo测试群".to_string(),
            group_id: 222_001,
            user_id: 333_001,
            sender_name: "Alice".to_string(),
            raw_message: "hello".to_string(),
            is_self: false,
            timestamp_hint: Some("10:00".to_string()),
            captured_at_ms: 1_700_000_000_000,
            chat_type: ChatTargetKind::Group,
            segments: vec![MessageSegment::Text {
                text: "hello".to_string(),
            }],
        })
        .expect("seed message");

    // 仅回复段或回复后无正文的组合不得附带尾部换行符或多余空格
    let reply_only = build_outbound_send_plan(
        &storage,
        &[],
        ("Demo测试群", ChatTargetKind::Group),
        &[MessageSegment::Reply {
            id: target_id.to_string(),
        }],
    )
    .expect("reply only plan");
    assert_eq!(reply_only.combined_text, "「回复 @Alice: hello」");
    assert!(!reply_only.combined_text.ends_with('\n'));
    assert!(!reply_only.combined_text.ends_with(' '));
    assert_eq!(reply_only.degraded_segments, vec!["reply".to_string()]);

    let reply_then_text = build_outbound_send_plan(
        &storage,
        &[],
        ("Demo测试群", ChatTargetKind::Group),
        &[
            MessageSegment::Reply {
                id: target_id.to_string(),
            },
            MessageSegment::Text {
                text: "收到".to_string(),
            },
        ],
    )
    .expect("reply + text plan");
    assert_eq!(reply_then_text.combined_text, "「回复 @Alice: hello」 收到");
    assert_eq!(reply_then_text.degraded_segments, vec!["reply".to_string()]);

    // 纯文本与常规图片不得被标记为降级段
    let tiny_png_b64 = "base64://iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+ip1sAAAAASUVORK5CYII=";
    let clean_plan = build_outbound_send_plan(
        &storage,
        &[],
        ("Demo测试群", ChatTargetKind::Group),
        &[
            MessageSegment::Text {
                text: "pic".to_string(),
            },
            MessageSegment::Image {
                file: tiny_png_b64.to_string(),
                url: None,
                summary: None,
                image_type: None,
            },
        ],
    )
    .expect("text + normal image");
    assert!(clean_plan.degraded_segments.is_empty());

    // 重复降级类型需去重保序，且闪照应标记为 image(flash)
    let degraded_plan = build_outbound_send_plan(
        &storage,
        &[],
        ("Demo测试群", ChatTargetKind::Group),
        &[
            MessageSegment::Face {
                id: "1".to_string(),
                text: None,
            },
            MessageSegment::Face {
                id: "2".to_string(),
                text: None,
            },
            MessageSegment::At {
                qq: "all".to_string(),
                name: None,
            },
            MessageSegment::Image {
                file: tiny_png_b64.to_string(),
                url: None,
                summary: None,
                image_type: Some("flash".to_string()),
            },
        ],
    )
    .expect("degraded segments plan");
    assert_eq!(
        degraded_plan.degraded_segments,
        vec![
            "face".to_string(),
            "at".to_string(),
            "image(flash)".to_string(),
        ]
    );

    // 匿名消息段受限于桌面端未实现真匿名发送，回落普通文本发送时必须如实报告 "anonymous" 降级
    let anon_req = common_types::SendRequest {
        group: "Demo测试群".to_string(),
        text: "[CQ:anonymous]hi".to_string(),
        mode: None,
        target_kind: ChatTargetKind::Group,
        segments: Vec::new(),
        ..Default::default()
    };
    let anon_segments =
        super::outbound_media::extract_effective_segments(&anon_req).expect("anon cq segments");
    let anon_plan = build_outbound_send_plan(
        &storage,
        &[],
        ("Demo测试群", ChatTargetKind::Group),
        &anon_segments,
    )
    .expect("anon fallback plan");
    assert_eq!(anon_plan.combined_text, "hi");
    assert_eq!(anon_plan.degraded_segments, vec!["anonymous".to_string()]);
}

#[test]
fn media_crop_dpi_v2_purges_only_underscore_pngs_once() {
    let storage = Storage::open(MEMORY_DB_PATH).expect("memory storage");
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("qq_uia_media_crop_test_{unique}"));
    let nested = dir.join("nested");
    std::fs::create_dir_all(&nested).expect("mkdirs");

    let stale_a = dir.join("deadbeef_0.png");
    let stale_b = dir.join("cafe_1.PNG");
    let keep_no_underscore = dir.join("plain.png");
    let keep_non_png = dir.join("deadbeef_0.txt");
    let keep_nested = nested.join("sub_0.png");
    for file in [
        &stale_a,
        &stale_b,
        &keep_no_underscore,
        &keep_non_png,
        &keep_nested,
    ] {
        std::fs::write(file, b"png").expect("write test file");
    }

    let removed_first = purge_stale_media_crop_files_in_dir(&storage, &dir);
    assert_eq!(removed_first, 2);
    assert!(!stale_a.exists());
    assert!(!stale_b.exists());
    assert!(keep_no_underscore.exists());
    assert!(keep_non_png.exists());
    assert!(keep_nested.exists());
    assert_eq!(
        storage
            .get_meta("migration.media_crop_dpi_v2")
            .expect("meta")
            .as_deref(),
        Some("1")
    );

    // 幂等门控生效后二次启动不得重复删除新建缓存文件
    std::fs::write(&stale_a, b"fresh").expect("rewrite");
    let removed_second = purge_stale_media_crop_files_in_dir(&storage, &dir);
    assert_eq!(removed_second, 0);
    assert!(stale_a.exists());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn confirmed_send_uses_ui_uid_and_reconciles_stub_preserving_onebot_id() {
    let cfg = AdapterConfig {
        storage: StorageConfig {
            db_path: MEMORY_DB_PATH.to_string(),
        },
        ..Default::default()
    };
    let storage = Arc::new(Storage::open(&cfg.storage.db_path).expect("memory db"));
    let (tx, _) = broadcast::channel(8);
    let shared = Arc::new(SharedInner::new(cfg, Arc::clone(&storage), tx));

    let plan = OutboundSendPlan {
        combined_text: "probe msg".to_string(),
        images: Vec::new(),
        canonical_segments: vec![MessageSegment::Text {
            text: "probe msg".to_string(),
        }],
        summary_text: "probe msg".to_string(),
        verify_norm: "probe msg".to_string(),
        degraded_segments: Vec::new(),
    };
    let verified_ui_msg = ChatMessage {
        message_id: "ui-real-101".to_string(),
        group_name: "Demo测试群".to_string(),
        sender: "BotSelf".to_string(),
        content: "probe msg".to_string(),
        timestamp_hint: Some("12:01".to_string()),
        is_self: true,
        captured_at_ms: 1_700_000_000_000,
        target_kind: ChatTargetKind::Group,
        ..Default::default()
    };

    let (onebot_id_opt, _entity_id, stored_uid) = shared.persist_confirmed_sent_plan(
        "Demo测试群",
        ChatTargetKind::Group,
        &plan,
        Some(&verified_ui_msg),
        Some(5),
    );
    let onebot_id = onebot_id_opt.expect("assigned onebot_message_id");
    assert_eq!(stored_uid, "ui-real-101");

    let stored = storage
        .get_message_by_id(onebot_id)
        .expect("query")
        .expect("stored stub");
    assert_eq!(stored.message_uid, "ui-real-101");

    shared
        .tracker
        .lock()
        .register_confirmed_outbound(OutboundStubBinding {
            onebot_message_id: onebot_id,
            stub_uid: stored_uid,
            norm_text: plan.verify_norm.clone(),
            media_token: None,
            ..Default::default()
        });

    let mut polled = vec![ChatMessage {
        message_id: "ui-scrolled-102".to_string(),
        group_name: "Demo测试群".to_string(),
        sender: "BotSelf".to_string(),
        content: "probe msg".to_string(),
        timestamp_hint: Some("12:02".to_string()),
        is_self: true,
        captured_at_ms: 1_700_000_010_000,
        target_kind: ChatTargetKind::Group,
        ..Default::default()
    }];
    let rebound = shared
        .tracker
        .lock()
        .reconcile_polled_self_messages(&mut polled);
    assert_eq!(rebound.len(), 1);
    assert_eq!(rebound[0].0, onebot_id);
    assert_eq!(rebound[0].1, "ui-scrolled-102");

    storage
        .reconcile_outbound_stub_uid(rebound[0].0, &rebound[0].1, rebound[0].2.as_deref())
        .expect("reconcile sql uid");
    let updated = storage
        .get_message_by_id(onebot_id)
        .expect("query updated")
        .expect("updated record");
    assert_eq!(updated.onebot_message_id, onebot_id);
    assert_eq!(updated.message_uid, "ui-scrolled-102");
    assert_eq!(updated.timestamp_hint.as_deref(), Some("12:02"));
}

#[test]
fn unverified_cleared_auto_outcome_preserves_truthful_failure_and_degraded_segments() {
    let cfg = AdapterConfig {
        storage: StorageConfig {
            db_path: MEMORY_DB_PATH.to_string(),
        },
        ..Default::default()
    };
    let storage = Arc::new(Storage::open(&cfg.storage.db_path).expect("memory db"));
    let (tx, _) = broadcast::channel(8);
    let shared = Arc::new(SharedInner::new(cfg, storage, tx));

    let plan = OutboundSendPlan {
        combined_text: "hello [语音]".to_string(),
        images: Vec::new(),
        canonical_segments: vec![
            MessageSegment::Text {
                text: "hello".to_string(),
            },
            MessageSegment::Record {
                file: "a.amr".to_string(),
                url: None,
                magic: None,
            },
        ],
        summary_text: "hello [语音]".to_string(),
        verify_norm: "hello [语音]".to_string(),
        degraded_segments: vec!["record".to_string()],
    };
    let outcome = common_types::SendOutcome {
        ok: false,
        group: "Demo测试群".to_string(),
        used_mode: common_types::InputMode::Auto,
        foreground_touched: false,
        mouse_touched: false,
        verified_in_chat: false,
        elapsed_ms: 800,
        detail: "submitted via background UIA (composer cleared) but message could not be verified in chat history".to_string(),
        target_kind: ChatTargetKind::Group,
        degraded_segments: plan.degraded_segments.clone(),
        ..Default::default()
    };
    let mut dummy_req = common_types::SendRequest {
        group: "Demo测试群".to_string(),
        text: plan.combined_text.clone(),
        mode: Some(common_types::InputMode::Auto),
        target_kind: ChatTargetKind::Group,
        segments: plan.canonical_segments.clone(),
        ..Default::default()
    };
    let final_out =
        super::send_handlers::finalize_send_result(&shared, &dummy_req, Some(5), Ok(outcome))
            .expect("truthful unverified outcome");
    dummy_req.text.clear();
    assert!(!final_out.ok);
    assert!(!final_out.verified_in_chat);
    assert_eq!(final_out.onebot_message_id, None);
    assert_eq!(shared.status_snapshot().messages_sent, 0);
}

#[test]
fn replays_04_tree_end_to_end_into_file_segment() {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/public/04_tree.json");
    let raw_text = std::fs::read_to_string(&path).expect("read 04_tree.json");
    let tree: UiNodeSnapshot = serde_json::from_str(&raw_text).expect("parse 04_tree.json");

    let mut rich =
        crate::parser::extract_rich_messages_from_tree(&tree, tree.rect, false, "Demo测试群");
    let storage = Storage::open(MEMORY_DB_PATH).expect("memory storage");
    let bot_login = common_types::BotLoginInfo {
        user_id: 10001,
        nickname: "测试机器人".to_string(),
        real_qq_id: None,
    };
    let ctx = EnrichContext {
        storage: &storage,
        session_key: "Demo测试群",
        chat_kind: ChatTargetKind::Group,
        hwnd: 0,
        members: &[],
        bot_login: &bot_login,
    };
    enrich_parsed_messages(&ctx, &mut rich);

    let file_item = rich
        .iter()
        .find(|item| {
            item.message
                .segments
                .iter()
                .any(|seg| matches!(seg, MessageSegment::File { .. }))
        })
        .expect("04_tree.json file bubble must enrich into MessageSegment::File");
    assert!(file_item.message.segments.iter().any(|seg| {
        matches!(
            seg,
            MessageSegment::File {
                name: Some(name),
                size: Some(56),
                ..
            } if name == "sample_live_verify.txt"
        )
    }));
}

#[tokio::test]
async fn bot_driver_get_recent_messages_by_kind_and_outbound_security_and_counters() {
    let driver = build_memory_driver();
    driver
        .shared
        .storage
        .record_message(&RecordMessageInput {
            message_uid: "uid-ext-same-group".to_string(),
            group_name: "同名会话".to_string(),
            group_id: 100200,
            user_id: 300400,
            sender_name: "群友甲".to_string(),
            raw_message: "群内消息".to_string(),
            is_self: false,
            timestamp_hint: Some("10:00".to_string()),
            captured_at_ms: 1_700_000_001_000,
            chat_type: ChatTargetKind::Group,
            segments: Vec::new(),
        })
        .expect("seed group message");
    driver
        .shared
        .storage
        .record_message(&RecordMessageInput {
            message_uid: "uid-ext-same-priv".to_string(),
            group_name: "同名会话".to_string(),
            group_id: 0,
            user_id: 500600,
            sender_name: "同名会话".to_string(),
            raw_message: "私聊消息".to_string(),
            is_self: false,
            timestamp_hint: Some("10:01".to_string()),
            captured_at_ms: 1_700_000_002_000,
            chat_type: ChatTargetKind::Private,
            segments: Vec::new(),
        })
        .expect("seed private message");

    // 同名群聊与私聊通过 get_recent_messages_by_kind(支持群人数括号后缀) 必须严格隔离
    let group_only = driver
        .get_recent_messages_by_kind("同名会话 (5)", Some(ChatTargetKind::Group), 10)
        .await
        .expect("query group kind");
    assert_eq!(group_only.len(), 1);
    assert_eq!(group_only[0].raw_message, "群内消息");

    let priv_only = driver
        .get_recent_messages_by_kind("同名会话", Some(ChatTargetKind::Private), 10)
        .await
        .expect("query private kind");
    assert_eq!(priv_only.len(), 1);
    assert_eq!(priv_only[0].raw_message, "私聊消息");

    // BIZ-RISK-005: 跨群与跨聊天类型 [CQ:reply,id=...] 均须返回 InputRejected
    let priv_id = priv_only[0].onebot_message_id;
    let err_cross_kind = build_outbound_send_plan(
        &driver.shared.storage,
        &[],
        ("同名会话", ChatTargetKind::Group),
        &[
            MessageSegment::Reply {
                id: priv_id.to_string(),
            },
            MessageSegment::Text {
                text: "reply".to_string(),
            },
        ],
    )
    .err()
    .expect("reply across chat_type must be rejected");
    assert!(matches!(
        err_cross_kind,
        common_types::AdapterError::InputRejected(ref m) if m.contains("belongs to another session")
    ));

    // BIZ-RISK-006: UNC 与远端 file:// 网络共享路径须统一拒绝，本地长路径与大写 BASE64:// 放行
    for bad in [
        r"\\fileserver\share\secret.png",
        "//fileserver/share/secret.png",
        "file://fileserver/share/secret.png",
    ] {
        let err_img = super::outbound_media::load_image_bytes_sync(bad)
            .expect_err("UNC image path must be rejected");
        assert!(matches!(
            err_img,
            common_types::AdapterError::InputRejected(ref m)
                if m.contains("UNC or remote network share paths are forbidden in offline mode")
        ));
    }
    let err_url = super::outbound_media::file_url_to_local_path("file://fileserver/share/pic.png")
        .expect_err("remote file:// host must be rejected");
    assert!(matches!(
        err_url,
        common_types::AdapterError::InputRejected(ref m)
            if m.contains("UNC or remote network share paths are forbidden in offline mode")
    ));
    assert!(
        super::outbound_media::file_url_to_local_path("file://localhost/C:/dir/ok.png").is_ok()
    );

    let upper_png = "BASE64://iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+ip1sAAAAASUVORK5CYII=";
    let b64_plan = build_outbound_send_plan(
        &driver.shared.storage,
        &[],
        ("同名会话", ChatTargetKind::Group),
        &[MessageSegment::Image {
            file: upper_png.to_string(),
            url: None,
            summary: None,
            image_type: None,
        }],
    )
    .expect("uppercase BASE64:// image plan");
    assert_eq!(b64_plan.images.len(), 1);

    // BIZ-RISK-008: 未成功确认发送时不得累加 messages_sent 计数
    let fail_req = common_types::SendRequest {
        group: "同名会话".to_string(),
        text: "failed send".to_string(),
        target_kind: ChatTargetKind::Group,
        ..Default::default()
    };
    let fail_outcome = super::send_handlers::finalize_send_result(
        &driver.shared,
        &fail_req,
        Some(5),
        Ok(common_types::SendOutcome {
            ok: false,
            group: "同名会话".to_string(),
            target_kind: ChatTargetKind::Group,
            detail: "mock failed".to_string(),
            ..Default::default()
        }),
    )
    .expect("finalize ok=false");
    assert!(!fail_outcome.ok);
    assert_eq!(driver.cached_status().expect("status").messages_sent, 0);
}
