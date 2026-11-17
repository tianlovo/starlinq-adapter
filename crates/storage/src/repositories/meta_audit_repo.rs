//! 元数据键值对、审计日志及数据库统计仓储

use common_types::{AdapterError, AuditEventRecord, Result, StorageStats};
use rusqlite::{params, Connection, OptionalExtension, Row, Transaction, TransactionBehavior};
use serde::Serialize;

use super::{i64_from_u64_saturating, map_sqlite_err, now_ms, u64_from_sql};
use crate::constants::{
    FORWARD_META_KEY_PREFIX, MAX_AUDIT_EVENTS, MAX_STORED_FORWARD_BUNDLES, TABLE_AUDIT_EVENTS,
    TABLE_FRIENDS, TABLE_GROUPS, TABLE_GROUP_MEMBERS, TABLE_MESSAGES,
};

const FORWARD_SEQ_META_KEY: &str = "forward_seq";
const FORWARD_UID_META_KEY_PREFIX: &str = "forward_uid:";
const INITIAL_FORWARD_SEQ: i64 = 10_000;

/// 在已开启的 `Immediate` 事务内原子分配下一枚 `fwd-*` 编号并写入有界合并转发节点表
fn allocate_forward_nodes_in_tx(tx: &Transaction<'_>, nodes_json: &str) -> Result<(String, i64)> {
    let stored_seq: i64 = tx
        .query_row(
            "SELECT value FROM kv_meta WHERE key = ?1",
            params![FORWARD_SEQ_META_KEY],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(map_sqlite_err)?
        .and_then(|raw| raw.trim().parse::<i64>().ok())
        .unwrap_or(INITIAL_FORWARD_SEQ);

    let max_existing_suffix = {
        let mut stmt = tx
            .prepare_cached("SELECT key FROM kv_meta WHERE key LIKE 'forward:fwd-%'")
            .map_err(map_sqlite_err)?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(map_sqlite_err)?;
        let mut highest = INITIAL_FORWARD_SEQ;
        for row in rows {
            let key = row.map_err(map_sqlite_err)?;
            if let Some(num) = key
                .strip_prefix("forward:fwd-")
                .and_then(|s| s.trim().parse::<i64>().ok())
            {
                highest = highest.max(num);
            }
        }
        highest
    };

    let mut next_seq = stored_seq.max(max_existing_suffix).saturating_add(1);
    loop {
        let candidate_key = format!("{FORWARD_META_KEY_PREFIX}fwd-{next_seq}");
        let exists: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM kv_meta WHERE key = ?1)",
                params![candidate_key],
                |row| row.get(0),
            )
            .map_err(map_sqlite_err)?;
        if !exists {
            break;
        }
        next_seq = next_seq.saturating_add(1);
    }

    let forward_id = format!("fwd-{next_seq}");
    let storage_key = format!("{FORWARD_META_KEY_PREFIX}{forward_id}");
    let updated_at_ms = i64_from_u64_saturating(now_ms());

    tx.execute(
        "INSERT INTO kv_meta (key, value, updated_at_ms) VALUES (?1, ?2, ?3)",
        params![storage_key, nodes_json, updated_at_ms],
    )
    .map_err(map_sqlite_err)?;
    tx.execute(
        "INSERT INTO kv_meta (key, value, updated_at_ms)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(key) DO UPDATE SET
            value = excluded.value,
            updated_at_ms = excluded.updated_at_ms",
        params![FORWARD_SEQ_META_KEY, next_seq.to_string(), updated_at_ms],
    )
    .map_err(map_sqlite_err)?;
    tx.execute(
        "DELETE FROM kv_meta
         WHERE key LIKE 'forward:%'
           AND key NOT IN (
               SELECT key FROM kv_meta
               WHERE key LIKE 'forward:%'
               ORDER BY updated_at_ms DESC, rowid DESC
               LIMIT ?1
           )",
        params![MAX_STORED_FORWARD_BUNDLES],
    )
    .map_err(map_sqlite_err)?;
    Ok((forward_id, updated_at_ms))
}

/// 原子递增分配全局唯一且永不复用覆盖的历史合并转发节点键（如 `fwd-10001`）并写入数据库
pub(crate) fn allocate_forward_nodes(conn: &mut Connection, nodes_json: &str) -> Result<String> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_sqlite_err)?;
    let (forward_id, _) = allocate_forward_nodes_in_tx(&tx, nodes_json)?;
    tx.commit().map_err(map_sqlite_err)?;
    Ok(forward_id)
}

