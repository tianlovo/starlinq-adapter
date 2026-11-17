//! 基于真实 QQNT 9.9.31 UIA 现场抓包树回放验证消息解析、视口裁切、会话作用域与出站 UID 关联回归测试

use common_types::{ChatMessage, UiNodeSnapshot};
use qqnt_driver::matcher::{extract_sessions_from_tree, is_valid_session_title};
use qqnt_driver::parser::{
    extract_messages_from_tree, extract_rich_messages_from_tree, MessageTracker,
    OutboundStubBinding,
};

fn load_fixture_tree(json_str: &str) -> UiNodeSnapshot {
    serde_json::from_str(json_str).expect("有效 UIA 快照夹具 JSON 解析失败")
}

#[test]
fn replays_02_ext2_ml_root_group_chat_into_distinct_messages() {
    let mut tree = load_fixture_tree(include_str!("fixtures/live_02_ext2_ml_root.json"));
    let msgs = extract_messages_from_tree(&tree, tree.rect, false, "Demo测试群");

    assert_eq!(msgs.len(), 5, "应拆分为 5 条独立群消息，实际: {msgs:?}");
    for msg in &msgs {
        assert_eq!(msg.sender, "测试机器人");
        assert!(msg.is_self);
        assert_ne!(msg.content.trim(), "LV");
        assert_ne!(msg.content.trim(), "3");
    }
    assert!(msgs[0].content.contains("[GPT-QA-102905-TEXT]"));
    assert!(msgs[1].content.contains("[GPT-QA-102946-STRUCT]"));
    assert!(msgs[2].content.contains("[GPT-QA-102946-CQ]"));
    assert!(msgs[3].content.contains("[GPT-QA-102946-EXT1]"));
    assert!(msgs[4].content.contains("[GPT-QA-102946-EXT2]"));
    assert!(msgs[4].content.contains("https://example.com/qa"));
    assert!(msgs[4].content.contains("QA文件.txt"));
    assert!(!msgs[0].content.contains("[GPT-QA-102946-EXT2]"));

    // 即使外层未标明 `ml-root` 标识，大视口无名 Group 只要满足扁平消息流特征也必须先解包
    clear_ml_root_automation_id(&mut tree);
    let unwrapped_msgs = extract_messages_from_tree(&tree, tree.rect, false, "Demo测试群");
    assert_eq!(unwrapped_msgs.len(), 5);
}

fn clear_ml_root_automation_id(node: &mut UiNodeSnapshot) {
    if node.automation_id == "ml-root" {
        node.automation_id.clear();
    }
    for child in &mut node.children {
        clear_ml_root_automation_id(child);
    }
}

#[test]
fn replays_02_text_ml_root_skips_notice_and_preserves_messages() {
    let tree = load_fixture_tree(include_str!("fixtures/live_02_text_ml_root.json"));
    let rich = extract_rich_messages_from_tree(&tree, tree.rect, false, "Demo测试群");

    assert_eq!(rich.len(), 3, "应保留 3 条真实消息，实际: {rich:?}");
    assert_eq!(rich[0].message.sender, "测试好友B");
    assert_eq!(rich[0].message.content, "[图片]");
    assert!(!rich[0].message.is_self);
    assert_eq!(rich[0].image_rects, vec![[640, 191, 790, 342]]);

    assert_eq!(rich[1].message.sender, "测试机器人");
    assert_eq!(rich[1].message.timestamp_hint.as_deref(), Some("04:35"));
    assert!(rich[1].message.is_self);
    assert!(rich[1].message.content.contains("[OneBot v11 迭代验收]"));

    assert_eq!(rich[2].message.sender, "测试机器人");
    assert_eq!(rich[2].message.timestamp_hint.as_deref(), Some("10:29"));
    assert!(rich[2].message.is_self);
    assert!(rich[2].message.content.contains("[GPT-QA-102905-TEXT]"));

    for item in &rich {
        assert!(!item.message.content.contains("回应了你的"));
    }
}

