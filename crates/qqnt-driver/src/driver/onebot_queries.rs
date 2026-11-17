//! OneBot v11 数据面扩展查询处理器（群列表、群详情、群成员表与冷启动真实群号安全探测）

use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use common_types::{
    infer_qq_max_member_count, AdapterError, GroupInfoSummary, GroupMemberInfo, GroupTargetSpec,
    OcrImageResponse, Result, WindowInfo,
};
use storage::StoredGroupRecord;
use tracing::debug;

use super::group_handlers::{
    handle_list_sessions, handle_open_group, persist_sidebar_members_from_tree,
};
use super::runtime_state::{lookup_group_record, SharedInner};
use super::worker_loop::WorkerContext;
use crate::composer::file_staging::{self, MAX_STAGED_FILE_BYTES, STAGING_UPLOADS_DIR};
use crate::constants::{INTERNAL_SCAN_DEPTH, MEDIA_CACHE_SUBDIR};
use crate::matcher::{self, DrawerInspectRequest};

/// 将内部群元数据存储记录转换为对外 OneBot 群概要信息载体
fn stored_to_summary(record: &StoredGroupRecord) -> GroupInfoSummary {
    GroupInfoSummary {
        group_id: record.effective_group_id(),
        real_group_id: record.real_group_id,
        group_name: record.group_name.clone(),
        member_count: record.member_count.unwrap_or(0),
        max_member_count: infer_qq_max_member_count(record.member_count, record.max_member_count),
    }
}

/// 按群号或群名称两级检索群元数据记录，未提供条件时依次回退当前已附着群聊或配置默认目标群
fn resolve_target_group_record(
    shared: &Arc<SharedInner>,
    group_id: Option<i64>,
    group_name: Option<&str>,
) -> Option<StoredGroupRecord> {
    if let Some(id) = group_id {
        if let Ok(Some(record)) = shared.storage.get_group_by_id(id) {
            return Some(record);
        }
    }
    if let Some(name) = group_name {
        let trimmed = name.trim();
        if !trimmed.is_empty() {
            if let Some(record) = lookup_group_record(&shared.storage, trimmed) {
                return Some(record);
            }
        }
    }
    if group_id.is_none() && group_name.is_none_or(|raw| raw.trim().is_empty()) {
        let active = shared.state.lock().active_group.clone();
        if let Some(active_name) = active {
            if let Some(record) = lookup_group_record(&shared.storage, &active_name) {
                return Some(record);
            }
        }
        let configured = shared.config.target.group_name.trim();
        if !configured.is_empty() {
            return lookup_group_record(&shared.storage, configured);
        }
    }
    None
}

fn find_attached_window_for_group(
    shared: &Arc<SharedInner>,
    group_name: &str,
) -> Option<(WindowInfo, String, Option<u32>)> {
    let guard = shared.state.lock();
    match (&guard.attached_window, &guard.active_group) {
        (Some(win), Some(active)) if matcher::group_title_matches(active, group_name) => {
            Some((win.clone(), active.clone(), guard.detected_member_count))
        }
        _ => None,
    }
}

/// 判定查询接口是否被安全策略允许在未附着时自动打开指定群以采集真实群号或成员清单
fn can_safely_attach_for_query(
    shared: &Arc<SharedInner>,
    group_name: &str,
    known_member_count: Option<i32>,
    no_cache: bool,
) -> bool {
    let configured_target = shared.config.target.group_name.trim();
    if !configured_target.is_empty() && matcher::group_title_matches(group_name, configured_target)
    {
        return true;
    }
    if !no_cache {
        return false;
    }
    let Some(count) = known_member_count.filter(|&val| val > 0) else {
        return false;
    };
    let guard = shared.config.target.member_guard_for_group(group_name);
    let mut relaxed_guard = guard;
    relaxed_guard.require_member_count_verified = false;
    matcher::passes_member_guard(&relaxed_guard, Some(count as u32)).is_ok()
        && shared.state.lock().active_group.is_none()
}

fn build_safe_query_open_spec(shared: &Arc<SharedInner>, group_name: &str) -> GroupTargetSpec {
    shared.config.target.member_guard_for_group(group_name)
}

