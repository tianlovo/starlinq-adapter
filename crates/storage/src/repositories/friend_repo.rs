//! 好友联系人仓储：负责好友会话事务批量写入与按 ID/名称多维检索

use common_types::constants::DEFAULT_ONEBOT_ID_BOUND;
use common_types::{stable_onebot_id, AdapterError, FriendInfo, Result};
use rusqlite::{params, Connection, OptionalExtension, Row};

use super::{i64_from_u64_saturating, map_sqlite_err, meta_audit_repo, now_ms, u64_from_sql};
use crate::constants::{SQL_DELETE_FRIEND_BY_NAME, SQL_UPSERT_FRIEND};
use crate::models::{StoredFriendRecord, UpsertFriendInput};

fn derive_friend_key(input: &UpsertFriendInput) -> String {
    let nick = input.nickname.trim();
    if !nick.is_empty() {
        return nick.to_string();
    }
    input.remark.trim().to_string()
}

fn execute_upsert_friend_in_tx(
    conn: &Connection,
    input: &UpsertFriendInput,
    updated_at_ms: i64,
) -> Result<()> {
    let friend_key = derive_friend_key(input);
    if friend_key.is_empty() {
        return Ok(());
    }
    let synthetic_uid = stable_onebot_id(&format!("friend:{friend_key}"), DEFAULT_ONEBOT_ID_BOUND);
    let mut stmt = conn
        .prepare_cached(SQL_UPSERT_FRIEND)
        .map_err(map_sqlite_err)?;
    stmt.execute(params![
        friend_key,
        input.nickname.trim(),
        input.remark.trim(),
        input.real_user_id,
        synthetic_uid,
        input.last_preview.as_deref(),
        updated_at_ms,
    ])
    .map_err(map_sqlite_err)?;
    Ok(())
}

/// 在单事务内写入或合并单个好友记录
pub(crate) fn upsert_friend(
    conn: &mut Connection,
    input: &UpsertFriendInput,
) -> Result<StoredFriendRecord> {
    let friend_key = derive_friend_key(input);
    let updated_at_ms = i64_from_u64_saturating(now_ms());
    let tx = conn.transaction().map_err(map_sqlite_err)?;
    execute_upsert_friend_in_tx(&tx, input, updated_at_ms)?;
    let record = get_friend_by_name(&tx, &friend_key)?.ok_or_else(|| {
        AdapterError::Io(std::io::Error::other(format!(
            "friend row missing right after upsert: {friend_key}"
        )))
    })?;
    tx.commit().map_err(map_sqlite_err)?;
    Ok(record)
}

/// 在单事务内批量更新多条好友会话记录
pub(crate) fn upsert_friends_batch(
    conn: &mut Connection,
    inputs: &[UpsertFriendInput],
) -> Result<()> {
    if inputs.is_empty() {
        return Ok(());
    }
    let updated_at_ms = i64_from_u64_saturating(now_ms());
    let tx = conn.transaction().map_err(map_sqlite_err)?;
    for input in inputs {
        execute_upsert_friend_in_tx(&tx, input, updated_at_ms)?;
    }
    tx.commit().map_err(map_sqlite_err)?;
    Ok(())
}

/// 按最近更新时间倒序列出全部已缓存好友信息
pub(crate) fn list_friends(conn: &Connection) -> Result<Vec<FriendInfo>> {
    let mut stmt = conn
        .prepare_cached(
            "SELECT nickname, remark, real_user_id, synthetic_user_id
             FROM friends
             ORDER BY updated_at_ms DESC, friend_key ASC",
        )
        .map_err(map_sqlite_err)?;
    let rows = stmt
        .query_map([], |row| {
            let nickname: String = row.get(0)?;
            let remark: String = row.get(1)?;
            let real_user_id: Option<i64> = row.get(2)?;
            let synthetic_user_id: i64 = row.get(3)?;
            Ok(FriendInfo {
                user_id: real_user_id.unwrap_or(synthetic_user_id),
                real_user_id,
                nickname,
                remark,
            })
        })
        .map_err(map_sqlite_err)?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(map_sqlite_err)
}

