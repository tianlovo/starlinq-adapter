//! OneBot v11 登录账号、群概要与群成员实体定义

use serde::{Deserialize, Serialize};

/// OneBot `get_login_info` 接口返回的当前机器人登录信息
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct BotLoginInfo {
    pub user_id: i64,
    pub nickname: String,
    pub real_qq_id: Option<i64>,
}

/// OneBot `get_group_list` 与 `get_group_info` 返回的群元数据摘要
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct GroupInfoSummary {
    pub group_id: i64,
    pub real_group_id: Option<i64>,
    pub group_name: String,
    pub member_count: i32,
    pub max_member_count: i32,
}

/// OneBot `get_group_member_list` 与 `get_group_member_info` 返回的群成员记录
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct GroupMemberInfo {
    pub group_id: i64,
    pub user_id: i64,
    pub real_user_id: Option<i64>,
    pub nickname: String,
    pub card: String,
    pub role: String,
}
