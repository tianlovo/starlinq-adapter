//! OneBot v11 好友信息数据传输对象定义

use serde::{Deserialize, Serialize};

/// 对外 OneBot v11 `get_friend_list` 与 `/api/friends` 返回的好友概要实体
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct FriendInfo {
    pub user_id: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub real_user_id: Option<i64>,
    pub nickname: String,
    pub remark: String,
}
