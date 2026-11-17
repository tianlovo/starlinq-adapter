//! 出站文件与视频的本地暂存、Base64 解码、魔数识别与缓存淘汰工具

use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use base64::Engine;
use common_types::{AdapterError, ChatTargetKind, Result};
use sha2::{Digest, Sha256};

use crate::constants::{
    IMAGE_SRC_BASE64_PREFIX, IMAGE_SRC_DATA_URL_PREFIX, IMAGE_SRC_HTTPS_PREFIX,
    IMAGE_SRC_HTTP_PREFIX, MEDIA_URL_FILE_SCHEME,
};

/// 单个经 Base64 或 data URI 解码暂存文件的最大允许字节数（16 MiB）
pub(crate) const MAX_STAGED_FILE_BYTES: usize = 16 * 1024 * 1024;

/// 出站临时暂存文件目录（相对当前工作目录）
pub(crate) const STAGING_UPLOADS_DIR: &str = ".runtime/staging_uploads";

/// 暂存目录允许保留的最大文件数量（超出时按修改时间淘汰最旧文件）
pub(crate) const STAGING_MAX_FILES: usize = 64;

/// 经本地解析、Base64 解码或重命名复制后可直接用于 `CF_HDROP` 投递的暂存文件描述
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StagedFile {
    /// 磁盘上真实存在的文件路径
    pub path: PathBuf,
    /// 发送气泡及审计日志中展示的文件名
    pub display_name: String,
    /// 文件实际字节大小
    pub size: u64,
}

/// 将文件或视频源解析为本地磁盘路径，并在提供自定义文件名或 Base64 载荷时落盘到暂存目录
pub(crate) fn stage_outbound_file(
    raw_source: &str,
    requested_name: Option<&str>,
    target_kind: ChatTargetKind,
    action_name: &str,
    default_ext: &str,
    staging_root: Option<&Path>,
) -> Result<StagedFile> {
    let trimmed = raw_source.trim();
    if trimmed.is_empty() {
        return Err(AdapterError::InputRejected(format!(
            "{action_name} file/video source is empty"
        )));
    }
    if is_http_or_https_url(trimmed) {
        return Err(AdapterError::UnsupportedAction {
            action: action_name.to_string(),
            reason: format!(
                "{target_kind:?}: external http(s) media URL is rejected in strict desktop UIA mode; provide local path, file://, base64://, or data: URI"
            ),
        });
    }

    let root = staging_root.unwrap_or_else(|| Path::new(STAGING_UPLOADS_DIR));
    let sanitized_name = match requested_name.map(str::trim).filter(|s| !s.is_empty()) {
        Some(raw) => Some(sanitize_stage_filename(raw)?),
        None => None,
    };

    if is_encoded_media_uri(trimmed) {
        return stage_encoded_payload(trimmed, sanitized_name.as_deref(), default_ext, root);
    }

    let source_path = resolve_local_or_file_uri(trimmed)?;
    stage_existing_local_file(&source_path, sanitized_name.as_deref(), root)
}

/// 生成可用于测试和校验的最小合法 ISO BMFF MP4 字节序列（包含 `ftyp`、`moov`/`mvhd` 与 `mdat` 盒）
#[cfg(test)]
pub(crate) fn build_minimal_valid_mp4_bytes() -> Vec<u8> {
    let mut out = Vec::with_capacity(156);
    // 写入 ftyp 盒（24 字节）
    out.extend_from_slice(&24u32.to_be_bytes());
    out.extend_from_slice(b"ftyp");
    out.extend_from_slice(b"isom");
    out.extend_from_slice(&0x0000_0200u32.to_be_bytes());
    out.extend_from_slice(b"isom");
    out.extend_from_slice(b"mp41");

    // 写入 moov 与内嵌版本 0 的 mvhd 盒（8 + 108 = 116 字节）
    out.extend_from_slice(&116u32.to_be_bytes());
    out.extend_from_slice(b"moov");
    out.extend_from_slice(&108u32.to_be_bytes());
    out.extend_from_slice(b"mvhd");
    out.extend_from_slice(&[0u8; 4]);
    out.extend_from_slice(&0u32.to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes());
    out.extend_from_slice(&1000u32.to_be_bytes());
    out.extend_from_slice(&100u32.to_be_bytes());
    out.extend_from_slice(&0x0001_0000u32.to_be_bytes());
    out.extend_from_slice(&0x0100u16.to_be_bytes());
    out.extend_from_slice(&[0u8; 10]);
    for word in [
        0x0001_0000u32,
        0,
        0,
        0,
        0x0001_0000u32,
        0,
        0,
        0,
        0x4000_0000u32,
    ] {
        out.extend_from_slice(&word.to_be_bytes());
    }
    out.extend_from_slice(&[0u8; 24]);
    out.extend_from_slice(&1u32.to_be_bytes());

    // 写入 mdat 盒（16 字节）
    out.extend_from_slice(&16u32.to_be_bytes());
    out.extend_from_slice(b"mdat");
    out.extend_from_slice(&[0u8; 8]);
    out
}

