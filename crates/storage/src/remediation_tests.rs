//! 存储层缺陷修复专项单元测试：覆盖出站存根去重归并、污染目录清理与跨命名空间映射 ID 统一

use common_types::constants::{DEFAULT_ONEBOT_ID_BOUND, DEFAULT_TARGET_GROUP_NAME};
use common_types::{stable_onebot_id, ChatTargetKind, MessageSegment};

use super::*;
use crate::constants::MEMORY_DB_PATH;

#[test]
fn outbound_stub_reconcile_dedups_and_preserves_structured_segments() {
    let storage = Storage::open(MEMORY_DB_PATH).expect("open memory");

    // 场景 1：先写入含 reply/face 结构化段的出站存根，重绑定 UI UID 后再收到降级纯文本回采应保持 1 行且保留结构化段
    let rich_segments = vec![
        MessageSegment::Reply {
            id: "10000".to_string(),
        },
        MessageSegment::Face {
            id: "14".to_string(),
            text: Some("微笑".to_string()),
        },
        MessageSegment::Text {
            text: "你好".to_string(),
        },
    ];
    let stub_id = storage
        .record_message(&RecordMessageInput {
            message_uid: "out-stub-1".to_string(),
            group_name: DEFAULT_TARGET_GROUP_NAME.to_string(),
            group_id: 100_000_001,
            user_id: 200_000_001,
            sender_name: "测试机器人".to_string(),
            raw_message: "[CQ:reply,id=10000][CQ:face,id=14]你好".to_string(),
            is_self: true,
            timestamp_hint: None,
            captured_at_ms: 1_700_000_010_000,
            chat_type: ChatTargetKind::Group,
            segments: rich_segments.clone(),
        })
        .expect("record stub 1");

    storage
        .reconcile_outbound_stub_uid(stub_id, "ui-uid-1", Some("12:34"))
        .expect("reconcile stub 1");

    let poll_id = storage
        .record_message(&RecordMessageInput {
            message_uid: "ui-uid-1".to_string(),
            group_name: DEFAULT_TARGET_GROUP_NAME.to_string(),
            group_id: 100_000_001,
            user_id: 200_000_001,
            sender_name: "测试机器人".to_string(),
            raw_message: "[表情:微笑]你好".to_string(),
            is_self: true,
            timestamp_hint: Some("12:34".to_string()),
            captured_at_ms: 1_700_000_010_800,
            chat_type: ChatTargetKind::Group,
            segments: vec![MessageSegment::Text {
                text: "[表情:微笑]你好".to_string(),
            }],
        })
        .expect("poll reuse stub 1");
    assert_eq!(poll_id, stub_id);

    let stored = storage
        .get_message_by_id(stub_id)
        .expect("get stub 1")
        .expect("stub 1 exists");
    assert_eq!(stored.message_uid, "ui-uid-1");
    assert_eq!(stored.timestamp_hint.as_deref(), Some("12:34"));
    assert_eq!(stored.raw_message, "[CQ:reply,id=10000][CQ:face,id=14]你好");
    assert_eq!(stored.segments, rich_segments);
    assert_eq!(storage.stats().expect("stats 1").message_count, 1);

    // 场景 2：若轮询先误插入了更高 ID 的重复纯文本行，reconcile 应删除较新重复行并保留存根结构化段
    let stub2_segments = vec![
        MessageSegment::At {
            qq: "778899".to_string(),
            name: Some("测试好友A".to_string()),
        },
        MessageSegment::Image {
            file: "snap.png".to_string(),
            url: None,
            summary: None,
            image_type: None,
        },
        MessageSegment::Forward {
            id: "fwd-9".to_string(),
        },
    ];
    let stub2_id = storage
        .record_message(&RecordMessageInput {
            message_uid: "out-stub-2".to_string(),
            group_name: DEFAULT_TARGET_GROUP_NAME.to_string(),
            group_id: 100_000_001,
            user_id: 200_000_001,
            sender_name: "测试机器人".to_string(),
            raw_message: "[CQ:at,qq=778899][CQ:image,file=snap.png][CQ:forward,id=fwd-9]"
                .to_string(),
            is_self: true,
            timestamp_hint: None,
            captured_at_ms: 1_700_000_020_000,
            chat_type: ChatTargetKind::Group,
            segments: stub2_segments.clone(),
        })
        .expect("record stub 2");
    let dup2_id = storage
        .record_message(&RecordMessageInput {
            message_uid: "ui-uid-2".to_string(),
            group_name: DEFAULT_TARGET_GROUP_NAME.to_string(),
            group_id: 100_000_001,
            user_id: 200_000_001,
            sender_name: "测试机器人".to_string(),
            raw_message: "@测试好友A [图片] [合并转发:fwd-9]".to_string(),
            is_self: true,
            timestamp_hint: Some("12:35".to_string()),
            captured_at_ms: 1_700_000_020_500,
            chat_type: ChatTargetKind::Group,
            segments: vec![MessageSegment::Text {
                text: "@测试好友A [图片] [合并转发:fwd-9]".to_string(),
            }],
        })
        .expect("record dup 2");
    assert!(dup2_id > stub2_id);

    storage
        .reconcile_outbound_stub_uid(stub2_id, "ui-uid-2", None)
        .expect("reconcile stub 2 merges dup");
    assert!(storage
        .get_message_by_id(dup2_id)
        .expect("query deleted dup")
        .is_none());
    let stored2 = storage
        .get_message_by_id(stub2_id)
        .expect("get stub 2")
        .expect("stub 2 exists");
    assert_eq!(stored2.message_uid, "ui-uid-2");
    assert_eq!(stored2.timestamp_hint.as_deref(), Some("12:35"));
    assert_eq!(stored2.segments, stub2_segments);
    assert_eq!(storage.stats().expect("stats 2").message_count, 2);

    // 场景 3：若更早消息行（ID < stub2_id）已持有目标 UID，不得误删旧行且不得触发唯一键失败
    storage
        .reconcile_outbound_stub_uid(stub2_id, "ui-uid-1", Some("12:36"))
        .expect("reconcile against older id should be safe");
    assert!(storage
        .get_message_by_id(stub_id)
        .expect("older row remains")
        .is_some());
}

