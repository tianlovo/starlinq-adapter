//! 消息解析、自账号识别与 `MessageTracker` 增量追踪器单元测试

use common_types::{ChatMessage, ChatTargetKind, UiNodeSnapshot};

use super::*;
use crate::constants::MESSAGE_TRACKER_MAX_SEEN_IDS;

fn text_node(name: &str, rect: [i32; 4]) -> UiNodeSnapshot {
    UiNodeSnapshot {
        control_type: "Text".to_string(),
        name: name.to_string(),
        rect,
        is_enabled: true,
        is_offscreen: false,
        ..Default::default()
    }
}

fn list_item(name: &str, rect: [i32; 4], children: Vec<UiNodeSnapshot>) -> UiNodeSnapshot {
    UiNodeSnapshot {
        control_type: "ListItem".to_string(),
        name: name.to_string(),
        rect,
        is_enabled: true,
        is_offscreen: false,
        children,
        ..Default::default()
    }
}

fn group_node(name: &str, rect: [i32; 4], children: Vec<UiNodeSnapshot>) -> UiNodeSnapshot {
    UiNodeSnapshot {
        control_type: "Group".to_string(),
        name: name.to_string(),
        rect,
        is_enabled: true,
        is_offscreen: false,
        children,
        ..Default::default()
    }
}

#[test]
fn parses_mock_qq_and_qqnt_subtree_messages() {
    let win_rect = [0, 0, 800, 640];
    let tree = UiNodeSnapshot {
        control_type: "Window".to_string(),
        rect: win_rect,
        is_enabled: true,
        children: vec![UiNodeSnapshot {
            control_type: "List".to_string(),
            rect: [300, 120, 780, 480],
            is_enabled: true,
            children: vec![
                list_item(
                    "群友A [14:00]: 欢迎来到Demo测试群",
                    [310, 130, 770, 155],
                    vec![],
                ),
                list_item(
                    "",
                    [310, 160, 770, 210],
                    vec![
                        text_node("群友B", [315, 162, 380, 180]),
                        text_node("14:02", [390, 162, 440, 180]),
                        text_node("ping", [315, 184, 500, 206]),
                    ],
                ),
                list_item("BotSelf [now]: pong", [310, 215, 770, 240], vec![]),
            ],
            ..Default::default()
        }],
        ..Default::default()
    };

    let msgs = extract_messages_from_tree(&tree, win_rect, false, "Demo测试群 (5)");
    assert_eq!(msgs.len(), 3);
    assert_eq!(msgs[0].sender, "群友A");
    assert_eq!(msgs[0].timestamp_hint.as_deref(), Some("14:00"));
    assert_eq!(msgs[0].content, "欢迎来到Demo测试群");

    assert_eq!(msgs[1].sender, "群友B");
    assert_eq!(msgs[1].timestamp_hint.as_deref(), Some("14:02"));
    assert_eq!(msgs[1].content, "ping");

    assert_eq!(msgs[2].sender, "BotSelf");
    assert_eq!(msgs[2].content, "pong");
}

#[test]
fn tracker_suppresses_baseline_and_self_echoes() {
    let win_rect = [0, 0, 800, 640];
    let baseline_tree = UiNodeSnapshot {
        control_type: "Window".to_string(),
        rect: win_rect,
        is_enabled: true,
        children: vec![UiNodeSnapshot {
            control_type: "List".to_string(),
            rect: [300, 120, 780, 480],
            is_enabled: true,
            children: vec![list_item(
                "群友A [14:00]: 历史消息",
                [310, 130, 770, 155],
                vec![],
            )],
            ..Default::default()
        }],
        ..Default::default()
    };

    let mut tracker = MessageTracker::new(vec!["我的机器人".to_string()]);
    let baseline = extract_messages_from_tree(&baseline_tree, win_rect, false, "Demo测试群 (5)");
    tracker.seed_baseline(&baseline);

    tracker.record_outbound("Hello Demo");

    let updated_tree = UiNodeSnapshot {
        control_type: "Window".to_string(),
        rect: win_rect,
        is_enabled: true,
        children: vec![UiNodeSnapshot {
            control_type: "List".to_string(),
            rect: [300, 120, 780, 480],
            is_enabled: true,
            children: vec![
                list_item("群友A [14:00]: 历史消息", [310, 130, 770, 155], vec![]),
                list_item("Unknown [14:01]: Hello Demo", [310, 160, 770, 185], vec![]),
                list_item(
                    "BotSelf [now]: 另一条自己消息",
                    [310, 190, 770, 215],
                    vec![],
                ),
                list_item("群友A [14:05]: ping", [310, 220, 770, 245], vec![]),
            ],
            ..Default::default()
        }],
        ..Default::default()
    };

    let parsed = extract_messages_from_tree(&updated_tree, win_rect, false, "Demo测试群 (5)");
    let (all, new_incoming) = tracker.observe_poll(parsed);

    assert_eq!(all.len(), 4);
    assert!(!all[0].is_self);
    assert!(all[1].is_self, "outbound echo must be marked is_self");
    assert!(all[2].is_self, "BotSelf must be marked is_self");
    assert!(!all[3].is_self);

    assert_eq!(new_incoming.len(), 1);
    assert_eq!(new_incoming[0].sender, "群友A");
    assert_eq!(new_incoming[0].content, "ping");

    // 同一棵消息树再次轮询时不得重复触发入站事件
    let parsed_again = extract_messages_from_tree(&updated_tree, win_rect, false, "Demo测试群 (5)");
    let (_, new_incoming_2) = tracker.observe_poll(parsed_again);
    assert!(new_incoming_2.is_empty());
}

