//! 嵌入式存储层单元测试模块：覆盖群元数据合并、幂等消息编号、成员读写与文件 WAL 重开持久化

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use common_types::constants::{DEFAULT_TARGET_GROUP_NAME, SQLITE_CACHE_SIZE_KIB};
use common_types::{ChatTargetKind, MessageSegment};
use rusqlite::Connection;

use super::*;
use crate::constants::MEMORY_DB_PATH;

fn unique_temp_db_path(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "qq_adapter_storage_{label}_{}_{nanos}.db",
        std::process::id()
    ))
}

#[test]
fn in_memory_groups_real_id_precedence_and_preservation() {
    let storage = Storage::open(MEMORY_DB_PATH).expect("open in-memory storage");
    assert_eq!(storage.db_path(), MEMORY_DB_PATH);
    assert_eq!(storage.inspect_pragma_i64("PRAGMA temp_store"), 2);
    assert_eq!(
        storage.inspect_pragma_i64("PRAGMA cache_size"),
        i64::from(SQLITE_CACHE_SIZE_KIB)
    );

    let first_record = storage
        .upsert_group(&UpsertGroupInput {
            group_name: DEFAULT_TARGET_GROUP_NAME.to_string(),
            real_group_id: None,
            member_count: Some(9),
            max_member_count: Some(200),
            last_preview: Some("hi".to_string()),
            unread_count: 1,
        })
        .expect("first upsert");
    assert!(first_record.real_group_id.is_none());
    assert_eq!(
        first_record.effective_group_id(),
        first_record.onebot_group_id
    );
    assert_eq!(first_record.member_count, Some(9));
    assert_eq!(first_record.max_member_count, Some(200));
    assert_eq!(first_record.last_preview.as_deref(), Some("hi"));
    assert_eq!(first_record.unread_count, 1);

    storage
        .mark_group_drawer_checked(DEFAULT_TARGET_GROUP_NAME, 123_456)
        .expect("mark drawer checked");

    // 二次更新传入真实群号并将成员数与预览留空，校验库内已有字段不会被覆盖丢失
    let second_record = storage
        .upsert_group(&UpsertGroupInput {
            group_name: DEFAULT_TARGET_GROUP_NAME.to_string(),
            real_group_id: Some(987_654_321),
            member_count: None,
            max_member_count: None,
            last_preview: None,
            unread_count: 0,
        })
        .expect("second upsert");
    assert_eq!(second_record.onebot_group_id, first_record.onebot_group_id);
    assert_eq!(second_record.real_group_id, Some(987_654_321));
    assert_eq!(second_record.effective_group_id(), 987_654_321);
    assert_eq!(second_record.member_count, Some(9));
    assert_eq!(second_record.max_member_count, Some(200));
    assert_eq!(second_record.last_preview.as_deref(), Some("hi"));
    assert_eq!(second_record.drawer_checked_at_ms, Some(123_456));
    assert_eq!(second_record.unread_count, 0);

    let by_real_id = storage
        .get_group_by_id(987_654_321)
        .expect("lookup real id")
        .expect("group present");
    let by_synthetic_id = storage
        .get_group_by_id(first_record.onebot_group_id)
        .expect("lookup synthetic id")
        .expect("group present");
    assert_eq!(by_real_id.group_name, DEFAULT_TARGET_GROUP_NAME);
    assert_eq!(by_synthetic_id.group_name, DEFAULT_TARGET_GROUP_NAME);

    let group_list = storage.list_groups().expect("list groups");
    assert_eq!(group_list.len(), 1);
    assert_eq!(group_list[0].group_id, 987_654_321);
    assert_eq!(group_list[0].real_group_id, Some(987_654_321));
    assert_eq!(group_list[0].member_count, 9);
    assert_eq!(group_list[0].max_member_count, 200);
}