#[test]
fn purge_invalid_friends_and_groups_respects_real_id_and_refs_and_audits() {
    let storage = Storage::open(MEMORY_DB_PATH).expect("open memory");
    let invalid_names = [
        "文件",
        "04:35",
        "在线状态",
        "消息列表",
        "群成员列表",
        "发送",
        "关闭",
    ];
    let is_valid = |title: &str| !invalid_names.contains(&title);

    storage
        .upsert_friends_batch(&[
            UpsertFriendInput {
                nickname: "文件".to_string(),
                ..Default::default()
            },
            UpsertFriendInput {
                nickname: "04:35".to_string(),
                ..Default::default()
            },
            UpsertFriendInput {
                nickname: "在线状态".to_string(),
                real_user_id: Some(888_123),
                ..Default::default()
            },
            UpsertFriendInput {
                nickname: "测试好友A".to_string(),
                ..Default::default()
            },
        ])
        .expect("seed friends");
    storage
        .record_message(&RecordMessageInput {
            message_uid: "priv-ref-1".to_string(),
            group_name: "04:35".to_string(),
            group_id: 1,
            user_id: 2,
            sender_name: "04:35".to_string(),
            raw_message: "retained private msg".to_string(),
            captured_at_ms: 1_700_000_000_001,
            chat_type: ChatTargetKind::Private,
            ..Default::default()
        })
        .expect("seed private msg ref");

    storage
        .upsert_groups_batch(&[
            UpsertGroupInput {
                group_name: "消息列表".to_string(),
                ..Default::default()
            },
            UpsertGroupInput {
                group_name: "群成员列表".to_string(),
                ..Default::default()
            },
            UpsertGroupInput {
                group_name: "发送".to_string(),
                ..Default::default()
            },
            UpsertGroupInput {
                group_name: "关闭".to_string(),
                real_group_id: Some(100_000_001),
                ..Default::default()
            },
            UpsertGroupInput {
                group_name: DEFAULT_TARGET_GROUP_NAME.to_string(),
                ..Default::default()
            },
        ])
        .expect("seed groups");
    storage
        .upsert_group_members(
            "群成员列表",
            1000,
            &[UpsertMemberInput {
                nickname: "保留成员".to_string(),
                ..Default::default()
            }],
        )
        .expect("seed member ref");
    storage
        .record_message(&RecordMessageInput {
            message_uid: "grp-ref-1".to_string(),
            group_name: "发送".to_string(),
            group_id: 1001,
            user_id: 2001,
            sender_name: "成员甲".to_string(),
            raw_message: "retained group msg".to_string(),
            captured_at_ms: 1_700_000_000_002,
            chat_type: ChatTargetKind::Group,
            ..Default::default()
        })
        .expect("seed group msg ref");

    let purged_friends = storage
        .purge_invalid_friends(is_valid)
        .expect("purge friends");
    assert_eq!(purged_friends, vec!["文件".to_string()]);
    let remaining_friends: Vec<String> = storage
        .list_friends()
        .expect("list friends")
        .into_iter()
        .map(|f| f.nickname)
        .collect();
    assert!(!remaining_friends.contains(&"文件".to_string()));
    assert!(remaining_friends.contains(&"04:35".to_string()));
    assert!(remaining_friends.contains(&"在线状态".to_string()));
    assert!(remaining_friends.contains(&"测试好友A".to_string()));

    let purged_groups = storage
        .purge_invalid_groups(is_valid)
        .expect("purge groups");
    assert_eq!(purged_groups, vec!["消息列表".to_string()]);
    let remaining_groups: Vec<String> = storage
        .list_groups()
        .expect("list groups")
        .into_iter()
        .map(|g| g.group_name)
        .collect();
    assert!(!remaining_groups.contains(&"消息列表".to_string()));
    assert!(remaining_groups.contains(&"群成员列表".to_string()));
    assert!(remaining_groups.contains(&"发送".to_string()));
    assert!(remaining_groups.contains(&"关闭".to_string()));
    assert!(remaining_groups.contains(&DEFAULT_TARGET_GROUP_NAME.to_string()));

    // 再次执行应无命中且不追加空审计日志
    assert!(storage
        .purge_invalid_friends(is_valid)
        .expect("second purge friends")
        .is_empty());
    assert!(storage
        .purge_invalid_groups(is_valid)
        .expect("second purge groups")
        .is_empty());
    assert_eq!(storage.stats().expect("audit stats").audit_count, 2);

    let conn = storage.conn.lock();
    let kind_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM audit_events WHERE event_kind = 'session_pollution_purged'",
            [],
            |row| row.get(0),
        )
        .expect("query audit count");
    assert_eq!(kind_count, 2);
}