#[test]
fn tracker_evicts_seen_ids_beyond_bounded_2048_capacity() {
    let mut tracker = MessageTracker::new(vec![]);
    let total = MESSAGE_TRACKER_MAX_SEEN_IDS + 64;
    let batch: Vec<ChatMessage> = (0..total)
        .map(|index| ChatMessage {
            message_id: format!("msg_{index}"),
            group_name: "Demo测试群".to_string(),
            sender: "群友A".to_string(),
            content: format!("content {index}"),
            ..Default::default()
        })
        .collect();
    tracker.seed_baseline(&batch);
    assert_eq!(tracker.seen_len(), MESSAGE_TRACKER_MAX_SEEN_IDS);

    // 最早被淘汰的第 0 条重新进入轮询时应被视为新消息，而最新的仍在集合中去重
    let (_, re_emitted) = tracker.observe_poll(vec![batch[0].clone()]);
    assert_eq!(re_emitted.len(), 1);
    let (_, suppressed) = tracker.observe_poll(vec![batch[total - 1].clone()]);
    assert!(suppressed.is_empty());
}

#[test]
fn parses_qqnt_150_dpi_messages_filters_noise_and_marks_self_alignment() {
    let logical_win = [451, 84, 1519, 863];
    let offscreen_sys = UiNodeSnapshot {
        is_offscreen: true,
        ..group_node(
            "",
            [1134, -148, 2001, -19],
            vec![text_node(
                "测试好友A邀请你加入了群聊。",
                [1437, -72, 1698, -48],
            )],
        )
    };
    let bot_card_row = group_node(
        "",
        [1134, -19, 2001, 655],
        vec![group_node(
            "",
            [1134, -19, 2001, 643],
            vec![
                UiNodeSnapshot {
                    is_offscreen: true,
                    ..group_node("测试成员01", [1164, -7, 1212, 41], vec![])
                },
                group_node(
                    "",
                    [1224, 20, 1740, 631],
                    vec![
                        UiNodeSnapshot {
                            control_type: "Link".to_string(),
                            name: "".to_string(),
                            value: Some("app://./renderer/config".to_string()),
                            rect: [1239, 32, 1725, 38],
                            is_enabled: true,
                            ..Default::default()
                        },
                        UiNodeSnapshot {
                            control_type: "Link".to_string(),
                            name: "示例命令一".to_string(),
                            value: Some("mqqapi://aio/inlinecmd?cmd=toggle".to_string()),
                            rect: [1260, 523, 1368, 546],
                            is_enabled: true,
                            children: vec![text_node("示例命令一", [1260, 523, 1368, 546])],
                            ..Default::default()
                        },
                        text_node("示例命令二", [1316, 572, 1401, 602]),
                    ],
                ),
            ],
        )],
    );
    let img_msg_row = group_node(
        "",
        [1134, 654, 2001, 886],
        vec![group_node(
            "",
            [1134, 654, 2001, 874],
            vec![
                group_node("测试好友B", [1164, 666, 1212, 715], vec![]),
                group_node(
                    "",
                    [1224, 663, 1457, 691],
                    vec![
                        text_node("测试好友B", [1224, 665, 1296, 688]),
                        group_node(
                            "",
                            [1302, 665, 1397, 689],
                            vec![
                                text_node("LV", [1308, 667, 1325, 687]),
                                text_node("18", [1324, 667, 1343, 687]),
                                text_node("管理员", [1345, 667, 1391, 687]),
                            ],
                        ),
                    ],
                ),
                group_node(
                    "",
                    [1224, 693, 1457, 862],
                    vec![UiNodeSnapshot {
                        control_type: "Image".to_string(),
                        name: "图片".to_string(),
                        rect: [1224, 693, 1392, 862],
                        is_enabled: true,
                        ..Default::default()
                    }],
                ),
            ],
        )],
    );
    let anti_fraud_row = group_node(
        "",
        [1134, 885, 2001, 960],
        vec![group_node(
            "",
            [1224, 890, 1911, 955],
            vec![
                text_node(
                    "该账号疑似非大陆地区，涉及金钱交易请电话核实身份，请勿轻信扫码或转账，谨防境外诈骗。",
                    [1243, 902, 1892, 949],
                ),
                text_node("立即核实>", [1578, 926, 1665, 949]),
            ],
        )],
    );
    let self_msg_row = group_node(
        "",
        [1134, 900, 2001, 975],
        vec![group_node(
            "",
            [1134, 900, 2001, 970],
            vec![
                group_node("测试机器人", [1925, 908, 1973, 956], vec![]),
                group_node(
                    "",
                    [1700, 910, 1910, 960],
                    vec![text_node(
                        "pong (from qq-uia-adapter)",
                        [1710, 918, 1900, 950],
                    )],
                ),
            ],
        )],
    );

    let tree = UiNodeSnapshot {
        control_type: "Pane".to_string(),
        name: "QQ".to_string(),
        rect: [675, 126, 2277, 1295],
        is_enabled: true,
        children: vec![
            UiNodeSnapshot {
                control_type: "Button".to_string(),
                name: "测试机器人的头像".to_string(),
                rect: [777, 138, 813, 174],
                is_enabled: true,
                ..Default::default()
            },
            text_node("测试机器人", [822, 142, 906, 169]),
            UiNodeSnapshot {
                control_type: "Window".to_string(),
                name: "消息列表".to_string(),
                rect: [1134, 264, 2001, 979],
                is_enabled: true,
                children: vec![
                    group_node(
                        "",
                        [1134, 264, 2001, 979],
                        vec![group_node(
                            "",
                            [1134, -148, 2001, 979],
                            vec![
                                offscreen_sys,
                                bot_card_row,
                                img_msg_row,
                                anti_fraud_row,
                                self_msg_row,
                            ],
                        )],
                    ),
                    group_node("", [1987, 264, 2001, 979], vec![]),
                ],
                ..Default::default()
            },
            UiNodeSnapshot {
                control_type: "Window".to_string(),
                name: "群成员列表".to_string(),
                rect: [2004, 340, 2268, 1295],
                is_enabled: true,
                children: vec![text_node("测试成员06", [2061, 544, 2097, 568])],
                ..Default::default()
            },
        ],
        ..Default::default()
    };

    let self_nicks = extract_self_nicknames_from_tree(&tree, logical_win);
    assert_eq!(self_nicks, vec!["测试机器人".to_string()]);

    let msgs = extract_messages_from_tree(&tree, logical_win, false, "Demo测试群");
    assert_eq!(msgs.len(), 3, "got msgs: {msgs:?}");
    assert_eq!(msgs[0].sender, "测试成员01");
    assert_eq!(msgs[0].content, "示例命令一\n示例命令二");
    assert!(!msgs[0].is_self);

    assert_eq!(msgs[1].sender, "测试好友B");
    assert_eq!(msgs[1].content, "[图片]");
    assert!(!msgs[1].is_self);

    assert_eq!(msgs[2].sender, "测试机器人");
    assert_eq!(msgs[2].content, "pong (from qq-uia-adapter)");
    assert!(
        msgs[2].is_self,
        "right-aligned self message must be is_self=true"
    );

    let mut tracker = MessageTracker::new(vec![]);
    tracker.add_self_nicknames(&self_nicks);
    let (annotated, new_incoming) = tracker.observe_poll(msgs);
    assert!(annotated[2].is_self);
    assert_eq!(new_incoming.len(), 2);
}

