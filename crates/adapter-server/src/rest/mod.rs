//! 原生 REST 与事件流模块门面：汇总只读查询、状态变更、WebSocket 事件推流及进程热重启逻辑

pub mod diag_handlers;
pub mod mutation_handlers;
pub mod ops_audit_handlers;
pub mod query_handlers;
pub mod restart_ops;
pub mod scheduler_handlers;
pub mod session_recall_handlers;

use serde::Deserialize;

/// `GET /api/diag` 查询参数载荷
#[derive(Debug, Deserialize)]
pub struct DiagQuery {
    pub hwnd: Option<isize>,
}

/// `POST /api/ensure-qq` 查询参数载荷
#[derive(Debug, Default, Deserialize)]
pub struct EnsureQqQuery {
    #[serde(default)]
    pub force_wake: bool,
}

/// `GET /api/friends` 查询参数载荷
#[derive(Debug, Default, Deserialize)]
pub struct FriendsQuery {
    #[serde(default)]
    pub no_cache: bool,
}

pub use diag_handlers::{ocr, probe_ui, screenshot};
pub use mutation_handlers::{ensure_qq, open_group, send, shutdown, ws_events, RestSendRequest};
pub use ops_audit_handlers::{audit_events, clean_cache, forward_msg, reset_security};
pub use query_handlers::{
    diag, friends, group_info, group_members, groups, health, messages, sessions, status,
    storage_overview, storage_stats, tree, windows,
};
pub use restart_ops::{prepend_path_dir, restart, RESTART_CREATION_FLAGS};
pub use scheduler_handlers::{
    get_scheduler, get_scheduler_policy, mark_session_read, update_scheduler_policy,
    MarkSessionReadRequest,
};
pub use session_recall_handlers::{
    delete_msg, messages_history, open_private, GroupsQuery, MessagesHistoryQuery,
};