#[test]
fn member_mapped_id_unification_on_upsert_and_reconcile() {
    let storage = Storage::open(MEMORY_DB_PATH).expect("open memory");
    storage
        .set_meta("self_nickname", "测试机器人")
        .expect("set self_nickname");
    storage
        .set_meta("self_user_id", "200000001")
        .expect("set self_user_id");
    let friend_rec = storage
        .upsert_friend(&UpsertFriendInput {
            nickname: "测试好友A".to_string(),
            ..Default::default()
        })
        .expect("upsert friend");
    let expected_friend_uid = friend_rec.effective_user_id();

    let grp = storage
        .upsert_group(&UpsertGroupInput {
            group_name: DEFAULT_TARGET_GROUP_NAME.to_string(),
            real_group_id: Some(100_000_001),
            ..Default::default()
        })
        .expect("upsert group");
    storage
        .upsert_group_members(
            &grp.group_name,
            grp.effective_group_id(),
            &[
                UpsertMemberInput {
                    nickname: "测试机器人".to_string(),
                    ..Default::default()
                },
                UpsertMemberInput {
                    nickname: "测试好友A".to_string(),
                    ..Default::default()
                },
                UpsertMemberInput {
                    nickname: "路人丙".to_string(),
                    ..Default::default()
                },
            ],
        )
        .expect("upsert unified members");

    let members = storage
        .list_group_members(&grp.group_name, grp.effective_group_id())
        .expect("list unified members");
    assert_eq!(
        members
            .iter()
            .find(|m| m.nickname == "测试机器人")
            .expect("self member")
            .user_id,
        200_000_001
    );
    assert_eq!(
        members
            .iter()
            .find(|m| m.nickname == "测试好友A")
            .expect("friend member")
            .user_id,
        expected_friend_uid
    );
    assert_eq!(
        members
            .iter()
            .find(|m| m.nickname == "路人丙")
            .expect("plain member")
            .user_id,
        stable_onebot_id("member:Demo测试群:路人丙", DEFAULT_ONEBOT_ID_BOUND)
    );

    // 验证历史已分裂的 member: 映射在调用 reconcile_member_mapped_ids 后同步统一成员表与消息表
    let old_self_uid = stable_onebot_id("member:旧群:新自账号", DEFAULT_ONEBOT_ID_BOUND);
    let old_friend_uid = stable_onebot_id("member:旧群:好友乙", DEFAULT_ONEBOT_ID_BOUND);
    storage
        .upsert_group_members(
            "旧群",
            10086,
            &[
                UpsertMemberInput {
                    nickname: "新自账号".to_string(),
                    ..Default::default()
                },
                UpsertMemberInput {
                    nickname: "好友乙".to_string(),
                    ..Default::default()
                },
                UpsertMemberInput {
                    nickname: "实号丁".to_string(),
                    real_user_id: Some(999_111),
                    ..Default::default()
                },
            ],
        )
        .expect("seed legacy group members");
    storage
        .record_messages_batch(&[
            RecordMessageInput {
                message_uid: "rec-m1".to_string(),
                group_name: "旧群".to_string(),
                group_id: 10086,
                user_id: old_self_uid,
                sender_name: "新自账号".to_string(),
                raw_message: "self hello".to_string(),
                is_self: true,
                captured_at_ms: 1_700_000_001_000,
                chat_type: ChatTargetKind::Group,
                ..Default::default()
            },
            RecordMessageInput {
                message_uid: "rec-m2".to_string(),
                group_name: "旧群".to_string(),
                group_id: 10086,
                user_id: old_friend_uid,
                sender_name: "好友乙".to_string(),
                raw_message: "friend hello".to_string(),
                is_self: false,
                captured_at_ms: 1_700_000_002_000,
                chat_type: ChatTargetKind::Group,
                ..Default::default()
            },
            RecordMessageInput {
                message_uid: "rec-m3".to_string(),
                group_name: "旧群".to_string(),
                group_id: 10086,
                user_id: 999_111,
                sender_name: "实号丁".to_string(),
                raw_message: "real hello".to_string(),
                is_self: false,
                captured_at_ms: 1_700_000_003_000,
                chat_type: ChatTargetKind::Group,
                ..Default::default()
            },
        ])
        .expect("seed legacy group messages");

    storage
        .upsert_friend(&UpsertFriendInput {
            nickname: "好友乙".to_string(),
            real_user_id: Some(777_222),
            ..Default::default()
        })
        .expect("add friend 好友乙");

    let reconciled = storage
        .reconcile_member_mapped_ids(Some("新自账号"), Some(555_111))
        .expect("reconcile mapped ids");
    assert_eq!(reconciled, 4);

    let legacy_members = storage
        .list_group_members("旧群", 10086)
        .expect("list reconciled members");
    assert_eq!(
        legacy_members
            .iter()
            .find(|m| m.nickname == "新自账号")
            .expect("self")
            .user_id,
        555_111
    );
    assert_eq!(
        legacy_members
            .iter()
            .find(|m| m.nickname == "好友乙")
            .expect("friend")
            .user_id,
        777_222
    );
    assert_eq!(
        legacy_members
            .iter()
            .find(|m| m.nickname == "实号丁")
            .expect("real")
            .user_id,
        999_111
    );

    let legacy_msgs = storage
        .list_recent_messages("旧群", 10)
        .expect("list reconciled msgs");
    assert_eq!(legacy_msgs[0].user_id, 555_111);
    assert_eq!(legacy_msgs[1].user_id, 777_222);
    assert_eq!(legacy_msgs[2].user_id, 999_111);

    let second_reconcile = storage
        .reconcile_member_mapped_ids(Some("新自账号"), Some(555_111))
        .expect("idempotent second reconcile");
    assert_eq!(second_reconcile, 0);
}

