//! 存储层共享 SQL 语句、表名与成员默认角色常量

/// 内存数据库特殊连接标识符
pub const MEMORY_DB_PATH: &str = ":memory:";
/// 合并转发节点在元数据表中的键前缀
pub const FORWARD_META_KEY_PREFIX: &str = "forward:";
/// 元数据表中合并转发节点包的最大有界保留条数
pub const MAX_STORED_FORWARD_BUNDLES: i64 = 512;
/// 审计日志表最大有界保留条数
pub const MAX_AUDIT_EVENTS: i64 = 5000;
/// 群成员角色缺省值
pub const DEFAULT_MEMBER_ROLE: &str = "member";
/// 消息标识表在 SQLite 内部序列表中的记录名称
pub const MESSAGES_TABLE_NAME: &str = "messages";

/// 各数据表名称常量
pub const TABLE_GROUPS: &str = "groups";
pub const TABLE_GROUP_MEMBERS: &str = "group_members";
pub const TABLE_FRIENDS: &str = "friends";
pub const TABLE_MESSAGES: &str = "messages";
pub const TABLE_KV_META: &str = "kv_meta";
pub const TABLE_AUDIT_EVENTS: &str = "audit_events";

/// 群组表插入与冲突合并 SQL 语句
pub const SQL_UPSERT_GROUP: &str = "INSERT INTO groups (
    group_name, real_group_id, onebot_group_id, member_count,
    max_member_count, last_preview, unread_count, drawer_checked_at_ms, updated_at_ms
 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, ?8)
 ON CONFLICT(group_name) DO UPDATE SET
    real_group_id = COALESCE(excluded.real_group_id, groups.real_group_id),
    member_count = COALESCE(excluded.member_count, groups.member_count),
    max_member_count = COALESCE(excluded.max_member_count, groups.max_member_count),
    last_preview = COALESCE(excluded.last_preview, groups.last_preview),
    unread_count = excluded.unread_count,
    updated_at_ms = excluded.updated_at_ms";

/// 群组抽屉巡检时间更新 SQL 语句
pub const SQL_MARK_DRAWER_CHECKED: &str = "INSERT INTO groups (
    group_name, onebot_group_id, unread_count, drawer_checked_at_ms, updated_at_ms
 ) VALUES (?1, ?2, 0, ?3, ?4)
 ON CONFLICT(group_name) DO UPDATE SET
    drawer_checked_at_ms = excluded.drawer_checked_at_ms,
    updated_at_ms = excluded.updated_at_ms";

/// 群成员表插入与非空字段保留更新 SQL 语句（未绑定真实 QQ 号时允许刷新映射用户号）
pub const SQL_UPSERT_MEMBER: &str = "INSERT INTO group_members (
    group_name, nickname, real_user_id, onebot_user_id, card, role, updated_at_ms
 ) VALUES (
    ?1, ?2, ?3, ?4, ?5, CASE WHEN ?6 != '' THEN ?6 ELSE 'member' END, ?7
 )
 ON CONFLICT(group_name, nickname) DO UPDATE SET
    real_user_id = COALESCE(excluded.real_user_id, group_members.real_user_id),
    onebot_user_id = CASE
        WHEN group_members.real_user_id IS NULL AND excluded.real_user_id IS NULL
            THEN excluded.onebot_user_id
        ELSE group_members.onebot_user_id
    END,
    card = CASE
        WHEN excluded.card != '' THEN excluded.card
        ELSE group_members.card
    END,
    role = CASE
        WHEN ?6 != '' THEN ?6
        ELSE group_members.role
    END,
    updated_at_ms = excluded.updated_at_ms";

/// 好友表插入与保留既有真实用户编号的合并更新 SQL 语句
pub const SQL_UPSERT_FRIEND: &str = "INSERT INTO friends (
    friend_key, nickname, remark, real_user_id, synthetic_user_id, last_preview, updated_at_ms
 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
 ON CONFLICT(friend_key) DO UPDATE SET
    nickname = CASE WHEN excluded.nickname != '' THEN excluded.nickname ELSE friends.nickname END,
    remark = CASE WHEN excluded.remark != '' THEN excluded.remark ELSE friends.remark END,
    real_user_id = COALESCE(excluded.real_user_id, friends.real_user_id),
    last_preview = COALESCE(excluded.last_preview, friends.last_preview),
    updated_at_ms = excluded.updated_at_ms";

/// 好友表按名称或备注删除污染记录的 SQL 语句
pub const SQL_DELETE_FRIEND_BY_NAME: &str =
    "DELETE FROM friends WHERE friend_key = ?1 OR nickname = ?1 OR remark = ?1";

/// 消息表防冲突插入 SQL 语句
pub const SQL_INSERT_MESSAGE: &str = "INSERT INTO messages (
    message_uid, group_name, group_id, user_id, sender_name,
    raw_message, is_self, timestamp_hint, captured_at_ms, chat_type, segments_json
 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
 ON CONFLICT(message_uid) DO NOTHING";

/// 按消息唯一键查询自增 OneBot 消息编号的 SQL 语句
pub const SQL_SELECT_MESSAGE_ID_BY_UID: &str =
    "SELECT onebot_message_id FROM messages WHERE message_uid = ?1";

/// 发现或更新真实群号后同步修正该群历史消息 `group_id` 的 SQL 语句
pub const SQL_SYNC_MESSAGES_GROUP_ID: &str = "UPDATE messages
 SET group_id = ?1
 WHERE group_name = ?2 AND group_id != ?1 AND chat_type = 'group'";

/// 启动阶段一次性修复尚未随真实群号回填的历史消息 `group_id` 的迁移 SQL
pub const SQL_HEAL_HISTORICAL_MESSAGE_GROUP_IDS: &str = "UPDATE messages
 SET group_id = (
     SELECT groups.real_group_id
     FROM groups
     WHERE groups.group_name = messages.group_name
 )
 WHERE EXISTS (
     SELECT 1
     FROM groups
     WHERE groups.group_name = messages.group_name
       AND groups.real_group_id IS NOT NULL
       AND groups.real_group_id != messages.group_id
 )
   AND messages.chat_type = 'group'";