#[test]
fn replays_02_scrolled_images_clip_and_skip_header_overlap() {
    let image_tree = load_fixture_tree(include_str!("fixtures/live_02_image_scrolled.json"));
    let rich = extract_rich_messages_from_tree(&image_tree, image_tree.rect, false, "Demo测试群");

    assert_eq!(rich.len(), 3, "应解析出 3 条图文消息，实际: {rich:?}");
    // 顶部越界半截图片 [884, 87, 1124, 208] 在消息列表 [580, 188, 1194, 665] 内仅余 20px 高度，必须跳过截图
    assert!(
        rich[0].image_rects.is_empty(),
        "越过标题栏且可见高度不足 24px 的图片不应产生截切坐标: {:?}",
        rich[0].image_rects
    );
    assert_eq!(rich[1].image_rects, vec![[884, 291, 1124, 412]]);
    assert_eq!(rich[2].image_rects, vec![[884, 495, 1124, 616]]);

    let struct_tree = load_fixture_tree(include_str!("fixtures/live_02_struct_scrolled.json"));
    let struct_rich =
        extract_rich_messages_from_tree(&struct_tree, struct_tree.rect, false, "Demo测试群");
    assert!(!struct_rich.is_empty());
    // [640, 89, 790, 240] 原高 151px、视口交集高 52px（不足原高一半），同样必须跳过
    assert!(
        struct_rich[0].image_rects.is_empty(),
        "交集高度不足原高一半的半截图片不应输出截切坐标: {:?}",
        struct_rich[0].image_rects
    );

    // 构造可见高度超过一半（112px >= 80px）的轻度越顶图片，应按消息列表上沿 188 裁剪输出
    let mut partial_tree = struct_tree;
    clip_first_image_to_partial(&mut partial_tree, [640, 140, 790, 300]);
    let partial_rich =
        extract_rich_messages_from_tree(&partial_tree, partial_tree.rect, false, "Demo测试群");
    assert_eq!(partial_rich[0].image_rects, vec![[640, 188, 790, 300]]);
}

fn clip_first_image_to_partial(node: &mut UiNodeSnapshot, new_rect: [i32; 4]) -> bool {
    if node.control_type == "Image" && node.name == "图片" && !node.is_offscreen {
        node.rect = new_rect;
        return true;
    }
    for child in &mut node.children {
        if clip_first_image_to_partial(child, new_rect) {
            return true;
        }
    }
    false
}

#[test]
fn replays_03_private_chat_tree_into_four_messages() {
    let tree = load_fixture_tree(include_str!("fixtures/live_03_private_chat.json"));
    let msgs = extract_messages_from_tree(&tree, tree.rect, false, "测试好友A");

    assert_eq!(
        msgs.len(),
        4,
        "私聊消息列表应解析出 4 条消息，实际: {msgs:?}"
    );
    assert!(msgs.iter().all(|m| m.sender == "测试机器人" && m.is_self));
    assert_eq!(msgs[0].timestamp_hint.as_deref(), Some("04:35"));
    assert_eq!(msgs[2].timestamp_hint.as_deref(), Some("10:33"));
    assert!(msgs[3].content.contains("[GPT-QA-PRIVATE-id]"));
}

#[test]
fn replays_00_main_tree_extracts_only_four_real_sessions() {
    let tree = load_fixture_tree(include_str!("fixtures/live_00_main_tree.json"));
    let sessions = extract_sessions_from_tree(&tree);
    let titles: Vec<&str> = sessions.iter().map(|s| s.title.as_str()).collect();

    assert_eq!(
        titles,
        vec!["示例讨论群", "测试好友A", "示例资料群", "Demo测试群"]
    );
    assert!(sessions.iter().all(|s| is_valid_session_title(&s.title)));
}

#[test]
fn tracker_rebinds_outbound_stub_uid_to_polled_self_uid_and_dedups_annotate() {
    let mut tracker = MessageTracker::new(vec![]);
    tracker.record_outbound("  [GPT-QA-REBIND]  hello   world ");
    tracker.register_confirmed_outbound(OutboundStubBinding {
        onebot_message_id: 7001,
        stub_uid: "stub-7001".to_string(),
        norm_text: "[GPT-QA-REBIND] hello world".to_string(),
        media_token: None,
        ..Default::default()
    });

    let self_poll = ChatMessage {
        message_id: "ui-abc-123".to_string(),
        group_name: "Demo测试群".to_string(),
        sender: "匿名自发".to_string(),
        content: "[GPT-QA-REBIND]\nhello world".to_string(),
        timestamp_hint: Some("11:50".to_string()),
        is_self: true,
        ..Default::default()
    };
    let peer_same = ChatMessage {
        message_id: "ui-peer-456".to_string(),
        group_name: "Demo测试群".to_string(),
        sender: "群友A".to_string(),
        content: "[GPT-QA-REBIND] hello world".to_string(),
        is_self: false,
        ..Default::default()
    };

    let mut batch = vec![self_poll.clone(), peer_same.clone()];
    let rebound = tracker.reconcile_polled_self_messages(&mut batch);
    assert_eq!(
        rebound,
        vec![(7001, "ui-abc-123".to_string(), Some("11:50".to_string()))]
    );
    assert_eq!(batch[0].onebot_message_id, Some(7001));
    assert!(batch[0].is_self);
    assert_eq!(batch[1].onebot_message_id, None);
    assert!(!batch[1].is_self);

    // 同一轮次随后经过 observe_poll：本端回显不得重复入站，而群友相同正文应放行入站
    let (annotated_poll, new_incoming) = tracker.observe_poll(batch);
    assert!(annotated_poll[0].is_self);
    assert_eq!(annotated_poll[0].onebot_message_id, Some(7001));
    assert_eq!(new_incoming.len(), 1);
    assert_eq!(new_incoming[0].message_id, "ui-peer-456");

    // 重复调用 reconcile_polled_self_messages 对已建立映射的 ui_uid 保持幂等重绑定
    let mut repeat = vec![self_poll.clone()];
    let repeat_rebound = tracker.reconcile_polled_self_messages(&mut repeat);
    assert_eq!(
        repeat_rebound,
        vec![(7001, "ui-abc-123".to_string(), Some("11:50".to_string()))]
    );
    assert_eq!(repeat[0].onebot_message_id, Some(7001));

    // annotate_only 应把已别名化的 ui_uid 改写为 stub_uid 并去除重复占位条目
    let stub_dup = ChatMessage {
        message_id: "stub-7001".to_string(),
        group_name: "Demo测试群".to_string(),
        sender: "BotSelf".to_string(),
        content: "[GPT-QA-REBIND] hello world".to_string(),
        onebot_message_id: Some(7001),
        is_self: true,
        ..Default::default()
    };
    let deduped = tracker.annotate_only(vec![self_poll, stub_dup]);
    assert_eq!(deduped.len(), 1);
    assert_eq!(deduped[0].message_id, "stub-7001");
    assert_eq!(deduped[0].onebot_message_id, Some(7001));
    assert!(deduped[0].is_self);
}