#[test]
fn allocate_forward_nodes_monotonic_across_reopen_without_overwriting() {
    let tmp_path = std::env::temp_dir().join(format!(
        "qq_uia_fwd_reopen_{}_{}.db",
        std::process::id(),
        1001
    ));
    let _ = std::fs::remove_file(&tmp_path);
    let path_str = tmp_path.to_string_lossy().to_string();

    let id1;
    let id2;
    {
        let storage = Storage::open(&path_str).expect("open 1");
        storage
            .set_meta("forward:fwd-10002", "{\"nodes\":\"legacy-kept\"}")
            .expect("seed legacy fwd-10002");
        id1 = storage
            .allocate_forward_nodes("{\"nodes\":\"first-new\"}")
            .expect("allocate id1");
        assert_eq!(id1, "fwd-10003");
    }

    // 模拟进程重启重新打开同一 SQLite 文件，序号不得回退且不得覆盖旧转发包
    {
        let storage = Storage::open(&path_str).expect("open 2");
        id2 = storage
            .allocate_forward_nodes("{\"nodes\":\"second-new\"}")
            .expect("allocate id2");
        assert_eq!(id2, "fwd-10004");
        assert_eq!(
            storage
                .get_meta("forward:fwd-10002")
                .expect("get legacy")
                .as_deref(),
            Some("{\"nodes\":\"legacy-kept\"}")
        );
        assert_eq!(
            storage
                .get_meta(&format!("forward:{id1}"))
                .expect("get id1")
                .as_deref(),
            Some("{\"nodes\":\"first-new\"}")
        );
    }

    let _ = std::fs::remove_file(&tmp_path);
}

