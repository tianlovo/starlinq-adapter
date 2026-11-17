//! 请求鉴权中间件模块：实现基于责任链拦截模式的可选 Bearer 与查询令牌校验

use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

use crate::constants::{BEARER_PREFIX, HTTP_MAX_BODY_BYTES, ONEBOT_TOKEN_PREFIX};
use crate::onebot::payloads::decode_url_query_component;
use crate::AppState;

fn is_content_length_oversized(raw_header: &str) -> bool {
    let trimmed = raw_header.trim();
    if trimmed.is_empty() {
        return false;
    }
    match trimmed.parse::<u128>() {
        Ok(len) => len > HTTP_MAX_BODY_BYTES as u128,
        Err(_) => trimmed.bytes().all(|b| b.is_ascii_digit()),
    }
}

fn check_content_length_limit(request: &Request) -> Option<Response> {
    let raw = request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())?;
    if !is_content_length_oversized(raw) {
        return None;
    }
    Some(
        (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(json!({
                "ok": false,
                "status": "failed",
                "retcode": 1413,
                "error": format!("payload too large: Content-Length exceeds {HTTP_MAX_BODY_BYTES} bytes"),
                "code": "PAYLOAD_TOO_LARGE",
            })),
        )
            .into_response(),
    )
}

fn constant_time_eq(left: &str, right: &str) -> bool {
    let a = left.as_bytes();
    let b = right.as_bytes();
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0_u8;
    for (&x, &y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn extract_auth_header_token(raw_header: &str) -> Option<&str> {
    let trimmed = raw_header.trim();
    for prefix in [BEARER_PREFIX, ONEBOT_TOKEN_PREFIX] {
        let len = prefix.len();
        // 使用字符边界安全的 get(..len) 切片，避免非 ASCII 请求头触发 UTF-8 切片崩溃
        if let (Some(head), Some(tail)) = (trimmed.get(..len), trimmed.get(len..)) {
            if head.eq_ignore_ascii_case(prefix) && !tail.trim().is_empty() {
                return Some(tail.trim());
            }
        }
    }
    None
}

/// 当配置了 `server.access_token` 时校验请求头 Bearer/Token 或查询参数 `access_token`
pub async fn bearer_auth_middleware(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let Some(expected_token) = state
        .config
        .server
        .access_token
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        if let Some(resp) = check_content_length_limit(&request) {
            return resp;
        }
        return next.run(request).await;
    };

    let header_matched = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(extract_auth_header_token)
        .is_some_and(|token| constant_time_eq(token, expected_token));

    let query_matched = request.uri().query().is_some_and(|query_str| {
        query_str.split('&').any(|pair| {
            pair.split_once('=').is_some_and(|(raw_key, raw_value)| {
                let decoded_key = decode_url_query_component(raw_key);
                let decoded_val = decode_url_query_component(raw_value);
                decoded_key == "access_token" && constant_time_eq(&decoded_val, expected_token)
            })
        })
    });

    if header_matched || query_matched {
        if let Some(resp) = check_content_length_limit(&request) {
            return resp;
        }
        next.run(request).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({
                "ok": false,
                "error": "unauthorized: valid Bearer token or access_token required",
                "code": "UNAUTHORIZED",
            })),
        )
            .into_response()
    }
}