/// 按真实 QQ 号或合成 OneBot 用户编号检索单个好友记录
pub(crate) fn get_friend_by_id(
    conn: &Connection,
    user_id: i64,
) -> Result<Option<StoredFriendRecord>> {
    let mut stmt = conn
        .prepare_cached(
            "SELECT friend_key, nickname, remark, real_user_id, synthetic_user_id,
                    last_preview, updated_at_ms
             FROM friends
             WHERE real_user_id = ?1 OR synthetic_user_id = ?1
             ORDER BY CASE WHEN real_user_id = ?1 THEN 0 ELSE 1 END, updated_at_ms DESC
             LIMIT 1",
        )
        .map_err(map_sqlite_err)?;
    stmt.query_row(params![user_id], row_to_friend_record)
        .optional()
        .map_err(map_sqlite_err)
}

/// 按主键、昵称或备注精确查询好友记录
pub(crate) fn get_friend_by_name(
    conn: &Connection,
    name: &str,
) -> Result<Option<StoredFriendRecord>> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let mut stmt = conn
        .prepare_cached(
            "SELECT friend_key, nickname, remark, real_user_id, synthetic_user_id,
                    last_preview, updated_at_ms
             FROM friends
             WHERE friend_key = ?1 OR nickname = ?1 OR remark = ?1
             LIMIT 1",
        )
        .map_err(map_sqlite_err)?;
    stmt.query_row(params![trimmed], row_to_friend_record)
        .optional()
        .map_err(map_sqlite_err)
}

/// 按主键、昵称或备注删除 `friends` 表中的匹配记录，实际删除了行时返回 `true`
pub(crate) fn delete_friend_by_name(conn: &Connection, name: &str) -> Result<bool> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Ok(false);
    }
    let mut stmt = conn
        .prepare_cached(SQL_DELETE_FRIEND_BY_NAME)
        .map_err(map_sqlite_err)?;
    let affected = stmt.execute(params![trimmed]).map_err(map_sqlite_err)?;
    Ok(affected > 0)
}

/// 清理无真实 QQ 号、未通过标题合法性校验且无任何私聊消息引用的好友污染行，并返回被清理名称列表
pub(crate) fn purge_invalid_friends<F>(
    conn: &mut Connection,
    is_valid_title: F,
) -> Result<Vec<String>>
where
    F: Fn(&str) -> bool,
{
    let transaction = conn.transaction().map_err(map_sqlite_err)?;
    let candidates: Vec<(String, String)> = {
        let mut stmt = transaction
            .prepare_cached(
                "SELECT friend_key, nickname
                 FROM friends
                 WHERE real_user_id IS NULL
                 ORDER BY friend_key ASC",
            )
            .map_err(map_sqlite_err)?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(map_sqlite_err)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(map_sqlite_err)?
    };

    let mut deleted = Vec::new();
    {
        let mut msg_check_stmt = transaction
            .prepare_cached(
                "SELECT EXISTS(
                    SELECT 1 FROM messages
                    WHERE chat_type = 'private'
                      AND (group_name = ?1 OR sender_name = ?1 OR group_name = ?2 OR sender_name = ?2)
                )",
            )
            .map_err(map_sqlite_err)?;
        let mut del_stmt = transaction
            .prepare_cached("DELETE FROM friends WHERE friend_key = ?1")
            .map_err(map_sqlite_err)?;

        for (friend_key, nickname) in candidates {
            let display_name = if nickname.trim().is_empty() {
                friend_key.as_str()
            } else {
                nickname.as_str()
            };
            if is_valid_title(display_name) {
                continue;
            }
            let has_messages: bool = msg_check_stmt
                .query_row(params![friend_key, display_name], |row| row.get(0))
                .map_err(map_sqlite_err)?;
            if has_messages {
                continue;
            }
            if del_stmt
                .execute(params![friend_key])
                .map_err(map_sqlite_err)?
                > 0
            {
                deleted.push(display_name.to_string());
            }
        }
    }

    if !deleted.is_empty() {
        meta_audit_repo::append_audit_json(
            &transaction,
            "session_pollution_purged",
            &serde_json::json!({
                "table": "friends",
                "deleted": &deleted,
            }),
        )?;
    }

    transaction.commit().map_err(map_sqlite_err)?;
    Ok(deleted)
}

fn row_to_friend_record(row: &Row<'_>) -> rusqlite::Result<StoredFriendRecord> {
    Ok(StoredFriendRecord {
        friend_key: row.get(0)?,
        nickname: row.get(1)?,
        remark: row.get(2)?,
        real_user_id: row.get(3)?,
        synthetic_user_id: row.get(4)?,
        last_preview: row.get(5)?,
        updated_at_ms: u64_from_sql(row.get::<_, i64>(6)?),
    })
}
