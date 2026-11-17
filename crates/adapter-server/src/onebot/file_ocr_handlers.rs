//! OneBot v11 扩展文件上传/读取与 OCR 识别动作处理器

use std::path::{Path, PathBuf};

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use common_types::{AdapterError, MessageSegment};
use serde_json::{json, Value};
use tokio::fs;

use crate::constants::{
    ACTION_GET_FILE, ACTION_UPLOAD_GROUP_FILE, ACTION_UPLOAD_PRIVATE_FILE,
    GET_FILE_MAX_BASE64_BYTES, MEDIA_CACHE_SUBDIR, STAGING_UPLOADS_SUBDIR,
};
use crate::onebot::extended_handlers::{format_display_path, strip_file_url_prefix};
use crate::onebot::handlers::{
    is_http_or_https_source, resolve_send_target_group, resolve_send_target_private,
    send_group_segments_with_deadline, send_private_segments_with_deadline,
};
use crate::onebot::payloads::{parse_optional_string, OneBotSendPayload};
use crate::AppState;

/// 规范化本地文件引用（剥离 `file://` 前缀，并修正 Windows `file:///C:/...` 前导斜杠）
fn normalize_local_file_ref(raw: &str) -> &str {
    let mut stripped = strip_file_url_prefix(raw).trim();
    if cfg!(windows) && stripped.starts_with('/') && stripped.as_bytes().get(2) == Some(&b':') {
        stripped = &stripped[1..];
    }
    stripped
}

/// 校验并返回合法规范路径，确保目标文件严格位于允许的 `.runtime` 沙箱目录内
async fn verify_canonical_within_roots(
    candidate: &Path,
    allowed_roots: &[PathBuf],
) -> std::result::Result<Option<PathBuf>, AdapterError> {
    let Ok(canon) = fs::canonicalize(candidate).await else {
        return Ok(None);
    };
    if !allowed_roots.iter().any(|root| canon.starts_with(root)) {
        return Err(AdapterError::InputRejected(
            "access outside allowed runtime media or staging directories is forbidden".to_string(),
        ));
    }
    let Ok(meta) = fs::metadata(&canon).await else {
        return Ok(None);
    };
    if meta.is_file() {
        return Ok(Some(canon));
    }
    Ok(None)
}

/// 在 `.runtime/media` 与 `.runtime/staging_uploads`（含一级哈希子目录）内安全解析缓存文件
async fn resolve_sandboxed_file(
    clean_ref: &str,
    raw_ref: &str,
) -> std::result::Result<PathBuf, AdapterError> {
    let mut allowed_roots = Vec::new();
    let mut staging_root = None;
    for subdir in [MEDIA_CACHE_SUBDIR, STAGING_UPLOADS_SUBDIR] {
        if let Ok(canon) = fs::canonicalize(Path::new(subdir)).await {
            if subdir == STAGING_UPLOADS_SUBDIR {
                staging_root = Some(canon.clone());
            }
            allowed_roots.push(canon);
        }
    }
    if allowed_roots.is_empty() {
        return Err(AdapterError::InputRejected(format!(
            "cached file not found for reference '{raw_ref}'"
        )));
    }

    let ref_path = Path::new(clean_ref);
    if ref_path.is_absolute() {
        if let Some(found) = verify_canonical_within_roots(ref_path, &allowed_roots).await? {
            return Ok(found);
        }
        return Err(AdapterError::InputRejected(format!(
            "cached file not found for reference '{raw_ref}'"
        )));
    }

    // 带路径分隔符的相对引用既可能包含 `.runtime/...` 前缀，也可能是 `<hash>/<filename>`
    if clean_ref.contains('/') || clean_ref.contains('\\') {
        if let Some(found) = verify_canonical_within_roots(ref_path, &allowed_roots).await? {
            return Ok(found);
        }
        for root in &allowed_roots {
            if let Some(found) =
                verify_canonical_within_roots(&root.join(ref_path), &allowed_roots).await?
            {
                return Ok(found);
            }
        }
        return Err(AdapterError::InputRejected(format!(
            "cached file not found for reference '{raw_ref}'"
        )));
    }

    // 仅文件名时依次查找根目录直存项与 `.runtime/staging_uploads/<hash>/<file>` 子目录
    for root in &allowed_roots {
        if let Some(found) =
            verify_canonical_within_roots(&root.join(clean_ref), &allowed_roots).await?
        {
            return Ok(found);
        }
    }
    if let Some(staging) = staging_root {
        if let Ok(mut entries) = fs::read_dir(&staging).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                let entry_path = entry.path();
                let Ok(file_type) = entry.file_type().await else {
                    continue;
                };
                if !file_type.is_dir() {
                    continue;
                }
                if let Some(found) =
                    verify_canonical_within_roots(&entry_path.join(clean_ref), &allowed_roots)
                        .await?
                {
                    return Ok(found);
                }
            }
        }
    }

    Err(AdapterError::InputRejected(format!(
        "cached file not found for reference '{raw_ref}'"
    )))
}

/// 从 OneBot 参数构建群/私聊文件上传共用的截止时间与目标字段载荷
fn parse_upload_send_payload(
    params: &Value,
    action_name: &str,
) -> std::result::Result<OneBotSendPayload, AdapterError> {
    serde_json::from_value(params.clone())
        .map_err(|err| AdapterError::InputRejected(format!("invalid {action_name} params: {err}")))
}