/// 处理 `get_group_list`：优先返回本地 SQLite 群表缓存，必要时或 `no_cache` 时通过会话列表刷新并顺带探测已附着群真实群号
pub(crate) fn handle_get_group_list(
    ctx: &mut WorkerContext,
    shared: &Arc<SharedInner>,
    no_cache: bool,
) -> Result<Vec<GroupInfoSummary>> {
    if no_cache {
        let _ = shared
            .storage
            .purge_invalid_groups(matcher::is_valid_session_title);
    } else {
        let cached = shared.storage.list_groups()?;
        if !cached.is_empty() {
            return Ok(cached);
        }
    }

    match handle_list_sessions(ctx, shared) {
        Ok(_) => {
            if no_cache {
                let _ = shared
                    .storage
                    .purge_invalid_groups(matcher::is_valid_session_title);
                let active_group = shared.state.lock().active_group.clone();
                if let Some(active_name) = active_group {
                    if let Some(record) = lookup_group_record(&shared.storage, &active_name) {
                        if record.real_group_id.is_none() {
                            try_inspect_drawer_if_attached(ctx, shared, &record, false);
                        }
                    }
                }
            }
            shared.storage.list_groups()
        }
        Err(err) if !no_cache => {
            debug!(error = %err, "被动刷新群列表未发现可用窗口，返回现有本地缓存");
            shared.storage.list_groups()
        }
        Err(err) => Err(err),
    }
}

/// 处理 `get_group_info`：支持群号/群名/默认目标群三级检索，并在缺失真实群号或 `no_cache` 时安全探测群信息抽屉
pub(crate) fn handle_get_group_info(
    ctx: &mut WorkerContext,
    shared: &Arc<SharedInner>,
    group_id: Option<i64>,
    group_name: Option<&str>,
    no_cache: bool,
) -> Result<GroupInfoSummary> {
    let mut record = resolve_target_group_record(shared, group_id, group_name);

    if record.is_none() {
        let _ = handle_list_sessions(ctx, shared);
        record = resolve_target_group_record(shared, group_id, group_name);
    }

    // 冷启动数据库尚为空且仅请求默认受控目标群时，尝试直接打开该受控群以播种元数据
    if record.is_none() && group_id.is_none() {
        let fallback_name = group_name
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| shared.config.target.group_name.trim());
        let configured_target = shared.config.target.group_name.trim();
        if !fallback_name.is_empty()
            && !configured_target.is_empty()
            && matcher::group_title_matches(fallback_name, configured_target)
        {
            if let Err(err) = handle_open_group(ctx, shared, &shared.config.target) {
                debug!(error = %err, group = %fallback_name, "冷启动尝试自动打开受控目标群失败");
            }
            record = resolve_target_group_record(shared, group_id, group_name);
        }
    }

    let Some(found) = record else {
        let label = group_name
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
            .or_else(|| group_id.map(|id| id.to_string()))
            .unwrap_or_else(|| shared.config.target.group_name.clone());
        return Err(AdapterError::GroupNotFound { group: label });
    };

    let needs_drawer = no_cache || found.real_group_id.is_none();
    if needs_drawer {
        if find_attached_window_for_group(shared, &found.group_name).is_some() {
            try_inspect_drawer_if_attached(ctx, shared, &found, no_cache);
        } else if can_safely_attach_for_query(
            shared,
            &found.group_name,
            found.member_count,
            no_cache,
        ) {
            let spec = build_safe_query_open_spec(shared, &found.group_name);
            match handle_open_group(ctx, shared, &spec) {
                Ok(_) if no_cache && found.real_group_id.is_some() => {
                    try_inspect_drawer_if_attached(ctx, shared, &found, true);
                }
                Ok(_) => {}
                Err(err) => {
                    debug!(error = %err, group = %found.group_name, "安全挂载目标群以采集真实群号失败");
                }
            }
        }
    }

    let refreshed = lookup_group_record(&shared.storage, &found.group_name).unwrap_or(found);
    Ok(stored_to_summary(&refreshed))
}

