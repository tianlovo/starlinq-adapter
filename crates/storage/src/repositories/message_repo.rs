//! 聊天消息仓储：负责幂等分配与批量持久化自增 OneBot 消息编号、结构化消息段及历史/引用检索

use std::collections::HashSet;

use common_types::{
    segments_to_plain_summary, ChatTargetKind, MessageSegment, Result, StoredMessageRecord,
};
use rusqlite::{params, Connection, OptionalExtension, Row, Transaction, TransactionBehavior};

use super::{i64_from_u64_saturating, map_sqlite_err, now_ms, u64_from_sql};
use crate::constants::{SQL_INSERT_MESSAGE, SQL_SELECT_MESSAGE_ID_BY_UID};
use crate::models::RecordMessageInput;

fn serialize_segments(segments: &[MessageSegment]) -> Option<String> {
    if segments.is_empty() {
        None
    } else {
        serde_json::to_string(segments).ok()
    }
}

fn has_structured_segments(segments: &[MessageSegment]) -> bool {
    segments
        .iter()
        .any(|seg| !matches!(seg, MessageSegment::Text { .. }))
}

fn has_structured_segments_json(segments_json: Option<&str>) -> bool {
    let Some(raw) = segments_json.map(str::trim).filter(|s| !s.is_empty()) else {
        return false;
    };
    if let Ok(parsed) = serde_json::from_str::<Vec<MessageSegment>>(raw) {
        return has_structured_segments(&parsed);
    }
    [
        "\"reply\"",
        "\"face\"",
        "\"at\"",
        "\"forward\"",
        "\"image\"",
        "\"file\"",
        "\"video\"",
    ]
    .iter()
    .any(|marker| raw.contains(marker))
}

fn deserialize_segments(segments_json: Option<String>, raw_message: &str) -> Vec<MessageSegment> {
    if let Some(json_str) = segments_json.as_deref() {
        if let Ok(parsed) = serde_json::from_str::<Vec<MessageSegment>>(json_str) {
            if !parsed.is_empty() {
                return parsed;
            }
        }
    }
    if raw_message.is_empty() {
        Vec::new()
    } else {
        vec![MessageSegment::Text {
            text: raw_message.to_string(),
        }]
    }
}

fn collect_batch_reserved_ids(
    transaction: &Transaction<'_>,
    inputs: &[RecordMessageInput],
) -> Result<HashSet<i64>> {
    let mut reserved = HashSet::with_capacity(inputs.len());
    let mut stmt = transaction
        .prepare_cached("SELECT onebot_message_id FROM messages WHERE message_uid = ?1")
        .map_err(map_sqlite_err)?;
    for input in inputs {
        if let Some(id) = stmt
            .query_row(params![input.message_uid], |row| row.get::<_, i64>(0))
            .optional()
            .map_err(map_sqlite_err)?
        {
            reserved.insert(id);
        }
    }
    Ok(reserved)
}

type TimestampSplitCandidate = (i64, String, i64, Option<String>, Option<String>);

