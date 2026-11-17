//! QQNT 群信息真实群号（5-12 位数字）与群人数上限提取器，采用责任链与策略模式串联“更多”设置抽屉与“资料卡”弹窗双通道并配合 10 分钟 SQLite 冷却缓存

use std::collections::HashSet;
use std::ffi::c_void;
use std::thread;
use std::time::Duration;

use chrono::Utc;
use common_types::constants::{
    CHROME_RENDER_WIDGET_CLASS, MOCK_QQ_MAIN_CLASS, QQ_PROCESS_NAMES, UI_LABEL_CLOSE_BUTTON,
    UI_LABEL_COPY_BUTTON, UI_LABEL_MORE_BUTTON, UI_LABEL_SESSION_LIST,
};
use common_types::{Result, UiNodeSnapshot, WindowInfo};
use storage::{Storage, UpsertGroupInput};
use uia_core::constants::{CONTROL_TYPE_BUTTON, CONTROL_TYPE_GROUP, CONTROL_TYPE_TEXT};
use uia_core::{
    find_child_window_by_class, find_pids_by_names, invoke_element_background_verified, win32,
    CollectedNode, ForegroundRestoreGuard, UiaSession, VerifiedWindowIdentity,
};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::VK_ESCAPE;
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_KEYDOWN, WM_KEYUP};

use super::header_matcher::group_title_matches;
use super::profile_card_probe;
use crate::constants::{
    AUDIT_CANDIDATE_SAMPLE_LIMIT, DRAWER_CAPACITY_RATIO_REGEX, DRAWER_CLOSE_BUTTON_KEYWORDS,
    DRAWER_CLOSE_WAIT_MS, DRAWER_GROUP_ID_LABELED_REGEX, DRAWER_GROUP_ID_STANDALONE_REGEX,
    DRAWER_GROUP_ID_TOKEN_REGEX, DRAWER_GROUP_NUMBER_LABELS, DRAWER_PANEL_MIN_REL_X,
    DRAWER_POLL_ATTEMPTS, DRAWER_POLL_INTERVAL_MS, DRAWER_RECHECK_COOLDOWN_MS,
    HEADER_MORE_MIN_REL_X, INTERNAL_SCAN_DEPTH, LEVEL_BADGE_REGEX, MIN_VALID_REAL_GROUP_ID,
    SCAN_KEYDOWN_ESCAPE_LPARAM, SCAN_KEYUP_ESCAPE_LPARAM,
};
use crate::parser;
use crate::profile;

/// 触发群信息抽屉真实群号探测的输入参数上下文
#[derive(Debug, Clone)]
pub struct DrawerInspectRequest<'a> {
    pub window: &'a WindowInfo,
    pub group_name: &'a str,
    pub active_title: &'a str,
    pub member_count: Option<u32>,
    pub force_refresh: bool,
}

/// 群信息抽屉探测或 SQLite 缓存命中后的结构化输出
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DrawerInspectionResult {
    pub real_group_id: Option<i64>,
    pub max_member_count: Option<i32>,
    pub cache_hit: bool,
}

/// 从抽屉或资料卡候选节点切片中提取出的群元数据及审计文本采样
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct ExtractedDrawerMetadata {
    pub(super) real_group_id: Option<i64>,
    pub(super) max_member_count: Option<i32>,
    pub(super) candidate_samples: Vec<String>,
}

fn now_utc_ms() -> u64 {
    Utc::now().timestamp_millis().max(0) as u64
}

/// 清理无障碍文本中的零宽空白符与零宽 BOM 字符
pub(super) fn clean_node_text(raw: &str) -> String {
    raw.replace(['\u{200b}', '\u{feff}'], "").trim().to_string()
}

fn collect_snapshot_texts(snapshot: &UiNodeSnapshot) -> Vec<String> {
    let mut texts = Vec::new();
    let name = clean_node_text(&snapshot.name);
    if !name.is_empty() {
        texts.push(name);
    }
    if let Some(value_raw) = snapshot.value.as_deref() {
        let value = clean_node_text(value_raw);
        if !value.is_empty() && texts.first() != Some(&value) {
            texts.push(value);
        }
    }
    texts
}