/// 若目标群恰好是目前已附着且在窗口可见的会话，则执行群信息抽屉探测并合并真实群号/容量上限
fn try_inspect_drawer_if_attached(
    ctx: &mut WorkerContext,
    shared: &Arc<SharedInner>,
    record: &StoredGroupRecord,
    force_refresh: bool,
) {
    let Some((mut win, active_title, cached_count)) =
        find_attached_window_for_group(shared, &record.group_name)
    else {
        return;
    };
    if !uia_core::refresh_window_info(&mut win) {
        return;
    }

    let mut safety_spec = shared
        .config
        .target
        .member_guard_for_group(&record.group_name);
    safety_spec.require_member_count_verified = false;
    let effective_count = cached_count.or_else(|| record.member_count.map(|c| c as u32));
    if matcher::passes_member_guard(&safety_spec, effective_count).is_err() {
        return;
    }

    if matcher::ensure_verified_window(&mut ctx.verified_cache, &win, shared.allow_mock()).is_err()
    {
        return;
    }
    if let Some(identity) = ctx.verified_cache.as_ref() {
        if identity.verify_fast().is_err() {
            return;
        }
        let req = DrawerInspectRequest {
            window: &win,
            group_name: &record.group_name,
            active_title: &active_title,
            member_count: effective_count,
            force_refresh,
        };
        if let Err(err) =
            matcher::inspect_group_drawer_if_needed(&ctx.session, identity, &shared.storage, &req)
        {
            debug!(error = %err, group = %record.group_name, "按需刷新群信息抽屉未获取新元数据");
        }
    }
}

/// 处理 `get_group_member_list`：读取数据库成员缓存，并在缺失缓存或 `no_cache` 时从已附着或安全允许的窗口右侧栏采集同步
pub(crate) fn handle_get_group_member_list(
    ctx: &mut WorkerContext,
    shared: &Arc<SharedInner>,
    group_id: Option<i64>,
    group_name: Option<&str>,
    no_cache: bool,
) -> Result<Vec<GroupMemberInfo>> {
    let mut record = resolve_target_group_record(shared, group_id, group_name);
    if record.is_none() {
        let _ = handle_list_sessions(ctx, shared);
        record = resolve_target_group_record(shared, group_id, group_name);
    }

    let Some(found) = record else {
        if let (None, Some(name)) = (group_id, group_name) {
            let trimmed = name.trim();
            if !trimmed.is_empty() {
                let fallback_id = shared.resolve_or_create_group_id(trimmed, None);
                return shared.storage.list_group_members(trimmed, fallback_id);
            }
        }
        return Ok(Vec::new());
    };

    let eff_id = found.effective_group_id();
    if !no_cache {
        let cached = shared
            .storage
            .list_group_members(&found.group_name, eff_id)?;
        if !cached.is_empty() {
            return Ok(cached);
        }
    }

    if let Some((mut win, _, _)) = find_attached_window_for_group(shared, &found.group_name) {
        if uia_core::refresh_window_info(&mut win) {
            if let Ok(identity) =
                matcher::ensure_verified_window(&mut ctx.verified_cache, &win, shared.allow_mock())
            {
                if identity.verify_fast().is_ok() {
                    if let Ok((tree, _guard)) = matcher::wake_and_dump_tree_ext(
                        &ctx.session,
                        win.hwnd,
                        INTERNAL_SCAN_DEPTH,
                        false,
                    ) {
                        persist_sidebar_members_from_tree(shared, &tree, &found.group_name, eff_id);
                    }
                }
            }
        }
    } else if can_safely_attach_for_query(shared, &found.group_name, found.member_count, no_cache) {
        let spec = build_safe_query_open_spec(shared, &found.group_name);
        if let Err(err) = handle_open_group(ctx, shared, &spec) {
            debug!(error = %err, group = %found.group_name, "查询成员列表时尝试安全挂载目标群失败");
        }
    }

    let refreshed = lookup_group_record(&shared.storage, &found.group_name).unwrap_or(found);
    shared
        .storage
        .list_group_members(&refreshed.group_name, refreshed.effective_group_id())
}

/// 在阻塞线程池中解析本地缓存/Base64/data URI 图片字节并执行 Windows 原生 OCR 识别
pub(crate) async fn handle_ocr_image(
    image: &str,
    language: Option<&str>,
) -> Result<OcrImageResponse> {
    let image_owned = image.to_string();
    let lang_owned = language.map(str::to_string);
    tokio::task::spawn_blocking(move || {
        let bytes = resolve_ocr_image_bytes(&image_owned, None, None)?;
        uia_core::recognize_image_bytes(&bytes, lang_owned.as_deref())
    })
    .await
    .map_err(|err| AdapterError::UiaFailure(format!("ocr_image task panicked or aborted: {err}")))?
}

