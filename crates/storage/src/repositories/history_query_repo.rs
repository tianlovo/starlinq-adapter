//! 历史消息深度分页与多条件过滤仓储：支持游标分页、会话隔离与转义子串检索

use common_types::{
    strip_group_count_suffix, ChatTargetKind, MessageHistoryFilter, MessageSegment, Result,
    StoredMessageRecord,
};
use rusqlite::types::Value as SqlValue;
use rusqlite::{params_from_iter, Connection, Row};

use super::{map_sqlite_err, u64_from_sql};

/// 将用户输入的字面量检索词转义为 `LIKE ... ESCAPE '\'` 安全模式串
fn escape_like_pattern(raw: &str) -> String {
    let mut escaped = String::with_capacity(raw.len() + 4);
    for ch in raw.chars() {
        match ch {
            '\\' | '%' | '_' => {
                escaped.push('\\');
                escaped.push(ch);
            }
            other => escaped.push(other),
        }
    }
    format!("%{escaped}%")
}

/// 检查文本是否包含存在大小写折叠差异的非 ASCII 字符（如带重音拉丁字母或西里尔字母）
fn has_cased_non_ascii(raw: &str) -> bool {
    raw.chars().any(|ch| {
        !ch.is_ascii()
            && (ch.to_lowercase().ne(std::iter::once(ch))
                || ch.to_uppercase().ne(std::iter::once(ch)))
    })
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

/// 按会话名、会话类型、游标上下界、发送者与关键词深度分页检索已缓存消息
pub(crate) fn query_messages_filtered(
    conn: &Connection,
    filter: &MessageHistoryFilter,
) -> Result<Vec<StoredMessageRecord>> {
    let mut conditions: Vec<String> = Vec::new();
    let mut bind_values: Vec<SqlValue> = Vec::new();

    let trimmed_session = filter.session_name.trim();
    if !trimmed_session.is_empty() {
        let canon_session = strip_group_count_suffix(trimmed_session);
        if !canon_session.is_empty() && canon_session != trimmed_session {
            let idx1 = bind_values.len() + 1;
            let idx2 = idx1 + 1;
            conditions.push(format!("(group_name = ?{idx1} OR group_name = ?{idx2})"));
            bind_values.push(SqlValue::Text(canon_session));
            bind_values.push(SqlValue::Text(trimmed_session.to_string()));
        } else {
            let idx = bind_values.len() + 1;
            conditions.push(format!("group_name = ?{idx}"));
            bind_values.push(SqlValue::Text(trimmed_session.to_string()));
        }
    }

    if let Some(kind) = filter.chat_type {
        let idx = bind_values.len() + 1;
        conditions.push(format!("chat_type = ?{idx}"));
        bind_values.push(SqlValue::Text(kind.as_str().to_string()));
    }
    if let Some(max_id) = filter.before_or_at_id {
        let idx = bind_values.len() + 1;
        conditions.push(format!("onebot_message_id <= ?{idx}"));
        bind_values.push(SqlValue::Integer(max_id));
    }
    if let Some(min_id) = filter.after_id {
        let idx = bind_values.len() + 1;
        conditions.push(format!("onebot_message_id > ?{idx}"));
        bind_values.push(SqlValue::Integer(min_id));
    }

    let sender_needle = filter
        .sender
        .as_deref()
        .map(str::trim)
        .filter(|val| !val.is_empty());
    let keyword_needle = filter
        .keyword
        .as_deref()
        .map(str::trim)
        .filter(|val| !val.is_empty());
    let mut needs_rust_text_filter = false;

    if let Some(sender) = sender_needle {
        if has_cased_non_ascii(sender) {
            needs_rust_text_filter = true;
        } else {
            let idx = bind_values.len() + 1;
            conditions.push(format!("LOWER(sender_name) LIKE LOWER(?{idx}) ESCAPE '\\'"));
            bind_values.push(SqlValue::Text(escape_like_pattern(sender)));
        }
    }
    if let Some(keyword) = keyword_needle {
        if has_cased_non_ascii(keyword) {
            needs_rust_text_filter = true;
        } else {
            let idx = bind_values.len() + 1;
            conditions.push(format!("LOWER(raw_message) LIKE LOWER(?{idx}) ESCAPE '\\'"));
            bind_values.push(SqlValue::Text(escape_like_pattern(keyword)));
        }
    }

    let where_clause = if conditions.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", conditions.join(" AND "))
    };
    let clamped_limit = filter.limit.clamp(1, 200);
    let incremental_asc = filter.after_id.is_some() && filter.before_or_at_id.is_none();
    let order_dir = if incremental_asc { "ASC" } else { "DESC" };

    let limit_clause = if needs_rust_text_filter {
        String::new()
    } else {
        let limit_idx = bind_values.len() + 1;
        bind_values.push(SqlValue::Integer(
            i64::try_from(clamped_limit).unwrap_or(200),
        ));
        format!(" LIMIT ?{limit_idx}")
    };

    let sql = format!(
        "SELECT onebot_message_id, message_uid, group_name, group_id, user_id,
                sender_name, raw_message, is_self, timestamp_hint, captured_at_ms,
                chat_type, segments_json
         FROM messages
         {where_clause}
         ORDER BY onebot_message_id {order_dir}{limit_clause}"
    );
    let mut stmt = conn.prepare(&sql).map_err(map_sqlite_err)?;
    let rows = stmt
        .query_map(params_from_iter(bind_values.iter()), row_to_stored_message)
        .map_err(map_sqlite_err)?;
    let mut records = rows
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(map_sqlite_err)?;

    if needs_rust_text_filter {
        let sender_lower = sender_needle.map(str::to_lowercase);
        let keyword_lower = keyword_needle.map(str::to_lowercase);
        records.retain(|rec| {
            sender_lower
                .as_deref()
                .is_none_or(|needle| rec.sender_name.to_lowercase().contains(needle))
                && keyword_lower
                    .as_deref()
                    .is_none_or(|needle| rec.raw_message.to_lowercase().contains(needle))
        });
        records.truncate(clamped_limit);
    }

    if incremental_asc {
        if filter.reverse_order {
            records.reverse();
        }
    } else if !filter.reverse_order {
        records.reverse();
    }
    Ok(records)
}