fn collect_excluded_drawer_rects(snapshots: &[&UiNodeSnapshot]) -> Vec<[i32; 4]> {
    snapshots
        .iter()
        .copied()
        .filter(|snapshot| {
            !snapshot.is_offscreen
                // 右侧设置抽屉会直接滑出覆盖消息历史与群成员侧栏的屏幕矩形，仅排除左侧会话列表并通过点击前后文本差分消除聊天区旧数字干扰
                && snapshot.name.trim() == UI_LABEL_SESSION_LIST
                && profile::rect_is_valid(snapshot.rect)
        })
        .map(|snapshot| snapshot.rect)
        .collect()
}

/// 收集增量新增或独立弹窗范围内的有效节点文本（同时检查 `name` 与 `value` 属性）
pub(super) fn collect_snapshot_candidates<'a>(
    before_snapshots: &[&UiNodeSnapshot],
    after_snapshots: &'a [&'a UiNodeSnapshot],
    win_rect: [i32; 4],
    min_rel_x: f64,
) -> Vec<(&'a UiNodeSnapshot, String)> {
    let eff_rect = profile::effective_window_rect_from_snapshots(win_rect, after_snapshots);
    if !profile::rect_is_valid(eff_rect) {
        return Vec::new();
    }
    let min_center_x = eff_rect[0] as f64 + profile::rect_width(eff_rect) as f64 * min_rel_x;
    let excluded_rects = collect_excluded_drawer_rects(after_snapshots);

    let mut before_tokens: HashSet<(&str, String)> = HashSet::new();
    for snapshot in before_snapshots.iter().copied() {
        if snapshot.is_offscreen {
            continue;
        }
        let control_type = snapshot.control_type.trim();
        for text in collect_snapshot_texts(snapshot) {
            before_tokens.insert((control_type, text));
        }
    }

    let mut candidates = Vec::new();
    for snapshot in after_snapshots.iter().copied() {
        if snapshot.is_offscreen
            || profile::is_system_chrome(snapshot)
            || !profile::rect_is_valid(snapshot.rect)
        {
            continue;
        }
        if min_rel_x > 0.0 {
            let (center_x, _) = profile::rect_center(snapshot.rect);
            if center_x < min_center_x {
                continue;
            }
        }
        if excluded_rects
            .iter()
            .any(|container| profile::rect_contains(*container, snapshot.rect))
        {
            continue;
        }
        let control_type = snapshot.control_type.trim();
        for text in collect_snapshot_texts(snapshot) {
            if !before_snapshots.is_empty() && before_tokens.contains(&(control_type, text.clone()))
            {
                continue;
            }
            candidates.push((snapshot, text));
        }
    }
    candidates
}

fn parse_valid_group_number(raw: &str) -> Option<i64> {
    let parsed = raw.trim().parse::<i64>().ok()?;
    (parsed >= MIN_VALID_REAL_GROUP_ID).then_some(parsed)
}

fn extract_labeled_group_id(text: &str) -> Option<i64> {
    DRAWER_GROUP_ID_LABELED_REGEX
        .captures(text)
        .and_then(|captures| captures.name("id"))
        .and_then(|matched| parse_valid_group_number(matched.as_str()))
}

fn extract_standalone_group_id(text: &str) -> Option<i64> {
    let trimmed = text.trim();
    if trimmed.is_empty() || LEVEL_BADGE_REGEX.is_match(trimmed) {
        return None;
    }
    DRAWER_GROUP_ID_STANDALONE_REGEX
        .captures(trimmed)
        .and_then(|captures| captures.name("id"))
        .and_then(|matched| parse_valid_group_number(matched.as_str()))
}

fn extract_max_member_count(text: &str) -> Option<i32> {
    let captures = DRAWER_CAPACITY_RATIO_REGEX.captures(text.trim())?;
    let current = captures.name("current")?.as_str().parse::<i32>().ok()?;
    let max_count = captures.name("max")?.as_str().parse::<i32>().ok()?;
    if (10..=10_000).contains(&max_count) && current <= max_count {
        Some(max_count)
    } else {
        None
    }
}

