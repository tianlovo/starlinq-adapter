//! 数据库表结构迁移与 SQLite 性能及内存缓存调优模块

use common_types::constants::{
    INITIAL_MESSAGE_SEQUENCE, SQLITE_BUSY_TIMEOUT_MS, SQLITE_CACHE_SIZE_KIB,
};
use common_types::Result;
use rusqlite::{params, Connection};

use crate::constants::{
    MESSAGES_TABLE_NAME, SQL_HEAL_HISTORICAL_MESSAGE_GROUP_IDS, TABLE_MESSAGES,
};
use crate::repositories::map_sqlite_err;

fn ensure_column_exists(
    conn: &Connection,
    table_name: &str,
    column_name: &str,
    alter_sql: &str,
) -> Result<()> {
    let pragma_sql = format!("PRAGMA table_info({table_name})");
    let mut stmt = conn.prepare(&pragma_sql).map_err(map_sqlite_err)?;
    let existing = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(map_sqlite_err)?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(map_sqlite_err)?;
    if !existing.iter().any(|name| name == column_name) {
        conn.execute(alter_sql, []).map_err(map_sqlite_err)?;
    }
    Ok(())
}

/// 启动阶段幂等清理历史版本遗留的全窗口抓取假消息、搜索噪声会话与时间分割重复行
fn cleanup_historical_pollution(conn: &Connection) -> Result<()> {
    conn.execute(
        "DELETE FROM messages
         WHERE sender_name = '切换为经典模式'
            OR raw_message LIKE '切换为经典模式%'",
        [],
    )
    .map_err(map_sqlite_err)?;

    conn.execute(
        "DELETE FROM groups
         WHERE real_group_id IS NULL
           AND (group_name IN ('进入综合搜索', '群聊', '查找用户、群聊等', '联系人', '聊天记录', '搜索')
                OR group_name LIKE '[qq-uia-adapter]%')",
        [],
    )
    .map_err(map_sqlite_err)?;

    conn.execute(
        "DELETE FROM friends
         WHERE real_user_id IS NULL
           AND (nickname IN ('进入综合搜索', '群聊', '查找用户、群聊等', '联系人', '聊天记录', '搜索')
                OR nickname LIKE '[qq-uia-adapter]%'
                OR instr(nickname, char(10)) > 0
                OR (length(nickname) > 36
                    AND NOT EXISTS (
                        SELECT 1 FROM messages m
                        WHERE m.chat_type = 'private'
                          AND (m.group_name = friends.nickname OR m.group_name = friends.friend_key)
                    )))",
        [],
    )
    .map_err(map_sqlite_err)?;

    // 幂等修复被同名群聊逻辑误写非零 group_id 的私聊历史消息
    conn.execute(
        "UPDATE messages SET group_id = 0 WHERE chat_type = 'private' AND group_id != 0",
        [],
    )
    .map_err(map_sqlite_err)?;

    let dup_pairs: Vec<(i64, i64, String)> = {
        let mut stmt = conn
            .prepare(
                "SELECT m1.onebot_message_id, m2.onebot_message_id, m2.timestamp_hint
                 FROM messages m1
                 JOIN messages m2
                   ON m1.group_name = m2.group_name
                  AND m1.chat_type = m2.chat_type
                  AND m1.sender_name = m2.sender_name
                  AND m1.raw_message = m2.raw_message
                  AND m1.onebot_message_id < m2.onebot_message_id
                 WHERE (m1.timestamp_hint IS NULL OR trim(m1.timestamp_hint) = '')
                   AND m2.timestamp_hint IS NOT NULL
                   AND trim(m2.timestamp_hint) != ''
                   AND ABS(m2.captured_at_ms - m1.captured_at_ms) < 3600000
                   AND NOT EXISTS (
                       SELECT 1 FROM messages m3
                       WHERE m3.group_name = m1.group_name
                         AND m3.chat_type = m1.chat_type
                         AND m3.sender_name = m1.sender_name
                         AND m3.raw_message = m1.raw_message
                         AND m3.onebot_message_id != m1.onebot_message_id
                         AND m3.onebot_message_id != m2.onebot_message_id
                         AND ABS(m3.captured_at_ms - m1.captured_at_ms) < 3600000
                   )",
            )
            .map_err(map_sqlite_err)?;
        let rows = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .map_err(map_sqlite_err)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(map_sqlite_err)?
    };

    for (keep_id, dup_id, ts_hint) in dup_pairs {
        conn.execute(
            "UPDATE messages SET timestamp_hint = ?2 WHERE onebot_message_id = ?1",
            params![keep_id, ts_hint],
        )
        .map_err(map_sqlite_err)?;
        conn.execute(
            "DELETE FROM messages WHERE onebot_message_id = ?1",
            params![dup_id],
        )
        .map_err(map_sqlite_err)?;
    }

    Ok(())
}