#[test]
fn tracker_merges_file_and_video_status_transitions_into_single_row_and_separates_consecutive_sends(
) {
    use common_types::{ChatTargetKind, MessageSegment};
    use storage::{RecordMessageInput, Storage};

    let storage = Storage::open(":memory:").expect("memory db");
    let mut tracker = MessageTracker::new(vec!["测试机器人".to_string()]);

    // 第 1 次发送文件：入库占位行并登记媒体文件名令牌
    let id1 = storage
        .record_message(&RecordMessageInput {
            message_uid: "stub-file-1".to_string(),
            group_name: "Demo测试群".to_string(),
            group_id: 100000001,
            user_id: 10001,
            sender_name: "测试机器人".to_string(),
            raw_message: "[文件:proto_v4_live_verify.txt]".to_string(),
            is_self: true,
            timestamp_hint: Some("08:22".to_string()),
            captured_at_ms: 1_794_788_558_058,
            chat_type: ChatTargetKind::Group,
            segments: vec![MessageSegment::File {
                file: ".runtime/staging_uploads/a/proto_v4_live_verify.txt".to_string(),
                name: Some("proto_v4_live_verify.txt".to_string()),
                size: Some(70),
            }],
        })
        .expect("insert stub 1");
    tracker.register_confirmed_outbound(OutboundStubBinding {
        onebot_message_id: id1,
        stub_uid: "stub-file-1".to_string(),
        norm_text: "proto_v4_live_verify.txt".to_string(),
        media_token: Some("proto_v4_live_verify.txt".to_string()),
        ..Default::default()
    });

    // 状态 1：“正在发送”瞬态气泡命中 id1
    let mut poll_sending = vec![ChatMessage {
        message_id: "ui-file-sending-1".to_string(),
        group_name: "Demo测试群".to_string(),
        sender: "测试机器人".to_string(),
        content: "proto_v4_live_verify.txt\n正在发送".to_string(),
        timestamp_hint: Some("08:22".to_string()),
        is_self: true,
        target_kind: ChatTargetKind::Group,
        ..Default::default()
    }];
    let rebound1 = tracker.reconcile_polled_self_messages(&mut poll_sending);
    assert_eq!(
        rebound1,
        vec![(
            id1,
            "ui-file-sending-1".to_string(),
            Some("08:22".to_string())
        )]
    );
    storage
        .reconcile_outbound_stub_uid(id1, "ui-file-sending-1", Some("08:22"))
        .expect("reconcile sending uid");

    // 状态 2：“70.00 B 已发送”稳态气泡仍命中同一个 id1 并就地合库（保持 1 条记录）
    let mut poll_settled = vec![ChatMessage {
        message_id: "ui-file-settled-1".to_string(),
        group_name: "Demo测试群".to_string(),
        sender: "测试机器人".to_string(),
        content: "proto_v4_live_verify.txt\n70.00\u{2009}B   已发送".to_string(),
        timestamp_hint: Some("08:22".to_string()),
        is_self: true,
        target_kind: ChatTargetKind::Group,
        segments: vec![MessageSegment::File {
            file: "proto_v4_live_verify.txt".to_string(),
            name: Some("proto_v4_live_verify.txt".to_string()),
            size: Some(70),
        }],
        ..Default::default()
    }];
    let rebound2 = tracker.reconcile_polled_self_messages(&mut poll_settled);
    assert_eq!(
        rebound2,
        vec![(
            id1,
            "ui-file-settled-1".to_string(),
            Some("08:22".to_string())
        )]
    );
    storage
        .reconcile_outbound_stub_uid(id1, "ui-file-settled-1", Some("08:22"))
        .expect("reconcile settled uid");

    let rows_after_first = storage
        .list_recent_messages("Demo测试群", 10)
        .expect("list rows 1");
    assert_eq!(rows_after_first.len(), 1);
    assert_eq!(rows_after_first[0].onebot_message_id, id1);
    assert_eq!(rows_after_first[0].message_uid, "ui-file-settled-1");

    // 第 2 次连续发送同名文件：必须匹配到新的 id2，最终保留 2 条独立记录
    let id2 = storage
        .record_message(&RecordMessageInput {
            message_uid: "stub-file-2".to_string(),
            group_name: "Demo测试群".to_string(),
            group_id: 100000001,
            user_id: 10001,
            sender_name: "测试机器人".to_string(),
            raw_message: "[文件:proto_v4_live_verify.txt]".to_string(),
            is_self: true,
            timestamp_hint: Some("08:23".to_string()),
            captured_at_ms: 1_794_788_565_000,
            chat_type: ChatTargetKind::Group,
            segments: vec![MessageSegment::File {
                file: ".runtime/staging_uploads/a/proto_v4_live_verify.txt".to_string(),
                name: Some("proto_v4_live_verify.txt".to_string()),
                size: Some(70),
            }],
        })
        .expect("insert stub 2");
    assert_ne!(id1, id2);
    tracker.register_confirmed_outbound(OutboundStubBinding {
        onebot_message_id: id2,
        stub_uid: "stub-file-2".to_string(),
        norm_text: "proto_v4_live_verify.txt".to_string(),
        media_token: Some("proto_v4_live_verify.txt".to_string()),
        ..Default::default()
    });

    let mut poll_two = vec![
        poll_settled[0].clone(),
        ChatMessage {
            message_id: "ui-file-sending-2".to_string(),
            group_name: "Demo测试群".to_string(),
            sender: "测试机器人".to_string(),
            content: "proto_v4_live_verify.txt\n正在发送".to_string(),
            timestamp_hint: Some("08:23".to_string()),
            is_self: true,
            target_kind: ChatTargetKind::Group,
            ..Default::default()
        },
    ];
    let rebound_two = tracker.reconcile_polled_self_messages(&mut poll_two);
    assert_eq!(rebound_two.len(), 2);
    assert_eq!(poll_two[0].onebot_message_id, Some(id1));
    assert_eq!(poll_two[1].onebot_message_id, Some(id2));
    storage
        .reconcile_outbound_stub_uid(id2, "ui-file-sending-2", Some("08:23"))
        .expect("reconcile second sending uid");

    let mut poll_two_settled = vec![
        poll_two[0].clone(),
        ChatMessage {
            message_id: "ui-file-settled-2".to_string(),
            group_name: "Demo测试群".to_string(),
            sender: "测试机器人".to_string(),
            content: "proto_v4_live_verify.txt\n70.00\u{2009}B   已发送".to_string(),
            timestamp_hint: Some("08:23".to_string()),
            is_self: true,
            target_kind: ChatTargetKind::Group,
            ..Default::default()
        },
    ];
    let rebound_two_settled = tracker.reconcile_polled_self_messages(&mut poll_two_settled);
    assert_eq!(rebound_two_settled.len(), 2);
    assert_eq!(poll_two_settled[0].onebot_message_id, Some(id1));
    assert_eq!(poll_two_settled[1].onebot_message_id, Some(id2));
    let (_annotated, new_incoming) = tracker.observe_poll(poll_two_settled);
    assert!(new_incoming.is_empty());
    storage
        .reconcile_outbound_stub_uid(id2, "ui-file-settled-2", Some("08:23"))
        .expect("reconcile second settled uid");

    let rows_final = storage
        .list_recent_messages("Demo测试群", 10)
        .expect("list rows final");
    assert_eq!(rows_final.len(), 2);
    assert_eq!(rows_final[0].onebot_message_id, id1);
    assert_eq!(rows_final[1].onebot_message_id, id2);
    assert_eq!(rows_final[1].message_uid, "ui-file-settled-2");
}