fn extract_token_group_id(text: &str) -> Option<i64> {
    let trimmed = text.trim();
    if trimmed.is_empty()
        || parser::is_timestamp_like(trimmed)
        || LEVEL_BADGE_REGEX.is_match(trimmed)
        || extract_max_member_count(trimmed).is_some()
    {
        return None;
    }
    DRAWER_GROUP_ID_TOKEN_REGEX
        .captures(trimmed)
        .and_then(|captures| captures.name("id"))
        .and_then(|matched| parse_valid_group_number(matched.as_str()))
}

fn find_nearest_id_to_anchors(
    candidates: &[(&UiNodeSnapshot, String)],
    anchor_rects: &[[i32; 4]],
) -> Option<i64> {
    if anchor_rects.is_empty() {
        return None;
    }
    candidates
        .iter()
        .filter_map(|(snapshot, text)| {
            let id_val =
                extract_standalone_group_id(text).or_else(|| extract_token_group_id(text))?;
            let (cx, cy) = profile::rect_center(snapshot.rect);
            let min_dist = anchor_rects
                .iter()
                .map(|rect| {
                    let (lx, ly) = profile::rect_center(*rect);
                    let dx = cx - lx;
                    let dy = cy - ly;
                    (dx * dx + dy * dy).round() as i64
                })
                .min()
                .unwrap_or(i64::MAX);
            Some((min_dist, id_val))
        })
        .min_by_key(|(dist, _)| *dist)
        .map(|(_, id_val)| id_val)
}

pub(super) fn merge_candidate_samples(target: &mut Vec<String>, extra: &[String]) {
    for sample in extra {
        if target.len() >= AUDIT_CANDIDATE_SAMPLE_LIMIT {
            break;
        }
        if !sample.is_empty() && !target.iter().any(|existing| existing == sample) {
            target.push(sample.clone());
        }
    }
}

/// 从已筛选的候选文本列表按优先级提取真实群号与群容量上限，并保留前若干条样本文本
pub(super) fn extract_metadata_from_candidates(
    candidates: &[(&UiNodeSnapshot, String)],
) -> ExtractedDrawerMetadata {
    if candidates.is_empty() {
        return ExtractedDrawerMetadata::default();
    }

    let mut candidate_samples = Vec::new();
    for (_, text) in candidates {
        if candidate_samples.len() >= AUDIT_CANDIDATE_SAMPLE_LIMIT {
            break;
        }
        if !candidate_samples.iter().any(|item| item == text) {
            candidate_samples.push(text.clone());
        }
    }

    let max_member_count = candidates
        .iter()
        .find_map(|(_, text)| extract_max_member_count(text));

    // 第一优先级：同一段文本内明确携带“群号/群聊号码”前缀标签的数字
    if let Some(labeled_id) = candidates
        .iter()
        .find_map(|(_, text)| extract_labeled_group_id(text))
    {
        return ExtractedDrawerMetadata {
            real_group_id: Some(labeled_id),
            max_member_count,
            candidate_samples,
        };
    }

    // 第二优先级：优先贴近“群号”标签，再兜底贴近“复制”按钮的 5-12 位群号节点
    let label_rects: Vec<[i32; 4]> = candidates
        .iter()
        .filter(|(_, text)| {
            DRAWER_GROUP_NUMBER_LABELS
                .iter()
                .any(|label| text.contains(label))
        })
        .map(|(snapshot, _)| snapshot.rect)
        .collect();
    if let Some(nearby_id) = find_nearest_id_to_anchors(candidates, &label_rects) {
        return ExtractedDrawerMetadata {
            real_group_id: Some(nearby_id),
            max_member_count,
            candidate_samples,
        };
    }

    let copy_rects: Vec<[i32; 4]> = candidates
        .iter()
        .filter(|(_, text)| text.trim() == UI_LABEL_COPY_BUTTON)
        .map(|(snapshot, _)| snapshot.rect)
        .collect();
    if let Some(copy_nearby_id) = find_nearest_id_to_anchors(candidates, &copy_rects) {
        return ExtractedDrawerMetadata {
            real_group_id: Some(copy_nearby_id),
            max_member_count,
            candidate_samples,
        };
    }

    // 第三优先级：独立纯数字文本节点（5-12 位），第四优先级：内嵌于括号或“复制”字样旁的词元
    let fallback_id = candidates
        .iter()
        .find_map(|(_, text)| extract_standalone_group_id(text))
        .or_else(|| {
            candidates
                .iter()
                .find_map(|(_, text)| extract_token_group_id(text))
        });

    ExtractedDrawerMetadata {
        real_group_id: fallback_id,
        max_member_count,
        candidate_samples,
    }
}