#[test]
fn parses_qqnt_nested_timestamp_subrow_with_badges_as_self_message() {
    let logical_win = [451, 84, 1519, 863];
    let body_text = "[qq-uia-adapter] V1 UIA 适配连接测试成功";

    let timestamp_subrow = group_node(
        "",
        [1134, 789, 2001, 814],
        vec![text_node("19:06", [1544, 789, 1591, 813])],
    );
    let message_subrow = group_node(
        "",
        [1134, 825, 2001, 967],
        vec![
            group_node(
                "测试机器人",
                [1923, 837, 1971, 886],
                vec![group_node("", [1923, 837, 1971, 886], vec![])],
            ),
            group_node(
                "",
                [1729, 834, 1911, 862],
                vec![
                    text_node("测试机器人", [1839, 836, 1911, 859]),
                    group_node(
                        "",
                        [1795, 836, 1833, 860],
                        vec![
                            text_node("LV", [1801, 838, 1819, 858]),
                            text_node("1", [1818, 838, 1827, 858]),
                        ],
                    ),
                ],
            ),
            group_node(
                "",
                [1224, 864, 1911, 955],
                vec![group_node(
                    "",
                    [1224, 864, 1911, 955],
                    vec![group_node(
                        "",
                        [1239, 876, 1896, 943],
                        vec![text_node(body_text, [1239, 879, 1862, 939])],
                    )],
                )],
            ),
        ],
    );
    let row4 = group_node(
        "",
        [1134, 765, 2001, 979],
        vec![group_node(
            "",
            [1134, 789, 2001, 979],
            vec![timestamp_subrow, message_subrow],
        )],
    );

    let tree = UiNodeSnapshot {
        control_type: "Pane".to_string(),
        name: "QQ".to_string(),
        rect: [675, 126, 2277, 1295],
        is_enabled: true,
        children: vec![UiNodeSnapshot {
            control_type: "Window".to_string(),
            name: "消息列表".to_string(),
            rect: [1134, 264, 2001, 979],
            is_enabled: true,
            children: vec![group_node(
                "",
                [1134, 264, 2001, 979],
                vec![group_node("", [1134, -361, 2001, 979], vec![row4])],
            )],
            ..Default::default()
        }],
        ..Default::default()
    };

    let msgs = extract_messages_from_tree(&tree, logical_win, false, "Demo测试群");
    assert_eq!(msgs.len(), 1, "got msgs: {msgs:?}");
    assert_eq!(msgs[0].sender, "测试机器人");
    assert_eq!(msgs[0].timestamp_hint.as_deref(), Some("19:06"));
    assert_eq!(msgs[0].content, body_text);
    assert!(msgs[0].is_self);
}