#[test]
fn record_messages_batch_reconciles_null_to_hhmm_while_keeping_genuine_repeats() {
    let storage = Storage::open(MEMORY_DB_PATH).expect("open memory");

    // 同批写入两条内容完全相同的真实重复消息，应分别分配不同编号
    let first_ids = storage
        .record_messages_batch(&[
            RecordMessageInput {
                message_uid: "uid-split-1".to_string(),
                group_name: DEFAULT_TARGET_GROUP_NAME.to_string(),
                group_id: 100_000_001,
                user_id: 200_000_001,
                sender_name: "测试机器人".to_string(),
                raw_message: "SPLIT-2".to_string(),
                is_self: true,
                timestamp_hint: None,
                captured_at_ms: 1_700_000_100_000,
                chat_type: ChatTargetKind::Group,
                segments: vec![MessageSegment::Text {
                    text: "SPLIT-2".to_string(),
                }],
            },
            RecordMessageInput {
                message_uid: "uid-repeat-same-batch".to_string(),
                group_name: DEFAULT_TARGET_GROUP_NAME.to_string(),
                group_id: 100_000_001,
                user_id: 200_000_001,
                sender_name: "测试机器人".to_string(),
                raw_message: "SPLIT-2".to_string(),
                is_self: true,
                timestamp_hint: None,
                captured_at_ms: 1_700_000_100_010,
                chat_type: ChatTargetKind::Group,
                segments: vec![MessageSegment::Text {
                    text: "SPLIT-2".to_string(),
                }],
            },
        ])
        .expect("batch 1");
    assert_eq!(first_ids.len(), 2);
    assert_ne!(first_ids[0], first_ids[1]);

    // 第一条消息因 QQNT 渲染出 `13:08` 时间戳而变更 UID 时，应归并复用原编号而不新增第三行
    let second_ids = storage
        .record_messages_batch(&[
            RecordMessageInput {
                message_uid: "uid-split-1-with-1308".to_string(),
                group_name: DEFAULT_TARGET_GROUP_NAME.to_string(),
                group_id: 100_000_001,
                user_id: 200_000_001,
                sender_name: "测试机器人".to_string(),
                raw_message: "SPLIT-2".to_string(),
                is_self: true,
                timestamp_hint: Some("13:08".to_string()),
                captured_at_ms: 1_700_000_110_000,
                chat_type: ChatTargetKind::Group,
                segments: vec![MessageSegment::Text {
                    text: "SPLIT-2".to_string(),
                }],
            },
            RecordMessageInput {
                message_uid: "uid-repeat-same-batch".to_string(),
                group_name: DEFAULT_TARGET_GROUP_NAME.to_string(),
                group_id: 100_000_001,
                user_id: 200_000_001,
                sender_name: "测试机器人".to_string(),
                raw_message: "SPLIT-2".to_string(),
                is_self: true,
                timestamp_hint: None,
                captured_at_ms: 1_700_000_110_010,
                chat_type: ChatTargetKind::Group,
                segments: vec![MessageSegment::Text {
                    text: "SPLIT-2".to_string(),
                }],
            },
        ])
        .expect("batch 2");
    assert_eq!(second_ids, first_ids);
    assert_eq!(storage.stats().expect("stats").message_count, 2);
    assert_eq!(
        storage
            .get_message_by_id(first_ids[0])
            .expect("get merged")
            .expect("exists")
            .timestamp_hint
            .as_deref(),
        Some("13:08")
    );
}

#[test]
fn initialize_schema_purges_chrome_search_pollution_and_split_duplicates() {
    let tmp_path = std::env::temp_dir().join(format!(
        "qq_uia_schema_cleanup_{}_{}.db",
        std::process::id(),
        1002
    ));
    let _ = std::fs::remove_file(&tmp_path);
    let path_str = tmp_path.to_string_lossy().to_string();

    {
        let storage = Storage::open(&path_str).expect("seed open");
        let conn = storage.conn.lock();
        conn.execute(
            "INSERT INTO groups (group_name, onebot_group_id, unread_count, updated_at_ms)
             VALUES ('进入综合搜索', 111, 0, 100), ('群聊', 222, 0, 100), ('[qq-uia-adapter]abc', 333, 0, 100)",
            [],
        )
        .expect("insert polluted groups");
        conn.execute(
            "INSERT INTO friends (friend_key, nickname, synthetic_user_id, updated_at_ms)
             VALUES ('查找用户、群聊等', '查找用户、群聊等', 444, 100), ('body', '[qq-uia-adapter] hello', 555, 100)",
            [],
        )
        .expect("insert polluted friends");
        conn.execute(
            "INSERT INTO messages (message_uid, group_name, group_id, user_id, sender_name, raw_message, is_self, timestamp_hint, captured_at_ms, chat_type)
             VALUES
             ('bad-chrome', 'Demo测试群', 100000001, 1, '切换为经典模式', '切换为经典模式 窗口 chrome', 0, NULL, 1700000000000, 'group'),
             ('pair-null', 'Demo测试群', 100000001, 2, '测试机器人', 'SPLIT-OLD', 1, NULL, 1700000001000, 'group'),
             ('pair-ts', 'Demo测试群', 100000001, 2, '测试机器人', 'SPLIT-OLD', 1, '13:08', 1700000020000, 'group')",
            [],
        )
        .expect("insert polluted messages");
    }

    let reopened = Storage::open(&path_str).expect("reopen triggers cleanup");
    assert!(reopened.list_groups().expect("groups").is_empty());
    assert!(reopened.list_friends().expect("friends").is_empty());
    let msgs = reopened
        .list_recent_messages(DEFAULT_TARGET_GROUP_NAME, 20)
        .expect("msgs");
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].raw_message, "SPLIT-OLD");
    assert_eq!(msgs[0].timestamp_hint.as_deref(), Some("13:08"));

    let _ = std::fs::remove_file(&tmp_path);
}