#[test]
fn reproduces_bug_live_v4_003_no_historical_hijack_and_zero_percent_merge() {
    use common_types::{ChatTargetKind, MessageSegment};
    use storage::{RecordMessageInput, Storage};

    let storage = Storage::open(":memory:").expect("memory db");
    let mut tracker = MessageTracker::new(vec!["测试机器人".to_string()]);

    // 历史可见区已存在旧的同文件名视频气泡，且服务重启后已通过 seed_baseline 进入 seen_ids
    let old_uid = "09badb9f-historical-video";
    let old_msg = ChatMessage {
        message_id: old_uid.to_string(),
        group_name: "Demo测试群".to_string(),
        sender: "测试机器人".to_string(),
        content: "[图片]\nproto_v4_live_video.mp4\n7.69\u{2009}KB 已发送\n[图片]".to_string(),
        timestamp_hint: Some("08:24".to_string()),
        is_self: true,
        captured_at_ms: 1_794_788_500_000,
        target_kind: ChatTargetKind::Group,
        segments: vec![MessageSegment::Video {
            file: "proto_v4_live_video.mp4".to_string(),
            url: None,
        }],
        ..Default::default()
    };
    let old_id = storage
        .record_message(&RecordMessageInput {
            message_uid: old_uid.to_string(),
            group_name: "Demo测试群".to_string(),
            group_id: 100000001,
            user_id: 10001,
            sender_name: "测试机器人".to_string(),
            raw_message: "[CQ:video,file=proto_v4_live_video.mp4]".to_string(),
            is_self: true,
            timestamp_hint: Some("08:24".to_string()),
            captured_at_ms: 1_794_788_500_000,
            chat_type: ChatTargetKind::Group,
            segments: vec![MessageSegment::Video {
                file: "proto_v4_live_video.mp4".to_string(),
                url: None,
            }],
        })
        .expect("insert historical video");
    tracker.seed_baseline(std::slice::from_ref(&old_msg));

    // 再次发送同名新视频，分配独立 onebot_message_id != old_id
    let new_id = storage
        .record_message(&RecordMessageInput {
            message_uid: "stub-new-video".to_string(),
            group_name: "Demo测试群".to_string(),
            group_id: 100000001,
            user_id: 10001,
            sender_name: "测试机器人".to_string(),
            raw_message: "[CQ:video,file=proto_v4_live_video.mp4]".to_string(),
            is_self: true,
            timestamp_hint: Some("08:51".to_string()),
            captured_at_ms: 1_794_790_000_000,
            chat_type: ChatTargetKind::Group,
            segments: vec![MessageSegment::Video {
                file: ".runtime/staging_uploads/v/proto_v4_live_video.mp4".to_string(),
                url: None,
            }],
        })
        .expect("insert new video stub");
    assert_ne!(new_id, old_id);
    tracker.register_confirmed_outbound(OutboundStubBinding {
        onebot_message_id: new_id,
        stub_uid: "stub-new-video".to_string(),
        norm_text: "proto_v4_live_video.mp4".to_string(),
        media_token: Some("proto_v4_live_video.mp4".to_string()),
        ..Default::default()
    });

    // 轮询 1：0% 进度新气泡到达，旧历史气泡绝不能抢占 new_id
    let new_0pct = ChatMessage {
        message_id: "9156dd8b-new-0pct".to_string(),
        group_name: "Demo测试群".to_string(),
        sender: "测试机器人".to_string(),
        content: "[图片]\nproto_v4_live_video.mp4\n10.97\u{2009}KB\n0%\n[图片]".to_string(),
        timestamp_hint: Some("08:51".to_string()),
        is_self: true,
        target_kind: ChatTargetKind::Group,
        segments: vec![MessageSegment::Video {
            file: "proto_v4_live_video.mp4".to_string(),
            url: None,
        }],
        ..Default::default()
    };
    let mut poll_1 = vec![old_msg.clone(), new_0pct];
    let rebound_1 = tracker.reconcile_polled_self_messages(&mut poll_1);
    assert_eq!(
        rebound_1,
        vec![(
            new_id,
            "9156dd8b-new-0pct".to_string(),
            Some("08:51".to_string())
        )]
    );
    assert_eq!(poll_1[0].onebot_message_id, None);
    assert_eq!(poll_1[1].onebot_message_id, Some(new_id));
    let (_ann1, inc1) = tracker.observe_poll(poll_1);
    assert!(inc1.is_empty());
    storage
        .reconcile_outbound_stub_uid(new_id, "9156dd8b-new-0pct", Some("08:51"))
        .expect("reconcile 0% uid");

    // 轮询 2：0% 跃迁至 10.97 KB 已发送，仍归并回同一条 new_id 且不篡改 old_id
    let new_settled = ChatMessage {
        message_id: "c97c5ede-new-settled".to_string(),
        group_name: "Demo测试群".to_string(),
        sender: "测试机器人".to_string(),
        content: "[图片]\nproto_v4_live_video.mp4\n10.97\u{2009}KB 已发送\n[图片]".to_string(),
        timestamp_hint: Some("08:51".to_string()),
        is_self: true,
        target_kind: ChatTargetKind::Group,
        segments: vec![MessageSegment::Video {
            file: "proto_v4_live_video.mp4".to_string(),
            url: None,
        }],
        ..Default::default()
    };
    let mut poll_2 = vec![old_msg, new_settled];
    let rebound_2 = tracker.reconcile_polled_self_messages(&mut poll_2);
    assert_eq!(
        rebound_2,
        vec![(
            new_id,
            "c97c5ede-new-settled".to_string(),
            Some("08:51".to_string())
        )]
    );
    assert_eq!(poll_2[0].onebot_message_id, None);
    assert_eq!(poll_2[1].onebot_message_id, Some(new_id));
    let (_ann2, inc2) = tracker.observe_poll(poll_2);
    assert!(inc2.is_empty());
    storage
        .reconcile_outbound_stub_uid(new_id, "c97c5ede-new-settled", Some("08:51"))
        .expect("reconcile settled uid");

    let rows = storage
        .list_recent_messages("Demo测试群", 10)
        .expect("list rows");
    assert_eq!(rows.len(), 2, "must contain strictly 2 rows (old + 1 new)");
    assert_eq!(rows[0].onebot_message_id, old_id);
    assert_eq!(rows[0].message_uid, old_uid);
    assert_eq!(rows[1].onebot_message_id, new_id);
    assert_eq!(rows[1].message_uid, "c97c5ede-new-settled");
}

