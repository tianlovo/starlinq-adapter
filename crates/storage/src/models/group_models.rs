//! 群聊持久化实体与更新参数对象定义

use serde::{Deserialize, Serialize};

/// 持久化在 SQLite 中的群聊元数据记录
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredGroupRecord {
    pub group_name: String,
    pub real_group_id: Option<i64>,
    pub onebot_group_id: i64,
    pub member_count: Option<i32>,
    pub max_member_count: Option<i32>,
    pub last_preview: Option<String>,
    pub unread_count: u32,
    pub drawer_checked_at_ms: Option<u64>,
    pub updated_at_ms: u64,
}

impl StoredGroupRecord {
    /// 优先返回真实数字 QQ 群号，未知时回退至确定性 OneBot 群号
    pub fn effective_group_id(&self) -> i64 {
        self.real_group_id.unwrap_or(self.onebot_group_id)
    }
}

/// 插入或更新群聊元数据使用的参数对象（Parameter Object）
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct UpsertGroupInput {
    pub group_name: String,
    pub real_group_id: Option<i64>,
    pub member_count: Option<i32>,
    pub max_member_count: Option<i32>,
    pub last_preview: Option<String>,
    pub unread_count: u32,
}
