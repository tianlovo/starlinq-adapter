//! HTTP 错误映射适配器模块：将领域层 AdapterError 统一转换为状态码与结构化 JSON 响应

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use common_types::AdapterError;
use serde_json::json;

/// 将内部 `AdapterError` 包装为可实现 Axum `IntoResponse` 的错误响应类型
#[derive(Debug)]
pub struct ApiError(pub AdapterError);

impl From<AdapterError> for ApiError {
    fn from(error: AdapterError) -> Self {
        Self(error)
    }
}

/// 统一处理器返回别名
pub type ApiResult<T> = std::result::Result<T, ApiError>;

/// 将 `AdapterError` 枚举变体映射为 HTTP 状态码、机器可读错误码及 OneBot 返回码
pub fn map_adapter_error(error: &AdapterError) -> (StatusCode, &'static str, i64) {
    match error {
        AdapterError::InputRejected(_) | AdapterError::ConfigError(_) => {
            (StatusCode::BAD_REQUEST, "BAD_REQUEST", 1400)
        }
        AdapterError::MemberCountExceeded { .. } => {
            (StatusCode::FORBIDDEN, "MEMBER_COUNT_EXCEEDED", 1403)
        }
        AdapterError::PrivateTargetNotAllowed { .. } => {
            (StatusCode::FORBIDDEN, "PRIVATE_TARGET_NOT_ALLOWED", 1403)
        }
        AdapterError::WindowNotFound(_) | AdapterError::GroupNotFound { .. } => {
            (StatusCode::NOT_FOUND, "NOT_FOUND", 1404)
        }
        AdapterError::SecurityPromptDetected { .. } => {
            (StatusCode::LOCKED, "SECURITY_PROMPT_DETECTED", 1423)
        }
        AdapterError::RateLimited { .. } => (StatusCode::TOO_MANY_REQUESTS, "RATE_LIMITED", 1429),
        AdapterError::ResponseWindowExpired { .. } => {
            (StatusCode::REQUEST_TIMEOUT, "RESPONSE_WINDOW_EXPIRED", 1408)
        }
        AdapterError::UnsupportedAction { .. } => {
            (StatusCode::NOT_IMPLEMENTED, "UNSUPPORTED_ACTION", 10004)
        }
        AdapterError::ActionNotFound { .. } => (StatusCode::NOT_FOUND, "ACTION_NOT_FOUND", 1404),
        AdapterError::UiaFailure(_) | AdapterError::Io(_) => {
            (StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL_ERROR", 1500)
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status_code, error_code, retcode) = map_adapter_error(&self.0);
        let body = json!({
            "ok": false,
            "error": self.0.to_string(),
            "code": error_code,
            "status": "failed",
            "retcode": retcode,
        });
        (status_code, Json(body)).into_response()
    }
}
