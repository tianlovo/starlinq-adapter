//! 群聊匹配、安全守卫、群成员侧栏解析与群信息抽屉真实群号提取单元测试

use common_types::{AdapterError, GroupTargetSpec, UiNodeSnapshot};

use super::drawer_inspector::{
    collect_snapshot_candidates, extract_metadata_from_candidates, parse_drawer_metadata,
};
use super::header_matcher::verify_attached_member_guard;
use super::*;

#[test]
fn parses_half_and_full_width_member_counts() {
    assert_eq!(parse_member_count("Demo测试群 (5)"), Some(5));
    assert_eq!(parse_member_count("Demo测试群 (26)"), Some(26));
    assert_eq!(parse_member_count("测试大群 (128)"), Some(128));
    assert_eq!(parse_member_count("测试群（8人）"), Some(8));
    assert_eq!(parse_member_count("小组 (v2) (9人)"), Some(9));
    assert_eq!(parse_member_count("Demo测试群"), None);
}

#[test]
fn member_guard_enforces_strict_below_and_verified_flag() {
    let spec = GroupTargetSpec {
        group_name: "Demo测试群".to_string(),
        strict_below_members: Some(10),
        require_member_count_verified: false,
        ..Default::default()
    };
    assert!(passes_member_guard(&spec, Some(5)).is_ok());
    assert!(passes_member_guard(&spec, Some(9)).is_ok());
    assert!(matches!(
        passes_member_guard(&spec, Some(10)),
        Err(AdapterError::MemberCountExceeded {
            detected: Some(10),
            limit: 10,
            ..
        })
    ));
    assert!(matches!(
        passes_member_guard(&spec, Some(26)),
        Err(AdapterError::MemberCountExceeded {
            detected: Some(26),
            limit: 10,
            ..
        })
    ));
    assert!(passes_member_guard(&spec, None).is_ok());

    let strict_verified = GroupTargetSpec {
        require_member_count_verified: true,
        ..spec
    };
    assert!(matches!(
        passes_member_guard(&strict_verified, None),
        Err(AdapterError::MemberCountExceeded {
            detected: None,
            limit: 10,
            ..
        })
    ));
}

#[test]
fn extracts_sessions_and_header_from_snapshots() {
    let win_rect = [80, 80, 880, 720];
    let session_list = UiNodeSnapshot {
        control_type: "Window".to_string(),
        name: "会话列表".to_string(),
        rect: [90, 150, 320, 620],
        is_enabled: true,
        ..Default::default()
    };
    let session_large = UiNodeSnapshot {
        control_type: "ListItem".to_string(),
        name: "测试大群 (128)".to_string(),
        rect: [98, 165, 310, 200],
        is_enabled: true,
        ..Default::default()
    };
    let session_target = UiNodeSnapshot {
        control_type: "ListItem".to_string(),
        name: "Demo测试群 (5)".to_string(),
        rect: [98, 210, 310, 245],
        is_enabled: true,
        ..Default::default()
    };
    let header_node = UiNodeSnapshot {
        control_type: "Text".to_string(),
        name: "Demo测试群 (5)".to_string(),
        rect: [338, 123, 860, 157],
        is_enabled: true,
        ..Default::default()
    };

    let snapshots = [&session_list, &session_large, &session_target, &header_node];
    let sessions = extract_sessions_from_snapshots(&snapshots, win_rect);
    assert_eq!(sessions.len(), 2);
    assert_eq!(sessions[0].member_count_hint, Some(128));
    assert_eq!(sessions[1].title, "Demo测试群 (5)");
    assert_eq!(sessions[1].member_count_hint, Some(5));

    let (header_title, header_count) =
        read_chat_header_from_snapshots(&snapshots, win_rect, false, None);
    assert_eq!(header_title.as_deref(), Some("Demo测试群 (5)"));
    assert_eq!(header_count, Some(5));
}