fn find_timestamp_split_candidate_in_tx(
    transaction: &Transaction<'_>,
    input: &RecordMessageInput,
    reserved_ids: &HashSet<i64>,
    claimed_ids: &HashSet<i64>,
) -> Result<Option<TimestampSplitCandidate>> {
    let incoming_ts = input
        .timestamp_hint
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let cutoff_ms = i64_from_u64_saturating(input.captured_at_ms).saturating_sub(86_400_000);
    let mut stmt = transaction
        .prepare_cached(
            "SELECT onebot_message_id, message_uid, is_self, segments_json, timestamp_hint
             FROM messages
             WHERE group_name = ?1
               AND sender_name = ?2
               AND raw_message = ?3
               AND captured_at_ms >= ?4
               AND chat_type = ?5
             ORDER BY onebot_message_id ASC
             LIMIT 30",
        )
        .map_err(map_sqlite_err)?;
    let rows = stmt
        .query_map(
            params![
                input.group_name,
                input.sender_name,
                input.raw_message,
                cutoff_ms,
                input.chat_type.as_str()
            ],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            },
        )
        .map_err(map_sqlite_err)?;
    for row in rows {
        let (cand_id, cand_uid, cand_is_self, cand_segs_json, cand_ts) =
            row.map_err(map_sqlite_err)?;
        if reserved_ids.contains(&cand_id) || claimed_ids.contains(&cand_id) {
            continue;
        }
        let cand_has_ts = cand_ts
            .as_deref()
            .map(str::trim)
            .is_some_and(|s| !s.is_empty());
        // 仅允许原本无时间头的记录在补全 HH:MM 分隔时跨 UID 归并，严禁将无时间头的新气泡倒灌进已带时间戳的历史锚点
        if !cand_has_ts && incoming_ts.is_some() {
            return Ok(Some((
                cand_id,
                cand_uid,
                cand_is_self,
                cand_segs_json,
                cand_ts,
            )));
        }
    }
    Ok(None)
}

fn update_matched_message_in_tx(
    transaction: &Transaction<'_>,
    existing_id: i64,
    existing_is_self: i64,
    existing_segs_json: Option<&str>,
    existing_ts: Option<&str>,
    input: &RecordMessageInput,
) -> Result<()> {
    // 若已入库记录为本端发出且含非纯文本结构化段（reply/face/at/forward/image/file/video），严禁被回采纯文本覆盖
    let preserve_structured =
        existing_is_self != 0 && has_structured_segments_json(existing_segs_json);
    let incoming_ts = input
        .timestamp_hint
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let missing_existing_ts = existing_ts.map(str::trim).is_none_or(|s| s.is_empty());

    if preserve_structured {
        if missing_existing_ts {
            if let Some(ts) = incoming_ts {
                transaction
                    .execute(
                        "UPDATE messages SET timestamp_hint = ?2 WHERE onebot_message_id = ?1",
                        params![existing_id, ts],
                    )
                    .map_err(map_sqlite_err)?;
            }
        }
        return Ok(());
    }

    let upgrade_to_structured = !has_structured_segments_json(existing_segs_json)
        && has_structured_segments(&input.segments);
    if upgrade_to_structured {
        let new_segs_json = serialize_segments(&input.segments);
        transaction
            .execute(
                "UPDATE messages
                 SET raw_message = ?2,
                     segments_json = ?3,
                     timestamp_hint = COALESCE(NULLIF(timestamp_hint, ''), ?4)
                 WHERE onebot_message_id = ?1",
                params![
                    existing_id,
                    input.raw_message,
                    new_segs_json.as_deref(),
                    incoming_ts,
                ],
            )
            .map_err(map_sqlite_err)?;
    } else if missing_existing_ts {
        if let Some(ts) = incoming_ts {
            transaction
                .execute(
                    "UPDATE messages SET timestamp_hint = ?2 WHERE onebot_message_id = ?1",
                    params![existing_id, ts],
                )
                .map_err(map_sqlite_err)?;
        }
    }
    Ok(())
}

fn message_uid_exists_in_tx(transaction: &Transaction<'_>, message_uid: &str) -> Result<bool> {
    let mut stmt = transaction
        .prepare_cached("SELECT EXISTS(SELECT 1 FROM messages WHERE message_uid = ?1)")
        .map_err(map_sqlite_err)?;
    stmt.query_row(params![message_uid], |row| row.get(0))
        .map_err(map_sqlite_err)
}

