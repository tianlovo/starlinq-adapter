//! 嵌入式存储记录与库级统计指标 DTO 定义

use serde::{Deserialize, Serialize};

use super::segments::{ChatTargetKind, MessageSegment};

/// 按自增数字 OneBot 消息编号索引的持久化聊天记录
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct StoredMessageRecord {
    pub onebot_message_id: i64,
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

/// 嵌入式 SQLite 数据库各核心表的当前记录数统计
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct StorageStats {
    pub db_path: String,
    pub group_count: u64,
    pub member_count: u64,
    pub message_count: u64,
    pub audit_count: u64,
    #[serde(default)]
    pub friend_count: u64,
}