/// 从缓存目录、暂存目录、本地路径或 Base64/data URI 安全加载用于 OCR 的图片字节（严格阻止路径穿越与远程 URL）
pub(crate) fn resolve_ocr_image_bytes(
    image: &str,
    media_dir: Option<&Path>,
    staging_dir: Option<&Path>,
) -> Result<Vec<u8>> {
    let trimmed = image.trim();
    if trimmed.is_empty() {
        return Err(AdapterError::InputRejected(
            "ocr_image `image` argument cannot be empty".to_string(),
        ));
    }
    if file_staging::is_http_or_https_url(trimmed) {
        return Err(AdapterError::UnsupportedAction {
            action: "ocr_image".to_string(),
            reason: "external http(s) URLs are not fetched by ocr_image; pass local path, file://, base64://, data: URI, or cached filename".to_string(),
        });
    }
    if file_staging::is_encoded_media_uri(trimmed) {
        let (bytes, _) = file_staging::decode_base64_or_data_uri(trimmed, MAX_STAGED_FILE_BYTES)?;
        return Ok(bytes);
    }
    if file_staging::is_unc_or_network_path(trimmed) {
        return Err(AdapterError::InputRejected(
            "UNC or remote network share paths are forbidden in offline mode".to_string(),
        ));
    }

    let candidate = file_staging::resolve_local_or_file_uri(trimmed)?;
    if has_parent_traversal(trimmed, &candidate) {
        return Err(AdapterError::InputRejected(format!(
            "path traversal `..` is rejected by ocr_image: `{trimmed}`"
        )));
    }

    let media_root = media_dir.unwrap_or_else(|| Path::new(MEDIA_CACHE_SUBDIR));
    let staging_root = staging_dir.unwrap_or_else(|| Path::new(STAGING_UPLOADS_DIR));
    let resolved =
        find_ocr_candidate_file(&candidate, media_root, staging_root).ok_or_else(|| {
            AdapterError::InputRejected(format!(
                "ocr_image image file not found: {}",
                candidate.display()
            ))
        })?;

    let meta = fs::metadata(&resolved).map_err(|err| {
        AdapterError::InputRejected(format!(
            "failed to read ocr_image file metadata {}: {err}",
            resolved.display()
        ))
    })?;
    if meta.len() == 0 {
        return Err(AdapterError::InputRejected(format!(
            "ocr_image file is empty: {}",
            resolved.display()
        )));
    }
    if meta.len() > MAX_STAGED_FILE_BYTES as u64 {
        return Err(AdapterError::InputRejected(format!(
            "ocr_image file size {} bytes exceeds limit {} bytes",
            meta.len(),
            MAX_STAGED_FILE_BYTES
        )));
    }

    let bytes = fs::read(&resolved).map_err(|err| {
        AdapterError::InputRejected(format!(
            "failed to read ocr_image file {}: {err}",
            resolved.display()
        ))
    })?;
    if bytes.len() > MAX_STAGED_FILE_BYTES {
        return Err(AdapterError::InputRejected(format!(
            "ocr_image file size {} bytes exceeds limit {} bytes",
            bytes.len(),
            MAX_STAGED_FILE_BYTES
        )));
    }
    Ok(bytes)
}

fn has_parent_traversal(raw: &str, candidate: &Path) -> bool {
    raw.split(['/', '\\']).any(|seg| seg.trim() == "..")
        || candidate
            .to_string_lossy()
            .split(['/', '\\'])
            .any(|seg| seg.trim() == "..")
        || candidate
            .components()
            .any(|comp| matches!(comp, Component::ParentDir))
}

