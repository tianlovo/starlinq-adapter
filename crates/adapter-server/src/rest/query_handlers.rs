//! 原生只读查询接口模块：提供健康检查、状态、窗口诊断、会话列表及嵌入式存储查询接口

use axum::extract::{Query, State};
use axum::Json;
use common_types::{
    AdapterStatus, ChatMessage, FriendInfo, GroupInfoSummary, GroupMemberInfo, SessionSummary,
    StorageStats, UiNodeSnapshot, WindowDiagnosticReport, WindowInfo,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::constants::{
    DEFAULT_TREE_DEPTH, MESSAGES_DEFAULT_LIMIT, MESSAGES_MAX_LIMIT, TREE_MAX_DEPTH_CAP,
};
use crate::error::ApiResult;
use crate::rest::{DiagQuery, FriendsQuery, GroupsQuery};
use crate::AppState;

/// `GET /api/tree` 查询参数载荷
#[derive(Debug, Deserialize)]
pub struct TreeQuery {
    pub hwnd: Option<isize>,
    pub depth: Option<usize>,
}

/// `GET /api/messages` 查询参数载荷
#[derive(Debug, Deserialize)]
pub struct MessagesQuery {
    pub group: Option<String>,
    pub limit: Option<usize>,
}

/// `GET /api/group-members` 与 `GET /api/group-info` 复用的群查询参数载荷
#[derive(Debug, Default, Deserialize)]
pub struct GroupMembersQuery {
    pub group: Option<String>,
    pub group_id: Option<i64>,
    #[serde(default)]
    pub no_cache: bool,
}

/// 单群详情与成员列表接口共用的查询参数类型别名
pub type GroupLookupQuery = GroupMembersQuery;

/// `GET /api/health` -> 返回服务存活状态与当前进程号
pub async fn health() -> Json<Value> {
    Json(json!({ "ok": true, "pid": std::process::id() }))
}

/// `GET /api/status` -> 返回驱动当前状态快照
pub async fn status(State(state): State<AppState>) -> ApiResult<Json<AdapterStatus>> {
    let snapshot = state.driver.status().await?;
    Ok(Json(snapshot))
}

/// `GET /api/windows` -> 枚举当前桌面上的候选 QQNT 窗口
pub async fn windows(State(state): State<AppState>) -> ApiResult<Json<Vec<WindowInfo>>> {
    let list = state.driver.list_windows().await?;
    Ok(Json(list))
}

/// `GET /api/diag?hwnd=<opt_isize>` -> 返回指定或默认窗口的综合无障碍诊断报告
pub async fn diag(
    State(state): State<AppState>,
    Query(query): Query<DiagQuery>,
) -> ApiResult<Json<WindowDiagnosticReport>> {
    let report = state.driver.diagnose_window(query.hwnd).await?;
    Ok(Json(report))
}

/// `GET /api/tree?hwnd=<opt_isize>&depth=<opt_usize>` -> 抓取 UI Automation 元素树快照
pub async fn tree(
    State(state): State<AppState>,
    Query(query): Query<TreeQuery>,
) -> ApiResult<Json<UiNodeSnapshot>> {
    let max_depth = query
        .depth
        .unwrap_or(DEFAULT_TREE_DEPTH)
        .min(TREE_MAX_DEPTH_CAP);
    let snapshot = state.driver.inspect_tree(query.hwnd, max_depth).await?;
    Ok(Json(snapshot))
}

/// `GET /api/sessions` -> 返回左侧会话列表摘要
pub async fn sessions(State(state): State<AppState>) -> ApiResult<Json<Vec<SessionSummary>>> {
    let list = state.driver.list_sessions().await?;
    Ok(Json(list))
}

/// `GET /api/messages?group=<opt>&limit=<opt>` -> 拉取指定群或当前激活群的近期消息
pub async fn messages(
    State(state): State<AppState>,
    Query(query): Query<MessagesQuery>,
) -> ApiResult<Json<Vec<ChatMessage>>> {
    let group_name = match query.group.filter(|name| !name.trim().is_empty()) {
        Some(name) => name,
        None => state
            .driver
            .status()
            .await?
            .active_group
            .unwrap_or_else(|| state.config.target.group_name.clone()),
    };
    let clamped_limit = query
        .limit
        .unwrap_or(MESSAGES_DEFAULT_LIMIT)
        .clamp(1, MESSAGES_MAX_LIMIT);
    let list = state
        .driver
        .poll_messages(&group_name, clamped_limit)
        .await?;
    Ok(Json(list))
}

/// `GET /api/friends?no_cache=<opt_bool>` -> 查询已缓存或刷新后的好友列表元数据
pub async fn friends(
    State(state): State<AppState>,
    Query(query): Query<FriendsQuery>,
) -> ApiResult<Json<Vec<FriendInfo>>> {
    let list = state.driver.get_friend_list(query.no_cache).await?;
    Ok(Json(list))
}

/// `GET /api/groups?no_cache=<opt_bool>` -> 查询已缓存群列表元数据（可选跳过缓存强制刷新）
pub async fn groups(
    State(state): State<AppState>,
    Query(query): Query<GroupsQuery>,
) -> ApiResult<Json<Vec<GroupInfoSummary>>> {
    let list = state.driver.get_group_list(query.no_cache).await?;
    Ok(Json(list))
}

async fn resolve_optional_group_query_name(
    state: &AppState,
    query: &GroupMembersQuery,
) -> Result<Option<String>, common_types::AdapterError> {
    let requested_name = query
        .group
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(ToOwned::to_owned);

    if query.group_id.is_none() && requested_name.is_none() {
        Ok(Some(
            state
                .driver
                .status()
                .await?
                .active_group
                .unwrap_or_else(|| state.config.target.group_name.clone()),
        ))
    } else {
        Ok(requested_name)
    }
}

/// `GET /api/group-info` -> 按群名称或数字群号查询单群详情并触发安全真实群号探测
pub async fn group_info(
    State(state): State<AppState>,
    Query(query): Query<GroupLookupQuery>,
) -> ApiResult<Json<GroupInfoSummary>> {
    let fallback_name = resolve_optional_group_query_name(&state, &query).await?;
    let info = state
        .driver
        .get_group_info(query.group_id, fallback_name, query.no_cache)
        .await?;
    Ok(Json(info))
}

/// `GET /api/group-members` -> 按群名称或数字群号查询群成员列表
pub async fn group_members(
    State(state): State<AppState>,
    Query(query): Query<GroupMembersQuery>,
) -> ApiResult<Json<Vec<GroupMemberInfo>>> {
    let fallback_name = resolve_optional_group_query_name(&state, &query).await?;
    let members = state
        .driver
        .get_group_member_list(query.group_id, fallback_name, query.no_cache)
        .await?;
    Ok(Json(members))
}

/// `GET /api/storage/stats` -> 返回 SQLite 持久化记录数统计
pub async fn storage_stats(State(state): State<AppState>) -> ApiResult<Json<StorageStats>> {
    let stats = state.driver.storage_stats().await?;
    Ok(Json(stats))
}

/// `GET /api/storage` -> 同时返回存储统计、已缓存群与好友概要
pub async fn storage_overview(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let stats = state.driver.storage_stats().await?;
    let cached_groups = state.driver.get_group_list(false).await?;
    let cached_friends = state.driver.get_friend_list(false).await?;
    Ok(Json(json!({
        "stats": stats,
        "groups": cached_groups,
        "friends": cached_friends,
    })))
}