/// 从开启抽屉前后的右侧新出现节点切片中提取真实群号（5-12 位）与可选群规模上限
#[cfg(test)]
pub(crate) fn parse_drawer_metadata(
    before_snapshots: &[&UiNodeSnapshot],
    after_snapshots: &[&UiNodeSnapshot],
    win_rect: [i32; 4],
) -> (Option<i64>, Option<i32>) {
    let candidates = collect_snapshot_candidates(
        before_snapshots,
        after_snapshots,
        win_rect,
        DRAWER_PANEL_MIN_REL_X,
    );
    let extracted = extract_metadata_from_candidates(&candidates);
    (extracted.real_group_id, extracted.max_member_count)
}

/// 在当前窗口聊天标题栏中定位与目标群名匹配的群标题按钮
pub(super) fn find_header_group_button<'a>(
    nodes: &'a [CollectedNode],
    eff_rect: [i32; 4],
    is_standalone: bool,
    group_name: &str,
) -> Option<&'a CollectedNode> {
    nodes
        .iter()
        .filter(|collected| {
            let snap = &collected.snapshot;
            if !snap.is_enabled
                || snap.is_offscreen
                || profile::is_system_chrome(snap)
                || profile::is_excluded_sidebar_container(snap)
            {
                return false;
            }
            let control_type = snap.control_type.trim();
            if !(control_type.eq_ignore_ascii_case(CONTROL_TYPE_BUTTON)
                || control_type.eq_ignore_ascii_case(CONTROL_TYPE_GROUP))
            {
                return false;
            }
            if !profile::is_in_chat_header(snap.rect, eff_rect, is_standalone) {
                return false;
            }
            let name = clean_node_text(&snap.name);
            !name.is_empty()
                && !DRAWER_CLOSE_BUTTON_KEYWORDS.contains(&name.as_str())
                && group_title_matches(&name, group_name)
        })
        .max_by_key(|collected| {
            let is_button = collected
                .snapshot
                .control_type
                .trim()
                .eq_ignore_ascii_case(CONTROL_TYPE_BUTTON);
            let exact_name =
                parser::canonical_group_name(&clean_node_text(&collected.snapshot.name))
                    == parser::canonical_group_name(group_name);
            (is_button, exact_name)
        })
}

fn find_header_more_button(
    nodes: &[CollectedNode],
    eff_rect: [i32; 4],
    is_standalone: bool,
) -> Option<&CollectedNode> {
    let min_center_x =
        eff_rect[0] as f64 + profile::rect_width(eff_rect) as f64 * HEADER_MORE_MIN_REL_X;
    nodes
        .iter()
        .filter(|collected| {
            let snap = &collected.snapshot;
            if !snap.is_enabled || snap.is_offscreen || profile::is_excluded_sidebar_container(snap)
            {
                return false;
            }
            if !snap
                .control_type
                .trim()
                .eq_ignore_ascii_case(CONTROL_TYPE_BUTTON)
            {
                return false;
            }
            if !profile::is_in_chat_header(snap.rect, eff_rect, is_standalone) {
                return false;
            }
            let (center_x, _) = profile::rect_center(snap.rect);
            center_x >= min_center_x && clean_node_text(&snap.name) == UI_LABEL_MORE_BUTTON
        })
        .max_by_key(|collected| collected.snapshot.rect[0])
}

