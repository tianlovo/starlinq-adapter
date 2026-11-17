//! 群成员写入参数与消息记录参数对象定义

use common_types::{ChatTargetKind, MessageSegment};
use serde::{Deserialize, Serialize};

/// 写入或更新单个群成员缓存记录时的输入载体
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct UpsertMemberInput {
    pub nickname: String,
    pub real_user_id: Option<i64>,
    pub card: String,
    pub role: String,
}

/// 幂等持久化群聊或私聊消息使用的参数对象（Parameter Object）
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RecordMessageInput {
    pub message_uid: String,
    pub group_name: String,
    pub group_id: i64,
    pub user_id: i64,
    pub sender_name: String,
    pub raw_message: String,
    pub is_self: bool,
    pub timestamp_hint: Option<String>,
    pub captured_at_ms: u64,
    #[serde(default)]
    pub chat_type: ChatTargetKind,
    #[serde(default)]
    pub segments: Vec<MessageSegment>,
}