#[test]
fn session_extraction_requires_scoped_container_and_rejects_chrome_titles() {
    let win_rect = [80, 80, 880, 720];
    let unscoped_row = UiNodeSnapshot {
        control_type: "ListItem".to_string(),
        name: "Demo测试群 (5)".to_string(),
        rect: [98, 165, 310, 215],
        is_enabled: true,
        ..Default::default()
    };
    assert!(extract_sessions_from_snapshots(&[&unscoped_row], win_rect).is_empty());

    let chrome_samples = [
        "截图 Ctrl + Alt + A",
        "文件",
        "04:35",
        "12:08",
        "昨天",
        "前天",
        "星期一",
        "周日",
        "10-18",
        "2026/11/15",
        "在线状态",
        "消息",
        "消息列表",
        "测试机器人的头像",
        "个性签名: 测试",
        "天气: 晴",
        "[图片]",
    ];
    for label in chrome_samples {
        assert!(
            !is_valid_session_title(label),
            "chrome label must be rejected: {label}"
        );
    }
    assert!(is_valid_session_title("Demo测试群"));
    assert!(is_valid_session_title("示例讨论群"));

    let session_list = UiNodeSnapshot {
        control_type: "Window".to_string(),
        name: "会话列表".to_string(),
        rect: [90, 150, 320, 620],
        is_enabled: true,
        children: vec![
            UiNodeSnapshot {
                control_type: "Group".to_string(),
                name: "Demo测试群\n04:35\n测试机器人：你好".to_string(),
                rect: [92, 160, 318, 224],
                is_enabled: true,
                ..Default::default()
            },
            UiNodeSnapshot {
                control_type: "Group".to_string(),
                name: "04:35".to_string(),
                rect: [92, 230, 318, 290],
                is_enabled: true,
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    let root = UiNodeSnapshot {
        control_type: "Pane".to_string(),
        name: "QQ".to_string(),
        rect: win_rect,
        is_enabled: true,
        children: vec![
            session_list,
            UiNodeSnapshot {
                control_type: "Button".to_string(),
                name: "截图 Ctrl + Alt + A".to_string(),
                rect: [85, 650, 260, 700],
                is_enabled: true,
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    let extracted = extract_sessions_from_tree(&root);
    assert_eq!(extracted.len(), 1);
    assert_eq!(extracted[0].title, "Demo测试群");
}

#[test]
fn read_chat_header_from_tree_flattens_nested_header() {
    let win_rect = [80, 80, 880, 720];
    let tree = UiNodeSnapshot {
        control_type: "Window".to_string(),
        class_name: "Chrome_WidgetWin_1".to_string(),
        rect: win_rect,
        is_enabled: true,
        children: vec![UiNodeSnapshot {
            control_type: "Document".to_string(),
            rect: win_rect,
            is_enabled: true,
            children: vec![UiNodeSnapshot {
                control_type: "Pane".to_string(),
                rect: [338, 120, 860, 160],
                is_enabled: true,
                children: vec![UiNodeSnapshot {
                    control_type: "Text".to_string(),
                    name: "Demo测试群 (5)".to_string(),
                    rect: [340, 123, 600, 155],
                    is_enabled: true,
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        }],
        ..Default::default()
    };

    let (title, count) = read_chat_header_from_tree(&tree, win_rect, false, None);
    assert_eq!(title.as_deref(), Some("Demo测试群 (5)"));
    assert_eq!(count, Some(5));
}

#[test]
fn qqnt_150_dpi_scopes_session_list_and_reads_split_or_sidebar_member_count() {
    let logical_win_rect = [450, 84, 1518, 863];
    let surface_rect = [675, 126, 2277, 1295];

    let root_pane = UiNodeSnapshot {
        control_type: "Pane".to_string(),
        name: "QQ".to_string(),
        class_name: "Chrome_WidgetWin_1".to_string(),
        rect: surface_rect,
        is_enabled: true,
        ..Default::default()
    };
    let toolbar_button = UiNodeSnapshot {
        control_type: "Button".to_string(),
        name: "聊天".to_string(),
        rect: [690, 210, 740, 255],
        is_enabled: true,
        ..Default::default()
    };
    let search_box = UiNodeSnapshot {
        control_type: "Edit".to_string(),
        name: "搜索".to_string(),
        rect: [815, 210, 1060, 240],
        is_enabled: true,
        ..Default::default()
    };
    let session_list = UiNodeSnapshot {
        control_type: "Window".to_string(),
        name: "会话列表".to_string(),
        rect: [759, 264, 1134, 1289],
        is_enabled: true,
        ..Default::default()
    };
    let row_target = UiNodeSnapshot {
        control_type: "Group".to_string(),
        name: "Demo测试群 23:25 群友A: ping".to_string(),
        rect: [765, 280, 1120, 355],
        is_enabled: true,
        ..Default::default()
    };
    let row_large = UiNodeSnapshot {
        control_type: "Group".to_string(),
        name: "测试大群 (128) 23:20 你好".to_string(),
        rect: [765, 360, 1120, 435],
        is_enabled: true,
        ..Default::default()
    };
    let header_button = UiNodeSnapshot {
        control_type: "Button".to_string(),
        name: "Demo测试群".to_string(),
        rect: [1158, 210, 1342, 244],
        is_enabled: true,
        ..Default::default()
    };
    let header_name = UiNodeSnapshot {
        control_type: "Text".to_string(),
        name: "Demo测试群".to_string(),
        rect: [1161, 211, 1298, 242],
        is_enabled: true,
        ..Default::default()
    };
    let header_count = UiNodeSnapshot {
        control_type: "Text".to_string(),
        name: "(9)".to_string(),
        rect: [1305, 211, 1339, 242],
        is_enabled: true,
        ..Default::default()
    };
    let close_button = UiNodeSnapshot {
        control_type: "Button".to_string(),
        name: "关闭".to_string(),
        rect: [2220, 130, 2270, 175],
        is_enabled: true,
        ..Default::default()
    };

    let snapshots = [
        &root_pane,
        &toolbar_button,
        &search_box,
        &session_list,
        &row_target,
        &row_large,
        &header_button,
        &header_name,
        &header_count,
        &close_button,
    ];

    let sessions = extract_sessions_from_snapshots(&snapshots, logical_win_rect);
    assert_eq!(sessions.len(), 2);
    assert!(sessions
        .iter()
        .all(|session| session.title != "聊天" && session.title != "搜索"));
    assert!(sessions
        .iter()
        .any(|session| session.title.contains("Demo测试群")));

    let (title, count) = read_chat_header_from_snapshots(&snapshots, logical_win_rect, false, None);
    assert_eq!(title.as_deref(), Some("Demo测试群"));
    assert_eq!(count, Some(9));

    let sidebar_member_label = UiNodeSnapshot {
        control_type: "Text".to_string(),
        name: "群聊成员 9".to_string(),
        rect: [2026, 480, 2110, 505],
        is_enabled: true,
        ..Default::default()
    };
    let snapshots_sidebar = [
        &root_pane,
        &header_button,
        &header_name,
        &sidebar_member_label,
        &close_button,
    ];
    let (title_sb, count_sb) =
        read_chat_header_from_snapshots(&snapshots_sidebar, logical_win_rect, false, None);
    assert_eq!(title_sb.as_deref(), Some("Demo测试群"));
    assert_eq!(count_sb, Some(9));
}

#[test]
fn attached_member_guard_rejects_unverified_or_oversized_even_without_explicit_flag() {
    let spec = GroupTargetSpec {
        group_name: "Demo测试群".to_string(),
        strict_below_members: Some(10),
        require_member_count_verified: false,
        ..Default::default()
    };
    assert!(verify_attached_member_guard(&spec, Some(9)).is_ok());
    assert!(matches!(
        verify_attached_member_guard(&spec, None),
        Err(AdapterError::MemberCountExceeded {
            detected: None,
            limit: 10,
            ..
        })
    ));
    assert!(matches!(
        verify_attached_member_guard(&spec, Some(10)),
        Err(AdapterError::MemberCountExceeded {
            detected: Some(10),
            limit: 10,
            ..
        })
    ));
}

#[test]
fn extracts_members_from_real_qqnt_sidebar_tree_sorted_with_roles() {
    let text_item = |name: &str, rect: [i32; 4]| UiNodeSnapshot {
        control_type: "Text".to_string(),
        name: name.to_string(),
        rect,
        is_enabled: true,
        ..Default::default()
    };
    let group_item = |rect: [i32; 4], children: Vec<UiNodeSnapshot>| UiNodeSnapshot {
        control_type: "Group".to_string(),
        rect,
        is_enabled: true,
        children,
        ..Default::default()
    };

    // 模拟真实 UIA 树中纵坐标乱序排列的三类角色行（群主、管理员、普通群员）
    let tree = UiNodeSnapshot {
        control_type: "Window".to_string(),
        name: "群成员列表".to_string(),
        rect: [2004, 340, 2268, 1295],
        is_enabled: true,
        children: vec![group_item(
            [2004, 340, 2268, 773],
            vec![
                group_item(
                    [2004, 580, 2268, 629],
                    vec![text_item("测试成员06", [2061, 592, 2097, 616])],
                ),
                group_item(
                    [2004, 340, 2268, 389],
                    vec![
                        text_item("测试好友A", [2061, 352, 2159, 376]),
                        group_item(
                            [2211, 352, 2253, 377],
                            vec![text_item("群主", [2217, 355, 2247, 374])],
                        ),
                    ],
                ),
                group_item(
                    [2004, 388, 2268, 437],
                    vec![
                        text_item("测试好友B", [2061, 400, 2133, 424]),
                        group_item(
                            [2196, 400, 2253, 425],
                            vec![text_item("管理员", [2202, 403, 2247, 422])],
                        ),
                    ],
                ),
            ],
        )],
        ..Default::default()
    };

    let members = extract_members_from_tree(&tree);
    assert_eq!(members.len(), 3);
    assert_eq!(members[0].nickname, "测试好友A");
    assert_eq!(members[0].role, "owner");
    assert_eq!(members[1].nickname, "测试好友B");
    assert_eq!(members[1].role, "admin");
    assert_eq!(members[2].nickname, "测试成员06");
    assert_eq!(members[2].role, "member");
}

#[test]
fn parses_drawer_real_group_id_and_filters_timestamps_and_existing_nodes() {
    let win_rect = [675, 126, 2277, 1295];
    let existing_msg = UiNodeSnapshot {
        control_type: "Window".to_string(),
        name: "消息列表".to_string(),
        rect: [1134, 264, 2001, 979],
        is_enabled: true,
        ..Default::default()
    };
    let msg_number_noise = UiNodeSnapshot {
        control_type: "Text".to_string(),
        name: "998877665".to_string(),
        rect: [1600, 500, 1720, 525],
        is_enabled: true,
        ..Default::default()
    };
    let timestamp_noise = UiNodeSnapshot {
        control_type: "Text".to_string(),
        name: "19:06".to_string(),
        rect: [1950, 260, 2020, 285],
        is_enabled: true,
        ..Default::default()
    };
    let level_noise = UiNodeSnapshot {
        control_type: "Text".to_string(),
        name: "LV 18".to_string(),
        rect: [1950, 290, 2020, 315],
        is_enabled: true,
        ..Default::default()
    };
    let small_count_noise = UiNodeSnapshot {
        control_type: "Text".to_string(),
        name: "9999".to_string(),
        rect: [1950, 320, 2020, 345],
        is_enabled: true,
        ..Default::default()
    };
    let drawer_label = UiNodeSnapshot {
        control_type: "Text".to_string(),
        name: "群号".to_string(),
        rect: [1900, 380, 1950, 405],
        is_enabled: true,
        ..Default::default()
    };
    let drawer_group_id = UiNodeSnapshot {
        control_type: "Text".to_string(),
        name: "100000008".to_string(),
        rect: [1960, 380, 2100, 405],
        is_enabled: true,
        ..Default::default()
    };
    let drawer_capacity = UiNodeSnapshot {
        control_type: "Text".to_string(),
        name: "9/200".to_string(),
        rect: [1960, 420, 2060, 445],
        is_enabled: true,
        ..Default::default()
    };

    let before = [&existing_msg, &msg_number_noise];
    let after = [
        &existing_msg,
        &msg_number_noise,
        &timestamp_noise,
        &level_noise,
        &small_count_noise,
        &drawer_label,
        &drawer_group_id,
        &drawer_capacity,
    ];

    let (real_id, max_cap) = parse_drawer_metadata(&before, &after, win_rect);
    assert_eq!(real_id, Some(100000008));
    assert_eq!(max_cap, Some(200));
}

#[test]
fn parses_drawer_real_group_id_even_when_overlapping_message_and_member_lists() {
    let win_rect = [275, 97, 1378, 876];
    let msg_list = UiNodeSnapshot {
        control_type: "Window".to_string(),
        name: "消息列表".to_string(),
        rect: [580, 188, 1194, 665],
        is_enabled: true,
        ..Default::default()
    };
    let member_list = UiNodeSnapshot {
        control_type: "Window".to_string(),
        name: "群成员列表".to_string(),
        rect: [1196, 239, 1372, 876],
        is_enabled: true,
        ..Default::default()
    };
    let old_chat_number = UiNodeSnapshot {
        control_type: "Text".to_string(),
        name: "12345678".to_string(),
        rect: [720, 420, 820, 442],
        is_enabled: true,
        ..Default::default()
    };

    // 新增右侧抽屉节点坐标恰好落在消息列表与群成员列表原有矩形范围内
    let drawer_id_overlapping_member_sidebar = UiNodeSnapshot {
        control_type: "Text".to_string(),
        name: "群号：100000009".to_string(),
        rect: [1210, 280, 1350, 304],
        is_enabled: true,
        ..Default::default()
    };
    let drawer_capacity_overlapping_msg = UiNodeSnapshot {
        control_type: "Text".to_string(),
        name: "9/200人".to_string(),
        rect: [1140, 320, 1190, 344],
        is_enabled: true,
        ..Default::default()
    };

    let before = [&msg_list, &member_list, &old_chat_number];
    let after = [
        &msg_list,
        &member_list,
        &old_chat_number,
        &drawer_id_overlapping_member_sidebar,
        &drawer_capacity_overlapping_msg,
    ];

    let (real_id, max_cap) = parse_drawer_metadata(&before, &after, win_rect);
    assert_eq!(real_id, Some(100000009));
    assert_eq!(max_cap, Some(200));
}

#[test]
fn extracts_profile_card_group_id_from_value_or_copy_button_anchor() {
    let card_rect = [600, 180, 960, 620];
    let card_title = UiNodeSnapshot {
        control_type: "Text".to_string(),
        name: "Demo测试群".to_string(),
        rect: [660, 210, 820, 240],
        is_enabled: true,
        ..Default::default()
    };
    let value_id_node = UiNodeSnapshot {
        control_type: "Text".to_string(),
        name: String::new(),
        value: Some("\u{200b}100000010".to_string()),
        rect: [680, 260, 790, 286],
        is_enabled: true,
        ..Default::default()
    };
    let copy_button = UiNodeSnapshot {
        control_type: "Button".to_string(),
        name: "复制".to_string(),
        rect: [800, 260, 848, 286],
        is_enabled: true,
        ..Default::default()
    };
    let ratio_node = UiNodeSnapshot {
        control_type: "Text".to_string(),
        name: "9/500".to_string(),
        rect: [680, 310, 740, 336],
        is_enabled: true,
        ..Default::default()
    };

    let snaps = [&card_title, &value_id_node, &copy_button, &ratio_node];
    let candidates = collect_snapshot_candidates(&[], &snaps, card_rect, 0.0);
    let meta = extract_metadata_from_candidates(&candidates);
    assert_eq!(meta.real_group_id, Some(100000010));
    assert_eq!(meta.max_member_count, Some(500));
}

#[test]
fn session_extractor_strips_inline_timestamp_and_preview_suffix() {
    let win_rect = [275, 97, 1378, 876];
    let session_list = UiNodeSnapshot {
        control_type: "Window".to_string(),
        name: "会话列表".to_string(),
        rect: [330, 188, 580, 872],
        is_enabled: true,
        ..Default::default()
    };
    let target_row = UiNodeSnapshot {
        control_type: "Group".to_string(),
        name: "Demo测试群 22:19 测试好友B 回应了你的 消息:".to_string(),
        rect: [336, 385, 574, 445],
        is_enabled: true,
        ..Default::default()
    };
    let large_row = UiNodeSnapshot {
        control_type: "Group".to_string(),
        name: "测试大群 (128) 23:20 你好 1条未读".to_string(),
        rect: [336, 450, 574, 510],
        is_enabled: true,
        ..Default::default()
    };

    let snapshots = [&session_list, &target_row, &large_row];
    let sessions = extract_sessions_from_snapshots(&snapshots, win_rect);
    assert_eq!(sessions.len(), 2);
    assert_eq!(sessions[0].title, "Demo测试群");
    assert_eq!(
        sessions[0].preview.as_deref(),
        Some("22:19 测试好友B 回应了你的 消息:")
    );
    assert_eq!(sessions[1].title, "测试大群 (128)");
    assert_eq!(sessions[1].member_count_hint, Some(128));
    assert_eq!(sessions[1].unread_count, Some(1));
}