fn find_drawer_close_button<'a>(
    before_nodes: &[CollectedNode],
    after_nodes: &'a [CollectedNode],
    eff_rect: [i32; 4],
) -> Option<&'a CollectedNode> {
    let min_center_x =
        eff_rect[0] as f64 + profile::rect_width(eff_rect) as f64 * DRAWER_PANEL_MIN_REL_X;
    let before_rects: HashSet<(String, [i32; 4])> = before_nodes
        .iter()
        .map(|node| (node.snapshot.name.clone(), node.snapshot.rect))
        .collect();

    // 抽屉自身可能携带名为“关闭”的按钮，因此不能调用会屏蔽“关闭”字样的 `is_system_chrome`
    after_nodes.iter().find(|collected| {
        let snap = &collected.snapshot;
        if !snap.is_enabled || snap.is_offscreen || !profile::rect_is_valid(snap.rect) {
            return false;
        }
        let (center_x, _) = profile::rect_center(snap.rect);
        if center_x < min_center_x {
            return false;
        }
        let control_type = snap.control_type.trim();
        let is_clickable = control_type.eq_ignore_ascii_case(CONTROL_TYPE_BUTTON)
            || control_type.eq_ignore_ascii_case(CONTROL_TYPE_TEXT);
        let cleaned = clean_node_text(&snap.name);
        is_clickable
            && !before_rects.contains(&(snap.name.clone(), snap.rect))
            && (cleaned == UI_LABEL_CLOSE_BUTTON
                || DRAWER_CLOSE_BUTTON_KEYWORDS
                    .iter()
                    .any(|kw| cleaned.contains(kw)))
    })
}

/// 向已校验窗口的渲染宿主或根句柄后台投递 `VK_ESCAPE` 按键消息以收起抽屉或浮层
pub(super) fn send_background_escape(identity: &VerifiedWindowIdentity) -> Result<()> {
    identity.verify_fast()?;
    let target_hwnd = find_child_window_by_class(identity.hwnd(), CHROME_RENDER_WIDGET_CLASS)
        .unwrap_or(identity.hwnd());

    unsafe {
        let _ = PostMessageW(
            Some(HWND(target_hwnd as *mut c_void)),
            WM_KEYDOWN,
            WPARAM(VK_ESCAPE.0 as usize),
            LPARAM(SCAN_KEYDOWN_ESCAPE_LPARAM),
        );
    }
    thread::sleep(Duration::from_millis(15));
    identity.verify_fast()?;
    unsafe {
        let _ = PostMessageW(
            Some(HWND(target_hwnd as *mut c_void)),
            WM_KEYUP,
            WPARAM(VK_ESCAPE.0 as usize),
            LPARAM(SCAN_KEYUP_ESCAPE_LPARAM),
        );
    }
    Ok(())
}

fn close_drawer_cleanly(
    session: &UiaSession,
    identity: &VerifiedWindowIdentity,
    before_nodes: &[CollectedNode],
    after_nodes: &[CollectedNode],
    request: &DrawerInspectRequest<'_>,
) -> Result<()> {
    let after_snaps: Vec<&UiNodeSnapshot> = after_nodes.iter().map(|n| &n.snapshot).collect();
    let eff_rect = profile::effective_window_rect_from_snapshots(request.window.rect, &after_snaps);
    let before_snaps: Vec<&UiNodeSnapshot> = before_nodes.iter().map(|n| &n.snapshot).collect();

    if let Some(close_node) = find_drawer_close_button(before_nodes, after_nodes, eff_rect) {
        let _ = invoke_element_background_verified(identity, &close_node.element);
        thread::sleep(Duration::from_millis(DRAWER_CLOSE_WAIT_MS));
    }

    let mut verify_nodes = session.collect_flat(request.window.hwnd, INTERNAL_SCAN_DEPTH)?;
    let mut verify_snaps: Vec<&UiNodeSnapshot> = verify_nodes.iter().map(|n| &n.snapshot).collect();
    let mut remaining_right = collect_snapshot_candidates(
        &before_snaps,
        &verify_snaps,
        request.window.rect,
        DRAWER_PANEL_MIN_REL_X,
    );

    if !remaining_right.is_empty() {
        let _ = send_background_escape(identity);
        thread::sleep(Duration::from_millis(DRAWER_CLOSE_WAIT_MS));
        verify_nodes = session.collect_flat(request.window.hwnd, INTERNAL_SCAN_DEPTH)?;
        verify_snaps = verify_nodes.iter().map(|n| &n.snapshot).collect();
        remaining_right = collect_snapshot_candidates(
            &before_snaps,
            &verify_snaps,
            request.window.rect,
            DRAWER_PANEL_MIN_REL_X,
        );
    }

    // 若“关闭”按钮与 ESC 均未收起，且右侧仍存在成组抽屉节点，则再次点击右上角“更多”按钮切换关闭
    if remaining_right.len() >= 3 {
        let toggle_eff =
            profile::effective_window_rect_from_snapshots(request.window.rect, &verify_snaps);
        if let Some(more_btn) =
            find_header_more_button(&verify_nodes, toggle_eff, request.window.is_standalone_chat)
        {
            let _ = invoke_element_background_verified(identity, &more_btn.element);
            thread::sleep(Duration::from_millis(DRAWER_CLOSE_WAIT_MS));
        }
    }

    Ok(())
}