/// 判断媒体源是否为大小写无关的 `http://` 或 `https://` 远程地址
pub(crate) fn is_http_or_https_url(raw: &str) -> bool {
    let trimmed = raw.trim();
    let lower_prefix: String = trimmed
        .chars()
        .take(8)
        .collect::<String>()
        .to_ascii_lowercase();
    lower_prefix.starts_with(IMAGE_SRC_HTTP_PREFIX)
        || lower_prefix.starts_with(IMAGE_SRC_HTTPS_PREFIX)
}

/// 清理请求文件名中的 `..`、路径分隔符及 Windows 非法字符，保留安全基名
pub(crate) fn sanitize_stage_filename(raw_name: &str) -> Result<String> {
    let last_part = raw_name
        .split(['/', '\\'])
        .rfind(|part| {
            let t = part.trim();
            !t.is_empty() && t != "." && t != ".."
        })
        .unwrap_or("")
        .trim();

    let without_dotdot = last_part.replace("..", "");
    let mut cleaned = String::with_capacity(without_dotdot.len());
    for ch in without_dotdot.chars() {
        if ch.is_control() || matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') {
            continue;
        }
        cleaned.push(ch);
    }
    let trimmed = cleaned.trim().trim_end_matches('.');
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        return Err(AdapterError::InputRejected(format!(
            "invalid requested filename `{raw_name}` after sanitization"
        )));
    }
    Ok(trimmed.to_string())
}

/// 根据文件魔数识别常见图片与 MP4 视频扩展名（无法匹配时返回 `None`）
pub(crate) fn detect_magic_extension(bytes: &[u8]) -> Option<&'static str> {
    if bytes.len() >= 8 && bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        return Some("png");
    }
    if bytes.len() >= 3 && bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some("jpg");
    }
    if bytes.len() >= 6 && (bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a")) {
        return Some("gif");
    }
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some("webp");
    }
    if bytes.len() >= 2 && bytes.starts_with(b"BM") {
        return Some("bmp");
    }
    if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" {
        return Some("mp4");
    }
    None
}

/// 判断字符串是否以大小写无关的 `base64://` 或 `data:` 编码协议前缀开头
pub(crate) fn is_encoded_media_uri(raw: &str) -> bool {
    let trimmed = raw.trim();
    strip_prefix_ascii_ci(trimmed, IMAGE_SRC_BASE64_PREFIX).is_some()
        || strip_prefix_ascii_ci(trimmed, IMAGE_SRC_DATA_URL_PREFIX).is_some()
}

