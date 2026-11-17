//! 领域仓储门面：汇总群组、群成员、好友、消息与元数据审计子仓储及公共行映射辅助工具

pub mod friend_repo;
pub mod group_repo;
pub mod history_query_repo;
pub mod member_repo;
pub mod message_repo;
pub mod meta_audit_repo;

use std::time::{SystemTime, UNIX_EPOCH};

use common_types::AdapterError;

/// 将 rusqlite 底层错误统一转换为适配器 I/O 错误
pub(crate) fn map_sqlite_err(err: rusqlite::Error) -> AdapterError {
    AdapterError::Io(std::io::Error::other(format!("sqlite error: {err}")))
}

/// 获取当前系统毫秒级 Unix 时间戳
pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// 将 SQLite 有符号整数列安全夹紧为非负 u64
pub(crate) fn u64_from_sql(value: i64) -> u64 {
    value.max(0) as u64
}

/// 将 SQLite 有符号整数列安全夹紧为非负 u32
pub(crate) fn u32_from_sql(value: i64) -> u32 {
    value.clamp(0, i64::from(u32::MAX)) as u32
}

/// 将无符号 u64 时间戳饱和转换为有符号 SQLite INTEGER，防止超限回绕为负数破坏排序
pub(crate) fn i64_from_u64_saturating(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}