#[test]
fn idempotent_record_message_and_starting_sequence() {
    let storage = Storage::open(MEMORY_DB_PATH).expect("open in-memory");
    let first_message_id = storage
        .record_message(&RecordMessageInput {
            message_uid: "uid-1".to_string(),
            group_name: DEFAULT_TARGET_GROUP_NAME.to_string(),
            group_id: 555,
            user_id: 222,
            sender_name: "Alice".to_string(),
            raw_message: "hello world".to_string(),
            is_self: false,
            timestamp_hint: Some("20:00".to_string()),
            captured_at_ms: 1_700_000_001_000,
            ..Default::default()
        })
        .expect("record first");
    assert_eq!(first_message_id, 10_001);

    // 重复写入相同 UID 时应直接返回既有 ID，且不能空耗自增序号
    let duplicate_id = storage
        .record_message(&RecordMessageInput {
            message_uid: "uid-1".to_string(),
            group_name: DEFAULT_TARGET_GROUP_NAME.to_string(),
            group_id: 555,
            user_id: 222,
            sender_name: "Alice".to_string(),
            raw_message: "hello world".to_string(),
            is_self: false,
            timestamp_hint: Some("20:00".to_string()),
            captured_at_ms: 1_700_000_001_000,
            ..Default::default()
        })
        .expect("duplicate uid returns same id");
    assert_eq!(duplicate_id, first_message_id);

    let second_message_id = storage
        .record_message(&RecordMessageInput {
            message_uid: "uid-2".to_string(),
            group_name: DEFAULT_TARGET_GROUP_NAME.to_string(),
            group_id: 555,
            user_id: 10001,
            sender_name: "Bot".to_string(),
            raw_message: "reply".to_string(),
            is_self: true,
            timestamp_hint: None,
            captured_at_ms: 1_700_000_002_000,
            ..Default::default()
        })
        .expect("record second without burning seq");
    assert_eq!(second_message_id, 10_002);

    let fetched_record = storage
        .get_message_by_id(first_message_id)
        .expect("get message")
        .expect("exists");
    assert_eq!(fetched_record.message_uid, "uid-1");
    assert_eq!(fetched_record.sender_name, "Alice");
    assert!(!fetched_record.is_self);

    let recent_one = storage
        .list_recent_messages(DEFAULT_TARGET_GROUP_NAME, 1)
        .expect("recent limit 1");
    assert_eq!(recent_one.len(), 1);
    assert_eq!(recent_one[0].onebot_message_id, second_message_id);

    let recent_two = storage
        .list_recent_messages(DEFAULT_TARGET_GROUP_NAME, 10)
        .expect("recent limit 10");
    assert_eq!(recent_two.len(), 2);
    assert_eq!(recent_two[0].onebot_message_id, first_message_id);
    assert_eq!(recent_two[1].onebot_message_id, second_message_id);
}