fn seed_session_msg(
    storage: &Storage,
    uid: &str,
    session_name: &str,
    sender_and_text: (&str, &str),
    kind_and_gid: (ChatTargetKind, i64),
    flags: (bool, Option<&str>, u64),
) -> i64 {
    let (sender_name, raw_message) = sender_and_text;
    let (chat_type, group_id) = kind_and_gid;
    let (is_self, timestamp_hint, captured_at_ms) = flags;
    storage
        .record_message(&RecordMessageInput {
            message_uid: uid.to_string(),
            group_name: session_name.to_string(),
            group_id,
            user_id: 20001,
            sender_name: sender_name.to_string(),
            raw_message: raw_message.to_string(),
            is_self,
            timestamp_hint: timestamp_hint.map(str::to_string),
            captured_at_ms,
            chat_type,
            ..Default::default()
        })
        .expect("seed session msg")
}

fn msg_group_id(storage: &Storage, id: i64) -> i64 {
    storage.get_message_by_id(id).unwrap().unwrap().group_id
}

#[test]
fn same_name_group_upsert_and_startup_heal_do_not_pollute_private_group_id() {
    let tmp_path = std::env::temp_dir().join(format!(
        "qq_uia_same_name_heal_{}_1003.db",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&tmp_path);
    let path_str = tmp_path.to_string_lossy().to_string();
    let (shared, real_gid) = ("同名会话", 888_999_001_i64);
    let (g1, p1, g2, p2);
    {
        let st = Storage::open(&path_str).expect("open temp storage");
        g1 = seed_session_msg(
            &st,
            "heal-g1",
            shared,
            ("群友甲", "g1"),
            (ChatTargetKind::Group, 10001),
            (false, None, 1_700_000_001_000),
        );
        p1 = seed_session_msg(
            &st,
            "heal-p1",
            shared,
            (shared, "p1"),
            (ChatTargetKind::Private, 0),
            (false, None, 1_700_000_002_000),
        );
        // 调用 upsert_group 写入真实群号后，仅群聊消息 group_id 被修正，同名私聊保持 0
        st.upsert_group(&UpsertGroupInput {
            group_name: shared.to_string(),
            real_group_id: Some(real_gid),
            ..Default::default()
        })
        .expect("upsert group with real id");
        assert_eq!(msg_group_id(&st, g1), real_gid);
        assert_eq!(msg_group_id(&st, p1), 0);

        // 即使后续 upsert_group 未传 real_group_id（走已有真实群号回填分支），也不得污染私聊
        g2 = seed_session_msg(
            &st,
            "heal-g2",
            shared,
            ("群友甲", "g2"),
            (ChatTargetKind::Group, 10002),
            (false, None, 1_700_000_003_000),
        );
        p2 = seed_session_msg(
            &st,
            "heal-p2",
            shared,
            (shared, "p2"),
            (ChatTargetKind::Private, 0),
            (false, None, 1_700_000_004_000),
        );
        st.upsert_group(&UpsertGroupInput {
            group_name: shared.to_string(),
            last_preview: Some("preview".to_string()),
            ..Default::default()
        })
        .expect("upsert group without real id");
        assert_eq!(
            (msg_group_id(&st, g2), msg_group_id(&st, p2)),
            (real_gid, 0)
        );

        // 模拟历史遗留污染状态：群消息为合成群号且私聊消息曾被误刷成非零群号，验证重启自动修复
        let conn = st.conn.lock();
        conn.execute(
            "UPDATE messages SET group_id = 12345 WHERE onebot_message_id = ?1",
            [g1],
        )
        .unwrap();
        conn.execute(
            "UPDATE messages SET group_id = ?2 WHERE onebot_message_id = ?1",
            [p1, real_gid],
        )
        .unwrap();
    }
    let reopened = Storage::open(&path_str).expect("reopen heals without polluting private");
    assert_eq!(
        (
            msg_group_id(&reopened, g1),
            msg_group_id(&reopened, p1),
            msg_group_id(&reopened, p2)
        ),
        (real_gid, 0, 0)
    );
    let _ = std::fs::remove_file(&tmp_path);
}

#[test]
fn same_name_timestamp_split_and_schema_cleanup_isolate_chat_types() {
    // 验证运行时时间戳拆分候选查询不跨 chat_type 合并，同类型内部拆分仍正常归并
    let mem = Storage::open(MEMORY_DB_PATH).expect("open memory");
    let shared = "同名对象";
    let g_null = seed_session_msg(
        &mem,
        "sp-g-null",
        shared,
        (shared, "晚安"),
        (ChatTargetKind::Group, 100_000_001),
        (false, None, 1_700_000_200_000),
    );
    let p_ts = seed_session_msg(
        &mem,
        "sp-p-ts",
        shared,
        (shared, "晚安"),
        (ChatTargetKind::Private, 0),
        (false, Some("23:10"), 1_700_000_205_000),
    );
    assert_ne!(g_null, p_ts);

    let p_null = seed_session_msg(
        &mem,
        "sp-p-null2",
        shared,
        (shared, "早安"),
        (ChatTargetKind::Private, 0),
        (false, None, 1_700_000_210_000),
    );
    let g_ts = seed_session_msg(
        &mem,
        "sp-g-ts2",
        shared,
        (shared, "早安"),
        (ChatTargetKind::Group, 100_000_001),
        (false, Some("08:00"), 1_700_000_215_000),
    );
    assert_ne!(p_null, g_ts);
    let reused_p = seed_session_msg(
        &mem,
        "sp-p-ts2",
        shared,
        (shared, "早安"),
        (ChatTargetKind::Private, 0),
        (false, Some("08:01"), 1_700_000_220_000),
    );
    assert_eq!(reused_p, p_null);
    assert_eq!(mem.stats().expect("stats").message_count, 4);

    // 验证启动阶段 cleanup_historical_pollution 既不跨 chat_type 合并，也不被对侧类型阻断同类型去重
    let tmp_path = std::env::temp_dir().join(format!(
        "qq_uia_cleanup_chat_type_{}_1004.db",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&tmp_path);
    let path_str = tmp_path.to_string_lossy().to_string();
    {
        let st = Storage::open(&path_str).expect("seed cleanup db");
        let conn = st.conn.lock();
        conn.execute(
            "INSERT INTO messages (message_uid, group_name, group_id, user_id, sender_name, raw_message, is_self, timestamp_hint, captured_at_ms, chat_type)
             VALUES
             ('cross-g', '同名对象', 100000001, 1, '同名对象', 'CROSS-HELLO', 0, NULL, 1700000001000, 'group'),
             ('cross-p', '同名对象', 0, 1, '同名对象', 'CROSS-HELLO', 0, '14:00', 1700000010000, 'private'),
             ('same-g1', '同名对象', 100000001, 2, '群友乙', 'SAME-GRP', 0, NULL, 1700000002000, 'group'),
             ('same-g2', '同名对象', 100000001, 2, '群友乙', 'SAME-GRP', 0, '14:05', 1700000012000, 'group'),
             ('other-p', '同名对象', 0, 2, '群友乙', 'SAME-GRP', 0, '14:06', 1700000015000, 'private')",
            [],
        )
        .expect("insert cleanup test rows");
    }
    let reopened = Storage::open(&path_str).expect("reopen cleanup db");
    let grp_msgs = reopened
        .list_recent_messages_by_kind(shared, Some(ChatTargetKind::Group), 20)
        .unwrap();
    let priv_msgs = reopened
        .list_recent_messages_by_kind(shared, Some(ChatTargetKind::Private), 20)
        .unwrap();
    assert_eq!((grp_msgs.len(), priv_msgs.len()), (2, 2));
    assert!(grp_msgs.iter().any(|m| m.raw_message == "CROSS-HELLO"));
    assert!(priv_msgs.iter().any(|m| m.raw_message == "CROSS-HELLO"));
    assert_eq!(
        grp_msgs
            .iter()
            .find(|m| m.raw_message == "SAME-GRP")
            .unwrap()
            .timestamp_hint
            .as_deref(),
        Some("14:05")
    );
    let _ = std::fs::remove_file(&tmp_path);
}

#[test]
fn session_isolated_queries_and_recall_resolution_never_cross_chat_types() {
    let st = Storage::open(MEMORY_DB_PATH).expect("open memory");
    let now_ms = crate::repositories::now_ms();
    let shared = "同名会话";
    let g1 = seed_session_msg(
        &st,
        "iso-g1",
        shared,
        ("Alice", "group only phrase"),
        (ChatTargetKind::Group, 100_000_001),
        (false, None, now_ms.saturating_sub(15_000)),
    );
    let g2 = seed_session_msg(
        &st,
        "iso-g2",
        shared,
        ("BotSelf", "shared excerpt phrase"),
        (ChatTargetKind::Group, 100_000_001),
        (true, None, now_ms.saturating_sub(10_000)),
    );
    let p1 = seed_session_msg(
        &st,
        "iso-p1",
        shared,
        ("Alice", "private only secret"),
        (ChatTargetKind::Private, 0),
        (false, None, now_ms.saturating_sub(5_000)),
    );
    let p2 = seed_session_msg(
        &st,
        "iso-p2",
        shared,
        ("BotSelf", "shared excerpt phrase"),
        (ChatTargetKind::Private, 0),
        (true, None, now_ms.saturating_sub(2_000)),
    );

    // 1. list_recent_messages_by_kind 严格按 chat_type 过滤，None 时保留全量兼容
    let ids_by_kind = |kind: Option<ChatTargetKind>| -> Vec<i64> {
        st.list_recent_messages_by_kind(shared, kind, 10)
            .unwrap()
            .into_iter()
            .map(|r| r.onebot_message_id)
            .collect()
    };
    assert_eq!(ids_by_kind(Some(ChatTargetKind::Group)), vec![g1, g2]);
    assert_eq!(ids_by_kind(Some(ChatTargetKind::Private)), vec![p1, p2]);
    assert_eq!(ids_by_kind(None), vec![g1, g2, p1, p2]);

    // 2. find_recent_message_for_reply_in_session 绝不跨会话类型误命中
    for (kind, sender, excerpt, expected) in [
        (
            ChatTargetKind::Group,
            "BotSelf",
            "shared excerpt phrase",
            Some(g2),
        ),
        (
            ChatTargetKind::Private,
            "BotSelf",
            "shared excerpt phrase",
            Some(p2),
        ),
        (ChatTargetKind::Group, "Alice", "private only secret", None),
        (ChatTargetKind::Private, "Alice", "group only phrase", None),
    ] {
        assert_eq!(
            st.find_recent_message_for_reply_in_session(shared, kind, Some(sender), excerpt)
                .unwrap(),
            expected
        );
    }

    // 3. resolve_recalled_message_in_session 在 UID 与 180s 回退双路径均严格隔离
    let grp_uid_recall = st
        .resolve_recalled_message_in_session(
            shared,
            ChatTargetKind::Group,
            &["iso-p1".to_string()],
            Some("Alice"),
            false,
        )
        .unwrap()
        .expect("falls back to group alice rather than private uid");
    assert_eq!(
        (grp_uid_recall.onebot_message_id, grp_uid_recall.chat_type),
        (g1, ChatTargetKind::Group)
    );

    let grp_self = st
        .resolve_recalled_message_in_session(shared, ChatTargetKind::Group, &[], Some("你"), true)
        .unwrap()
        .expect("resolves g2");
    assert_eq!(grp_self.onebot_message_id, g2);
    assert!(st.delete_message_by_id(g2).unwrap());
    assert!(st.get_message_by_id(p2).unwrap().is_some());
    assert!(st
        .resolve_recalled_message_in_session(
            shared,
            ChatTargetKind::Group,
            &["iso-p2".to_string()],
            Some("你"),
            true
        )
        .unwrap()
        .is_none());
    let priv_self = st
        .resolve_recalled_message_in_session(shared, ChatTargetKind::Private, &[], Some("你"), true)
        .unwrap()
        .expect("resolves p2");
    assert_eq!(priv_self.onebot_message_id, p2);
}

#[test]
fn initialize_schema_creates_group_type_recent_index() {
    let storage = Storage::open(MEMORY_DB_PATH).expect("open memory");
    let conn = storage.conn.lock();
    let mut stmt = conn
        .prepare("PRAGMA index_info(idx_messages_group_type_recent)")
        .expect("prepare index_info");
    let cols = stmt
        .query_map([], |row| row.get::<_, String>(2))
        .unwrap()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(
        cols,
        vec![
            "group_name",
            "chat_type",
            "captured_at_ms",
            "onebot_message_id"
        ]
    );

    // 验证缺少 chat_type 列的 V2 遗留表经 initialize_schema 迁移后同样补齐该索引
    drop(stmt);
    drop(conn);
    let legacy = rusqlite::Connection::open_in_memory().expect("open legacy memory");
    legacy
        .execute_batch(
            "CREATE TABLE messages (
                onebot_message_id INTEGER PRIMARY KEY AUTOINCREMENT, message_uid TEXT NOT NULL UNIQUE,
                group_name TEXT NOT NULL, group_id INTEGER NOT NULL, user_id INTEGER NOT NULL,
                sender_name TEXT NOT NULL, raw_message TEXT NOT NULL, is_self INTEGER NOT NULL,
                timestamp_hint TEXT NULL, captured_at_ms INTEGER NOT NULL
            );",
        )
        .unwrap();
    schema::initialize_schema(&legacy).expect("migrate legacy schema");
    let idx: String = legacy
        .query_row(
            "SELECT name FROM sqlite_master WHERE type = 'index' AND tbl_name = 'messages' AND name = 'idx_messages_group_type_recent'",
            [],
            |row| row.get(0),
        )
        .expect("migrated index exists");
    assert_eq!(idx, "idx_messages_group_type_recent");
}