fn make_leaf_node(
    control_type: &str,
    name: &str,
    rect: [i32; 4],
    is_offscreen: bool,
) -> UiNodeSnapshot {
    UiNodeSnapshot {
        control_type: control_type.to_string(),
        localized_control_type: String::new(),
        name: name.to_string(),
        automation_id: String::new(),
        class_name: String::new(),
        rect,
        is_enabled: true,
        is_offscreen,
        patterns: Vec::new(),
        value: None,
        children: Vec::new(),
    }
}

fn build_flat_media_tree(offscreen: bool) -> UiNodeSnapshot {
    let avatar_rect = if offscreen {
        [0, 0, 0, 0]
    } else {
        [1210, 250, 1242, 282]
    };
    let text_rect = |l: i32, t: i32, r: i32, b: i32| {
        if offscreen {
            [0, 0, 0, 0]
        } else {
            [l, t, r, b]
        }
    };
    let ml_children = vec![
        // win[39] 类折行文件卡片（is_offscreen + [0,0,0,0] 与屏内同坐标对照）
        make_leaf_node("Group", "测试机器人", avatar_rect, offscreen),
        UiNodeSnapshot {
            control_type: "Group".to_string(),
            localized_control_type: String::new(),
            name: String::new(),
            automation_id: String::new(),
            class_name: String::new(),
            rect: text_rect(960, 250, 1180, 320),
            is_enabled: true,
            is_offscreen: offscreen,
            patterns: Vec::new(),
            value: None,
            children: vec![
                make_leaf_node(
                    "Text",
                    "proto_v4_live_ve",
                    text_rect(970, 260, 1080, 276),
                    offscreen,
                ),
                make_leaf_node(
                    "Text",
                    "rify.txt",
                    text_rect(1080, 260, 1125, 276),
                    offscreen,
                ),
                make_leaf_node(
                    "Text",
                    "70.00\u{2009}B   已发送",
                    text_rect(970, 290, 1070, 306),
                    offscreen,
                ),
            ],
        },
        // win[41] 类折行视频卡片（含前后缩略/播放 Image 节点）
        make_leaf_node("Group", "测试机器人", avatar_rect, offscreen),
        UiNodeSnapshot {
            control_type: "Group".to_string(),
            localized_control_type: String::new(),
            name: String::new(),
            automation_id: String::new(),
            class_name: String::new(),
            rect: text_rect(960, 340, 1180, 500),
            is_enabled: true,
            is_offscreen: offscreen,
            patterns: Vec::new(),
            value: None,
            children: vec![
                make_leaf_node("Image", "图片", text_rect(965, 345, 1175, 445), offscreen),
                make_leaf_node(
                    "Text",
                    "proto_v4_live_vid",
                    text_rect(970, 452, 1080, 468),
                    offscreen,
                ),
                make_leaf_node("Text", "eo.mp4", text_rect(1080, 452, 1125, 468), offscreen),
                make_leaf_node(
                    "Text",
                    "7.69\u{2009}KB 已发送",
                    text_rect(970, 474, 1070, 490),
                    offscreen,
                ),
                make_leaf_node("Image", "图片", text_rect(1050, 385, 1090, 425), offscreen),
                make_leaf_node("Image", "图片", text_rect(1140, 460, 1170, 490), offscreen),
            ],
        },
        // 视口底端可见消息锚点（模拟真实历史卡片滚出屏外时下方仍存在当前在屏消息）
        make_leaf_node("Group", "群友A", [660, 560, 692, 592], false),
        make_leaf_node("Text", "anchor msg", [704, 560, 820, 588], false),
    ];

    UiNodeSnapshot {
        control_type: "Pane".to_string(),
        localized_control_type: String::new(),
        name: "QQ".to_string(),
        automation_id: String::new(),
        class_name: "Chrome_WidgetWin_1".to_string(),
        rect: [343, 101, 1446, 880],
        is_enabled: true,
        is_offscreen: false,
        patterns: Vec::new(),
        value: None,
        children: vec![UiNodeSnapshot {
            control_type: "Window".to_string(),
            localized_control_type: String::new(),
            name: "消息列表".to_string(),
            automation_id: "ml-root".to_string(),
            class_name: String::new(),
            rect: [648, 192, 1262, 669],
            is_enabled: true,
            is_offscreen: false,
            patterns: Vec::new(),
            value: None,
            children: ml_children,
        }],
    }
}