fn insert_message_row_in_tx(
    transaction: &Transaction<'_>,
    message_uid: &str,
    input: &RecordMessageInput,
) -> Result<i64> {
    let segments_json = serialize_segments(&input.segments);
    let mut insert_stmt = transaction
        .prepare_cached(SQL_INSERT_MESSAGE)
        .map_err(map_sqlite_err)?;
    insert_stmt
        .execute(params![
            message_uid,
            input.group_name,
            input.group_id,
            input.user_id,
            input.sender_name,
            input.raw_message,
            if input.is_self { 1_i64 } else { 0_i64 },
            input.timestamp_hint.as_deref(),
            i64_from_u64_saturating(input.captured_at_ms),
            input.chat_type.as_str(),
            segments_json.as_deref(),
        ])
        .map_err(map_sqlite_err)?;
    let mut id_stmt = transaction
        .prepare_cached(SQL_SELECT_MESSAGE_ID_BY_UID)
        .map_err(map_sqlite_err)?;
    id_stmt
        .query_row(params![message_uid], |row| row.get::<_, i64>(0))
        .map_err(map_sqlite_err)
}

fn insert_or_reuse_message_in_tx(
    transaction: &Transaction<'_>,
    input: &RecordMessageInput,
    reserved_ids: &HashSet<i64>,
    claimed_ids: &mut HashSet<i64>,
) -> Result<i64> {
    let mut select_stmt = transaction
        .prepare_cached(
            "SELECT onebot_message_id, is_self, segments_json, timestamp_hint
             FROM messages
             WHERE message_uid = ?1",
        )
        .map_err(map_sqlite_err)?;
    if let Some((existing_id, existing_is_self, existing_segs_json, existing_ts)) = select_stmt
        .query_row(params![input.message_uid], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })
        .optional()
        .map_err(map_sqlite_err)?
    {
        claimed_ids.insert(existing_id);
        update_matched_message_in_tx(
            transaction,
            existing_id,
            existing_is_self,
            existing_segs_json.as_deref(),
            existing_ts.as_deref(),
            input,
        )?;
        return Ok(existing_id);
    }

    if let Some((cand_id, cand_uid, cand_is_self, cand_segs_json, cand_ts)) =
        find_timestamp_split_candidate_in_tx(transaction, input, reserved_ids, claimed_ids)?
    {
        claimed_ids.insert(cand_id);
        update_matched_message_in_tx(
            transaction,
            cand_id,
            cand_is_self,
            cand_segs_json.as_deref(),
            cand_ts.as_deref(),
            input,
        )?;
        // 当原本无时间标签的历史 UI 气泡首次补全时间戳时，将持久化 UID 同步演进为带时间新 UID 以便后续直击
        if cand_ts
            .as_deref()
            .map(str::trim)
            .is_none_or(|s| s.is_empty())
            && !cand_uid.starts_with("out-stub-")
        {
            let _ = transaction.execute(
                "UPDATE messages SET message_uid = ?2 WHERE onebot_message_id = ?1",
                params![cand_id, input.message_uid],
            );
        }
        return Ok(cand_id);
    }

    let inserted_id = insert_message_row_in_tx(transaction, &input.message_uid, input)?;
    claimed_ids.insert(inserted_id);
    Ok(inserted_id)
}