/// 为文件库与内存库配置 WAL、同步级别、外键约束及内存页面缓存参数
pub fn apply_pragmas(conn: &Connection, is_memory: bool) -> Result<()> {
    let pragma_sql = if is_memory {
        format!(
            "PRAGMA foreign_keys = ON;
             PRAGMA temp_store = MEMORY;
             PRAGMA cache_size = {SQLITE_CACHE_SIZE_KIB};"
        )
    } else {
        format!(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;
             PRAGMA busy_timeout = {SQLITE_BUSY_TIMEOUT_MS};
             PRAGMA temp_store = MEMORY;
             PRAGMA cache_size = {SQLITE_CACHE_SIZE_KIB};"
        )
    };

    conn.execute_batch(&pragma_sql).map_err(map_sqlite_err)
}

/// 幂等执行全部业务表与索引建表脚本，并在消息表为空时初始化自增起点
pub fn initialize_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS groups (
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
        CREATE INDEX IF NOT EXISTS idx_groups_onebot_id ON groups (onebot_group_id);
        CREATE INDEX IF NOT EXISTS idx_groups_real_id ON groups (real_group_id);

        CREATE TABLE IF NOT EXISTS group_members (
            group_name TEXT NOT NULL,
            nickname TEXT NOT NULL,
            real_user_id INTEGER NULL,
            onebot_user_id INTEGER NOT NULL,
            card TEXT NOT NULL DEFAULT '',
            role TEXT NOT NULL DEFAULT 'member',
            updated_at_ms INTEGER NOT NULL,
            PRIMARY KEY (group_name, nickname)
        );
        CREATE INDEX IF NOT EXISTS idx_group_members_group_uid
            ON group_members (group_name, onebot_user_id);

        CREATE TABLE IF NOT EXISTS friends (
            friend_key TEXT PRIMARY KEY,
            nickname TEXT NOT NULL,
            remark TEXT NOT NULL DEFAULT '',
            real_user_id INTEGER NULL,
            synthetic_user_id INTEGER NOT NULL,
            last_preview TEXT NULL,
            updated_at_ms INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_friends_real_uid ON friends (real_user_id);
        CREATE INDEX IF NOT EXISTS idx_friends_syn_uid ON friends (synthetic_user_id);

        CREATE TABLE IF NOT EXISTS messages (
            onebot_message_id INTEGER PRIMARY KEY AUTOINCREMENT,
            message_uid TEXT NOT NULL UNIQUE,
            group_name TEXT NOT NULL,
            group_id INTEGER NOT NULL,
            user_id INTEGER NOT NULL,
            sender_name TEXT NOT NULL,
            raw_message TEXT NOT NULL,
            is_self INTEGER NOT NULL,
            timestamp_hint TEXT NULL,
            captured_at_ms INTEGER NOT NULL,
            chat_type TEXT NOT NULL DEFAULT 'group',
            segments_json TEXT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_messages_group_recent
            ON messages (group_name, captured_at_ms DESC, onebot_message_id DESC);

        CREATE TABLE IF NOT EXISTS kv_meta (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL,
            updated_at_ms INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS audit_events (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            event_kind TEXT NOT NULL,
            detail_json TEXT NOT NULL,
            created_at_ms INTEGER NOT NULL
        );",
    )
    .map_err(map_sqlite_err)?;

    // 针对 V2 遗留文件库执行幂等加列迁移，补齐私聊标识与结构化消息段列
    ensure_column_exists(
        conn,
        TABLE_MESSAGES,
        "chat_type",
        "ALTER TABLE messages ADD COLUMN chat_type TEXT NOT NULL DEFAULT 'group'",
    )?;
    // 为按会话名与会话类型隔离的最近消息及引用/撤回检索建立联合索引
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_messages_group_type_recent
         ON messages (group_name, chat_type, captured_at_ms DESC, onebot_message_id DESC)",
        [],
    )
    .map_err(map_sqlite_err)?;
    ensure_column_exists(
        conn,
        TABLE_MESSAGES,
        "segments_json",
        "ALTER TABLE messages ADD COLUMN segments_json TEXT NULL",
    )?;

    // 库启动时顺带修复早前以合成群号入库的历史消息，确保历史消息可按真实群号检索
    conn.execute(SQL_HEAL_HISTORICAL_MESSAGE_GROUP_IDS, [])
        .map_err(map_sqlite_err)?;

    cleanup_historical_pollution(conn)?;

    let message_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
        .map_err(map_sqlite_err)?;

    if message_count == 0 {
        let has_seq_row: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_sequence WHERE name = ?1)",
                params![MESSAGES_TABLE_NAME],
                |row| row.get(0),
            )
            .map_err(map_sqlite_err)?;

        if has_seq_row {
            conn.execute(
                "UPDATE sqlite_sequence SET seq = ?2 WHERE name = ?1 AND seq < ?2",
                params![MESSAGES_TABLE_NAME, INITIAL_MESSAGE_SEQUENCE],
            )
            .map_err(map_sqlite_err)?;
        } else {
            conn.execute(
                "INSERT INTO sqlite_sequence (name, seq) VALUES (?1, ?2)",
                params![MESSAGES_TABLE_NAME, INITIAL_MESSAGE_SEQUENCE],
            )
            .map_err(map_sqlite_err)?;
        }
    }

    Ok(())
}