/// 按入站消息 UID 幂等复用既有 `fwd-*` 编号（仅当 `forward:{fwd_id}` 仍在库中），否则新分配并同步裁剪 `forward:` 与 `forward_uid:`
pub(crate) fn get_or_allocate_forward_for_uid(
    conn: &mut Connection,
    message_uid: &str,
    nodes_json: &str,
) -> Result<String> {
    let trimmed_uid = message_uid.trim();
    if trimmed_uid.is_empty() {
        return allocate_forward_nodes(conn, nodes_json);
    }

    let uid_key = format!("{FORWARD_UID_META_KEY_PREFIX}{trimmed_uid}");
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_sqlite_err)?;

    let existing_fwd_id: Option<String> = tx
        .query_row(
            "SELECT value FROM kv_meta WHERE key = ?1",
            params![uid_key],
            |row| row.get(0),
        )
        .optional()
        .map_err(map_sqlite_err)?;

    if let Some(fwd_id) = existing_fwd_id {
        let forward_key = format!("{FORWARD_META_KEY_PREFIX}{fwd_id}");
        let bundle_exists: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM kv_meta WHERE key = ?1)",
                params![forward_key],
                |row| row.get(0),
            )
            .map_err(map_sqlite_err)?;
        if bundle_exists {
            return Ok(fwd_id);
        }
    }

    let (forward_id, updated_at_ms) = allocate_forward_nodes_in_tx(&tx, nodes_json)?;
    // 采用 INSERT OR REPLACE 刷新 rowid，确保同毫秒高频写入时新映射不被旧 rowid 误淘汰
    tx.execute(
        "INSERT OR REPLACE INTO kv_meta (key, value, updated_at_ms) VALUES (?1, ?2, ?3)",
        params![uid_key, forward_id, updated_at_ms],
    )
    .map_err(map_sqlite_err)?;
    tx.execute(
        "DELETE FROM kv_meta
         WHERE key LIKE 'forward\\_uid:%' ESCAPE '\\'
           AND key NOT IN (
               SELECT key FROM kv_meta
               WHERE key LIKE 'forward\\_uid:%' ESCAPE '\\'
               ORDER BY updated_at_ms DESC, rowid DESC
               LIMIT ?1
           )",
        params![MAX_STORED_FORWARD_BUNDLES],
    )
    .map_err(map_sqlite_err)?;
    tx.commit().map_err(map_sqlite_err)?;
    Ok(forward_id)
}

/// 写入或覆盖 `kv_meta` 表中的配置与运行时状态条目，并对合并转发节点包执行有界淘汰
pub(crate) fn set_meta(conn: &Connection, key: &str, value: &str) -> Result<()> {
    let updated_at_ms = i64_from_u64_saturating(now_ms());
    let mut stmt = conn
        .prepare_cached(
            "INSERT INTO kv_meta (key, value, updated_at_ms)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(key) DO UPDATE SET
                value = excluded.value,
                updated_at_ms = excluded.updated_at_ms",
        )
        .map_err(map_sqlite_err)?;
    stmt.execute(params![key, value, updated_at_ms])
        .map_err(map_sqlite_err)?;

    if key.starts_with(FORWARD_META_KEY_PREFIX) {
        let mut prune_stmt = conn
            .prepare_cached(
                "DELETE FROM kv_meta
                 WHERE key LIKE 'forward:%'
                   AND key NOT IN (
                       SELECT key FROM kv_meta
                       WHERE key LIKE 'forward:%'
                       ORDER BY updated_at_ms DESC, rowid DESC
                       LIMIT ?1
                   )",
            )
            .map_err(map_sqlite_err)?;
        prune_stmt
            .execute(params![MAX_STORED_FORWARD_BUNDLES])
            .map_err(map_sqlite_err)?;
    }
    Ok(())
}

/// 按键名从 `kv_meta` 表读取元数据值
pub(crate) fn get_meta(conn: &Connection, key: &str) -> Result<Option<String>> {
    let mut stmt = conn
        .prepare_cached("SELECT value FROM kv_meta WHERE key = ?1")
        .map_err(map_sqlite_err)?;
    stmt.query_row(params![key], |row| row.get::<_, String>(0))
        .optional()
        .map_err(map_sqlite_err)
}

