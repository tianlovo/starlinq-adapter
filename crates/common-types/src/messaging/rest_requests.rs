//! 原生 REST 聊天动作请求载体定义：汇总打开私聊与撤回消息请求结构体

use serde::{Deserialize, Serialize};

/// 打开指定白名单好友私聊窗口请求载荷（兼容 `nickname`、`friend` 与 `group` 字段别名）
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OpenPrivateRequest {
    #[serde(alias = "nickname", alias = "friend", alias = "group")]
    pub target: String,
}

/// 撤回或删除指定消息 ID 请求载荷
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeleteMessageRequest {
    pub message_id: i64,
}

/// 原生 REST 图片 OCR 识别请求载荷
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OcrImageRequest {
    pub image: String,
    #[serde(default)]
    pub language: Option<String>,
}