fn probe_more_drawer(
    session: &UiaSession,
    identity: &VerifiedWindowIdentity,
    request: &DrawerInspectRequest<'_>,
    before_nodes: &[CollectedNode],
) -> Result<(ExtractedDrawerMetadata, &'static str)> {
    let before_snaps: Vec<&UiNodeSnapshot> = before_nodes.iter().map(|n| &n.snapshot).collect();
    let eff_rect =
        profile::effective_window_rect_from_snapshots(request.window.rect, &before_snaps);

    let Some(more_button) =
        find_header_more_button(before_nodes, eff_rect, request.window.is_standalone_chat)
    else {
        return Ok((ExtractedDrawerMetadata::default(), "more_button_missing"));
    };

    identity.verify_fast()?;
    if let Err(err) = invoke_element_background_verified(identity, &more_button.element) {
        let _ = err;
        return Ok((
            ExtractedDrawerMetadata::default(),
            "more_drawer_open_failed",
        ));
    }

    let mut best_meta = ExtractedDrawerMetadata::default();
    let mut latest_after_nodes: Vec<CollectedNode> = Vec::new();
    for _ in 0..DRAWER_POLL_ATTEMPTS {
        thread::sleep(Duration::from_millis(DRAWER_POLL_INTERVAL_MS));
        let Ok(after_nodes) =
            super::wake_and_collect(session, request.window.hwnd, INTERNAL_SCAN_DEPTH)
        else {
            continue;
        };
        let after_snaps: Vec<&UiNodeSnapshot> = after_nodes.iter().map(|n| &n.snapshot).collect();
        let candidates = collect_snapshot_candidates(
            &before_snaps,
            &after_snaps,
            request.window.rect,
            DRAWER_PANEL_MIN_REL_X,
        );
        let parsed = extract_metadata_from_candidates(&candidates);
        if best_meta.max_member_count.is_none() {
            best_meta.max_member_count = parsed.max_member_count;
        }
        merge_candidate_samples(&mut best_meta.candidate_samples, &parsed.candidate_samples);
        latest_after_nodes = after_nodes;

        if parsed.real_group_id.is_some() {
            best_meta.real_group_id = parsed.real_group_id;
            break;
        }
    }

    if !latest_after_nodes.is_empty() {
        let _ = close_drawer_cleanly(
            session,
            identity,
            before_nodes,
            &latest_after_nodes,
            request,
        );
    }

    let source = if best_meta.real_group_id.is_some() {
        "more_drawer"
    } else {
        "more_drawer_miss"
    };
    Ok((best_meta, source))
}

fn collect_qq_pids_for_window(hwnd: isize, window_pid: u32) -> Vec<u32> {
    let names: Vec<String> = QQ_PROCESS_NAMES
        .iter()
        .copied()
        .map(str::to_string)
        .collect();
    let mut pids = find_pids_by_names(&names);
    if window_pid != 0 && !pids.contains(&window_pid) {
        pids.push(window_pid);
    }
    let actual_pid = win32::get_window_pid(hwnd);
    if actual_pid != 0 && !pids.contains(&actual_pid) {
        pids.push(actual_pid);
    }
    pids
}