/// 将已发送出站存根行重新绑定为 UI 回采的 `message_uid`，并在存在较新重复回采行时清理合并以保证单气泡单行
pub(crate) fn reconcile_outbound_stub_uid(
    conn: &mut Connection,
    onebot_message_id: i64,
    ui_uid: &str,
    timestamp_hint: Option<&str>,
) -> Result<()> {
    let trimmed_uid = ui_uid.trim();
    if trimmed_uid.is_empty() {
        return Ok(());
    }
    let clean_ts = timestamp_hint.map(str::trim).filter(|val| !val.is_empty());

    let transaction = conn.transaction().map_err(map_sqlite_err)?;
    let stub_row: Option<(String, Option<String>)> = transaction
        .query_row(
            "SELECT message_uid, timestamp_hint FROM messages WHERE onebot_message_id = ?1",
            params![onebot_message_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(map_sqlite_err)?;

    let Some((stub_uid, _stub_ts)) = stub_row else {
        return Ok(());
    };

    if stub_uid == trimmed_uid {
        if let Some(ts) = clean_ts {
            transaction
                .execute(
                    "UPDATE messages SET timestamp_hint = ?2 WHERE onebot_message_id = ?1",
                    params![onebot_message_id, ts],
                )
                .map_err(map_sqlite_err)?;
        }
        transaction.commit().map_err(map_sqlite_err)?;
        return Ok(());
    }

    let dup_row: Option<(i64, Option<String>)> = transaction
        .query_row(
            "SELECT onebot_message_id, timestamp_hint FROM messages WHERE message_uid = ?1",
            params![trimmed_uid],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(map_sqlite_err)?;

    if let Some((other_id, other_ts)) = dup_row {
        if other_id > onebot_message_id {
            // 仅当冲突行是较新生成的重复回采行时删除该重复行，确保原始存根较低的 OneBot 消息编号胜出
            transaction
                .execute(
                    "DELETE FROM messages WHERE onebot_message_id = ?1",
                    params![other_id],
                )
                .map_err(map_sqlite_err)?;
            let merged_ts = clean_ts.or_else(|| {
                other_ts
                    .as_deref()
                    .map(str::trim)
                    .filter(|val| !val.is_empty())
            });
            transaction
                .execute(
                    "UPDATE messages
                     SET message_uid = ?2,
                         timestamp_hint = COALESCE(?3, timestamp_hint)
                     WHERE onebot_message_id = ?1",
                    params![onebot_message_id, trimmed_uid, merged_ts],
                )
                .map_err(map_sqlite_err)?;
        } else if other_id < onebot_message_id {
            // 若较早编号已绑定该 UID，不删除历史记录以免误删旧消息，仅按需补全当前行时间提示
            if let Some(ts) = clean_ts {
                transaction
                    .execute(
                        "UPDATE messages SET timestamp_hint = COALESCE(?2, timestamp_hint)
                         WHERE onebot_message_id = ?1",
                        params![onebot_message_id, ts],
                    )
                    .map_err(map_sqlite_err)?;
            }
        }
    } else {
        transaction
            .execute(
                "UPDATE messages
                 SET message_uid = ?2,
                     timestamp_hint = COALESCE(?3, timestamp_hint)
                 WHERE onebot_message_id = ?1",
                params![onebot_message_id, trimmed_uid, clean_ts],
            )
            .map_err(map_sqlite_err)?;
    }

    transaction.commit().map_err(map_sqlite_err)?;
    Ok(())
}

/// 幂等持久化群聊或私聊消息；若相同 `message_uid` 已入库则直接复用原有消息编号而不消耗自增序列
pub(crate) fn record_message(conn: &mut Connection, input: &RecordMessageInput) -> Result<i64> {
    let transaction = conn.transaction().map_err(map_sqlite_err)?;
    let reserved = collect_batch_reserved_ids(&transaction, std::slice::from_ref(input))?;
    let mut claimed = HashSet::with_capacity(1);
    let message_id = insert_or_reuse_message_in_tx(&transaction, input, &reserved, &mut claimed)?;
    transaction.commit().map_err(map_sqlite_err)?;
    Ok(message_id)
}

/// 为已确认发送或转发的出站消息在 `Immediate` 事务内强制分配全新 `onebot_message_id`，碰撞 UID 时自动加盐去重
pub(crate) fn record_confirmed_outbound_message(
    conn: &mut Connection,
    input: &RecordMessageInput,
) -> Result<(i64, String)> {
    let transaction = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_sqlite_err)?;
    let mut effective_uid = input.message_uid.clone();
    if message_uid_exists_in_tx(&transaction, &effective_uid)? {
        let mut attempt = 1_u64;
        loop {
            let salted_uid = format!(
                "{}#out:{}:{}",
                input.message_uid, input.captured_at_ms, attempt
            );
            if !message_uid_exists_in_tx(&transaction, &salted_uid)? {
                effective_uid = salted_uid;
                break;
            }
            attempt = attempt.saturating_add(1);
        }
    }
    let new_id = insert_message_row_in_tx(&transaction, &effective_uid, input)?;
    transaction.commit().map_err(map_sqlite_err)?;
    Ok((new_id, effective_uid))
}

/// 在单事务内批量幂等持久化一组聊天消息并按原顺序返回各自生效的 OneBot 消息编号
pub(crate) fn record_messages_batch(
    conn: &mut Connection,
    inputs: &[RecordMessageInput],
) -> Result<Vec<i64>> {
    if inputs.is_empty() {
        return Ok(Vec::new());
    }
    let transaction = conn.transaction().map_err(map_sqlite_err)?;
    let reserved = collect_batch_reserved_ids(&transaction, inputs)?;
    let mut claimed = HashSet::with_capacity(inputs.len());
    let mut ids = Vec::with_capacity(inputs.len());
    for input in inputs {
        ids.push(insert_or_reuse_message_in_tx(
            &transaction,
            input,
            &reserved,
            &mut claimed,
        )?);
    }
    transaction.commit().map_err(map_sqlite_err)?;
    Ok(ids)
}

/// 按自增数字 OneBot 消息编号检索消息详情
pub(crate) fn get_message_by_id(
    conn: &Connection,
    onebot_message_id: i64,
) -> Result<Option<StoredMessageRecord>> {
    let mut stmt = conn
        .prepare_cached(
            "SELECT onebot_message_id, message_uid, group_name, group_id, user_id,
                    sender_name, raw_message, is_self, timestamp_hint, captured_at_ms,
                    chat_type, segments_json
             FROM messages
             WHERE onebot_message_id = ?1",
        )
        .map_err(map_sqlite_err)?;
    stmt.query_row(params![onebot_message_id], row_to_stored_message)
        .optional()
        .map_err(map_sqlite_err)
}

/// 按自增数字 OneBot 消息编号从持久化表移除已撤回的消息记录
pub(crate) fn delete_message_by_id(conn: &Connection, onebot_message_id: i64) -> Result<bool> {
    let mut stmt = conn
        .prepare_cached("DELETE FROM messages WHERE onebot_message_id = ?1")
        .map_err(map_sqlite_err)?;
    let affected = stmt
        .execute(params![onebot_message_id])
        .map_err(map_sqlite_err)?;
    Ok(affected > 0)
}

fn query_stored_messages(
    conn: &Connection,
    sql: &str,
    sql_params: &[&dyn rusqlite::ToSql],
) -> Result<Vec<StoredMessageRecord>> {
    let mut statement = conn.prepare_cached(sql).map_err(map_sqlite_err)?;
    let rows = statement
        .query_map(sql_params, row_to_stored_message)
        .map_err(map_sqlite_err)?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(map_sqlite_err)
}

/// 按可选会话类型过滤查询指定会话最近 `limit` 条缓存消息，并反转为按时间从旧到新排列
pub(crate) fn list_recent_messages_by_kind(
    conn: &Connection,
    group_name: &str,
    chat_type: Option<ChatTargetKind>,
    limit: usize,
) -> Result<Vec<StoredMessageRecord>> {
    if limit == 0 {
        return Ok(Vec::new());
    }

    let limit_value = i64::try_from(limit).unwrap_or(i64::MAX);
    let mut collected_items = if let Some(kind) = chat_type {
        let kind_str = kind.as_str();
        query_stored_messages(
            conn,
            "SELECT onebot_message_id, message_uid, group_name, group_id, user_id,
                    sender_name, raw_message, is_self, timestamp_hint, captured_at_ms,
                    chat_type, segments_json
             FROM messages
             WHERE group_name = ?1
               AND chat_type = ?2
             ORDER BY captured_at_ms DESC, onebot_message_id DESC
             LIMIT ?3",
            &[&group_name, &kind_str, &limit_value],
        )?
    } else {
        query_stored_messages(
            conn,
            "SELECT onebot_message_id, message_uid, group_name, group_id, user_id,
                    sender_name, raw_message, is_self, timestamp_hint, captured_at_ms,
                    chat_type, segments_json
             FROM messages
             WHERE group_name = ?1
             ORDER BY captured_at_ms DESC, onebot_message_id DESC
             LIMIT ?2",
            &[&group_name, &limit_value],
        )?
    };
    collected_items.reverse();
    Ok(collected_items)
}

/// 查询指定会话最近 `limit` 条缓存消息，并反转为按时间从旧到新排列
pub(crate) fn list_recent_messages(
    conn: &Connection,
    group_name: &str,
    limit: usize,
) -> Result<Vec<StoredMessageRecord>> {
    list_recent_messages_by_kind(conn, group_name, None, limit)
}

fn normalize_match_text(raw: &str) -> String {
    raw.split_whitespace().collect::<Vec<_>>().join(" ")
}

type ReplyCandidateRow = (i64, String, String, Option<String>);

fn query_reply_candidates(
    conn: &Connection,
    sql: &str,
    sql_params: &[&dyn rusqlite::ToSql],
) -> Result<Vec<ReplyCandidateRow>> {
    let mut stmt = conn.prepare_cached(sql).map_err(map_sqlite_err)?;
    let rows = stmt
        .query_map(sql_params, |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })
        .map_err(map_sqlite_err)?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(map_sqlite_err)
}

/// 按可选会话类型与会话名称，在最近 200 条消息中根据发送者提示与内容摘要反查被引用消息编号
pub(crate) fn find_recent_message_for_reply_with_kind(
    conn: &Connection,
    session: (&str, Option<ChatTargetKind>),
    sender_hint: Option<&str>,
    content_excerpt: &str,
) -> Result<Option<i64>> {
    let (group_name, chat_type) = session;
    let normalized_excerpt = normalize_match_text(content_excerpt);
    if normalized_excerpt.is_empty() {
        return Ok(None);
    }
    let normalized_sender = sender_hint.map(str::trim).filter(|val| !val.is_empty());

    let candidates = if let Some(kind) = chat_type {
        let kind_str = kind.as_str();
        query_reply_candidates(
            conn,
            "SELECT onebot_message_id, sender_name, raw_message, segments_json
             FROM messages
             WHERE group_name = ?1
               AND chat_type = ?2
             ORDER BY captured_at_ms DESC, onebot_message_id DESC
             LIMIT 200",
            &[&group_name, &kind_str],
        )?
    } else {
        query_reply_candidates(
            conn,
            "SELECT onebot_message_id, sender_name, raw_message, segments_json
             FROM messages
             WHERE group_name = ?1
             ORDER BY captured_at_ms DESC, onebot_message_id DESC
             LIMIT 200",
            &[&group_name],
        )?
    };

    let prefix_short: String = normalized_excerpt.chars().take(16).collect();
    for strict_sender in [true, false] {
        if strict_sender && normalized_sender.is_none() {
            continue;
        }
        for (id, sender_name, raw_message, segments_json) in &candidates {
            if strict_sender {
                let Some(hint) = normalized_sender else {
                    continue;
                };
                if sender_name.trim() != hint && !sender_name.contains(hint) {
                    continue;
                }
            }
            let plain = normalize_match_text(&segments_to_plain_summary(&deserialize_segments(
                segments_json.clone(),
                raw_message,
            )));
            let raw_norm = normalize_match_text(raw_message);
            if raw_norm.contains(&normalized_excerpt)
                || plain.contains(&normalized_excerpt)
                || (prefix_short.chars().count() >= 4
                    && (raw_norm.contains(&prefix_short) || plain.contains(&prefix_short)))
            {
                return Ok(Some(*id));
            }
        }
    }

    Ok(None)
}

/// 根据引用块提取的发送者提示与内容摘要，在指定会话最近 200 条消息中反查被引用消息的 OneBot 编号
pub(crate) fn find_recent_message_for_reply(
    conn: &Connection,
    group_name: &str,
    sender_hint: Option<&str>,
    content_excerpt: &str,
) -> Result<Option<i64>> {
    find_recent_message_for_reply_with_kind(conn, (group_name, None), sender_hint, content_excerpt)
}

/// 撤回消息时间窗回退匹配的最大允许时间差（毫秒，覆盖 QQ 两分钟撤回时限与轮询延迟）
const RECALL_FALLBACK_WINDOW_MS: u64 = 180_000;

fn sender_match_rank(record_sender: &str, clean_hint: Option<&str>) -> u8 {
    let Some(hint) = clean_hint else {
        return 0;
    };
    let rs = record_sender.trim();
    if rs == hint {
        2
    } else if rs.contains(hint) {
        1
    } else {
        0
    }
}

/// 结合可选会话类型、消失气泡 UID 与时间窗兜底规则，解析被撤回的消息记录
pub(crate) fn resolve_recalled_message_with_kind(
    conn: &Connection,
    session: (&str, Option<ChatTargetKind>),
    disappeared_uids: &[String],
    sender_name: Option<&str>,
    actor_is_self: bool,
) -> Result<Option<StoredMessageRecord>> {
    resolve_recalled_message_at_with_kind(
        conn,
        session,
        disappeared_uids,
        sender_name,
        actor_is_self,
        now_ms(),
    )
}

/// 结合消失气泡 UID 与时间窗兜底规则，解析指定群或私聊会话中被撤回的消息记录
pub(crate) fn resolve_recalled_message(
    conn: &Connection,
    group_name: &str,
    disappeared_uids: &[String],
    sender_name: Option<&str>,
    actor_is_self: bool,
) -> Result<Option<StoredMessageRecord>> {
    resolve_recalled_message_at(
        conn,
        group_name,
        disappeared_uids,
        sender_name,
        actor_is_self,
        now_ms(),
    )
}

/// 指定参考时间戳与会话边界，在消失气泡 UID 或 180 秒时间窗内解析被撤回的消息记录
pub(crate) fn resolve_recalled_message_at_with_kind(
    conn: &Connection,
    session: (&str, Option<ChatTargetKind>),
    disappeared_uids: &[String],
    sender_name: Option<&str>,
    actor_is_self: bool,
    current_ms: u64,
) -> Result<Option<StoredMessageRecord>> {
    let (group_name, chat_type) = session;
    if group_name.trim().is_empty() {
        return Ok(None);
    }
    let clean_sender = sender_name.map(str::trim).filter(|val| !val.is_empty());
    let kind_str = chat_type.map(|kind| kind.as_str());

    if !disappeared_uids.is_empty() {
        let uid_sql = if kind_str.is_some() {
            "SELECT onebot_message_id, message_uid, group_name, group_id, user_id,
                    sender_name, raw_message, is_self, timestamp_hint, captured_at_ms,
                    chat_type, segments_json
             FROM messages
             WHERE group_name = ?1 AND message_uid = ?2 AND chat_type = ?3"
        } else {
            "SELECT onebot_message_id, message_uid, group_name, group_id, user_id,
                    sender_name, raw_message, is_self, timestamp_hint, captured_at_ms,
                    chat_type, segments_json
             FROM messages
             WHERE group_name = ?1 AND message_uid = ?2"
        };
        let mut uid_stmt = conn.prepare_cached(uid_sql).map_err(map_sqlite_err)?;

        let mut best: Option<((bool, u8, u64, i64), StoredMessageRecord)> = None;
        for raw_uid in disappeared_uids {
            let uid = raw_uid.trim();
            if uid.is_empty() {
                continue;
            }
            let matched = if let Some(kind) = kind_str {
                uid_stmt
                    .query_row(params![group_name, uid, kind], row_to_stored_message)
                    .optional()
                    .map_err(map_sqlite_err)?
            } else {
                uid_stmt
                    .query_row(params![group_name, uid], row_to_stored_message)
                    .optional()
                    .map_err(map_sqlite_err)?
            };
            let Some(rec) = matched else {
                continue;
            };
            let self_match = rec.is_self == actor_is_self;
            let sender_rank = if actor_is_self && clean_sender == Some("你") {
                0
            } else {
                sender_match_rank(&rec.sender_name, clean_sender)
            };
            let score = (
                self_match,
                sender_rank,
                rec.captured_at_ms,
                rec.onebot_message_id,
            );
            if best
                .as_ref()
                .is_none_or(|(best_score, _)| score > *best_score)
            {
                best = Some((score, rec));
            }
        }
        if let Some((_, matched)) = best {
            return Ok(Some(matched));
        }
    }

    let cutoff_ms = i64_from_u64_saturating(current_ms.saturating_sub(RECALL_FALLBACK_WINDOW_MS));
    let target_is_self = if actor_is_self { 1_i64 } else { 0_i64 };
    let candidates = if let Some(kind) = kind_str {
        query_stored_messages(
            conn,
            "SELECT onebot_message_id, message_uid, group_name, group_id, user_id,
                    sender_name, raw_message, is_self, timestamp_hint, captured_at_ms,
                    chat_type, segments_json
             FROM messages
             WHERE group_name = ?1
               AND chat_type = ?2
               AND is_self = ?3
               AND captured_at_ms >= ?4
             ORDER BY captured_at_ms DESC, onebot_message_id DESC
             LIMIT 500",
            &[&group_name, &kind, &target_is_self, &cutoff_ms],
        )?
    } else {
        query_stored_messages(
            conn,
            "SELECT onebot_message_id, message_uid, group_name, group_id, user_id,
                    sender_name, raw_message, is_self, timestamp_hint, captured_at_ms,
                    chat_type, segments_json
             FROM messages
             WHERE group_name = ?1
               AND is_self = ?2
               AND captured_at_ms >= ?3
             ORDER BY captured_at_ms DESC, onebot_message_id DESC
             LIMIT 500",
            &[&group_name, &target_is_self, &cutoff_ms],
        )?
    };

    if actor_is_self || clean_sender.is_none() {
        return Ok(candidates.into_iter().next());
    }

    let Some(hint) = clean_sender else {
        return Ok(None);
    };
    if let Some(exact) = candidates
        .iter()
        .find(|rec| rec.sender_name.trim() == hint)
        .cloned()
    {
        return Ok(Some(exact));
    }
    Ok(candidates
        .into_iter()
        .find(|rec| rec.sender_name.trim().contains(hint)))
}

/// 指定参考时间戳并在不区分会话类型下解析被撤回的消息记录（向后兼容接口）
pub(crate) fn resolve_recalled_message_at(
    conn: &Connection,
    group_name: &str,
    disappeared_uids: &[String],
    sender_name: Option<&str>,
    actor_is_self: bool,
    current_ms: u64,
) -> Result<Option<StoredMessageRecord>> {
    resolve_recalled_message_at_with_kind(
        conn,
        (group_name, None),
        disappeared_uids,
        sender_name,
        actor_is_self,
        current_ms,
    )
}

fn row_to_stored_message(row: &Row<'_>) -> rusqlite::Result<StoredMessageRecord> {
    let raw_message: String = row.get(6)?;
    let chat_type_raw: String = row.get(10)?;
    let segments_json: Option<String> = row.get(11)?;
    let segments = deserialize_segments(segments_json, &raw_message);
    Ok(StoredMessageRecord {
        onebot_message_id: row.get(0)?,
        message_uid: row.get(1)?,
        group_name: row.get(2)?,
        group_id: row.get(3)?,
        user_id: row.get(4)?,
        sender_name: row.get(5)?,
        raw_message,
        is_self: row.get::<_, i64>(7)? != 0,
        timestamp_hint: row.get(8)?,
        captured_at_ms: u64_from_sql(row.get::<_, i64>(9)?),
        chat_type: ChatTargetKind::from_db_str(&chat_type_raw),
        segments,
    })
}