fn find_ocr_candidate_file(
    candidate: &Path,
    media_root: &Path,
    staging_root: &Path,
) -> Option<PathBuf> {
    let mut comps = candidate.components();
    let is_bare_filename = matches!(comps.next(), Some(Component::Normal(_)))
        && comps.next().is_none()
        && !candidate.to_string_lossy().contains(['/', '\\']);

    if is_bare_filename {
        let in_media = media_root.join(candidate);
        if in_media.is_file() {
            return Some(in_media);
        }
        let in_staging = staging_root.join(candidate);
        if in_staging.is_file() {
            return Some(in_staging);
        }
        if let Ok(read_dir) = fs::read_dir(staging_root) {
            for entry in read_dir.flatten() {
                let sub_path = entry.path();
                if sub_path.is_dir() {
                    let nested = sub_path.join(candidate);
                    if nested.is_file() {
                        return Some(nested);
                    }
                }
            }
        }
    }

    if candidate.is_file() {
        return Some(candidate.to_path_buf());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;

    fn build_minimal_white_bmp_bytes() -> Vec<u8> {
        let mut out = Vec::with_capacity(822);
        out.extend_from_slice(b"BM");
        out.extend_from_slice(&822u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&54u32.to_le_bytes());
        out.extend_from_slice(&40u32.to_le_bytes());
        out.extend_from_slice(&16i32.to_le_bytes());
        out.extend_from_slice(&16i32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&24u16.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&768u32.to_le_bytes());
        out.extend_from_slice(&0i32.to_le_bytes());
        out.extend_from_slice(&0i32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&[255u8; 768]);
        out
    }

    #[test]
    fn ocr_image_resolves_media_staging_encoded_and_rejects_traversal_and_http() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("qq_ocr_test_{nanos}"));
        let media_dir = root.join("media");
        let staging_dir = root.join("staging");
        let nested_staging = staging_dir.join("abcd1234");
        fs::create_dir_all(&media_dir).expect("mkdir media");
        fs::create_dir_all(&nested_staging).expect("mkdir staging");

        let bmp = build_minimal_white_bmp_bytes();
        fs::write(media_dir.join("cached_img.bmp"), &bmp).expect("write media");
        fs::write(nested_staging.join("staged_img.bmp"), &bmp).expect("write staging");

        assert_eq!(
            resolve_ocr_image_bytes("cached_img.bmp", Some(&media_dir), Some(&staging_dir))
                .expect("resolve media"),
            bmp
        );
        assert_eq!(
            resolve_ocr_image_bytes("staged_img.bmp", Some(&media_dir), Some(&staging_dir))
                .expect("resolve staging"),
            bmp
        );
        let file_uri = format!(
            "file:///{}",
            media_dir
                .join("cached_img.bmp")
                .to_string_lossy()
                .replace('\\', "/")
        );
        assert_eq!(
            resolve_ocr_image_bytes(&file_uri, Some(&media_dir), Some(&staging_dir))
                .expect("resolve file uri"),
            bmp
        );
        let b64 = format!("base64://{}", STANDARD.encode(&bmp));
        assert_eq!(
            resolve_ocr_image_bytes(&b64, Some(&media_dir), Some(&staging_dir))
                .expect("resolve base64"),
            bmp
        );
        let data_uri = format!("data:image/bmp;base64,{}", STANDARD.encode(&bmp));
        assert_eq!(
            resolve_ocr_image_bytes(&data_uri, Some(&media_dir), Some(&staging_dir))
                .expect("resolve data uri"),
            bmp
        );

        for traversal in [
            "../secret.png",
            "foo/../../secret.png",
            "file:///C:/foo/%2e%2e/secret.png",
        ] {
            let err = resolve_ocr_image_bytes(traversal, Some(&media_dir), Some(&staging_dir))
                .expect_err("traversal must be rejected");
            assert!(matches!(err, AdapterError::InputRejected(_)));
        }

        for remote in ["http://example.com/a.png", "HTTPS://example.com/a.png"] {
            let err = resolve_ocr_image_bytes(remote, Some(&media_dir), Some(&staging_dir))
                .expect_err("http(s) must be rejected");
            assert!(matches!(err, AdapterError::UnsupportedAction { .. }));
        }

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio rt");
        let ocr_res = rt
            .block_on(handle_ocr_image(&b64, None))
            .expect("ocr on valid bmp should succeed cleanly");
        assert!(ocr_res.texts.is_empty());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn ocr_image_blocks_unc_network_paths_and_accepts_uppercase_base64() {
        for blocked in [
            r"\\fileserver\share\a.bmp",
            r"\\?\UNC\fileserver\share\a.bmp",
            "//fileserver/share/a.bmp",
            "file://fileserver/share/a.bmp",
        ] {
            let err =
                resolve_ocr_image_bytes(blocked, None, None).expect_err("UNC must be rejected");
            assert!(
                matches!(
                    err,
                    AdapterError::InputRejected(ref m)
                        if m.contains("UNC or remote network share paths are forbidden in offline mode")
                ),
                "unexpected error for {blocked}: {err:?}"
            );
        }

        let bmp = build_minimal_white_bmp_bytes();
        let encoded = STANDARD.encode(&bmp);
        assert_eq!(
            resolve_ocr_image_bytes(&format!("BASE64://{encoded}"), None, None)
                .expect("resolve uppercase BASE64://"),
            bmp
        );
        assert_eq!(
            resolve_ocr_image_bytes(&format!("DATA:image/bmp;BASE64,{encoded}"), None, None)
                .expect("resolve uppercase DATA URI"),
            bmp
        );
    }
}
