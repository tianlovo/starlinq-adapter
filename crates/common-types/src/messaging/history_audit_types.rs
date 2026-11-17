//! 历史消息深度分页过滤、审计事件记录与单条转发请求载荷定义

use serde::{Deserialize, Serialize};

use crate::onebot::ChatTargetKind;

/// 离线消息历史深度分页与多条件过滤查询参数（Parameter Object）
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct MessageHistoryFilter {
    /// 目标群名称或好友昵称（空串表示不限定会话）
    pub session_name: String,
    /// 可选会话分类过滤（群聊或好友私聊）
    #[serde(default)]
    pub chat_type: Option<ChatTargetKind>,
    /// 向前翻页上界游标（仅返回 `onebot_message_id <= before_or_at_id` 的记录）
    #[serde(default)]
    pub before_or_at_id: Option<i64>,
    /// 向后增量游标（仅返回 `onebot_message_id > after_id` 的记录）
    #[serde(default)]
    pub after_id: Option<i64>,
    /// 发送者昵称不区分大小写子串过滤条件
    #[serde(default)]
    pub sender: Option<String>,
    /// 消息正文不区分大小写子串过滤条件
    #[serde(default)]
    pub keyword: Option<String>,
    /// 单页最大返回条数（默认实现与仓储层均限制在 `[1, 200]` 区间）
    #[serde(default)]
    pub limit: usize,
    /// 是否按消息编号从新到旧倒序输出
    #[serde(default)]
    pub reverse_order: bool,
}

/// 持久化安全熔断、污染清理与异步异常审计事件记录
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuditEventRecord {
    /// 审计流水自增主键编号
    pub id: i64,
    /// 审计事件类型分类标识
    pub event_kind: String,
    /// 结构化 JSON 详情载荷字符串
    pub detail_json: String,
    /// 事件写入时的 Unix 毫秒时间戳
    pub created_at_ms: u64,
}

/// 将单条已缓存消息原生转发至指定群聊或好友会话的请求载荷
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SingleForwardRequest {
    /// 待转发历史消息的 OneBot 数字编号
    pub message_id: i64,
    /// 目标群聊名称或私聊好友昵称
    pub target: String,
    /// 目标会话类型（群聊或好友私聊）
    pub target_kind: ChatTargetKind,
    /// 可选发送截止 Unix 毫秒时间戳，超时后中止外发
    #[serde(default)]
    pub deadline_unix_ms: Option<u64>,
}
