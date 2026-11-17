//! 错误与结果类型定义模块：统一描述 UIA 引擎、QQNT 驱动及服务层错误枚举

use thiserror::Error;

/// 在 UIA 引擎、QQNT 驱动与 HTTP/OneBot 服务层之间传递的统一错误类型
#[derive(Debug, Error)]
pub enum AdapterError {
    #[error("target window not found: {0}")]
    WindowNotFound(String),

    #[error("group not found: {group}")]
    GroupNotFound { group: String },

    #[error(
        "private chat target '{target}' is not allowed; configure [target].allowed_private_friends"
    )]
    PrivateTargetNotAllowed { target: String },

    #[error(
        "member count exceeded safety guard for '{group}': detected={detected:?}, strict_below={limit}"
    )]
    MemberCountExceeded {
        group: String,
        detected: Option<u32>,
        limit: u32,
    },

    #[error("security prompt detected ({title}): {detail}")]
    SecurityPromptDetected { title: String, detail: String },

    #[error("UIA failure: {0}")]
    UiaFailure(String),

    #[error("input rejected: {0}")]
    InputRejected(String),

    #[error("rate limited; retry after {retry_after_ms} ms")]
    RateLimited { retry_after_ms: u64 },

    #[error(
        "response window expired for '{target}': elapsed={elapsed_ms}ms, window={window_ms}ms"
    )]
    ResponseWindowExpired {
        target: String,
        elapsed_ms: u64,
        window_ms: u64,
    },

    #[error("OneBot action '{action}' is not supported: {reason}")]
    UnsupportedAction { action: String, reason: String },

    #[error("OneBot action not found: {action}")]
    ActionNotFound { action: String },

    #[error("configuration error: {0}")]
    ConfigError(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// 适配器全局结果别名
pub type Result<T> = std::result::Result<T, AdapterError>;