#[test]
fn parses_flat_native_uia_message_list_from_step5_dump() {
    let win_rect = [451, 84, 1519, 863];
    let text_off = |name: &str| UiNodeSnapshot {
        control_type: "Text".to_string(),
        name: name.to_string(),
        rect: [0, 0, 0, 0],
        is_enabled: true,
        is_offscreen: true,
        ..Default::default()
    };
    let group_off = |name: &str, children: Vec<UiNodeSnapshot>| UiNodeSnapshot {
        control_type: "Group".to_string(),
        name: name.to_string(),
        rect: [0, 0, 0, 0],
        is_enabled: true,
        is_offscreen: true,
        children,
        ..Default::default()
    };
    let link_node = |name: &str, val: Option<&str>, rect: [i32; 4], off: bool| UiNodeSnapshot {
        control_type: "Hyperlink".to_string(),
        name: name.to_string(),
        value: val.map(str::to_string),
        rect,
        is_enabled: true,
        is_offscreen: off,
        ..Default::default()
    };

    let msg_list_children = vec![
        // 对应第 82-90 行：屏外时间戳与无头像邀请入群通知
        group_off("", vec![text_off("13:45")]),
        group_off(
            "",
            vec![
                link_node("测试好友A", None, [0, 0, 0, 0], true),
                text_off("邀请"),
                text_off("你"),
                text_off("加入了"),
                text_off("群聊"),
                text_off("。"),
            ],
        ),
        // 对应第 91-100 行：来自“测试成员01”的屏外机器人卡片消息
        group_off("测试成员01", vec![]),
        group_off(
            "",
            vec![
                link_node(
                    "",
                    Some("app://./renderer/%7B%22version%22%3A2%7D"),
                    [826, -485, 1150, -481],
                    true,
                ),
                link_node(
                    "@测试机器人",
                    Some("mqqapi://markdown/mention?at_type=1&at_tinyid=200000004"),
                    [0, 0, 0, 0],
                    true,
                ),
                group_off(
                    "",
                    vec![link_node(
                        "示例命令一",
                        Some("mqqapi://aio/inlinecmd?command=%E5%8A%9F%E8%83%BD%E5%BC%80%E5%85%B3"),
                        [0, 0, 0, 0],
                        true,
                    )],
                ),
                text_off("示例命令二"),
                text_off("示例命令三"),
            ],
        ),
        // 对应第 101-107 行：来自“测试好友B”的屏外纯图片消息
        group_off("测试好友B", vec![]),
        text_off("测试好友B"),
        text_off("LV"),
        text_off("18"),
        text_off("管理员"),
        group_off(
            "",
            vec![UiNodeSnapshot {
                control_type: "Image".to_string(),
                name: "图片".to_string(),
                rect: [0, 0, 0, 0],
                is_enabled: true,
                is_offscreen: true,
                ..Default::default()
            }],
        ),
        // 对应第 108-110 行：涉境外观测与反诈警告横幅（应过滤）
        group_off(
            "",
            vec![
                text_off(
                    "该账号疑似非大陆地区，涉及金钱交易请电话核实身份，请勿轻信扫码或转账，谨防境外诈骗。",
                ),
                link_node("立即核实>", None, [0, 0, 0, 0], true),
            ],
        ),
        // 对应第 111-118 行：19:06 右对齐本端消息
        group_node(
            "",
            [756, 162, 1334, 179],
            vec![text_node("19:06", [1029, 162, 1060, 178])],
        ),
        group_node("测试机器人", [1282, 194, 1314, 227], vec![]),
        text_node("测试机器人", [1226, 193, 1274, 208]),
        text_node("LV", [1201, 195, 1213, 208]),
        text_node("2", [1212, 195, 1218, 208]),
        group_node(
            "",
            [816, 212, 1274, 273],
            vec![text_node(
                "[qq-uia-adapter] V1 UIA 适配连接测试成功[qq-uia-adapter] V1 UIA 适配连接测试成功",
                [826, 222, 1241, 262],
            )],
        ),
        // 对应第 127-135 行：19:31 左对齐“测试好友A”消息
        group_node(
            "",
            [756, 446, 1334, 463],
            vec![text_node("19:31", [1029, 446, 1060, 462])],
        ),
        group_node("测试好友A", [776, 478, 808, 511], vec![]),
        text_node("测试好友A", [816, 477, 881, 492]),
        text_node("LV", [889, 479, 901, 492]),
        text_node("8", [900, 479, 907, 492]),
        text_node("群主", [908, 479, 929, 492]),
        group_node(
            "",
            [816, 496, 1246, 557],
            vec![text_node(
                "[qq-uia-adapter] V1 UIA 适配连接测试成功[qq-uia-adapter] V1 UIA 适配连接测试成功",
                [826, 506, 1219, 546],
            )],
        ),
        // 对应第 136-141 行：最新一条单行右对齐本端消息
        group_node("测试机器人", [1282, 580, 1314, 613], vec![]),
        text_node("测试机器人", [1226, 579, 1274, 594]),
        text_node("LV", [1201, 581, 1213, 594]),
        text_node("2", [1212, 581, 1218, 594]),
        group_node(
            "",
            [976, 598, 1274, 637],
            vec![text_node(
                "[qq-uia-adapter] V1 UIA 适配连接测试成功",
                [986, 608, 1264, 626],
            )],
        ),
    ];

    let tree = UiNodeSnapshot {
        control_type: "Pane".to_string(),
        name: "QQ".to_string(),
        rect: win_rect,
        is_enabled: true,
        children: vec![UiNodeSnapshot {
            control_type: "Document".to_string(),
            rect: win_rect,
            is_enabled: true,
            children: vec![
                UiNodeSnapshot {
                    control_type: "Button".to_string(),
                    name: "测试机器人的头像".to_string(),
                    rect: [518, 92, 542, 116],
                    is_enabled: true,
                    ..Default::default()
                },
                text_node("测试机器人", [548, 95, 604, 113]),
                UiNodeSnapshot {
                    control_type: "Window".to_string(),
                    name: "消息列表".to_string(),
                    rect: [756, 176, 1334, 653],
                    is_enabled: true,
                    children: msg_list_children,
                    ..Default::default()
                },
            ],
            ..Default::default()
        }],
        ..Default::default()
    };

    let msgs = extract_messages_from_tree(&tree, win_rect, false, "Demo测试群");
    assert_eq!(msgs.len(), 5, "got msgs: {msgs:?}");
    assert!(msgs.iter().all(|msg| msg.sender != "Unknown"));
    assert!(msgs.iter().all(|msg| {
        msg.content != "LV" && !msg.content.contains("app://") && !msg.content.contains("mqqapi://")
    }));

    assert_eq!(msgs[0].sender, "测试成员01");
    assert!(msgs[0].content.contains("示例命令一"));
    assert!(msgs[0].content.contains("示例命令二"));
    assert!(!msgs[0].is_self);

    assert_eq!(msgs[1].sender, "测试好友B");
    assert_eq!(msgs[1].content, "[图片]");
    assert!(!msgs[1].is_self);

    assert_eq!(msgs[2].sender, "测试机器人");
    assert_eq!(msgs[2].timestamp_hint.as_deref(), Some("19:06"));
    assert!(msgs[2].is_self);

    assert_eq!(msgs[3].sender, "测试好友A");
    assert_eq!(msgs[3].timestamp_hint.as_deref(), Some("19:31"));
    assert!(!msgs[3].is_self);

    assert_eq!(msgs[4].sender, "测试机器人");
    assert_eq!(msgs[4].content, "[qq-uia-adapter] V1 UIA 适配连接测试成功");
    assert!(msgs[4].is_self);
}