/// 追加一条原始 JSON 字符串负载的审计事件，按主键窗口有界保留最近记录
pub(crate) fn append_audit(conn: &Connection, event_kind: &str, detail_json: &str) -> Result<()> {
    let created_at_ms = i64_from_u64_saturating(now_ms());
    let mut stmt = conn
        .prepare_cached(
            "INSERT INTO audit_events (event_kind, detail_json, created_at_ms)
             VALUES (?1, ?2, ?3)",
        )
        .map_err(map_sqlite_err)?;
    stmt.execute(params![event_kind, detail_json, created_at_ms])
        .map_err(map_sqlite_err)?;

    let inserted_id = conn.last_insert_rowid();
    if inserted_id > MAX_AUDIT_EVENTS {
        let cutoff = inserted_id - MAX_AUDIT_EVENTS;
        let mut prune_stmt = conn
            .prepare_cached("DELETE FROM audit_events WHERE id <= ?1")
            .map_err(map_sqlite_err)?;
        prune_stmt
            .execute(params![cutoff])
            .map_err(map_sqlite_err)?;
    }
    Ok(())
}

/// 序列化任意可序列化载荷并写入审计日志表
pub(crate) fn append_audit_json<T: Serialize>(
    conn: &Connection,
    event_kind: &str,
    detail: &T,
) -> Result<()> {
    let detail_json = serde_json::to_string(detail)
        .map_err(|err| AdapterError::Io(std::io::Error::other(format!("json error: {err}"))))?;
    append_audit(conn, event_kind, &detail_json)
}

fn row_to_audit_event(row: &Row<'_>) -> rusqlite::Result<AuditEventRecord> {
    Ok(AuditEventRecord {
        id: row.get(0)?,
        event_kind: row.get(1)?,
        detail_json: row.get(2)?,
        created_at_ms: u64_from_sql(row.get::<_, i64>(3)?),
    })
}

/// 按可选事件类型过滤并按主键倒序分页查询持久化的审计流水记录（上限 `[1, 200]`）
pub(crate) fn list_audit_events(
    conn: &Connection,
    event_kind: Option<&str>,
    limit: usize,
) -> Result<Vec<AuditEventRecord>> {
    let clamped_limit = i64::try_from(limit.clamp(1, 200)).unwrap_or(200);
    let clean_kind = event_kind.map(str::trim).filter(|val| !val.is_empty());

    if let Some(kind) = clean_kind {
        let mut stmt = conn
            .prepare_cached(
                "SELECT id, event_kind, detail_json, created_at_ms
                 FROM audit_events
                 WHERE event_kind = ?1
                 ORDER BY id DESC
                 LIMIT ?2",
            )
            .map_err(map_sqlite_err)?;
        let rows = stmt
            .query_map(params![kind, clamped_limit], row_to_audit_event)
            .map_err(map_sqlite_err)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(map_sqlite_err)
    } else {
        let mut stmt = conn
            .prepare_cached(
                "SELECT id, event_kind, detail_json, created_at_ms
                 FROM audit_events
                 ORDER BY id DESC
                 LIMIT ?1",
            )
            .map_err(map_sqlite_err)?;
        let rows = stmt
            .query_map(params![clamped_limit], row_to_audit_event)
            .map_err(map_sqlite_err)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(map_sqlite_err)
    }
}

/// 统计各核心持久化表当前行数
pub(crate) fn stats(conn: &Connection, db_path: &str) -> Result<StorageStats> {
    let group_count = query_table_count(conn, TABLE_GROUPS)?;
    let member_count = query_table_count(conn, TABLE_GROUP_MEMBERS)?;
    let message_count = query_table_count(conn, TABLE_MESSAGES)?;
    let audit_count = query_table_count(conn, TABLE_AUDIT_EVENTS)?;
    let friend_count = query_table_count(conn, TABLE_FRIENDS)?;

    Ok(StorageStats {
        db_path: db_path.to_string(),
        group_count,
        member_count,
        message_count,
        audit_count,
        friend_count,
    })
}

fn query_table_count(conn: &Connection, table_name: &str) -> Result<u64> {
    let sql = format!("SELECT COUNT(*) FROM {table_name}");
    let count: i64 = conn
        .query_row(&sql, [], |row| row.get(0))
        .map_err(map_sqlite_err)?;
    Ok(u64_from_sql(count))
}