/// 判断路径或 URI 是否指向 UNC 网络共享或远程主机路径（放行 `\\?\C:\` 本地驱动器长路径与 `file://localhost/`）
pub(crate) fn is_unc_or_network_path(raw: &str) -> bool {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return false;
    }
    if trimmed.starts_with(r"\\") {
        // 仅放行 `\\?\<Drive>:\...` 本地驱动器长路径，其余 `\\server\share` 与 `\\?\UNC\...` 均视为网络路径
        if let Some(after_prefix) = trimmed.strip_prefix(r"\\?\") {
            let bytes = after_prefix.as_bytes();
            let is_local_drive = bytes.len() >= 3
                && bytes[0].is_ascii_alphabetic()
                && bytes[1] == b':'
                && (bytes[2] == b'\\' || bytes[2] == b'/');
            return !is_local_drive;
        }
        return true;
    }
    if trimmed.starts_with("//") {
        return true;
    }
    if let Some(after_scheme) = strip_prefix_ascii_ci(trimmed, MEDIA_URL_FILE_SCHEME) {
        if after_scheme.starts_with("//") || after_scheme.starts_with(r"\\") {
            return true;
        }
        if let Some(after_slash) = after_scheme.strip_prefix('/') {
            return is_unc_or_network_path(after_slash);
        }
        if after_scheme.starts_with('\\') {
            return is_unc_or_network_path(after_scheme.trim_start_matches('\\'));
        }
        if let Some((host, rest)) = after_scheme.split_once(['/', '\\']) {
            let host_trimmed = host.trim();
            if !host_trimmed.is_empty() && !host_trimmed.eq_ignore_ascii_case("localhost") {
                return true;
            }
            return rest.starts_with('/') || rest.starts_with('\\') || is_unc_or_network_path(rest);
        }
        let host_only = after_scheme.trim();
        return !host_only.is_empty() && !host_only.eq_ignore_ascii_case("localhost");
    }
    false
}

/// 解码 `base64://` 或 `data:...;base64,...` 载荷并严格限制在 `max_bytes` 以内
pub(crate) fn decode_base64_or_data_uri(
    raw: &str,
    max_bytes: usize,
) -> Result<(Vec<u8>, Option<String>)> {
    let trimmed = raw.trim();
    let (mime_ext, b64_payload) =
        if let Some(rest) = strip_prefix_ascii_ci(trimmed, IMAGE_SRC_BASE64_PREFIX) {
            (None, rest)
        } else if let Some(rest) = strip_prefix_ascii_ci(trimmed, IMAGE_SRC_DATA_URL_PREFIX) {
            let Some((meta, data_part)) = rest.split_once(',') else {
                return Err(AdapterError::InputRejected(
                    "invalid data URI: missing comma separator".to_string(),
                ));
            };
            let has_base64 = meta
                .split(';')
                .any(|seg| seg.trim().eq_ignore_ascii_case("base64"));
            if !has_base64 {
                return Err(AdapterError::InputRejected(
                    "invalid data URI: expected `;base64,` encoding".to_string(),
                ));
            }
            let mime = meta.split(';').next().map(str::trim).unwrap_or("");
            (extension_from_mime(mime), data_part)
        } else {
            return Err(AdapterError::InputRejected(
                "unsupported encoded source prefix".to_string(),
            ));
        };

    let compact: String = b64_payload.chars().filter(|c| !c.is_whitespace()).collect();
    if compact.is_empty() {
        return Err(AdapterError::InputRejected(
            "base64 media payload is empty".to_string(),
        ));
    }
    let max_encoded_len = max_bytes.saturating_mul(4).div_ceil(3).saturating_add(4);
    if compact.len() > max_encoded_len {
        return Err(AdapterError::InputRejected(format!(
            "base64 media payload exceeds maximum allowed size ({max_bytes} bytes)"
        )));
    }

    let decoded = STANDARD
        .decode(&compact)
        .or_else(|_| STANDARD_NO_PAD.decode(&compact))
        .map_err(|err| {
            AdapterError::InputRejected(format!("invalid base64 media payload: {err}"))
        })?;
    if decoded.is_empty() {
        return Err(AdapterError::InputRejected(
            "decoded base64 media payload is empty".to_string(),
        ));
    }
    if decoded.len() > max_bytes {
        return Err(AdapterError::InputRejected(format!(
            "decoded media size {} bytes exceeds limit {} bytes",
            decoded.len(),
            max_bytes
        )));
    }
    Ok((decoded, mime_ext))
}

fn extension_from_mime(mime: &str) -> Option<String> {
    let lower = mime.to_ascii_lowercase();
    let ext = match lower.as_str() {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/bmp" | "image/x-ms-bmp" => "bmp",
        "video/mp4" => "mp4",
        _ => return None,
    };
    Some(ext.to_string())
}

