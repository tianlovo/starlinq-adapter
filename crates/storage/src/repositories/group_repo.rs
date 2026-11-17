//! 群聊元数据仓储：负责群基本信息事务写入、历史消息群号同步回填、抽屉巡检标记与多维度查询

use common_types::constants::DEFAULT_ONEBOT_ID_BOUND;
use common_types::{
    infer_qq_max_member_count, stable_onebot_id, AdapterError, GroupInfoSummary, Result,
};
use rusqlite::{params, Connection, OptionalExtension, Row};

use super::{
    i64_from_u64_saturating, map_sqlite_err, meta_audit_repo, now_ms, u32_from_sql, u64_from_sql,
};
use crate::constants::{SQL_MARK_DRAWER_CHECKED, SQL_SYNC_MESSAGES_GROUP_ID, SQL_UPSERT_GROUP};
use crate::models::{StoredGroupRecord, UpsertGroupInput};

fn execute_upsert_group_in_tx(
    conn: &Connection,
    input: &UpsertGroupInput,
    updated_at_ms: i64,
) -> Result<()> {
    let onebot_group_id = stable_onebot_id(&input.group_name, DEFAULT_ONEBOT_ID_BOUND);
    let mut upsert_stmt = conn
        .prepare_cached(SQL_UPSERT_GROUP)
        .map_err(map_sqlite_err)?;
    upsert_stmt
        .execute(params![
            input.group_name,
            input.real_group_id,
            onebot_group_id,
            input.member_count,
            input.max_member_count,
            input.last_preview.as_deref(),
            i64::from(input.unread_count),
            updated_at_ms,
        ])
        .map_err(map_sqlite_err)?;

    // 若本次显式带入真实群号，在同事务内同步修正该群既往以合成群号入库的消息记录
    if let Some(real_id) = input.real_group_id {
        let mut sync_stmt = conn
            .prepare_cached(SQL_SYNC_MESSAGES_GROUP_ID)
            .map_err(map_sqlite_err)?;
        sync_stmt
            .execute(params![real_id, input.group_name])
            .map_err(map_sqlite_err)?;
    }

    Ok(())
}

/// 在单事务内插入或合并更新群聊元数据，并同步将该群历史消息回填为最新真实群号
pub(crate) fn upsert_group(
    conn: &mut Connection,
    input: &UpsertGroupInput,
) -> Result<StoredGroupRecord> {
    if input.group_name.trim().is_empty() {
        return Err(AdapterError::InputRejected(
            "group_name must not be empty".to_string(),
        ));
    }
    let updated_at_ms = i64_from_u64_saturating(now_ms());
    let transaction = conn.transaction().map_err(map_sqlite_err)?;

    execute_upsert_group_in_tx(&transaction, input, updated_at_ms)?;

    let record = get_group_by_name(&transaction, &input.group_name)?.ok_or_else(|| {
        AdapterError::Io(std::io::Error::other(format!(
            "group row missing right after upsert: {}",
            input.group_name
        )))
    })?;

    if input.real_group_id.is_none() {
        if let Some(existing_real_id) = record.real_group_id {
            let mut sync_stmt = transaction
                .prepare_cached(SQL_SYNC_MESSAGES_GROUP_ID)
                .map_err(map_sqlite_err)?;
            sync_stmt
                .execute(params![existing_real_id, record.group_name])
                .map_err(map_sqlite_err)?;
        }
    }

    transaction.commit().map_err(map_sqlite_err)?;
    Ok(record)
}

/// 在单个数据库事务内批量更新多条群聊摘要，降低高频会话刷新时的磁盘事务提交开销
pub(crate) fn upsert_groups_batch(
    conn: &mut Connection,
    inputs: &[UpsertGroupInput],
) -> Result<()> {
    if inputs.is_empty() {
        return Ok(());
    }

    let updated_at_ms = i64_from_u64_saturating(now_ms());
    let transaction = conn.transaction().map_err(map_sqlite_err)?;
    for input in inputs {
        if input.group_name.trim().is_empty() {
            continue;
        }
        execute_upsert_group_in_tx(&transaction, input, updated_at_ms)?;
    }
    transaction.commit().map_err(map_sqlite_err)?;
    Ok(())
}

/// 更新或初始化指定群聊右侧抽屉巡检时间戳
pub(crate) fn mark_group_drawer_checked(
    conn: &Connection,
    group_name: &str,
    checked_at_ms: u64,
) -> Result<()> {
    if group_name.trim().is_empty() {
        return Err(AdapterError::InputRejected(
            "group_name must not be empty".to_string(),
        ));
    }
    let onebot_group_id = stable_onebot_id(group_name, DEFAULT_ONEBOT_ID_BOUND);
    let updated_at_ms = i64_from_u64_saturating(now_ms());

    let mut stmt = conn
        .prepare_cached(SQL_MARK_DRAWER_CHECKED)
        .map_err(map_sqlite_err)?;
    stmt.execute(params![
        group_name,
        onebot_group_id,
        i64_from_u64_saturating(checked_at_ms),
        updated_at_ms,
    ])
    .map_err(map_sqlite_err)?;

    Ok(())
}

/// 按群显示名称精确查询持久化记录
pub(crate) fn get_group_by_name(
    conn: &Connection,
    group_name: &str,
) -> Result<Option<StoredGroupRecord>> {
    let mut stmt = conn
        .prepare_cached(
            "SELECT group_name, real_group_id, onebot_group_id, member_count, max_member_count,
                    last_preview, unread_count, drawer_checked_at_ms, updated_at_ms
             FROM groups
             WHERE group_name = ?1",
        )
        .map_err(map_sqlite_err)?;
    stmt.query_row(params![group_name], row_to_group_record)
        .optional()
        .map_err(map_sqlite_err)
}