#[test]
fn reproduces_bug_live_v4_004_offscreen_zero_rect_wrapped_file_and_video_keep_identical_uid() {
    let onscreen_tree = build_flat_media_tree(false);
    let offscreen_tree = build_flat_media_tree(true);

    let onscreen =
        extract_messages_from_tree(&onscreen_tree, onscreen_tree.rect, false, "Demo测试群");
    let offscreen =
        extract_messages_from_tree(&offscreen_tree, offscreen_tree.rect, false, "Demo测试群");

    assert_eq!(onscreen.len(), 3, "expected 3 onscreen messages");
    assert_eq!(offscreen.len(), 3, "expected 3 offscreen+anchor messages");

    assert_eq!(offscreen[0].content, onscreen[0].content);
    assert_eq!(offscreen[0].message_id, onscreen[0].message_id);
    assert!(offscreen[0].content.contains("proto_v4_live_verify.txt"));

    assert_eq!(offscreen[1].content, onscreen[1].content);
    assert_eq!(offscreen[1].message_id, onscreen[1].message_id);
    assert!(offscreen[1].content.contains("proto_v4_live_video.mp4"));
}

#[test]
fn replays_03_tree_native_quote_bubble_and_unmatched_quote_at_isolation() {
    use common_types::{BotLoginInfo, ChatTargetKind, MessageSegment};
    use qqnt_driver::driver::enrich_parsed_messages_with_storage;
    use std::path::Path;
    use storage::{RecordMessageInput, Storage};

    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/public/03_tree.json");
    let raw_text = std::fs::read_to_string(&path).expect("读取 03_tree.json");
    let tree = load_fixture_tree(&raw_text);
    let mut rich = extract_rich_messages_from_tree(&tree, tree.rect, false, "Demo测试群");
    let quote_idx = rich
        .iter()
        .position(|item| item.message.content.contains("[LIVE-03-REPLY-IMG]"))
        .expect("03_tree 必须包含原生引用回复气泡");
    assert!(
        rich[quote_idx]
            .message
            .content
            .starts_with("「回复 @测试机器人: "),
        "原生引用气泡应规整为引用预览前缀: {:?}",
        rich[quote_idx].message.content
    );
    assert!(rich[quote_idx]
        .message
        .content
        .contains("[LIVE-02-FACE-AT]」\n[LIVE-03-REPLY-IMG]"));

    let bot_login = BotLoginInfo {
        user_id: 200000003,
        nickname: "测试机器人".to_string(),
        real_qq_id: Some(200000003),
    };

    // 未入库被引用原文时，引文头保留为纯文本且引文内 @测试好友A 不产生 At 段
    let empty_storage = Storage::open(":memory:").expect("memory db");
    let mut unmatched_slice = vec![rich[quote_idx].clone()];
    enrich_parsed_messages_with_storage(
        &empty_storage,
        "Demo测试群",
        ChatTargetKind::Group,
        &bot_login,
        &mut unmatched_slice,
    );
    assert!(unmatched_slice[0].message.reply_to_message_id.is_none());
    assert!(
        !unmatched_slice[0]
            .message
            .segments
            .iter()
            .any(|seg| matches!(
                seg,
                MessageSegment::Reply { .. } | MessageSegment::At { .. }
            )),
        "未命中历史时不得从引文中解析出 Reply 或 At 段: {:?}",
        unmatched_slice[0].message.segments
    );

    // 写入被引用原消息后再次丰富，应精确生成 Reply 段并标记 reply_targets_self=true
    let seeded_storage = Storage::open(":memory:").expect("memory db");
    let quoted_id = seeded_storage
        .record_message(&RecordMessageInput {
            message_uid: "uid-quoted-02".to_string(),
            group_name: "Demo测试群".to_string(),
            group_id: 100000001,
            user_id: 200000003,
            sender_name: "测试机器人".to_string(),
            raw_message: "@测试好友A [LIVE-02-FACE-AT]".to_string(),
            is_self: true,
            timestamp_hint: None,
            captured_at_ms: 1_794_740_000_000,
            chat_type: ChatTargetKind::Group,
            segments: vec![],
        })
        .expect("insert quoted message");
    enrich_parsed_messages_with_storage(
        &seeded_storage,
        "Demo测试群",
        ChatTargetKind::Group,
        &bot_login,
        &mut rich[quote_idx..=quote_idx],
    );
    assert_eq!(rich[quote_idx].message.reply_to_message_id, Some(quoted_id));
    assert!(rich[quote_idx].message.reply_targets_self);
    assert_eq!(
        rich[quote_idx].message.segments.first(),
        Some(&MessageSegment::Reply {
            id: quoted_id.to_string(),
        })
    );
}