fn stage_encoded_payload(
    raw: &str,
    sanitized_name: Option<&str>,
    default_ext: &str,
    staging_root: &Path,
) -> Result<StagedFile> {
    let (bytes, mime_ext) = decode_base64_or_data_uri(raw, MAX_STAGED_FILE_BYTES)?;
    let resolved_ext = detect_magic_extension(&bytes)
        .map(str::to_string)
        .or(mime_ext)
        .unwrap_or_else(|| {
            let trimmed = default_ext.trim().trim_start_matches('.');
            if trimmed.is_empty() {
                "bin".to_string()
            } else {
                trimmed.to_string()
            }
        });
    let hash_prefix = short_sha256_hex(&bytes);
    fs::create_dir_all(staging_root).map_err(|err| {
        AdapterError::InputRejected(format!(
            "failed to create staging directory {}: {err}",
            staging_root.display()
        ))
    })?;

    let (target_path, display_name) = if let Some(name) = sanitized_name {
        let sub_dir = staging_root.join(&hash_prefix);
        fs::create_dir_all(&sub_dir).map_err(|err| {
            AdapterError::InputRejected(format!(
                "failed to create staging subdir {}: {err}",
                sub_dir.display()
            ))
        })?;
        (sub_dir.join(name), name.to_string())
    } else {
        let filename = format!("{hash_prefix}.{resolved_ext}");
        (staging_root.join(&filename), filename)
    };

    fs::write(&target_path, &bytes).map_err(|err| {
        AdapterError::InputRejected(format!(
            "failed to write staged file {}: {err}",
            target_path.display()
        ))
    })?;
    evict_oldest_staging_files(staging_root, STAGING_MAX_FILES);

    Ok(StagedFile {
        path: target_path,
        display_name,
        size: bytes.len() as u64,
    })
}