/// 按真实群号或合成 OneBot 群号解析对应群聊记录，真实群号匹配优先
pub(crate) fn get_group_by_id(
    conn: &Connection,
    group_id: i64,
) -> Result<Option<StoredGroupRecord>> {
    let mut stmt = conn
        .prepare_cached(
            "SELECT group_name, real_group_id, onebot_group_id, member_count, max_member_count,
                    last_preview, unread_count, drawer_checked_at_ms, updated_at_ms
             FROM groups
             WHERE real_group_id = ?1 OR onebot_group_id = ?1
             ORDER BY CASE WHEN real_group_id = ?1 THEN 0 ELSE 1 END, updated_at_ms DESC
             LIMIT 1",
        )
        .map_err(map_sqlite_err)?;
    stmt.query_row(params![group_id], row_to_group_record)
        .optional()
        .map_err(map_sqlite_err)
}

/// 删除无真实群号且尚未记录任何成员统计的临时或污染群元数据行
pub(crate) fn delete_transient_group_by_name(conn: &Connection, group_name: &str) -> Result<bool> {
    let deleted = conn
        .execute(
            "DELETE FROM groups
             WHERE group_name = ?1
               AND real_group_id IS NULL
               AND (member_count IS NULL OR member_count = 0)",
            params![group_name],
        )
        .map_err(map_sqlite_err)?;
    Ok(deleted > 0)
}

/// 清理无真实群号、未通过标题合法性校验且无消息或群成员引用的群聊污染行，并返回被清理群名列表
pub(crate) fn purge_invalid_groups<F>(
    conn: &mut Connection,
    is_valid_title: F,
) -> Result<Vec<String>>
where
    F: Fn(&str) -> bool,
{
    let transaction = conn.transaction().map_err(map_sqlite_err)?;
    let candidates: Vec<String> = {
        let mut stmt = transaction
            .prepare_cached(
                "SELECT group_name
                 FROM groups
                 WHERE real_group_id IS NULL
                 ORDER BY group_name ASC",
            )
            .map_err(map_sqlite_err)?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(map_sqlite_err)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(map_sqlite_err)?
    };

    let mut deleted = Vec::new();
    {
        let mut ref_check_stmt = transaction
            .prepare_cached(
                "SELECT EXISTS(SELECT 1 FROM messages WHERE group_name = ?1)
                     OR EXISTS(SELECT 1 FROM group_members WHERE group_name = ?1)",
            )
            .map_err(map_sqlite_err)?;
        let mut del_stmt = transaction
            .prepare_cached("DELETE FROM groups WHERE group_name = ?1")
            .map_err(map_sqlite_err)?;

        for group_name in candidates {
            if is_valid_title(&group_name) {
                continue;
            }
            let has_refs: bool = ref_check_stmt
                .query_row(params![group_name], |row| row.get(0))
                .map_err(map_sqlite_err)?;
            if has_refs {
                continue;
            }
            if del_stmt
                .execute(params![group_name])
                .map_err(map_sqlite_err)?
                > 0
            {
                deleted.push(group_name);
            }
        }
    }

    if !deleted.is_empty() {
        meta_audit_repo::append_audit_json(
            &transaction,
            "session_pollution_purged",
            &serde_json::json!({
                "table": "groups",
                "deleted": &deleted,
            }),
        )?;
    }

    transaction.commit().map_err(map_sqlite_err)?;
    Ok(deleted)
}

/// 按最近更新时间倒序列出全部群摘要信息
pub(crate) fn list_groups(conn: &Connection) -> Result<Vec<GroupInfoSummary>> {
    let mut statement = conn
        .prepare_cached(
            "SELECT group_name, real_group_id, onebot_group_id, member_count, max_member_count
             FROM groups
             ORDER BY updated_at_ms DESC, group_name ASC",
        )
        .map_err(map_sqlite_err)?;

    let rows = statement
        .query_map([], |row| {
            let group_name: String = row.get(0)?;
            let real_group_id: Option<i64> = row.get(1)?;
            let onebot_group_id: i64 = row.get(2)?;
            let member_count: Option<i32> = row.get(3)?;
            let raw_max_member_count: Option<i32> = row.get(4)?;
            Ok(GroupInfoSummary {
                group_id: real_group_id.unwrap_or(onebot_group_id),
                real_group_id,
                group_name,
                member_count: member_count.unwrap_or(0),
                max_member_count: infer_qq_max_member_count(member_count, raw_max_member_count),
            })
        })
        .map_err(map_sqlite_err)?;

    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(map_sqlite_err)
}

fn row_to_group_record(row: &Row<'_>) -> rusqlite::Result<StoredGroupRecord> {
    Ok(StoredGroupRecord {
        group_name: row.get(0)?,
        real_group_id: row.get(1)?,
        onebot_group_id: row.get(2)?,
        member_count: row.get(3)?,
        max_member_count: row.get(4)?,
        last_preview: row.get(5)?,
        unread_count: u32_from_sql(row.get::<_, i64>(6)?),
        drawer_checked_at_ms: row.get::<_, Option<i64>>(7)?.map(u64_from_sql),
        updated_at_ms: u64_from_sql(row.get::<_, i64>(8)?),
    })
}