#[test]
fn replays_06_tree_native_forward_card_and_reuses_forward_id_idempotently() {
    use common_types::{BotLoginInfo, ChatTargetKind, MessageSegment};
    use qqnt_driver::driver::enrich_parsed_messages_with_storage;
    use std::path::Path;
    use storage::Storage;

    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/public/06_tree.json");
    let raw_text = std::fs::read_to_string(&path).expect("读取 06_tree.json");
    let tree = load_fixture_tree(&raw_text);
    let rich = extract_rich_messages_from_tree(&tree, tree.rect, false, "Demo测试群");
    let fwd_idx = rich
        .iter()
        .position(|item| {
            item.message.content.contains("群聊的聊天记录")
                && item.message.content.contains("查看2条转发消息")
        })
        .expect("06_tree 必须包含原生合并转发气泡");

    let storage = Storage::open(":memory:").expect("memory db");
    let bot_login = BotLoginInfo {
        user_id: 200000003,
        nickname: "测试机器人".to_string(),
        real_qq_id: Some(200000003),
    };

    let mut first = vec![rich[fwd_idx].clone()];
    enrich_parsed_messages_with_storage(
        &storage,
        "Demo测试群",
        ChatTargetKind::Group,
        &bot_login,
        &mut first,
    );
    let fwd_id = match first[0].message.segments.as_slice() {
        [MessageSegment::Forward { id }] => id.clone(),
        other => panic!("期待单个 Forward 消息段，实际为 {other:?}"),
    };
    assert!(fwd_id.starts_with("fwd-"));

    // 同一气泡重复轮询入库时必须幂等复用同一个 fwd-* 编号
    let mut second = vec![rich[fwd_idx].clone()];
    enrich_parsed_messages_with_storage(
        &storage,
        "Demo测试群",
        ChatTargetKind::Group,
        &bot_login,
        &mut second,
    );
    assert_eq!(
        second[0].message.segments,
        vec![MessageSegment::Forward { id: fwd_id.clone() }]
    );

    let nodes_raw = storage
        .get_meta(&format!("forward:{fwd_id}"))
        .expect("read forward nodes")
        .expect("forward nodes must exist");
    let nodes: serde_json::Value = serde_json::from_str(&nodes_raw).expect("valid forward json");
    let arr = nodes.as_array().expect("nodes array");
    assert_eq!(arr.len(), 2);
    assert_eq!(
        arr[0]["data"]["content"][0]["data"]["text"],
        "[LIVE-06-ANCHOR-1]"
    );
    assert_eq!(
        arr[1]["data"]["content"][0]["data"]["text"],
        "[LIVE-06-ANCHOR-2]"
    );
}

