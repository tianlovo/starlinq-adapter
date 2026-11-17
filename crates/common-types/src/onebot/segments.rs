//! OneBot v11 全量 21 种结构化消息段（Composite 模式节点）与聊天目标类型定义

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 聊天会话与发送目标的领域分类（群聊或好友私聊）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ChatTargetKind {
    #[default]
    Group,
    Private,
}

impl ChatTargetKind {
    /// 转换为 SQLite 持久化与 OneBot 字段共用的标准小写标识
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Group => "group",
            Self::Private => "private",
        }
    }

    /// 从数据库存储字符串无错还原目标类型（未知值保守回退为群聊）
    pub fn from_db_str(raw: &str) -> Self {
        if raw.trim().eq_ignore_ascii_case("private") {
            Self::Private
        } else {
            Self::Group
        }
    }
}

/// 标准化 OneBot v11 全量 21 种结构化消息段（组合模式节点）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum MessageSegment {
    /// 纯文本片段
    Text { text: String },
    /// QQ 经典系统表情（附带可选中文语义名称）
    Face {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
    },
    /// 图片或闪照消息段（兼容官方 `type="flash"` 与扩展 `sub_type`）
    Image {
        file: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
        #[serde(
            default,
            rename = "type",
            alias = "sub_type",
            skip_serializing_if = "Option::is_none"
        )]
        image_type: Option<String>,
    },
    /// 语音消息段
    Record {
        file: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        magic: Option<String>,
    },
    /// 短视频消息段
    Video {
        file: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
    },
    /// `@` 群成员或 `@全体成员` 消息段
    At {
        qq: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    /// 猜拳魔法表情
    Rps {},
    /// 掷骰子魔法表情
    Dice {},
    /// 窗口抖动（戳一戳前身）
    Shake {},
    /// 戳一戳动作段
    Poke {
        #[serde(rename = "type")]
        poke_type: String,
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    /// 匿名发消息修饰段
    Anonymous {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ignore: Option<String>,
    },
    /// 链接分享卡片
    Share {
        url: String,
        title: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        image: Option<String>,
    },
    /// 推荐好友或推荐群聊名片
    Contact {
        #[serde(rename = "type")]
        contact_type: String,
        id: String,
    },
    /// 地理位置分享卡片
    Location {
        lat: String,
        lon: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content: Option<String>,
    },
    /// 音乐平台或自定义音乐卡片分享
    Music {
        #[serde(rename = "type")]
        music_type: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        audio: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        image: Option<String>,
    },
    /// 引用回复消息段
    Reply { id: String },
    /// 合并转发消息段
    Forward { id: String },
    /// 合并转发节点（支持消息 ID 引用与自定义虚拟节点）
    Node {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(default, alias = "uin", skip_serializing_if = "Option::is_none")]
        user_id: Option<String>,
        #[serde(default, alias = "name", skip_serializing_if = "Option::is_none")]
        nickname: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content: Option<Value>,
    },
    /// XML 富卡片消息段
    Xml { data: String },
    /// JSON 富卡片消息段
    Json { data: String },
    /// 生态扩展文件消息段
    File {
        file: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        size: Option<u64>,
    },
}