#[test]
fn denoise_reaction_badges_notices_and_reply_preview() {
    let win_rect = [450, 80, 1520, 860];
    let msg_list = UiNodeSnapshot {
        control_type: "Window".to_string(),
        name: "消息列表".to_string(),
        rect: [756, 176, 1334, 653],
        is_enabled: true,
        children: vec![
            // 系统通知短语“回应了你的”和“拍了拍”均应被降噪剥离
            text_node("测试好友A 回应了你的 消息", [920, 210, 1140, 232]),
            text_node("群友B 拍了拍 我", [930, 240, 1120, 262]),
            // 合法群友气泡与其正下方的表情反应计数角标 Group("1")
            group_node("群友A", [770, 280, 802, 312], vec![]),
            text_node("群友A", [816, 279, 860, 294]),
            group_node(
                "",
                [816, 300, 1020, 340],
                vec![text_node("你好世界", [826, 308, 960, 330])],
            ),
            group_node(
                "",
                [816, 342, 864, 366],
                vec![text_node("1", [842, 344, 858, 362])],
            ),
        ],
        ..Default::default()
    };

    let tree = UiNodeSnapshot {
        control_type: "Pane".to_string(),
        name: "QQ".to_string(),
        rect: win_rect,
        is_enabled: true,
        children: vec![UiNodeSnapshot {
            control_type: "Document".to_string(),
            rect: win_rect,
            is_enabled: true,
            children: vec![msg_list],
            ..Default::default()
        }],
        ..Default::default()
    };

    let parsed = extract_messages_from_tree(&tree, win_rect, false, "Demo测试群");
    assert_eq!(
        parsed.len(),
        1,
        "expected only real message, got: {parsed:?}"
    );
    assert_eq!(parsed[0].sender, "群友A");
    assert_eq!(parsed[0].content, "你好世界");

    let (preview_cn, body_cn) =
        row_cleaner::extract_reply_preview("「回复 @群友A: 上一句消息」 好的收到");
    assert_eq!(
        preview_cn,
        Some((Some("群友A".to_string()), "上一句消息".to_string()))
    );
    assert_eq!(body_cn, "好的收到");

    let (preview_en, body_en) = row_cleaner::extract_reply_preview("[回复 群友B：你好] 收到");
    assert_eq!(
        preview_en,
        Some((Some("群友B".to_string()), "你好".to_string()))
    );
    assert_eq!(body_en, "收到");
}