#[test]
fn reproduces_bug_live_v6_001_top_clipped_unknown_sticker_yields_zero_ghost_rows() {
    let win_rect = [342, 100, 1446, 880];
    let ml_rect = [648, 192, 1262, 669];
    let clipped_sticker_row = UiNodeSnapshot {
        control_type: "Group".to_string(),
        rect: [708, 107, 775, 210],
        is_enabled: true,
        children: vec![UiNodeSnapshot {
            control_type: "Image".to_string(),
            name: "图片".to_string(),
            rect: [708, 107, 775, 210],
            is_enabled: true,
            ..Default::default()
        }],
        ..Default::default()
    };
    let recovered_row = UiNodeSnapshot {
        control_type: "Group".to_string(),
        rect: [660, 140, 820, 260],
        is_enabled: true,
        children: vec![
            UiNodeSnapshot {
                control_type: "Text".to_string(),
                name: "09:31".to_string(),
                is_enabled: true,
                is_offscreen: true,
                ..Default::default()
            },
            UiNodeSnapshot {
                control_type: "Button".to_string(),
                name: "测试好友A的头像".to_string(),
                is_enabled: true,
                is_offscreen: true,
                ..Default::default()
            },
            UiNodeSnapshot {
                control_type: "Image".to_string(),
                name: "图片".to_string(),
                rect: [708, 145, 775, 255],
                is_enabled: true,
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    let tree = UiNodeSnapshot {
        control_type: "Pane".to_string(),
        name: "QQ".to_string(),
        rect: win_rect,
        is_enabled: true,
        children: vec![UiNodeSnapshot {
            control_type: "Window".to_string(),
            name: "消息列表".to_string(),
            rect: ml_rect,
            is_enabled: true,
            children: vec![clipped_sticker_row, recovered_row],
            ..Default::default()
        }],
        ..Default::default()
    };
    let msgs = extract_messages_from_tree(&tree, win_rect, false, "Demo测试群");
    assert_eq!(
        msgs.len(),
        1,
        "顶边越出历史视口且无发件人归属的半裁贴图不得产生 Unknown 幽灵行，仅保留已归属行: {msgs:?}"
    );
    assert_eq!(msgs[0].sender, "测试好友A");
    assert_eq!(msgs[0].timestamp_hint.as_deref(), Some("09:31"));
}
