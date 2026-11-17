//! Round-2 真实客户端 UIA 树与时间戳分裂证据回放单测

use std::path::Path;

use common_types::{ChatMessage, UiNodeSnapshot};

use super::{extract_messages_from_tree, has_message_list_container, MessageTracker};

fn load_reverify_tree(file_name: &str) -> UiNodeSnapshot {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/public")
        .join(file_name);
    let raw_text = std::fs::read_to_string(&path).expect("read reverify tree json");
    let raw_json: serde_json::Value = serde_json::from_str(&raw_text).expect("parse wrapper json");
    serde_json::from_value(raw_json["response"].clone()).expect("parse UiNodeSnapshot response")
}

#[test]
fn reverify_07_blank_tree_rejects_message_list_container_and_yields_empty_messages() {
    let tree = load_reverify_tree("07-blank-tree.json");
    assert!(!has_message_list_container(&tree, tree.rect, false));
    let msgs = extract_messages_from_tree(&tree, tree.rect, false, "Demo测试群");
    assert!(msgs.is_empty(), "blank tree should extract zero messages");
}

#[test]
fn reverify_07_user_recovered_tree_has_message_list_container_and_extracts_messages() {
    let tree = load_reverify_tree("07-user-recovered-tree.json");
    assert!(has_message_list_container(&tree, tree.rect, false));
    let msgs = extract_messages_from_tree(&tree, tree.rect, false, "Demo测试群");
    assert!(!msgs.is_empty(), "active chat tree should extract messages");
}

#[test]
fn reverify_timestamp_split_null_to_hhmm_reuses_same_id_and_uid() {
    let mut tracker = MessageTracker::new(vec!["测试机器人".to_string()]);
    let initial = ChatMessage {
        message_id: "uid-null-1".to_string(),
        group_name: "Demo测试群".to_string(),
        sender: "测试机器人".to_string(),
        content: "[REVERIFY-131622-WS-SLOW]".to_string(),
        timestamp_hint: None,
        is_self: true,
        captured_at_ms: 1_700_000_000_000,
        ..Default::default()
    };
    tracker.seed_baseline(&[initial]);

    let split_hhmm = ChatMessage {
        message_id: "uid-hhmm-2".to_string(),
        group_name: "Demo测试群".to_string(),
        sender: "测试机器人".to_string(),
        content: "[REVERIFY-131622-WS-SLOW]".to_string(),
        timestamp_hint: Some("13:16".to_string()),
        is_self: true,
        captured_at_ms: 1_700_000_060_000,
        ..Default::default()
    };
    let annotated = tracker.annotate_only(vec![split_hhmm.clone()]);
    assert_eq!(annotated[0].message_id, "uid-null-1");

    let (polled_all, new_incoming) = tracker.observe_poll(vec![split_hhmm]);
    assert_eq!(polled_all[0].message_id, "uid-null-1");
    assert!(new_incoming.is_empty());
}

#[test]
fn replays_04_tree_merges_wrapped_filename_without_space() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/public/04_tree.json");
    let raw_text = std::fs::read_to_string(&path).expect("read 04_tree.json");
    let tree: UiNodeSnapshot = serde_json::from_str(&raw_text).expect("parse 04_tree.json");
    let rich = super::extract_rich_messages_from_tree(&tree, tree.rect, false, "Demo测试群");
    let file_msg = rich
        .iter()
        .find(|item| item.message.content.contains("sample_live_verify.txt"))
        .expect("wrapped file name sample_live_ve + rify.txt must merge without space");
    assert!(!file_msg
        .message
        .content
        .contains("sample_live_ve\nrify.txt"));
    assert!(!file_msg.message.content.contains("sample_live_ve rify.txt"));
    assert!(file_msg.message.content.contains("已发送"));
}

#[test]
fn replays_06_probe_bubble_menu_extracts_and_classifies_reaction_notice() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/public/06_probe_bubble_menu.json");
    let raw_text = std::fs::read_to_string(&path).expect("read 06_probe_bubble_menu.json");
    let probe: serde_json::Value = serde_json::from_str(&raw_text).expect("parse probe json");
    let samples = probe["popups"][0]["node_samples"]
        .as_array()
        .expect("node_samples array");

    let list_idx = samples
        .iter()
        .position(|n| n["automation_id"].as_str() == Some("ml-root"))
        .expect("ml-root sample index");

    let sample_to_node = |item: &serde_json::Value| -> UiNodeSnapshot {
        let arr = item["rect"].as_array().expect("rect array");
        let rect = [
            arr[0].as_i64().unwrap_or(0) as i32,
            arr[1].as_i64().unwrap_or(0) as i32,
            arr[2].as_i64().unwrap_or(0) as i32,
            arr[3].as_i64().unwrap_or(0) as i32,
        ];
        UiNodeSnapshot {
            control_type: item["control_type"].as_str().unwrap_or("").to_string(),
            localized_control_type: String::new(),
            name: item["name"].as_str().unwrap_or("").to_string(),
            automation_id: item["automation_id"].as_str().unwrap_or("").to_string(),
            class_name: String::new(),
            rect,
            is_enabled: true,
            is_offscreen: false,
            patterns: Vec::new(),
            value: None,
            children: Vec::new(),
        }
    };

    let mut ml_root = sample_to_node(&samples[list_idx]);
    for item in samples.iter().skip(list_idx + 1) {
        let mut child = sample_to_node(item);
        if child.rect[0] >= ml_root.rect[0] && child.rect[2] <= ml_root.rect[2] + 4 {
            child.children = Vec::new();
            ml_root.children.push(child);
        }
    }

    let root = UiNodeSnapshot {
        control_type: "Pane".to_string(),
        localized_control_type: "窗格".to_string(),
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
            automation_id: String::new(),
            class_name: String::new(),
            rect: ml_root.rect,
            is_enabled: true,
            is_offscreen: false,
            patterns: Vec::new(),
            value: None,
            children: vec![ml_root],
        }],
    };

    let notices = super::extract_system_notices_from_tree(&root, root.rect, false, "Demo测试群");
    let reaction = notices
        .iter()
        .find(|n| n.text.contains("回应了你的"))
        .expect("reaction notice extracted from 06_probe_bubble_menu");
    assert!(reaction.text.contains("测试好友B"));

    let members = vec![common_types::GroupMemberInfo {
        group_id: 999888,
        user_id: 424242,
        real_user_id: Some(424242),
        nickname: "测试好友B".to_string(),
        card: String::new(),
        role: "member".to_string(),
    }];
    let ctx = super::NoticeContext {
        self_id: 10001,
        chat_kind: common_types::ChatTargetKind::Group,
        session_name: "Demo测试群",
        group_id: Some(999888),
        peer_user_id: None,
        members: &members,
    };
    let json_ev = super::parse_notice_text_to_onebot_json(&ctx, &reaction.text, 1_700_000_000);
    assert_eq!(json_ev["post_type"], "notice");
    assert_eq!(json_ev["notice_type"], "group_msg_emoji_like");
    assert_eq!(json_ev["group_id"], 999888);
    assert_eq!(json_ev["user_id"], 424242);
    assert_eq!(json_ev["likes"][0]["emoji_id"], "0");
    assert_eq!(json_ev["likes"][0]["count"], 1);
}
