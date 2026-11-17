//! 好友联系人数据库写入参数与持久化记录定义

use serde::{Deserialize, Serialize};

/// 写入或更新单个好友会话记录时的参数载体
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct UpsertFriendInput {
    pub nickname: String,
    pub remark: String,
    pub real_user_id: Option<i64>,
    pub last_preview: Option<String>,
}

/// 持久化在 SQLite `friends` 表中的完整好友元数据记录
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct StoredFriendRecord {
    pub friend_key: String,
    pub nickname: String,
    pub remark: String,
    pub real_user_id: Option<i64>,
    pub synthetic_user_id: i64,
    pub last_preview: Option<String>,
    pub updated_at_ms: u64,
}

impl StoredFriendRecord {
    /// 返回对外 OneBot 展示的有效数字用户标识（优先使用真实 QQ 号）
    pub fn effective_user_id(&self) -> i64 {
        self.real_user_id.unwrap_or(self.synthetic_user_id)
    }
}
