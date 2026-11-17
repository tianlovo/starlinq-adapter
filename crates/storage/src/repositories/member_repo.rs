//! 群成员仓储：负责事务性批量写入群成员与按用户标识检索成员资料

use common_types::constants::DEFAULT_ONEBOT_ID_BOUND;
use common_types::{stable_onebot_id, GroupMemberInfo, Result};
use rusqlite::{params, Connection, OptionalExtension, Row, Transaction};
use tracing::debug;

use super::{i64_from_u64_saturating, map_sqlite_err, meta_audit_repo, now_ms};
use crate::constants::SQL_UPSERT_MEMBER;
use crate::models::UpsertMemberInput;

fn load_self_meta(conn: &Connection) -> Result<(Option<String>, Option<i64>)> {
    let self_nickname = meta_audit_repo::get_meta(conn, "self_nickname")?
        .map(|val| val.trim().to_string())
        .filter(|val| !val.is_empty());
    let self_user_id = meta_audit_repo::get_meta(conn, "self_user_id")?
        .and_then(|val| val.trim().parse::<i64>().ok())
        .filter(|&id| id > 0);
    Ok((self_nickname, self_user_id))
}

fn query_unique_friend_id_by_sql(
    conn: &Connection,
    sql: &str,
    needle: &str,
) -> Result<Option<i64>> {
    let mut stmt = conn.prepare_cached(sql).map_err(map_sqlite_err)?;
    let rows = stmt
        .query_map(params![needle], |row| row.get::<_, i64>(0))
        .map_err(map_sqlite_err)?;
    let mut ids = rows
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(map_sqlite_err)?;
    ids.sort_unstable();
    ids.dedup();
    if ids.len() == 1 {
        Ok(Some(ids[0]))
    } else {
        Ok(None)
    }
}

fn lookup_unique_friend_user_id(conn: &Connection, nickname: &str) -> Result<Option<i64>> {
    let trimmed = nickname.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if let Some(uid) = query_unique_friend_id_by_sql(
        conn,
        "SELECT COALESCE(real_user_id, synthetic_user_id)
         FROM friends
         WHERE friend_key = ?1 OR nickname = ?1",
        trimmed,
    )? {
        return Ok(Some(uid));
    }
    query_unique_friend_id_by_sql(
        conn,
        "SELECT COALESCE(real_user_id, synthetic_user_id)
         FROM friends
         WHERE remark = ?1",
        trimmed,
    )
}

fn resolve_member_mapped_id(
    transaction: &Transaction<'_>,
    group_name: &str,
    trimmed_nickname: &str,
    self_nickname: Option<&str>,
    self_user_id: Option<i64>,
) -> Result<i64> {
    if self_nickname.is_some_and(|sn| sn == trimmed_nickname) {
        return Ok(self_user_id.unwrap_or_else(|| {
            stable_onebot_id(&format!("self:{trimmed_nickname}"), DEFAULT_ONEBOT_ID_BOUND)
        }));
    }
    if let Some(friend_uid) = lookup_unique_friend_user_id(transaction, trimmed_nickname)? {
        return Ok(friend_uid);
    }
    Ok(stable_onebot_id(
        &format!("member:{group_name}:{trimmed_nickname}"),
        DEFAULT_ONEBOT_ID_BOUND,
    ))
}

/// 在单个事务内批量插入或更新群成员列表，传入空白字段时保留库内已有资料
pub(crate) fn upsert_group_members(
    conn: &mut Connection,
    group_name: &str,
    group_id: i64,
    members: &[UpsertMemberInput],
) -> Result<()> {
    if members.is_empty() {
        return Ok(());
    }

    let updated_at_ms = i64_from_u64_saturating(now_ms());
    let transaction = conn.transaction().map_err(map_sqlite_err)?;
    let (self_nickname, self_user_id) = load_self_meta(&transaction)?;

    {
        let mut statement = transaction
            .prepare_cached(SQL_UPSERT_MEMBER)
            .map_err(map_sqlite_err)?;

        for member in members {
            let trimmed_nickname = member.nickname.trim();
            if trimmed_nickname.is_empty() {
                continue;
            }
            // 无真实 QQ 号时优先复用自账号或唯一好友命名空间 ID，避免跨命名空间分裂
            let onebot_user_id = if member.real_user_id.is_some() {
                stable_onebot_id(
                    &format!("member:{group_name}:{trimmed_nickname}"),
                    DEFAULT_ONEBOT_ID_BOUND,
                )
            } else {
                resolve_member_mapped_id(
                    &transaction,
                    group_name,
                    trimmed_nickname,
                    self_nickname.as_deref(),
                    self_user_id,
                )?
            };
            statement
                .execute(params![
                    group_name,
                    trimmed_nickname,
                    member.real_user_id,
                    onebot_user_id,
                    member.card.trim(),
                    member.role.trim(),
                    updated_at_ms,
                ])
                .map_err(map_sqlite_err)?;
        }
    }

    transaction.commit().map_err(map_sqlite_err)?;
    debug!(
        group_name,
        group_id,
        count = members.len(),
        "批量写入或合并群成员缓存完成"
    );
    Ok(())
}

