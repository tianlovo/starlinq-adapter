//! 原生会话切换、撤回与离线历史查询处理模块：提供 `/api/open-private`、`/api/delete-msg` 与 `/api/messages/history`

use axum::extract::{Query, State};
use axum::Json;
use common_types::{
    AdapterError, AdapterStatus, ChatTargetKind, DeleteMessageRequest, MessageHistoryFilter,
    OpenPrivateRequest, StoredMessageRecord,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::constants::{MESSAGES_DEFAULT_LIMIT, MESSAGES_MAX_LIMIT};
use crate::error::ApiResult;
use crate::AppState;

/// `GET /api/groups` 查询参数载荷
#[derive(Debug, Default, Deserialize)]
pub struct GroupsQuery {
    #[serde(default)]
    pub no_cache: bool,
}

/// `GET /api/messages/history` 查询参数载荷
#[derive(Debug, Default, Deserialize)]
pub struct MessagesHistoryQuery {
    pub session: Option<String>,
    pub group: Option<String>,
    pub limit: Option<usize>,
    #[serde(default)]
    pub chat_type: Option<ChatTargetKind>,
    #[serde(default)]
    pub before_id: Option<i64>,
    #[serde(default)]
    pub after_id: Option<i64>,
    #[serde(default)]
    pub sender: Option<String>,
    #[serde(default)]
    pub keyword: Option<String>,
    #[serde(default)]
    pub reverse_order: bool,
}

fn non_empty_trimmed(raw: Option<String>) -> Option<String> {
    raw.and_then(|value| {
        let trimmed = value.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    })
}

/// `POST /api/open-private` -> 在发送通道互斥锁保护下切换并打开白名单好友私聊窗口
pub async fn open_private(
    State(state): State<AppState>,
    Json(payload): Json<OpenPrivateRequest>,
) -> ApiResult<Json<AdapterStatus>> {
    let target = payload.target.trim();
    if target.is_empty() {
        return Err(AdapterError::InputRejected(
            "open-private target must not be empty".to_string(),
        )
        .into());
    }
    let _send_guard = state.send_gate.lock().await;
    let status = state.driver.open_private(target).await?;
    Ok(Json(status))
}

/// `POST /api/delete-msg` -> 在发送通道互斥锁保护下撤回或删除指定消息 ID
pub async fn delete_msg(
    State(state): State<AppState>,
    Json(payload): Json<DeleteMessageRequest>,
) -> ApiResult<Json<Value>> {
    if payload.message_id <= 0 {
        return Err(AdapterError::InputRejected(
            "message_id must be a positive integer".to_string(),
        )
        .into());
    }
    let _send_guard = state.send_gate.lock().await;
    state.driver.delete_message(payload.message_id).await?;
    Ok(Json(json!({
        "ok": true,
        "message_id": payload.message_id,
    })))
}

/// `GET /api/messages/history` -> 仅从本地 SQLite 缓存读取近期消息记录（不触碰 UIA 且不切群）
pub async fn messages_history(
    State(state): State<AppState>,
    Query(query): Query<MessagesHistoryQuery>,
) -> ApiResult<Json<Vec<StoredMessageRecord>>> {
    let session_name = non_empty_trimmed(query.session)
        .or_else(|| non_empty_trimmed(query.group))
        .or_else(|| {
            state
                .driver
                .cached_status()
                .and_then(|status| status.active_group)
                .and_then(|active| non_empty_trimmed(Some(active)))
        })
        .unwrap_or_else(|| state.config.target.group_name.clone());
    if query.chat_type == Some(ChatTargetKind::Private)
        && !state.config.target.is_private_friend_allowed(&session_name)
    {
        return Err(AdapterError::PrivateTargetNotAllowed {
            target: session_name,
        }
        .into());
    }
    let clamped = query
        .limit
        .unwrap_or(MESSAGES_DEFAULT_LIMIT)
        .clamp(1, MESSAGES_MAX_LIMIT);
    let mut records = state
        .driver
        .query_messages_history(MessageHistoryFilter {
            session_name: session_name.clone(),
            chat_type: query.chat_type,
            before_or_at_id: query.before_id.filter(|&id| id > 0),
            after_id: query.after_id.filter(|&id| id > 0),
            sender: non_empty_trimmed(query.sender),
            keyword: non_empty_trimmed(query.keyword),
            limit: clamped,
            reverse_order: query.reverse_order,
        })
        .await?;
    if query.chat_type.is_none() && !records.is_empty() {
        records.retain(|rec| {
            rec.chat_type != ChatTargetKind::Private
                || state
                    .config
                    .target
                    .is_private_friend_allowed(&rec.group_name)
        });
        if records.is_empty() {
            return Err(AdapterError::PrivateTargetNotAllowed {
                target: session_name,
            }
            .into());
        }
    }
    Ok(Json(records))
}