#[test]
fn members_upsert_query_meta_audit_and_stats() {
    let storage = Storage::open(MEMORY_DB_PATH).expect("open in-memory");
    let group_record = storage
        .upsert_group(&UpsertGroupInput {
            group_name: DEFAULT_TARGET_GROUP_NAME.to_string(),
            member_count: Some(2),
            ..Default::default()
        })
        .expect("upsert group");
    let effective_gid = group_record.effective_group_id();

    storage
        .upsert_group_members(
            DEFAULT_TARGET_GROUP_NAME,
            effective_gid,
            &[
                UpsertMemberInput {
                    nickname: "Alice".to_string(),
                    real_user_id: Some(777_888),
                    card: "A-Card".to_string(),
                    role: "admin".to_string(),
                },
                UpsertMemberInput {
                    nickname: "Bob".to_string(),
                    real_user_id: None,
                    card: String::new(),
                    role: String::new(),
                },
            ],
        )
        .expect("upsert members");

    // 浅层复查仅返回昵称且其余字段留空时，必须保留原有的真实 QQ 号、群名片与管理员角色
    storage
        .upsert_group_members(
            DEFAULT_TARGET_GROUP_NAME,
            effective_gid,
            &[UpsertMemberInput {
                nickname: "Alice".to_string(),
                ..Default::default()
            }],
        )
        .expect("second upsert preserves");

    let member_list = storage
        .list_group_members(DEFAULT_TARGET_GROUP_NAME, effective_gid)
        .expect("list members");
    assert_eq!(member_list.len(), 2);

    let alice_info = storage
        .get_group_member_by_id(DEFAULT_TARGET_GROUP_NAME, 777_888, effective_gid)
        .expect("get alice")
        .expect("alice present");
    assert_eq!(alice_info.user_id, 777_888);
    assert_eq!(alice_info.card, "A-Card");
    assert_eq!(alice_info.role, "admin");

    let bob_info = member_list
        .iter()
        .find(|member| member.nickname == "Bob")
        .expect("bob present");
    assert!(bob_info.real_user_id.is_none());
    assert!(bob_info.user_id > 0);
    assert_eq!(bob_info.role, "member");

    let bob_by_id = storage
        .get_group_member_by_id(DEFAULT_TARGET_GROUP_NAME, bob_info.user_id, effective_gid)
        .expect("get bob by synthetic id")
        .expect("bob present");
    assert_eq!(bob_by_id.nickname, "Bob");

    storage.set_meta("self_nick", "BotNick").expect("set meta");
    assert_eq!(
        storage.get_meta("self_nick").expect("get meta").as_deref(),
        Some("BotNick")
    );
    storage
        .set_meta("self_nick", "BotNick2")
        .expect("overwrite meta");
    assert_eq!(
        storage.get_meta("self_nick").expect("get meta2").as_deref(),
        Some("BotNick2")
    );

    storage
        .append_audit("drawer_inspect", r#"{"group":"Demo测试群"}"#)
        .expect("append audit");
    storage
        .append_audit_json("group_sync", &alice_info)
        .expect("append audit json");

    let storage_stats = storage.stats().expect("stats");
    assert_eq!(storage_stats.group_count, 1);
    assert_eq!(storage_stats.member_count, 2);
    assert_eq!(storage_stats.message_count, 0);
    assert_eq!(storage_stats.audit_count, 2);
}

#[test]
fn temp_file_wal_and_reopen_preserves_data_and_sequence() {
    let db_path = unique_temp_db_path("reopen");
    let db_path_str = db_path.to_string_lossy().to_string();

    {
        let storage = Storage::open(&db_path_str).expect("open file db");
        assert_eq!(
            storage
                .inspect_pragma_text("PRAGMA journal_mode")
                .to_ascii_lowercase(),
            "wal"
        );
        assert_eq!(storage.inspect_pragma_i64("PRAGMA temp_store"), 2);
        assert_eq!(
            storage.inspect_pragma_i64("PRAGMA cache_size"),
            i64::from(SQLITE_CACHE_SIZE_KIB)
        );

        storage
            .upsert_group(&UpsertGroupInput {
                group_name: "TestGroup".to_string(),
                real_group_id: Some(12345),
                member_count: Some(5),
                ..Default::default()
            })
            .expect("insert group");
        let first_id = storage
            .record_message(&RecordMessageInput {
                message_uid: "persist-1".to_string(),
                group_name: "TestGroup".to_string(),
                group_id: 12345,
                user_id: 99,
                sender_name: "Tester".to_string(),
                raw_message: "before close".to_string(),
                is_self: false,
                timestamp_hint: None,
                captured_at_ms: 1_700_000_000_001,
                ..Default::default()
            })
            .expect("record message");
        assert_eq!(first_id, 10_001);
        storage.set_meta("version", "0.2.0").expect("set meta");
        storage
            .append_audit("startup", r#"{"ok":true}"#)
            .expect("append audit");
    }

    {
        let reopened = Storage::open(&db_path_str).expect("reopen file db");
        let group_record = reopened
            .get_group_by_name("TestGroup")
            .expect("get group")
            .expect("group exists after reopen");
        assert_eq!(group_record.real_group_id, Some(12345));

        let next_id = reopened
            .record_message(&RecordMessageInput {
                message_uid: "persist-2".to_string(),
                group_name: "TestGroup".to_string(),
                group_id: 12345,
                user_id: 99,
                sender_name: "Tester".to_string(),
                raw_message: "after reopen".to_string(),
                is_self: true,
                timestamp_hint: None,
                captured_at_ms: 1_700_000_000_002,
                ..Default::default()
            })
            .expect("next seq increments after reopen");
        assert_eq!(next_id, 10_002);

        assert_eq!(
            reopened.get_meta("version").expect("get meta").as_deref(),
            Some("0.2.0")
        );
        let storage_stats = reopened.stats().expect("stats");
        assert_eq!(storage_stats.group_count, 1);
        assert_eq!(storage_stats.message_count, 2);
        assert_eq!(storage_stats.audit_count, 1);
    }

    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_file(format!("{db_path_str}-wal"));
    let _ = std::fs::remove_file(format!("{db_path_str}-shm"));
}

#[test]
fn real_group_id_discovery_heals_historical_messages_and_batches_work() {
    let storage = Storage::open(MEMORY_DB_PATH).expect("open in-memory");
    let synthetic_gid = 100_000_005_i64;
    let real_gid = 100_000_001_i64;

    storage
        .upsert_groups_batch(&[
            UpsertGroupInput {
                group_name: DEFAULT_TARGET_GROUP_NAME.to_string(),
                member_count: Some(9),
                last_preview: Some("preview-1".to_string()),
                unread_count: 1,
                ..Default::default()
            },
            UpsertGroupInput {
                group_name: "备选测试群".to_string(),
                member_count: Some(4),
                unread_count: 0,
                ..Default::default()
            },
        ])
        .expect("batch upsert groups");

    let batch_ids = storage
        .record_messages_batch(&[
            RecordMessageInput {
                message_uid: "heal-1".to_string(),
                group_name: DEFAULT_TARGET_GROUP_NAME.to_string(),
                group_id: synthetic_gid,
                user_id: 111,
                sender_name: "群友甲".to_string(),
                raw_message: "msg-1".to_string(),
                is_self: false,
                timestamp_hint: None,
                captured_at_ms: 1_700_000_001_000,
                ..Default::default()
            },
            RecordMessageInput {
                message_uid: "heal-1".to_string(),
                group_name: DEFAULT_TARGET_GROUP_NAME.to_string(),
                group_id: synthetic_gid,
                user_id: 111,
                sender_name: "群友甲".to_string(),
                raw_message: "msg-1".to_string(),
                is_self: false,
                timestamp_hint: None,
                captured_at_ms: 1_700_000_001_000,
                ..Default::default()
            },
            RecordMessageInput {
                message_uid: "heal-2".to_string(),
                group_name: DEFAULT_TARGET_GROUP_NAME.to_string(),
                group_id: synthetic_gid,
                user_id: 222,
                sender_name: "群友乙".to_string(),
                raw_message: "msg-2".to_string(),
                is_self: true,
                timestamp_hint: None,
                captured_at_ms: 1_700_000_002_000,
                ..Default::default()
            },
        ])
        .expect("batch record messages");
    assert_eq!(batch_ids, vec![10_001, 10_001, 10_002]);

    // 发现真实数字群号后调用 upsert_group，历史消息应被同事务回填为真实群号
    storage
        .upsert_group(&UpsertGroupInput {
            group_name: DEFAULT_TARGET_GROUP_NAME.to_string(),
            real_group_id: Some(real_gid),
            member_count: Some(9),
            ..Default::default()
        })
        .expect("upsert real group id");

    let msg1 = storage
        .get_message_by_id(10_001)
        .expect("get msg1")
        .expect("msg1 exists");
    let recent = storage
        .list_recent_messages(DEFAULT_TARGET_GROUP_NAME, 10)
        .expect("list recent");
    assert_eq!(msg1.group_id, real_gid);
    assert_eq!(recent.len(), 2);
    assert!(recent.iter().all(|item| item.group_id == real_gid));

    let groups = storage.list_groups().expect("list groups");
    let hoyo = groups
        .iter()
        .find(|item| item.group_name == DEFAULT_TARGET_GROUP_NAME)
        .expect("hoyo present");
    assert_eq!(hoyo.group_id, real_gid);
    assert_eq!(hoyo.max_member_count, 200);
}

#[test]
fn v2_legacy_file_db_migrates_and_friends_segments_reply_work() {
    let db_path = unique_temp_db_path("v2_migrate");
    let db_path_str = db_path.to_string_lossy().to_string();

    // 构造不含 friends 表及 V3 新列的旧版数据库，验证升级迁移幂等无缝
    {
        let legacy = Connection::open(&db_path).expect("open raw sqlite");
        legacy
            .execute_batch(
                "CREATE TABLE groups (
                    group_name TEXT PRIMARY KEY,
                    real_group_id INTEGER NULL,
                    onebot_group_id INTEGER NOT NULL,
                    member_count INTEGER NULL,
                    max_member_count INTEGER NULL,
                    last_preview TEXT NULL,
                    unread_count INTEGER NOT NULL DEFAULT 0,
                    drawer_checked_at_ms INTEGER NULL,
                    updated_at_ms INTEGER NOT NULL
                );
                CREATE TABLE group_members (
                    group_name TEXT NOT NULL,
                    nickname TEXT NOT NULL,
                    real_user_id INTEGER NULL,
                    onebot_user_id INTEGER NOT NULL,
                    card TEXT NOT NULL DEFAULT '',
                    role TEXT NOT NULL DEFAULT 'member',
                    updated_at_ms INTEGER NOT NULL,
                    PRIMARY KEY (group_name, nickname)
                );
                CREATE TABLE messages (
                    onebot_message_id INTEGER PRIMARY KEY AUTOINCREMENT,
                    message_uid TEXT NOT NULL UNIQUE,
                    group_name TEXT NOT NULL,
                    group_id INTEGER NOT NULL,
                    user_id INTEGER NOT NULL,
                    sender_name TEXT NOT NULL,
                    raw_message TEXT NOT NULL,
                    is_self INTEGER NOT NULL,
                    timestamp_hint TEXT NULL,
                    captured_at_ms INTEGER NOT NULL
                );
                INSERT INTO messages VALUES (
                    10001, 'legacy-1', 'Demo测试群', 100000001, 123,
                    '测试好友A', '请测试引用一下这条消息', 0, '12:00', 1700000000000
                );",
            )
            .expect("seed v2 db");
    }

    {
        let storage = Storage::open(&db_path_str).expect("migrate v2 db to v3");
        let legacy_msg = storage
            .get_message_by_id(10_001)
            .expect("query migrated msg")
            .expect("migrated msg exists");
        assert_eq!(legacy_msg.chat_type, ChatTargetKind::Group);
        assert_eq!(
            legacy_msg.segments,
            vec![MessageSegment::Text {
                text: "请测试引用一下这条消息".to_string()
            }]
        );

        let reply_target = storage
            .find_recent_message_for_reply("Demo测试群", Some("测试好友A"), "请测试引用一下这条")
            .expect("find reply target");
        assert_eq!(reply_target, Some(10_001));

        let friend = storage
            .upsert_friend(&UpsertFriendInput {
                nickname: "测试好友A".to_string(),
                remark: String::new(),
                real_user_id: Some(778899),
                last_preview: Some("你好呀".to_string()),
            })
            .expect("upsert friend");
        assert_eq!(friend.effective_user_id(), 778899);
        assert_eq!(
            storage
                .get_friend_by_id(778899)
                .expect("by id")
                .expect("friend present")
                .nickname,
            "测试好友A"
        );
        assert_eq!(storage.list_friends().expect("list friends").len(), 1);
        assert_eq!(storage.stats().expect("stats").friend_count, 1);
        assert!(storage
            .delete_friend_by_name("测试好友A")
            .expect("delete friend by name first"));
        assert!(!storage
            .delete_friend_by_name("测试好友A")
            .expect("delete friend by name second"));
        assert!(storage
            .list_friends()
            .expect("list empty friends")
            .is_empty());
        storage
            .upsert_friend(&UpsertFriendInput {
                nickname: "测试好友A".to_string(),
                remark: String::new(),
                real_user_id: Some(778899),
                last_preview: Some("你好呀".to_string()),
            })
            .expect("restore friend for message test");

        let rich_id = storage
            .record_message(&RecordMessageInput {
                message_uid: "rich-private-1".to_string(),
                group_name: "测试好友A".to_string(),
                group_id: 778899,
                user_id: 778899,
                sender_name: "测试好友A".to_string(),
                raw_message: "[CQ:reply,id=10001][CQ:at,qq=200000001] 收到".to_string(),
                is_self: false,
                timestamp_hint: Some("12:01".to_string()),
                captured_at_ms: 1_700_000_005_000,
                chat_type: ChatTargetKind::Private,
                segments: vec![
                    MessageSegment::Reply {
                        id: "10001".to_string(),
                    },
                    MessageSegment::At {
                        qq: "200000001".to_string(),
                        name: Some("测试机器人".to_string()),
                    },
                    MessageSegment::Text {
                        text: " 收到".to_string(),
                    },
                ],
            })
            .expect("record rich private msg");
        let rich = storage
            .get_message_by_id(rich_id)
            .expect("get rich")
            .expect("rich exists");
        assert_eq!(rich.chat_type, ChatTargetKind::Private);
        assert_eq!(rich.segments.len(), 3);
    }

    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_file(format!("{db_path_str}-wal"));
    let _ = std::fs::remove_file(format!("{db_path_str}-shm"));
}

#[test]
fn qa_storage_timestamp_saturation_and_bounded_retention() {
    let storage = Storage::open(MEMORY_DB_PATH).expect("open memory");

    // 验证 QA-BUG-008：空白群名写入拒绝以及 u64::MAX 时间戳不发生负数回绕
    let empty_err = storage.upsert_group(&UpsertGroupInput {
        group_name: "   ".to_string(),
        ..Default::default()
    });
    assert!(empty_err.is_err());

    storage
        .record_message(&RecordMessageInput {
            message_uid: "ts-early".to_string(),
            group_name: "BoundaryGroup".to_string(),
            group_id: 1001,
            user_id: 2001,
            sender_name: "A".to_string(),
            raw_message: "early".to_string(),
            captured_at_ms: 1000,
            ..Default::default()
        })
        .expect("record early");
    storage
        .record_message(&RecordMessageInput {
            message_uid: "ts-max".to_string(),
            group_name: "BoundaryGroup".to_string(),
            group_id: 1001,
            user_id: 2001,
            sender_name: "A".to_string(),
            raw_message: "max".to_string(),
            captured_at_ms: u64::MAX,
            ..Default::default()
        })
        .expect("record u64 max");
    let recent = storage
        .list_recent_messages("BoundaryGroup", 5)
        .expect("recent boundary");
    assert_eq!(recent.len(), 2);
    assert_eq!(recent[0].raw_message, "early");
    assert_eq!(recent[1].raw_message, "max");

    // 验证 QA-BUG-009：forward:* 键按 512 上限淘汰且 audit_events 严格控制在 5000 条以内
    for idx in 0..600 {
        storage
            .set_meta(&format!("forward:fwd-{idx}"), "[{\"type\":\"node\"}]")
            .expect("set bounded forward");
    }
    assert!(storage
        .get_meta("forward:fwd-599")
        .expect("latest forward")
        .is_some());
    assert!(storage
        .get_meta("forward:fwd-0")
        .expect("evicted oldest forward")
        .is_none());

    for idx in 0..5200 {
        storage
            .append_audit("qa_flood", &format!("{{\"seq\":{idx}}}"))
            .expect("append bounded audit");
    }
    assert!(storage.stats().expect("stats").audit_count <= 5000);

    // 验证 delete_message_by_id 删除已撤回消息及重复删除幂等行为
    let deleted_target_id = recent[0].onebot_message_id;
    assert!(storage
        .delete_message_by_id(deleted_target_id)
        .expect("delete existing message"));
    assert!(storage
        .get_message_by_id(deleted_target_id)
        .expect("query deleted message")
        .is_none());
    assert!(!storage
        .delete_message_by_id(deleted_target_id)
        .expect("repeat delete returns false"));
}

#[test]
fn structured_file_video_preserved_and_delete_by_onebot_id_works() {
    let storage = Storage::open(MEMORY_DB_PATH).expect("open memory");

    // 校验 File 与 Video 结构化段在相同 UID 回采纯文本时不会被覆盖丢失
    let file_id = storage
        .record_message(&RecordMessageInput {
            message_uid: "self-file-1".to_string(),
            group_name: DEFAULT_TARGET_GROUP_NAME.to_string(),
            group_id: 100_000_001,
            user_id: 10001,
            sender_name: "Bot".to_string(),
            raw_message: "[CQ:file,file=report.pdf,name=report.pdf,size=2048]".to_string(),
            is_self: true,
            timestamp_hint: None,
            captured_at_ms: 1_700_000_000_000,
            chat_type: ChatTargetKind::Group,
            segments: vec![MessageSegment::File {
                file: "report.pdf".to_string(),
                name: Some("report.pdf".to_string()),
                size: Some(2048),
            }],
        })
        .expect("record file msg");
    let reused_file_id = storage
        .record_message(&RecordMessageInput {
            message_uid: "self-file-1".to_string(),
            group_name: DEFAULT_TARGET_GROUP_NAME.to_string(),
            group_id: 100_000_001,
            user_id: 10001,
            sender_name: "Bot".to_string(),
            raw_message: "[文件:report.pdf]".to_string(),
            is_self: true,
            timestamp_hint: Some("10:00".to_string()),
            captured_at_ms: 1_700_000_001_000,
            chat_type: ChatTargetKind::Group,
            segments: vec![MessageSegment::Text {
                text: "[文件:report.pdf]".to_string(),
            }],
        })
        .expect("re-record plain text for same file uid");
    assert_eq!(reused_file_id, file_id);
    let stored_file = storage
        .get_message_by_id(file_id)
        .expect("get file")
        .expect("file exists");
    assert!(matches!(
        stored_file.segments.as_slice(),
        [MessageSegment::File {
            size: Some(2048),
            ..
        }]
    ));
    assert_eq!(stored_file.timestamp_hint.as_deref(), Some("10:00"));

    // 校验 Video fallback 文本判定函数覆盖 "\"file\"" 与 "\"video\""
    let video_id = storage
        .record_message(&RecordMessageInput {
            message_uid: "self-video-1".to_string(),
            group_name: DEFAULT_TARGET_GROUP_NAME.to_string(),
            group_id: 100_000_001,
            user_id: 10001,
            sender_name: "Bot".to_string(),
            raw_message: "[CQ:video,file=clip.mp4]".to_string(),
            is_self: true,
            timestamp_hint: None,
            captured_at_ms: 1_700_000_002_000,
            chat_type: ChatTargetKind::Group,
            segments: vec![MessageSegment::Video {
                file: "clip.mp4".to_string(),
                url: None,
            }],
        })
        .expect("record video msg");
    let _ = storage
        .record_message(&RecordMessageInput {
            message_uid: "self-video-1".to_string(),
            group_name: DEFAULT_TARGET_GROUP_NAME.to_string(),
            group_id: 100_000_001,
            user_id: 10001,
            sender_name: "Bot".to_string(),
            raw_message: "[视频]".to_string(),
            is_self: true,
            timestamp_hint: None,
            captured_at_ms: 1_700_000_003_000,
            chat_type: ChatTargetKind::Group,
            segments: vec![MessageSegment::Text {
                text: "[视频]".to_string(),
            }],
        })
        .expect("re-record plain text for video uid");
    let stored_video = storage
        .get_message_by_id(video_id)
        .expect("get video")
        .expect("video exists");
    assert!(matches!(
        stored_video.segments.as_slice(),
        [MessageSegment::Video { .. }]
    ));

    // 校验 delete_message_by_onebot_id 语义别名及其幂等性
    assert!(storage
        .delete_message_by_onebot_id(file_id)
        .expect("delete by onebot id"));
    assert!(storage
        .get_message_by_id(file_id)
        .expect("query deleted")
        .is_none());
    assert!(!storage
        .delete_message_by_onebot_id(file_id)
        .expect("repeat delete by onebot id returns false"));
}

#[test]
fn resolve_recalled_message_prefers_uid_matches_and_enforces_180s_fallback_window() {
    let storage = Storage::open(MEMORY_DB_PATH).expect("open memory");
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    // 写入多条不同群、不同发送者、不同时间窗的消息记录
    let other_group_id = storage
        .record_message(&RecordMessageInput {
            message_uid: "uid-other-group".to_string(),
            group_name: "其他群".to_string(),
            group_id: 999,
            user_id: 201,
            sender_name: "Alice".to_string(),
            raw_message: "wrong group".to_string(),
            is_self: false,
            captured_at_ms: now_ms.saturating_sub(1_000),
            ..Default::default()
        })
        .expect("other group");
    let _ = other_group_id;

    let stale_alice_id = storage
        .record_message(&RecordMessageInput {
            message_uid: "uid-alice-stale".to_string(),
            group_name: DEFAULT_TARGET_GROUP_NAME.to_string(),
            group_id: 100_000_001,
            user_id: 201,
            sender_name: "Alice".to_string(),
            raw_message: "stale alice".to_string(),
            is_self: false,
            captured_at_ms: now_ms.saturating_sub(240_000),
            ..Default::default()
        })
        .expect("stale alice");

    let recent_alice_id = storage
        .record_message(&RecordMessageInput {
            message_uid: "uid-alice-recent".to_string(),
            group_name: DEFAULT_TARGET_GROUP_NAME.to_string(),
            group_id: 100_000_001,
            user_id: 201,
            sender_name: "Alice".to_string(),
            raw_message: "recent alice".to_string(),
            is_self: false,
            captured_at_ms: now_ms.saturating_sub(20_000),
            ..Default::default()
        })
        .expect("recent alice");

    let recent_bob_id = storage
        .record_message(&RecordMessageInput {
            message_uid: "uid-bob-recent".to_string(),
            group_name: DEFAULT_TARGET_GROUP_NAME.to_string(),
            group_id: 100_000_001,
            user_id: 202,
            sender_name: "Bob".to_string(),
            raw_message: "recent bob".to_string(),
            is_self: false,
            captured_at_ms: now_ms.saturating_sub(5_000),
            ..Default::default()
        })
        .expect("recent bob");

    let recent_self_id = storage
        .record_message(&RecordMessageInput {
            message_uid: "uid-self-recent".to_string(),
            group_name: DEFAULT_TARGET_GROUP_NAME.to_string(),
            group_id: 100_000_001,
            user_id: 10001,
            sender_name: "测试机器人".to_string(),
            raw_message: "self recent".to_string(),
            is_self: true,
            captured_at_ms: now_ms.saturating_sub(3_000),
            ..Default::default()
        })
        .expect("recent self");

    // 1. 多个消失 UID 同时出现时，按 actor_is_self 与 sender_name 偏好精准命中
    let uid_matched_alice = storage
        .resolve_recalled_message(
            DEFAULT_TARGET_GROUP_NAME,
            &[
                "uid-bob-recent".to_string(),
                "uid-alice-recent".to_string(),
                "uid-self-recent".to_string(),
            ],
            Some("Alice"),
            false,
        )
        .expect("uid match alice")
        .expect("should resolve alice");
    assert_eq!(uid_matched_alice.onebot_message_id, recent_alice_id);

    let uid_matched_self = storage
        .resolve_recalled_message(
            DEFAULT_TARGET_GROUP_NAME,
            &["uid-bob-recent".to_string(), "uid-self-recent".to_string()],
            Some("你"),
            true,
        )
        .expect("uid match self")
        .expect("should resolve self");
    assert_eq!(uid_matched_self.onebot_message_id, recent_self_id);

    // 跨群 UID 绝不能跨群直接命中（空群应返回 None，原群仅回落到本群近期 Alice 消息）
    assert!(storage
        .resolve_recalled_message(
            "无消息群",
            &["uid-other-group".to_string()],
            Some("Alice"),
            false,
        )
        .expect("empty group cross-group uid returns none")
        .is_none());
    let cross_fallback = storage
        .resolve_recalled_message(
            DEFAULT_TARGET_GROUP_NAME,
            &["uid-other-group".to_string()],
            Some("Alice"),
            false,
        )
        .expect("cross group uid falls back to current group")
        .expect("current group has recent alice");
    assert_eq!(cross_fallback.onebot_message_id, recent_alice_id);
    assert_ne!(cross_fallback.onebot_message_id, other_group_id);

    // 2. 无命中消失 UID 时回退到 180_000 ms 时间窗：校验发送者过滤与本端标记
    let fallback_alice = storage
        .resolve_recalled_message(DEFAULT_TARGET_GROUP_NAME, &[], Some("Alice"), false)
        .expect("fallback alice")
        .expect("recent alice within 180s");
    assert_eq!(fallback_alice.onebot_message_id, recent_alice_id);

    let fallback_none_sender = storage
        .resolve_recalled_message(DEFAULT_TARGET_GROUP_NAME, &[], None, false)
        .expect("fallback latest non-self")
        .expect("bob is latest non-self");
    assert_eq!(fallback_none_sender.onebot_message_id, recent_bob_id);

    let fallback_self = storage
        .resolve_recalled_message(DEFAULT_TARGET_GROUP_NAME, &[], Some("你"), true)
        .expect("fallback self")
        .expect("self within 180s");
    assert_eq!(fallback_self.onebot_message_id, recent_self_id);

    // 180 秒窗口内不存在指定发送者时返回 None（不误命中 Bob）
    assert!(storage
        .resolve_recalled_message(DEFAULT_TARGET_GROUP_NAME, &[], Some("Charlie"), false)
        .expect("missing sender returns none")
        .is_none());

    // 3. 精确验证 180_000 ms 边界（仅余 240s 前的过期消息时回退必须返回 None）
    assert!(storage
        .delete_message_by_onebot_id(recent_alice_id)
        .unwrap());
    assert!(storage
        .resolve_recalled_message(DEFAULT_TARGET_GROUP_NAME, &[], Some("Alice"), false)
        .expect("expired alice returns none")
        .is_none());
    // 即使消息已超 180s，只要出现在 disappeared_uids 中仍可直接命中
    let stale_uid_hit = storage
        .resolve_recalled_message(
            DEFAULT_TARGET_GROUP_NAME,
            &["uid-alice-stale".to_string()],
            Some("Alice"),
            false,
        )
        .expect("stale uid direct match")
        .expect("direct uid match ignores 180s window");
    assert_eq!(stale_uid_hit.onebot_message_id, stale_alice_id);

    // 校验边界函数精确包含 180_000 ms 并拒绝 180_001 ms
    let conn = storage.conn.lock();
    let boundary_ts = now_ms.saturating_sub(240_000) + 180_000;
    assert_eq!(
        repositories::message_repo::resolve_recalled_message_at(
            &conn,
            DEFAULT_TARGET_GROUP_NAME,
            &[],
            Some("Alice"),
            false,
            boundary_ts,
        )
        .expect("inclusive 180_000 ms boundary")
        .map(|r| r.onebot_message_id),
        Some(stale_alice_id)
    );
    assert!(repositories::message_repo::resolve_recalled_message_at(
        &conn,
        DEFAULT_TARGET_GROUP_NAME,
        &[],
        Some("Alice"),
        false,
        boundary_ts + 1,
    )
    .expect("exclusive 180_001 ms boundary")
    .is_none());
}