fn stage_existing_local_file(
    source_path: &Path,
    sanitized_name: Option<&str>,
    staging_root: &Path,
) -> Result<StagedFile> {
    if !source_path.exists() || !source_path.is_file() {
        return Err(AdapterError::InputRejected(format!(
            "outbound file/video path does not exist or is not a file: {}",
            source_path.display()
        )));
    }
    let meta = fs::metadata(source_path).map_err(|err| {
        AdapterError::InputRejected(format!(
            "failed to read file metadata {}: {err}",
            source_path.display()
        ))
    })?;
    let size = meta.len();
    let source_name = source_path
        .file_name()
        .and_then(|s| s.to_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("attachment")
        .to_string();

    if let Some(requested) = sanitized_name {
        if requested != source_name {
            let mut key_bytes = source_path.to_string_lossy().as_bytes().to_vec();
            key_bytes.push(0);
            key_bytes.extend_from_slice(requested.as_bytes());
            let hash_prefix = short_sha256_hex(&key_bytes);
            let sub_dir = staging_root.join(&hash_prefix);
            fs::create_dir_all(&sub_dir).map_err(|err| {
                AdapterError::InputRejected(format!(
                    "failed to create staging subdir {}: {err}",
                    sub_dir.display()
                ))
            })?;
            let target_path = sub_dir.join(requested);
            fs::copy(source_path, &target_path).map_err(|err| {
                AdapterError::InputRejected(format!(
                    "failed to copy {} to staged path {}: {err}",
                    source_path.display(),
                    target_path.display()
                ))
            })?;
            evict_oldest_staging_files(staging_root, STAGING_MAX_FILES);
            return Ok(StagedFile {
                path: target_path,
                display_name: requested.to_string(),
                size,
            });
        }
    }

    Ok(StagedFile {
        path: source_path.to_path_buf(),
        display_name: source_name,
        size,
    })
}

pub(crate) fn resolve_local_or_file_uri(raw: &str) -> Result<PathBuf> {
    if is_unc_or_network_path(raw) {
        return Err(AdapterError::InputRejected(
            "UNC or remote network share paths are forbidden in offline mode".to_string(),
        ));
    }
    if let Some(rest) = strip_prefix_ascii_ci(raw, MEDIA_URL_FILE_SCHEME) {
        let mut without_host = if let Some(after_local) = strip_prefix_ascii_ci(rest, "localhost/")
            .or_else(|| strip_prefix_ascii_ci(rest, r"localhost\"))
        {
            format!("/{after_local}")
        } else if let Some(after_slash) = rest.strip_prefix('/') {
            format!("/{after_slash}")
        } else {
            return Err(AdapterError::InputRejected(
                "UNC or remote network share paths are forbidden in offline mode".to_string(),
            ));
        };
        if without_host.starts_with('/')
            && without_host.len() >= 3
            && without_host.as_bytes()[2] == b':'
        {
            without_host.remove(0);
        }
        let decoded = percent_decode_utf8(&without_host);
        if is_unc_or_network_path(&decoded) {
            return Err(AdapterError::InputRejected(
                "UNC or remote network share paths are forbidden in offline mode".to_string(),
            ));
        }
        return Ok(PathBuf::from(decoded.replace('/', "\\")));
    }
    Ok(PathBuf::from(raw))
}

fn strip_prefix_ascii_ci<'a>(input: &'a str, prefix: &str) -> Option<&'a str> {
    let head = input.get(..prefix.len())?;
    if head.eq_ignore_ascii_case(prefix) {
        input.get(prefix.len()..)
    } else {
        None
    }
}

fn percent_decode_utf8(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut idx = 0usize;
    while idx < bytes.len() {
        if bytes[idx] == b'%' && idx + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex_nibble(bytes[idx + 1]), hex_nibble(bytes[idx + 2])) {
                out.push((h << 4) | l);
                idx += 3;
                continue;
            }
        }
        out.push(bytes[idx]);
        idx += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn short_sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(16);
    for byte in &digest[..8] {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn evict_oldest_staging_files(staging_root: &Path, max_files: usize) {
    let mut entries: Vec<(u128, PathBuf, bool)> = Vec::new();
    let Ok(read_dir) = fs::read_dir(staging_root) else {
        return;
    };
    for entry in read_dir.flatten() {
        let path = entry.path();
        if path.is_file() {
            let ts = file_mtime_nanos(&path);
            entries.push((ts, path, false));
        } else if path.is_dir() {
            if let Ok(sub_entries) = fs::read_dir(&path) {
                for sub in sub_entries.flatten() {
                    let sub_path = sub.path();
                    if sub_path.is_file() {
                        let ts = file_mtime_nanos(&sub_path);
                        entries.push((ts, sub_path, true));
                    }
                }
            }
        }
    }
    if entries.len() <= max_files {
        return;
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    let to_remove = entries.len() - max_files;
    for (_, path, has_parent_subdir) in entries.into_iter().take(to_remove) {
        let _ = fs::remove_file(&path);
        if has_parent_subdir {
            if let Some(parent) = path.parent() {
                let _ = fs::remove_dir(parent);
            }
        }
    }
}

fn file_mtime_nanos(path: &Path) -> u128 {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn make_temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("qq_stage_{tag}_{nanos}"));
        let _ = fs::create_dir_all(&dir);
        dir
    }

    #[test]
    fn rejects_http_and_https_sources() {
        for url in ["http://example.com/a.mp4", "HTTPS://example.com/b.txt"] {
            let err = stage_outbound_file(
                url,
                None,
                ChatTargetKind::Group,
                "send_group_msg",
                "mp4",
                None,
            )
            .expect_err("http(s) must be rejected");
            assert!(matches!(err, AdapterError::UnsupportedAction { .. }));
        }
    }

    #[test]
    fn stages_base64_mp4_and_data_png_with_magic_extension() {
        let root = make_temp_dir("b64");
        let mp4 = build_minimal_valid_mp4_bytes();
        let encoded_mp4 = format!("base64://{}", STANDARD.encode(&mp4));
        let staged_mp4 = stage_outbound_file(
            &encoded_mp4,
            None,
            ChatTargetKind::Group,
            "send_group_msg",
            "bin",
            Some(&root),
        )
        .expect("stage mp4");
        assert!(staged_mp4.path.is_file());
        assert_eq!(
            staged_mp4.path.extension().and_then(|s| s.to_str()),
            Some("mp4")
        );
        assert_eq!(staged_mp4.size, mp4.len() as u64);

        let png = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 1, 2, 3, 4];
        let data_png = format!("data:image/png;base64,{}", STANDARD.encode(png));
        let staged_png = stage_outbound_file(
            &data_png,
            Some("../../evil:clip?.png"),
            ChatTargetKind::Private,
            "send_private_msg",
            "bin",
            Some(&root),
        )
        .expect("stage renamed png");
        assert!(staged_png.path.is_file());
        assert_eq!(staged_png.display_name, "evilclip.png");
        assert_eq!(
            staged_png.path.file_name().and_then(|s| s.to_str()),
            Some("evilclip.png")
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn renames_existing_local_file_into_staging_and_evicts_over_64() {
        let root = make_temp_dir("evict");
        let src_dir = make_temp_dir("src");
        let src_file = src_dir.join("original_name.txt");
        fs::write(&src_file, b"hello staged").expect("write src");

        let renamed = stage_outbound_file(
            &format!("file:///{}", src_file.to_string_lossy().replace('\\', "/")),
            Some("requested_report.txt"),
            ChatTargetKind::Group,
            "send_group_msg",
            "bin",
            Some(&root),
        )
        .expect("rename staged");
        assert!(renamed.path.starts_with(&root));
        assert_eq!(renamed.display_name, "requested_report.txt");
        assert_eq!(renamed.size, 12);

        for i in 0..70u8 {
            let p = root.join(format!("old_{i:02}.bin"));
            fs::write(&p, [i; 8]).expect("write bulk");
        }
        evict_oldest_staging_files(&root, STAGING_MAX_FILES);
        let remaining = fs::read_dir(&root)
            .expect("read root")
            .flatten()
            .filter(|e| e.path().is_file())
            .count();
        assert!(remaining <= STAGING_MAX_FILES);
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&src_dir);
    }

    #[test]
    fn rejects_base64_payload_over_16_mib() {
        let root = make_temp_dir("limit");
        let oversized = vec![b'A'; (MAX_STAGED_FILE_BYTES * 4 / 3) + 64];
        let src = format!("base64://{}", String::from_utf8(oversized).expect("ascii"));
        let err = stage_outbound_file(
            &src,
            None,
            ChatTargetKind::Group,
            "send_group_msg",
            "bin",
            Some(&root),
        )
        .expect_err("oversized must fail");
        assert!(matches!(err, AdapterError::InputRejected(_)));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn blocks_unc_network_paths_and_accepts_case_insensitive_base64_and_data_uri() {
        // 识别各类 UNC 与远端 file:// 网络路径，仅放行本地盘符长路径与 localhost
        for blocked in [
            r"\\fileserver\share\a.png",
            r"\\?\UNC\fileserver\share\a.png",
            "//fileserver/share/a.png",
            "file://fileserver/share/a.png",
            "FILE://192.168.1.50/share/a.png",
        ] {
            assert!(is_unc_or_network_path(blocked), "must block {blocked}");
            let err = resolve_local_or_file_uri(blocked).expect_err("must reject UNC");
            assert!(
                matches!(
                    err,
                    AdapterError::InputRejected(ref m)
                        if m.contains("UNC or remote network share paths are forbidden in offline mode")
                ),
                "unexpected error for {blocked}: {err:?}"
            );
        }
        for allowed in [
            r"\\?\C:\dir\a.png",
            "file:///C:/dir/a.png",
            "file://localhost/C:/dir/a.png",
        ] {
            assert!(!is_unc_or_network_path(allowed), "must allow {allowed}");
        }

        // 大写 BASE64:// 与 DATA:...;BASE64, 均应正确离线解码落盘
        let root = make_temp_dir("ci_b64");
        let png_bytes = b"\x89PNG\r\n\x1a\ncase-insensitive-payload";
        let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, png_bytes);
        let upper_b64 = format!("BASE64://{b64}");
        let staged_b64 = stage_outbound_file(
            &upper_b64,
            None,
            ChatTargetKind::Group,
            "send_group_msg",
            "image",
            Some(&root),
        )
        .expect("stage uppercase BASE64://");
        assert_eq!(
            fs::read(&staged_b64.path).expect("read staged uppercase"),
            png_bytes
        );

        let upper_data = format!("DATA:image/png;BASE64,{b64}");
        let staged_data = stage_outbound_file(
            &upper_data,
            None,
            ChatTargetKind::Group,
            "send_group_msg",
            "image",
            Some(&root),
        )
        .expect("stage uppercase DATA URI");
        assert_eq!(
            fs::read(&staged_data.path).expect("read staged data"),
            png_bytes
        );
        let _ = fs::remove_dir_all(&root);
    }
}