/// `ocr_image` / `.ocr_image` -> 调用驱动本地 OCR 引擎识别图片文本与坐标框
pub(crate) async fn exec_ocr_image(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, AdapterError> {
    let image = parse_optional_string(&params, "image")
        .or_else(|| parse_optional_string(&params, "file"))
        .map(|val| val.trim().to_string())
        .filter(|val| !val.is_empty())
        .ok_or_else(|| AdapterError::InputRejected("image or file is required".to_string()))?;

    let language = parse_optional_string(&params, "language")
        .or_else(|| parse_optional_string(&params, "lang"))
        .map(|val| val.trim().to_string())
        .filter(|val| !val.is_empty());

    let result = state.driver.ocr_image(&image, language.as_deref()).await?;
    Ok(json!(result))
}

/// `upload_group_file` -> 通过群聊文件单段发送链路上传本地文件到群根目录
pub(crate) async fn exec_upload_group_file(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, AdapterError> {
    let file = parse_optional_string(&params, "file")
        .map(|val| val.trim().to_string())
        .filter(|val| !val.is_empty())
        .ok_or_else(|| AdapterError::InputRejected("file is required".to_string()))?;

    if let Some(folder) = parse_optional_string(&params, "folder") {
        let trimmed_folder = folder.trim();
        if !trimmed_folder.is_empty() && trimmed_folder != "/" {
            return Err(AdapterError::UnsupportedAction {
                action: ACTION_UPLOAD_GROUP_FILE.to_string(),
                reason: format!(
                    "desktop UIA file sending only delivers to group root; custom folder '{trimmed_folder}' is unsupported"
                ),
            });
        }
    }

    if is_http_or_https_source(&file) {
        return Err(AdapterError::UnsupportedAction {
            action: ACTION_UPLOAD_GROUP_FILE.to_string(),
            reason: "strict UIA mode rejects remote http(s):// file URLs; provide a local file path, file:// URI, or base64:// payload".to_string(),
        });
    }

    let name = parse_optional_string(&params, "name")
        .map(|val| val.trim().to_string())
        .filter(|val| !val.is_empty());
    let payload = parse_upload_send_payload(&params, ACTION_UPLOAD_GROUP_FILE)?;
    let target_group = resolve_send_target_group(state, &payload).await?;

    let segments = vec![MessageSegment::File {
        file,
        name,
        size: None,
    }];
    send_group_segments_with_deadline(state, target_group, segments, &payload).await
}

/// `upload_private_file` -> 校验私聊白名单并通过私聊文件单段发送链路发送本地文件
pub(crate) async fn exec_upload_private_file(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, AdapterError> {
    let file = parse_optional_string(&params, "file")
        .map(|val| val.trim().to_string())
        .filter(|val| !val.is_empty())
        .ok_or_else(|| AdapterError::InputRejected("file is required".to_string()))?;

    let payload = parse_upload_send_payload(&params, ACTION_UPLOAD_PRIVATE_FILE)?;
    let target_private = resolve_send_target_private(state, &payload).await?;

    if is_http_or_https_source(&file) {
        return Err(AdapterError::UnsupportedAction {
            action: ACTION_UPLOAD_PRIVATE_FILE.to_string(),
            reason: "strict UIA mode rejects remote http(s):// file URLs; provide a local file path, file:// URI, or base64:// payload".to_string(),
        });
    }

    let name = parse_optional_string(&params, "name")
        .map(|val| val.trim().to_string())
        .filter(|val| !val.is_empty());

    let segments = vec![MessageSegment::File {
        file,
        name,
        size: None,
    }];
    send_private_segments_with_deadline(state, target_private, segments, &payload).await
}

/// `get_file` -> 受限读取 `.runtime/media` 与 `.runtime/staging_uploads` 内文件元数据及可控体积 Base64
pub(crate) async fn exec_get_file(
    _state: &AppState,
    params: Value,
) -> std::result::Result<Value, AdapterError> {
    let raw_ref = parse_optional_string(&params, "file_id")
        .map(|val| val.trim().to_string())
        .filter(|val| !val.is_empty())
        .or_else(|| {
            parse_optional_string(&params, "file")
                .map(|val| val.trim().to_string())
                .filter(|val| !val.is_empty())
        })
        .ok_or_else(|| AdapterError::InputRejected("file or file_id is required".to_string()))?;

    if is_http_or_https_source(&raw_ref) {
        return Err(AdapterError::UnsupportedAction {
            action: ACTION_GET_FILE.to_string(),
            reason: "get_file does not fetch remote http(s):// URLs; provide a cached file reference under .runtime/media or .runtime/staging_uploads".to_string(),
        });
    }

    let clean_ref = normalize_local_file_ref(&raw_ref);
    if clean_ref.is_empty() || clean_ref.contains("..") || clean_ref.contains('\0') {
        return Err(AdapterError::InputRejected(format!(
            "invalid file reference '{raw_ref}'"
        )));
    }

    let canon_file = resolve_sandboxed_file(clean_ref, &raw_ref).await?;
    let meta = fs::metadata(&canon_file).await.map_err(|err| {
        AdapterError::InputRejected(format!("failed to read file metadata: {err}"))
    })?;
    if !meta.is_file() {
        return Err(AdapterError::InputRejected(
            "target file entry is not a regular file".to_string(),
        ));
    }

    let file_size = meta.len();
    let base64_data = if file_size <= GET_FILE_MAX_BASE64_BYTES {
        let bytes = fs::read(&canon_file).await.map_err(|err| {
            AdapterError::InputRejected(format!("failed to read cached file bytes: {err}"))
        })?;
        Some(BASE64_STANDARD.encode(bytes))
    } else {
        None
    };

    let display = format_display_path(&canon_file);
    let file_name = canon_file
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();

    Ok(json!({
        "file": display,
        "file_name": file_name,
        "file_size": file_size,
        "base64": base64_data,
    }))
}