/// 按缓存与 10 分钟冷却策略决定是否串联“更多”设置抽屉与“资料卡”弹窗双通道提取真实群号与群容量上限
pub fn inspect_group_drawer_if_needed(
    session: &UiaSession,
    identity: &VerifiedWindowIdentity,
    storage: &Storage,
    request: &DrawerInspectRequest<'_>,
) -> Result<DrawerInspectionResult> {
    if request.window.class_name == MOCK_QQ_MAIN_CLASS {
        return Ok(DrawerInspectionResult::default());
    }

    let canonical_name = parser::canonical_group_name(request.group_name);
    let lookup_name = if canonical_name.is_empty() {
        request.group_name.trim()
    } else {
        canonical_name.as_str()
    };

    let existing = storage.get_group_by_name(lookup_name)?;
    let now_ms = now_utc_ms();

    if !request.force_refresh {
        if let Some(ref stored) = existing {
            if stored.real_group_id.is_some() {
                return Ok(DrawerInspectionResult {
                    real_group_id: stored.real_group_id,
                    max_member_count: stored.max_member_count,
                    cache_hit: true,
                });
            }
            if let Some(last_checked) = stored.drawer_checked_at_ms {
                if now_ms.saturating_sub(last_checked) < DRAWER_RECHECK_COOLDOWN_MS {
                    return Ok(DrawerInspectionResult {
                        real_group_id: None,
                        max_member_count: stored.max_member_count,
                        cache_hit: true,
                    });
                }
            }
        }
    }

    let qq_pids = collect_qq_pids_for_window(request.window.hwnd, request.window.pid);
    let _fg = ForegroundRestoreGuard::capture(&qq_pids);
    let before_nodes = super::wake_and_collect(session, request.window.hwnd, INTERNAL_SCAN_DEPTH)?;
    let (mut combined_meta, mut primary_source) =
        probe_more_drawer(session, identity, request, &before_nodes)?;

    // 责任链第二级：若右侧“更多”抽屉未直接暴露群号，再后台触发群标题唤醒独立“资料卡”弹窗探测
    if combined_meta.real_group_id.is_none() {
        let card_result = profile_card_probe::probe_profile_card_group_id(
            session,
            identity,
            request,
            &before_nodes,
        )?;
        if card_result.real_group_id.is_some() {
            combined_meta.real_group_id = card_result.real_group_id;
            primary_source = card_result.source;
        } else {
            primary_source = card_result.source;
        }
        if combined_meta.max_member_count.is_none() {
            combined_meta.max_member_count = card_result.max_member_count;
        }
        merge_candidate_samples(
            &mut combined_meta.candidate_samples,
            &card_result.candidate_samples,
        );
    }

    let preserved_unread = existing.as_ref().map(|item| item.unread_count).unwrap_or(0);
    let _ = storage.upsert_group(&UpsertGroupInput {
        group_name: lookup_name.to_string(),
        real_group_id: combined_meta.real_group_id,
        member_count: request.member_count.map(|count| count as i32),
        max_member_count: combined_meta.max_member_count,
        last_preview: None,
        unread_count: preserved_unread,
    })?;
    let checked_ms = now_utc_ms();
    let _ = storage.mark_group_drawer_checked(lookup_name, checked_ms);

    let audit_detail = if combined_meta.real_group_id.is_some() {
        format!(
            "group={lookup_name} source={primary_source} real_group_id={:?} max_member_count={:?}",
            combined_meta.real_group_id, combined_meta.max_member_count
        )
    } else {
        format!(
            "group={lookup_name} source={primary_source} real_group_id=None max_member_count={:?} samples={:?}",
            combined_meta.max_member_count, combined_meta.candidate_samples
        )
    };
    let _ = storage.append_audit("drawer_inspect", &audit_detail);

    Ok(DrawerInspectionResult {
        real_group_id: combined_meta.real_group_id,
        max_member_count: combined_meta.max_member_count,
        cache_hit: false,
    })
}