/// 跨命名空间统一已缓存群成员与历史群消息的映射用户 ID（自账号与唯一匹配好友优先于群成员散列）
pub(crate) fn reconcile_member_mapped_ids(
    conn: &mut Connection,
    self_nickname: Option<&str>,
    self_user_id: Option<i64>,
) -> Result<usize> {
    let transaction = conn.transaction().map_err(map_sqlite_err)?;
    let (meta_nick, meta_uid) = load_self_meta(&transaction)?;
    let effective_self_nick = self_nickname
        .map(str::trim)
        .filter(|val| !val.is_empty())
        .map(str::to_string)
        .or(meta_nick);
    let effective_self_uid = self_user_id.filter(|&id| id > 0).or(meta_uid);

    let member_rows: Vec<(String, String, i64, String)> = {
        let mut stmt = transaction
            .prepare_cached(
                "SELECT group_name, nickname, onebot_user_id, card
                 FROM group_members
                 WHERE real_user_id IS NULL",
            )
            .map_err(map_sqlite_err)?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })
            .map_err(map_sqlite_err)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(map_sqlite_err)?
    };

    let mut updated_count = 0usize;
    {
        let mut update_member_stmt = transaction
            .prepare_cached(
                "UPDATE group_members
                 SET onebot_user_id = ?1
                 WHERE group_name = ?2 AND nickname = ?3 AND real_user_id IS NULL",
            )
            .map_err(map_sqlite_err)?;
        let mut update_member_msgs_stmt = transaction
            .prepare_cached(
                "UPDATE messages
                 SET user_id = ?1
                 WHERE chat_type = 'group'
                   AND group_name = ?2
                   AND (sender_name = ?3 OR (?4 != '' AND sender_name = ?4))
                   AND user_id != ?1",
            )
            .map_err(map_sqlite_err)?;

        for (group_name, nickname, old_uid, card) in member_rows {
            let target_uid = if effective_self_nick.as_deref() == Some(nickname.as_str()) {
                effective_self_uid
            } else {
                lookup_unique_friend_user_id(&transaction, &nickname)?
            };
            let Some(new_uid) = target_uid else {
                continue;
            };
            if old_uid != new_uid {
                updated_count += update_member_stmt
                    .execute(params![new_uid, group_name, nickname])
                    .map_err(map_sqlite_err)?;
            }
            updated_count += update_member_msgs_stmt
                .execute(params![new_uid, group_name, nickname, card])
                .map_err(map_sqlite_err)?;
        }
    }

    // 补充修复尚未进入群成员抽屉缓存但历史群消息中已出现的自账号及唯一好友发送者 ID
    let distinct_senders: Vec<String> = {
        let mut stmt = transaction
            .prepare_cached(
                "SELECT DISTINCT sender_name
                 FROM messages
                 WHERE chat_type = 'group' AND sender_name != ''",
            )
            .map_err(map_sqlite_err)?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(map_sqlite_err)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(map_sqlite_err)?
    };

    {
        let mut heal_msg_stmt = transaction
            .prepare_cached(
                "UPDATE messages
                 SET user_id = ?1
                 WHERE chat_type = 'group'
                   AND sender_name = ?2
                   AND user_id != ?1
                   AND NOT EXISTS (
                       SELECT 1 FROM group_members gm
                       WHERE gm.group_name = messages.group_name
                         AND gm.nickname = messages.sender_name
                         AND gm.real_user_id IS NOT NULL
                   )",
            )
            .map_err(map_sqlite_err)?;

        for sender_name in distinct_senders {
            let target_uid = if effective_self_nick.as_deref() == Some(sender_name.as_str()) {
                effective_self_uid
            } else {
                lookup_unique_friend_user_id(&transaction, &sender_name)?
            };
            if let Some(new_uid) = target_uid {
                updated_count += heal_msg_stmt
                    .execute(params![new_uid, sender_name])
                    .map_err(map_sqlite_err)?;
            }
        }
    }

    transaction.commit().map_err(map_sqlite_err)?;
    Ok(updated_count)
}

/// 列出指定群聊下的全部缓存成员记录
pub(crate) fn list_group_members(
    conn: &Connection,
    group_name: &str,
    effective_group_id: i64,
) -> Result<Vec<GroupMemberInfo>> {
    let mut statement = conn
        .prepare_cached(
            "SELECT nickname, real_user_id, onebot_user_id, card, role
             FROM group_members
             WHERE group_name = ?1
             ORDER BY updated_at_ms DESC, nickname ASC",
        )
        .map_err(map_sqlite_err)?;

    let rows = statement
        .query_map(params![group_name], |row| {
            row_to_group_member(row, effective_group_id)
        })
        .map_err(map_sqlite_err)?;

    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(map_sqlite_err)
}

/// 按真实 QQ 号或合成 OneBot 用户号在群成员缓存中查找单条成员资料
pub(crate) fn get_group_member_by_id(
    conn: &Connection,
    group_name: &str,
    user_id: i64,
    effective_group_id: i64,
) -> Result<Option<GroupMemberInfo>> {
    let mut stmt = conn
        .prepare_cached(
            "SELECT nickname, real_user_id, onebot_user_id, card, role
             FROM group_members
             WHERE group_name = ?1 AND (real_user_id = ?2 OR onebot_user_id = ?2)
             ORDER BY CASE WHEN real_user_id = ?2 THEN 0 ELSE 1 END, updated_at_ms DESC
             LIMIT 1",
        )
        .map_err(map_sqlite_err)?;
    stmt.query_row(params![group_name, user_id], |row| {
        row_to_group_member(row, effective_group_id)
    })
    .optional()
    .map_err(map_sqlite_err)
}

fn row_to_group_member(
    row: &Row<'_>,
    effective_group_id: i64,
) -> rusqlite::Result<GroupMemberInfo> {
    let nickname: String = row.get(0)?;
    let real_user_id: Option<i64> = row.get(1)?;
    let onebot_user_id: i64 = row.get(2)?;
    let card: String = row.get(3)?;
    let role: String = row.get(4)?;

    Ok(GroupMemberInfo {
        group_id: effective_group_id,
        user_id: real_user_id.unwrap_or(onebot_user_id),
        real_user_id,
        nickname,
        card,
        role,
    })
}