#[test]
fn tracker_consumes_each_outbound_echo_once_without_suppressing_peer_same_text() {
    let mut tracker = MessageTracker::new(vec![]);
    tracker.record_outbound("ping");

    let echo_msg = ChatMessage {
        message_id: "m_echo_1".to_string(),
        group_name: "Demo测试群".to_string(),
        sender: "匿名昵称".to_string(),
        content: "ping".to_string(),
        ..Default::default()
    };

    // 第一轮回显消费唯一的出站抑制令牌并标记为 is_self=true
    let (all1, new1) = tracker.observe_poll(vec![echo_msg.clone()]);
    assert!(all1[0].is_self);
    assert!(new1.is_empty());

    // 同一回显 ID 再次被轮询到时，仍保持 is_self=true 且不触发入站事件；而群友随后发送相同文本应放行入站
    let peer_same = ChatMessage {
        message_id: "m_peer_2".to_string(),
        group_name: "Demo测试群".to_string(),
        sender: "群友A".to_string(),
        content: "ping".to_string(),
        ..Default::default()
    };
    let (all2, new2) = tracker.observe_poll(vec![echo_msg, peer_same]);
    assert!(all2[0].is_self);
    assert!(!all2[1].is_self);
    assert_eq!(new2.len(), 1);
    assert_eq!(new2[0].message_id, "m_peer_2");

    // 连续记录两次相同发送并回滚一次后，应恰好保留 1 枚抑制令牌
    tracker.record_outbound("收到");
    tracker.record_outbound("收到");
    tracker.rollback_outbound("收到");

    let self_receipt = ChatMessage {
        message_id: "m_echo_receipt".to_string(),
        group_name: "Demo测试群".to_string(),
        sender: "匿名昵称".to_string(),
        content: "收到".to_string(),
        ..Default::default()
    };
    let peer_receipt = ChatMessage {
        message_id: "m_peer_receipt".to_string(),
        group_name: "Demo测试群".to_string(),
        sender: "群友B".to_string(),
        content: "收到".to_string(),
        ..Default::default()
    };
    let (all3, new3) = tracker.observe_poll(vec![self_receipt, peer_receipt]);
    assert!(all3[0].is_self);
    assert!(!all3[1].is_self);
    assert_eq!(new3.len(), 1);
    assert_eq!(new3[0].message_id, "m_peer_receipt");
}

