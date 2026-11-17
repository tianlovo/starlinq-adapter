//! 网络服务、嵌入式存储与 OneBot 传输配置结构体定义

use serde::{Deserialize, Serialize};

use crate::constants::{
    DEFAULT_DB_PATH, DEFAULT_ONEBOT_HEARTBEAT_MS, DEFAULT_ONEBOT_RECONNECT_MS, DEFAULT_SERVER_HOST,
    DEFAULT_SERVER_PORT,
};

/// HTTP 与 WebSocket 服务端监听配置
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    pub access_token: Option<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: DEFAULT_SERVER_HOST.to_string(),
            port: DEFAULT_SERVER_PORT,
            access_token: None,
        }
    }
}

/// 嵌入式 SQLite 持久化存储配置
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct StorageConfig {
    pub db_path: String,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            db_path: DEFAULT_DB_PATH.to_string(),
        }
    }
}

/// OneBot v11 协议适配与多通道传输配置（正反向 WS、HTTP Webhook 与限流）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct OneBotConfig {
    pub reverse_ws_urls: Vec<String>,
    pub reverse_ws_api_urls: Vec<String>,
    pub reverse_ws_event_urls: Vec<String>,
    pub http_post_urls: Vec<String>,
    pub access_token: Option<String>,
    pub secret: Option<String>,
    pub post_message_sent: bool,
    #[serde(default)]
    pub allow_degraded_media_summary: bool,
    pub rate_limit_interval_ms: u64,
    pub heartbeat_interval_ms: u64,
    pub reconnect_interval_ms: u64,
}

impl Default for OneBotConfig {
    fn default() -> Self {
        Self {
            reverse_ws_urls: Vec::new(),
            reverse_ws_api_urls: Vec::new(),
            reverse_ws_event_urls: Vec::new(),
            http_post_urls: Vec::new(),
            access_token: None,
            secret: None,
            post_message_sent: false,
            allow_degraded_media_summary: false,
            rate_limit_interval_ms: 250,
            heartbeat_interval_ms: DEFAULT_ONEBOT_HEARTBEAT_MS,
            reconnect_interval_ms: DEFAULT_ONEBOT_RECONNECT_MS,
        }
    }
}
