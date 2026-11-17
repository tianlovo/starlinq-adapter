//! 原生无边框窗口截图与 UI 弹窗探针诊断接口模块

use axum::extract::{Query, State};
use axum::http::header::CONTENT_TYPE;
use axum::http::HeaderValue;
use axum::response::{IntoResponse, Response};
use axum::Json;
use common_types::{AdapterError, OcrImageRequest, OcrImageResponse};
use serde::Deserialize;
use serde_json::Value;

use crate::error::ApiResult;
use crate::rest::DiagQuery;
use crate::AppState;

/// `POST /api/diag/probe-ui` 只读弹窗探针请求载荷
#[derive(Debug, Deserialize)]
pub struct ProbeUiRequest {
    pub action: String,
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub query: Option<String>,
}

/// `GET /api/screenshot?hwnd=<opt_isize>` -> 返回目标或当前 QQ 窗口的无边框 `PrintWindow` PNG 图像
pub async fn screenshot(
    State(state): State<AppState>,
    Query(query): Query<DiagQuery>,
) -> ApiResult<Response> {
    let png_bytes = state.driver.capture_screenshot(query.hwnd).await?;
    let mut response = png_bytes.into_response();
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("image/png"));
    Ok(response)
}

/// `POST /api/diag/probe-ui` -> 在安全互斥门控内执行只读且自动复原的界面弹窗探针
pub async fn probe_ui(
    State(state): State<AppState>,
    Json(req): Json<ProbeUiRequest>,
) -> ApiResult<Json<Value>> {
    let _send_guard = state.send_gate.lock().await;
    let report = state
        .driver
        .probe_ui_action(&req.action, req.target, req.query)
        .await?;
    Ok(Json(report))
}

/// `POST /api/ocr` -> 调用本地 Windows OCR 引擎识别图片并返回文本与边界坐标框
pub async fn ocr(
    State(state): State<AppState>,
    Json(req): Json<OcrImageRequest>,
) -> ApiResult<Json<OcrImageResponse>> {
    let image = req.image.trim();
    if image.is_empty() {
        return Err(AdapterError::InputRejected("image is required".to_string()).into());
    }
    let language = req
        .language
        .as_deref()
        .map(str::trim)
        .filter(|val| !val.is_empty());
    let resp = state.driver.ocr_image(image, language).await?;
    Ok(Json(resp))
}