#[test]
fn poke_notices_parse_both_pai_and_chuo_phrases() {
    let members = vec![common_types::GroupMemberInfo {
        group_id: 100000001,
        user_id: 200000002,
        real_user_id: Some(200000002),
        nickname: "测试好友A".to_string(),
        card: "测试好友A".to_string(),
        role: "owner".to_string(),
    }];
    let ctx = super::NoticeContext {
        self_id: 200000003,
        chat_kind: common_types::ChatTargetKind::Group,
        session_name: "Demo测试群",
        group_id: Some(100000001),
        peer_user_id: None,
        members: &members,
    };

    for phrase in [
        "测试机器人 拍了拍 \"测试好友A\"",
        "测试机器人 戳了戳 测试好友A",
    ] {
        assert!(row_cleaner::is_qqnt_system_notice_text(phrase));
        let event = super::parse_notice_text_to_onebot_json(&ctx, phrase, 1_794_740_000);
        assert_eq!(event["notice_type"], "notify");
        assert_eq!(event["sub_type"], "poke");
        assert_eq!(event["target_id"], 200000002);
        assert_eq!(event["group_id"], 100000001);
    }

    let win_rect = [342, 100, 1446, 880];
    let banner_row = group_node(
        "",
        [648, 640, 1442, 662],
        vec![
            text_node("你拍了拍", [920, 641, 976, 661]),
            UiNodeSnapshot {
                control_type: "Button".to_string(),
                name: "小王".to_string(),
                rect: [980, 640, 1008, 662],
                is_enabled: true,
                ..Default::default()
            },
        ],
    );
    let tree = UiNodeSnapshot {
        control_type: "Pane".to_string(),
        name: "QQ".to_string(),
        rect: win_rect,
        is_enabled: true,
        children: vec![UiNodeSnapshot {
            control_type: "Document".to_string(),
            rect: win_rect,
            is_enabled: true,
            children: vec![UiNodeSnapshot {
                control_type: "Window".to_string(),
                name: "消息列表".to_string(),
                rect: [648, 192, 1442, 669],
                is_enabled: true,
                children: vec![banner_row],
                ..Default::default()
            }],
            ..Default::default()
        }],
        ..Default::default()
    };
    let extracted = extract_system_notices_from_tree(&tree, win_rect, false, "Demo测试群");
    assert_eq!(extracted.len(), 1, "extracted notices: {extracted:?}");
    assert_eq!(extracted[0].text, "你拍了拍 小王");
}

#[test]
fn tracker_ignores_case_insensitive_media_uri_and_isolates_sessions_and_peers() {
    // 验证九字符 base64 与大小写敏感前缀均可正确忽略，本地文件名保留规范分词
    assert_eq!(
        super::tracker::normalize_media_token("base64://AAAABBBBCCCC"),
        None
    );
    assert_eq!(
        super::tracker::normalize_media_token("BASE64://AAAABBBBCCCC"),
        None
    );
    assert_eq!(
        super::tracker::normalize_media_token("Data:image/png;base64,AAAA"),
        None
    );
    assert_eq!(
        super::tracker::normalize_media_token("HTTPS://example.com/demo.png"),
        None
    );
    assert_eq!(
        super::tracker::normalize_media_token("C:\\tmp\\Demo_Report.PDF"),
        Some("demo_report.pdf".to_string())
    );

    let mut tracker = MessageTracker::new(vec!["我的机器人".to_string()]);
    tracker.record_outbound_for_session(ChatTargetKind::Group, "同名会话 (5)", "同文碰撞");

    // 同一文本落入同名私聊会话时不得消费群聊出站令牌，也不得将明确群友误标为自发
    let priv_peer = ChatMessage {
        message_id: "uid-priv-peer".to_string(),
        target_kind: ChatTargetKind::Private,
        group_name: "同名会话".to_string(),
        sender: "好友A".to_string(),
        content: "同文碰撞".to_string(),
        ..Default::default()
    };
    let group_peer = ChatMessage {
        message_id: "uid-group-peer".to_string(),
        target_kind: ChatTargetKind::Group,
        group_name: "同名会话".to_string(),
        sender: "群友B".to_string(),
        content: "同文碰撞".to_string(),
        ..Default::default()
    };
    assert!(tracker.is_definite_peer_message(&priv_peer));
    assert!(tracker.is_definite_peer_message(&group_peer));

    let annotated = tracker.annotate_only(vec![priv_peer.clone(), group_peer.clone()]);
    assert!(!annotated[0].is_self);
    assert!(!annotated[1].is_self);

    let (all1, inc1) = tracker.observe_poll(vec![priv_peer, group_peer]);
    assert!(!all1[0].is_self);
    assert!(!all1[1].is_self);
    assert_eq!(inc1.len(), 2);

    // 未能提取出发送者或为己方昵称时才消费对应群聊令牌，同名私聊后续回滚不跨会话清除
    let group_unknown_echo = ChatMessage {
        message_id: "uid-group-unknown".to_string(),
        target_kind: ChatTargetKind::Group,
        group_name: "同名会话".to_string(),
        sender: "Unknown".to_string(),
        content: "同文碰撞".to_string(),
        ..Default::default()
    };
    assert!(!tracker.is_definite_peer_message(&group_unknown_echo));
    let (all2, inc2) = tracker.observe_poll(vec![group_unknown_echo]);
    assert!(all2[0].is_self);
    assert!(inc2.is_empty());

    // 校验跨群会话回滚与基线 UID 按聊天类型隔离，同名群与私聊互不抢占散列别名
    tracker.record_outbound_for_session(ChatTargetKind::Group, "群甲", "双端同文");
    tracker.rollback_outbound_for_session(ChatTargetKind::Private, "群甲", "双端同文");
    let group_alpha_echo = ChatMessage {
        message_id: "uid-alpha".to_string(),
        target_kind: ChatTargetKind::Group,
        group_name: "群甲".to_string(),
        sender: "我的机器人".to_string(),
        content: "双端同文".to_string(),
        ..Default::default()
    };
    assert!(!tracker.is_definite_peer_message(&group_alpha_echo));

    let seed_group = ChatMessage {
        message_id: "stable-group-uid".to_string(),
        target_kind: ChatTargetKind::Group,
        group_name: "同名会话".to_string(),
        sender: "甲".to_string(),
        content: "统一内容".to_string(),
        ..Default::default()
    };
    tracker.seed_baseline(&[seed_group]);
    let same_name_private = ChatMessage {
        message_id: "stable-private-uid".to_string(),
        target_kind: ChatTargetKind::Private,
        group_name: "同名会话".to_string(),
        sender: "甲".to_string(),
        content: "统一内容".to_string(),
        ..Default::default()
    };
    let (_, inc_priv) = tracker.observe_poll(vec![same_name_private]);
    assert_eq!(inc_priv.len(), 1);
    assert_eq!(inc_priv[0].message_id, "stable-private-uid");
}

#[test]
fn skips_unowned_top_clipped_row_and_parses_offscreen_avatar_and_quote_bubble() {
    let hist = Some([648, 192, 1262, 669]);
    let img = |r: [i32; 4]| UiNodeSnapshot {
        control_type: "Image".to_string(),
        name: "图片".to_string(),
        rect: r,
        is_enabled: true,
        ..Default::default()
    };
    let ghost = group_node("", [708, 107, 775, 210], vec![img([708, 107, 775, 210])]);
    assert!(nested_msaa::parse_message_node(&ghost, hist).is_none());

    let off_av = UiNodeSnapshot {
        control_type: "Button".to_string(),
        name: "测试好友A的头像".to_string(),
        is_enabled: true,
        is_offscreen: true,
        ..Default::default()
    };
    let off_ts = UiNodeSnapshot {
        is_offscreen: true,
        ..text_node("09:31", [0, 0, 0, 0])
    };
    let owned = group_node(
        "",
        [660, 140, 820, 260],
        vec![off_ts, off_av, img([708, 145, 775, 255])],
    );
    assert_eq!(
        row_cleaner::find_offscreen_avatar_name_in_row(&owned, 2).as_deref(),
        Some("测试好友A")
    );
    let parsed = nested_msaa::parse_message_node(&owned, hist).expect("offscreen avatar row");
    assert_eq!(parsed.sender, "测试好友A");
    assert_eq!(parsed.timestamp_hint.as_deref(), Some("09:31"));

    let quote = group_node(
        "",
        [959, 425, 1202, 654],
        vec![
            text_node("测试机器人", [981, 440, 1030, 456]),
            UiNodeSnapshot {
                rect: [1168, 441, 1182, 456],
                ..img([1168, 441, 1182, 456])
            },
            text_node("@测试好友A", [996, 462, 1074, 478]),
            text_node("回复正文第一行", [969, 501, 1184, 541]),
        ],
    );
    let mut quote_mut = quote;
    quote_mut.children[1].name.clear();
    let mut out = Vec::new();
    row_cleaner::collect_leaf_fragments(&quote_mut, &mut out);
    assert_eq!(
        out,
        vec!["「回复 @测试机器人: @测试好友A」", "回复正文第一行"]
    );
}
