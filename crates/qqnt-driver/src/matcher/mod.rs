//! 群匹配、小于 10 人强制安全守卫、右侧群成员提取与群信息抽屉探测门面模块

mod composer_matcher;
pub mod drawer_inspector;
mod header_matcher;
pub mod member_sidebar;
mod profile_card_probe;
mod session_matcher;

#[cfg(test)]
mod replay_tests;
#[cfg(test)]
mod tests;

use std::collections::HashSet;
use std::thread;
use std::time::Duration;

use common_types::constants::MOCK_QQ_MAIN_CLASS;
use common_types::{AdapterError, GroupTargetSpec, Result, UiNodeSnapshot, WindowInfo};
use uia_core::{
    flat_needs_chromium_wake, invoke_element_background_verified, refresh_window_info,
    restore_no_activate, snapshot_needs_chromium_wake, type_text_background_verified,
    CollectedNode, TopmostUnoccludeGuard, TypingDelayRange, UiaSession, VerifiedWindowIdentity,
};

use crate::constants::{
    GROUP_SWITCH_SETTLE_DELAY, INTERNAL_SCAN_DEPTH, SEARCH_SETTLE_DELAY, WAKE_PRE_UNOCCLUDE_DELAY,
    WAKE_UNOCCLUDE_MAX_ATTEMPTS, WAKE_UNOCCLUDE_RETRY_DELAY, WAKE_UNOCCLUDE_SETTLE_DELAY,
};
use crate::parser;
use crate::profile::{self, QqntProfile, DEFAULT_WINDOW_CLASSES};

pub use composer_matcher::{locate_composer_and_send_button, locate_composer_and_send_indices};
pub use drawer_inspector::{
    inspect_group_drawer_if_needed, DrawerInspectRequest, DrawerInspectionResult,
};
pub use header_matcher::{
    group_title_matches, parse_member_count, parse_member_label_count, passes_member_guard,
    read_chat_header, read_chat_header_from_snapshots, read_chat_header_from_tree,
};
pub use member_sidebar::{
    extract_members_from_snapshots, extract_members_from_tree, has_group_member_sidebar,
    has_group_member_sidebar_in_snapshots,
};
pub(crate) use session_matcher::infer_is_group_session;
pub use session_matcher::{
    extract_sessions, extract_sessions_from_snapshots, extract_sessions_from_tree,
    is_search_mode_active, is_search_mode_active_in_tree, is_search_mode_active_snapshots,
    is_valid_session_title,
};

use header_matcher::verify_attached_member_guard;
use session_matcher::{
    build_session_candidates, find_clear_search_element, find_search_element, text_contains_target,
};

/// 若当前左上角搜索框处于激活/已输入状态，优先点击“清空”按钮否则清空搜索框文本并等待界面还原
pub fn clear_search_mode_if_active(
    hwnd: isize,
    allow_mock: bool,
    nodes: &[CollectedNode],
    win_rect: [i32; 4],
) -> bool {
    if !is_search_mode_active(nodes, win_rect) {
        return false;
    }
    let Ok(verified) = uia_core::VerifiedWindowIdentity::verify_and_bind(hwnd, allow_mock) else {
        return false;
    };
    let cleared = clear_search_mode_with_verified(&verified, nodes, win_rect);
    if cleared {
        thread::sleep(std::time::Duration::from_millis(200));
    }
    cleared
}

/// 在已持有校验身份句柄的前提下执行搜索状态清理（先点“清空”按钮，必要时清空输入框内容）
fn clear_search_mode_with_verified(
    verified: &uia_core::VerifiedWindowIdentity,
    nodes: &[CollectedNode],
    win_rect: [i32; 4],
) -> bool {
    let mut cleared = false;
    if let Some(clear_btn) = find_clear_search_element(nodes, win_rect) {
        if invoke_element_background_verified(verified, clear_btn).is_ok() {
            cleared = true;
        }
    }
    if let Some(search_element) = find_search_element(nodes, win_rect) {
        if !cleared
            && (uia_core::clear_element_text(search_element).is_ok()
                || uia_core::clear_editor_focus_restore_verified(verified, search_element).is_ok())
        {
            cleared = true;
        }
    }
    if cleared {
        let _ = uia_core::send_escape_key_verified(verified);
    }
    cleared
}

/// 目标聊天选择模式，用于参数化共享“窗口识别-会话点击-搜索回退”流程
#[derive(Clone, Copy)]
enum SelectChatMode<'a> {
    /// 选中并按群人数护栏校验目标群聊
    Group(&'a GroupTargetSpec),
    /// 选中并严格核验确认为非群聊的目标私聊好友会话
    Private { target_name: &'a str },
}

impl<'a> SelectChatMode<'a> {
    fn target_name(&self) -> &'a str {
        match self {
            Self::Group(spec) => spec.group_name.as_str(),
            Self::Private { target_name } => target_name,
        }
    }

    fn is_private(&self) -> bool {
        matches!(self, Self::Private { .. })
    }
}

/// 判断一组平铺采集节点中是否存在群聊右侧成员列表侧边栏
pub fn has_group_member_sidebar_in_nodes(nodes: &[CollectedNode]) -> bool {
    let snapshots: Vec<&UiNodeSnapshot> = nodes.iter().map(|node| &node.snapshot).collect();
    has_group_member_sidebar_in_snapshots(&snapshots)
}

/// 安全选中并验证目标群聊后返回的附着窗口与人数上下文
#[derive(Debug)]
pub struct SelectedGroup {
    pub window: WindowInfo,
    pub active_title: String,
    pub detected_member_count: Option<u32>,
    pub _unocclude_guard: Option<TopmostUnoccludeGuard>,
}

/// 群匹配执行上下文，封装 UIA 会话、规则画像与 COM 工作线程缓存的目标窗口身份护栏
pub struct GroupSelectContext<'a> {
    pub session: &'a UiaSession,
    pub profile: &'a QqntProfile,
    pub allow_mock: bool,
    pub verified_cache: &'a mut Option<VerifiedWindowIdentity>,
}

/// 获取或绑定指定窗口的不可变 Win32 身份凭证，并在命中缓存时执行 `O(1)` 快速核验
pub fn ensure_verified_window<'a>(
    cache: &'a mut Option<VerifiedWindowIdentity>,
    window: &WindowInfo,
    allow_mock: bool,
) -> Result<&'a VerifiedWindowIdentity> {
    let effective_mock = allow_mock || window.class_name == MOCK_QQ_MAIN_CLASS;
    let cache_valid = cache
        .as_ref()
        .filter(|verified| verified.hwnd() == window.hwnd && verified.verify_fast().is_ok())
        .is_some();

    if !cache_valid {
        *cache = Some(VerifiedWindowIdentity::verify_and_bind(
            window.hwnd,
            effective_mock,
        )?);
    }

    cache
        .as_ref()
        .ok_or_else(|| AdapterError::UiaFailure("verified window cache missing".to_string()))
}

fn wake_with_optional_unocclude<T>(
    session: &UiaSession,
    hwnd: isize,
    allow_unocclude: bool,
    mut fetch: impl FnMut() -> Result<T>,
    mut needs_wake: impl FnMut(&T) -> bool,
) -> Result<(T, Option<TopmostUnoccludeGuard>)> {
    let mut data = fetch()?;
    if !needs_wake(&data) {
        return Ok((data, None));
    }

    let _ = session.wake_uia_subtree_ext(hwnd, false);
    thread::sleep(WAKE_PRE_UNOCCLUDE_DELAY);
    data = fetch()?;
    if !needs_wake(&data) || !allow_unocclude {
        return Ok((data, None));
    }

    let guard = TopmostUnoccludeGuard::acquire(hwnd);
    thread::sleep(WAKE_UNOCCLUDE_SETTLE_DELAY);
    for attempt in 0..WAKE_UNOCCLUDE_MAX_ATTEMPTS {
        let report = session.wake_uia_subtree_ext(hwnd, attempt >= 1);
        tracing::info!(
            hwnd,
            attempt = attempt + 1,
            visited_msaa = report.visited_msaa_nodes,
            ia2_ok = report.ia2_query_service_ok,
            ia2_first_hr = report.ia2_first_hr,
            acc_hit = report.acc_hit_test_ok,
            acc_pt = report.acc_from_point_ok,
            child_after = report.target_hwnds_after.len(),
            "waking Chromium/Electron UIA subtree"
        );
        thread::sleep(WAKE_UNOCCLUDE_RETRY_DELAY);
        data = fetch()?;
        if !needs_wake(&data) {
            break;
        }
    }

    Ok((data, guard))
}

/// 采集平铺 UIA 节点并按需唤醒 Chromium/Electron 无障碍子树
pub fn wake_and_collect_ext(
    session: &UiaSession,
    hwnd: isize,
    max_depth: usize,
    allow_unocclude: bool,
) -> Result<(Vec<CollectedNode>, Option<TopmostUnoccludeGuard>)> {
    wake_with_optional_unocclude(
        session,
        hwnd,
        allow_unocclude,
        || session.collect_flat(hwnd, max_depth),
        |nodes| flat_needs_chromium_wake(nodes),
    )
}

/// 转储层级 UIA 快照树并按需唤醒 Chromium/Electron 无障碍子树
pub fn wake_and_dump_tree_ext(
    session: &UiaSession,
    hwnd: isize,
    max_depth: usize,
    allow_unocclude: bool,
) -> Result<(UiNodeSnapshot, Option<TopmostUnoccludeGuard>)> {
    wake_with_optional_unocclude(
        session,
        hwnd,
        allow_unocclude,
        || session.dump_tree(hwnd, max_depth),
        |tree| max_depth > 0 && snapshot_needs_chromium_wake(tree),
    )
}

/// 不触碰 Z 序遮挡的被动版平铺节点采集封装
pub fn wake_and_collect(
    session: &UiaSession,
    hwnd: isize,
    max_depth: usize,
) -> Result<Vec<CollectedNode>> {
    wake_and_collect_ext(session, hwnd, max_depth, false).map(|(nodes, _)| nodes)
}

/// 不触碰 Z 序遮挡的被动版快照树转储封装
pub fn wake_and_dump_tree(
    session: &UiaSession,
    hwnd: isize,
    max_depth: usize,
) -> Result<UiNodeSnapshot> {
    wake_and_dump_tree_ext(session, hwnd, max_depth, false).map(|(tree, _)| tree)
}

/// 对候选主窗口执行确定性排序，优先选择配置扩展类与非最小化主窗口
pub fn order_main_windows(windows: &[WindowInfo], profile: &QqntProfile) -> Vec<WindowInfo> {
    let extra_classes: HashSet<&str> = profile
        .window_classes()
        .iter()
        .map(String::as_str)
        .filter(|class_name| !DEFAULT_WINDOW_CLASSES.contains(class_name))
        .collect();
    let self_pid = std::process::id();

    let mut ordered = windows.to_vec();
    ordered.sort_by(|left_win, right_win| {
        let left_extra = extra_classes.contains(left_win.class_name.as_str());
        let right_extra = extra_classes.contains(right_win.class_name.as_str());
        let left_self_pid = left_win.pid == self_pid;
        let right_self_pid = right_win.pid == self_pid;
        let left_main = !left_win.is_standalone_chat;
        let right_main = !right_win.is_standalone_chat;
        let left_area = profile::rect_area(left_win.rect);
        let right_area = profile::rect_area(right_win.rect);

        right_extra
            .cmp(&left_extra)
            .then_with(|| right_self_pid.cmp(&left_self_pid))
            .then_with(|| right_main.cmp(&left_main))
            .then_with(|| left_win.is_iconic.cmp(&right_win.is_iconic))
            .then_with(|| right_area.cmp(&left_area))
            .then_with(|| left_win.hwnd.cmp(&right_win.hwnd))
    });
    ordered
}

fn try_candidates_in_window(
    context: &mut GroupSelectContext<'_>,
    window: &WindowInfo,
    nodes: &[CollectedNode],
    mode: SelectChatMode<'_>,
    member_exceeded_err: &mut Option<AdapterError>,
    allow_unscoped_search_fallback: bool,
) -> Result<Option<SelectedGroup>> {
    let target_name = mode.target_name();
    let snapshots: Vec<&UiNodeSnapshot> = nodes.iter().map(|node| &node.snapshot).collect();
    let elements: Vec<&uia_core::UiNodeHandle> = nodes.iter().map(|node| &node.element).collect();
    let mut candidates = build_session_candidates(&snapshots, Some(&elements), window.rect, false);

    candidates.retain(|candidate| {
        if mode.is_private()
            && (candidate.member_count_hint.is_some()
                || (candidate.is_group_hint == Some(true)
                    && candidate.title.trim() != target_name.trim()))
        {
            return false;
        }
        text_contains_target(&candidate.title, target_name)
            || text_contains_target(&candidate.raw_text, target_name)
    });

    if candidates.is_empty() && allow_unscoped_search_fallback {
        candidates = build_session_candidates(&snapshots, Some(&elements), window.rect, true);
        candidates.retain(|candidate| {
            if mode.is_private()
                && (candidate.member_count_hint.is_some()
                    || (candidate.is_group_hint == Some(true)
                        && candidate.title.trim() != target_name.trim()))
            {
                return false;
            }
            text_contains_target(&candidate.title, target_name)
                || text_contains_target(&candidate.raw_text, target_name)
        });
    }

    if candidates.is_empty() {
        return Ok(None);
    }

    let target_canonical = parser::canonical_group_name(target_name);
    candidates.sort_by_key(|candidate| match mode {
        SelectChatMode::Group(spec) => {
            let exact = parser::canonical_group_name(&candidate.title) == target_canonical;
            let has_valid_inline = candidate
                .member_count_hint
                .map(|count| {
                    spec.strict_below_members
                        .map(|limit| count < limit)
                        .unwrap_or(true)
                })
                .unwrap_or(false);
            (!exact, !has_valid_inline, candidate.rect[1])
        }
        SelectChatMode::Private { .. } => {
            let exact = candidate.title.trim() == target_name.trim();
            (!exact, false, candidate.rect[1])
        }
    });

    for candidate in candidates {
        if let SelectChatMode::Group(spec) = mode {
            if let Some(inline_count) = candidate.member_count_hint {
                if let Err(err) = passes_member_guard(spec, Some(inline_count)) {
                    tracing::info!(
                        candidate = %candidate.title,
                        inline_count,
                        limit = ?spec.strict_below_members,
                        "skipping group candidate pre-filtered by inline member count"
                    );
                    if member_exceeded_err.is_none() {
                        *member_exceeded_err = Some(err);
                    }
                    continue;
                }
            }
        }

        let Some(element) = candidate.element else {
            continue;
        };

        let verified = ensure_verified_window(context.verified_cache, window, context.allow_mock)?;
        if let Err(err) = invoke_element_background_verified(verified, element) {
            tracing::debug!(
                candidate = %candidate.title,
                error = %err,
                "verified background invoke failed"
            );
            continue;
        }

        thread::sleep(GROUP_SWITCH_SETTLE_DELAY);

        let after_nodes = context
            .session
            .collect_flat(window.hwnd, INTERNAL_SCAN_DEPTH)?;
        let (header_title, header_count) = read_chat_header(&after_nodes, window.rect, false, None);
        if let Some(ref ht) = header_title {
            if !(text_contains_target(ht, target_name) || group_title_matches(ht, target_name)) {
                tracing::debug!(
                    candidate = %candidate.title,
                    header = %ht,
                    target = %target_name,
                    "opened header does not match target chat; continuing"
                );
                continue;
            }
        }

        match mode {
            SelectChatMode::Group(spec) => {
                let effective_count = header_count.or(candidate.member_count_hint);
                if let Err(err) = verify_attached_member_guard(spec, effective_count) {
                    tracing::info!(
                        candidate = %candidate.title,
                        header_count = ?header_count,
                        limit = ?spec.strict_below_members,
                        "rejecting opened group after header member count verification"
                    );
                    if member_exceeded_err.is_none() {
                        *member_exceeded_err = Some(err);
                    }
                    continue;
                }
                let active_title = header_title
                    .filter(|title| text_contains_target(title, target_name))
                    .unwrap_or(candidate.title);
                return Ok(Some(SelectedGroup {
                    window: window.clone(),
                    active_title,
                    detected_member_count: effective_count,
                    _unocclude_guard: None,
                }));
            }
            SelectChatMode::Private { .. } => {
                if header_count.is_some() || has_group_member_sidebar_in_nodes(&after_nodes) {
                    tracing::info!(
                        candidate = %candidate.title,
                        header_count = ?header_count,
                        "opened candidate shows group count or member sidebar; rejecting for private chat"
                    );
                    continue;
                }
                let active_title = header_title
                    .filter(|title| text_contains_target(title, target_name))
                    .unwrap_or(candidate.title);
                return Ok(Some(SelectedGroup {
                    window: window.clone(),
                    active_title,
                    detected_member_count: None,
                    _unocclude_guard: None,
                }));
            }
        }
    }

    Ok(None)
}

/// 在独立窗口、主窗口左侧会话列表与全局搜索框中安全检索并校验目标群聊
pub fn select_target_group(
    context: &mut GroupSelectContext<'_>,
    windows: &[WindowInfo],
    spec: &GroupTargetSpec,
) -> Result<SelectedGroup> {
    select_target_chat(context, windows, SelectChatMode::Group(spec))
}

/// 在独立窗口、主窗口左侧会话列表与全局搜索框中安全选中并校验目标私聊好友
pub fn select_target_private(
    context: &mut GroupSelectContext<'_>,
    windows: &[WindowInfo],
    friend_target: &str,
) -> Result<SelectedGroup> {
    select_target_chat(
        context,
        windows,
        SelectChatMode::Private {
            target_name: friend_target,
        },
    )
}

fn select_target_chat(
    context: &mut GroupSelectContext<'_>,
    windows: &[WindowInfo],
    mode: SelectChatMode<'_>,
) -> Result<SelectedGroup> {
    let target_name = mode.target_name();
    if target_name.trim().is_empty() {
        return Err(AdapterError::ConfigError(
            "target chat name cannot be empty".to_string(),
        ));
    }
    if windows.is_empty() {
        return Err(AdapterError::WindowNotFound(
            "no visible or iconic QQNT / MockQQ windows found".to_string(),
        ));
    }

    let mut member_exceeded_err: Option<AdapterError> = None;

    // 1. 优先排查已打开的独立聊天子窗口
    for window in windows
        .iter()
        .filter(|win| win.is_standalone_chat && text_contains_target(&win.title, target_name))
    {
        if let SelectChatMode::Group(spec) = mode {
            if let Some(title_count) = parse_member_count(&window.title) {
                if let Err(err) = passes_member_guard(spec, Some(title_count)) {
                    tracing::info!(
                        window_title = %window.title,
                        title_count,
                        "skipping standalone window violating member guard"
                    );
                    if member_exceeded_err.is_none() {
                        member_exceeded_err = Some(err);
                    }
                    continue;
                }
            }
        } else if parse_member_count(&window.title).is_some() {
            continue;
        }

        let mut window = window.clone();
        if window.is_iconic {
            restore_no_activate(window.hwnd);
            thread::sleep(Duration::from_millis(120));
            let _ = refresh_window_info(&mut window);
        }
        let _ = ensure_verified_window(context.verified_cache, &window, context.allow_mock)?;
        let (nodes, guard) =
            wake_and_collect_ext(context.session, window.hwnd, INTERNAL_SCAN_DEPTH, true)?;
        let (header_title, header_count) =
            read_chat_header(&nodes, window.rect, true, Some(&window.title));
        let effective_count = header_count.or_else(|| parse_member_count(&window.title));

        match mode {
            SelectChatMode::Group(spec) => {
                match verify_attached_member_guard(spec, effective_count) {
                    Ok(()) => {
                        let fallback_title = window.title.clone();
                        return Ok(SelectedGroup {
                            window,
                            active_title: header_title.unwrap_or(fallback_title),
                            detected_member_count: effective_count,
                            _unocclude_guard: guard,
                        });
                    }
                    Err(err) => {
                        if member_exceeded_err.is_none() {
                            member_exceeded_err = Some(err);
                        }
                    }
                }
            }
            SelectChatMode::Private { .. } => {
                if effective_count.is_none() && !has_group_member_sidebar_in_nodes(&nodes) {
                    let fallback_title = window.title.clone();
                    return Ok(SelectedGroup {
                        window,
                        active_title: header_title.unwrap_or(fallback_title),
                        detected_member_count: None,
                        _unocclude_guard: guard,
                    });
                }
            }
        }
    }

    // 2. 附着主 QQ 窗口，先检查右侧已打开聊天头快路径，未命中再扫描左侧会话与搜索回退
    for mut window in order_main_windows(windows, context.profile) {
        if window.is_iconic {
            restore_no_activate(window.hwnd);
            thread::sleep(Duration::from_millis(120));
            let _ = refresh_window_info(&mut window);
        }

        let _ = ensure_verified_window(context.verified_cache, &window, context.allow_mock)?;
        let (nodes, mut guard) =
            wake_and_collect_ext(context.session, window.hwnd, INTERNAL_SCAN_DEPTH, true)?;

        let (current_title, current_count) = read_chat_header(&nodes, window.rect, false, None);
        if let Some(active) = current_title {
            if group_title_matches(&active, target_name)
                || text_contains_target(&active, target_name)
            {
                match mode {
                    SelectChatMode::Group(spec) => {
                        match verify_attached_member_guard(spec, current_count) {
                            Ok(()) => {
                                return Ok(SelectedGroup {
                                    window,
                                    active_title: active,
                                    detected_member_count: current_count,
                                    _unocclude_guard: guard.take(),
                                });
                            }
                            Err(err) => {
                                tracing::info!(
                                    active = %active,
                                    current_count = ?current_count,
                                    "currently open chat matches name but failed verified member guard; scanning session list"
                                );
                                if member_exceeded_err.is_none() {
                                    member_exceeded_err = Some(err);
                                }
                            }
                        }
                    }
                    SelectChatMode::Private { .. } => {
                        if current_count.is_none() && !has_group_member_sidebar_in_nodes(&nodes) {
                            return Ok(SelectedGroup {
                                window,
                                active_title: active,
                                detected_member_count: None,
                                _unocclude_guard: guard.take(),
                            });
                        }
                    }
                }
            }
        }

        let had_member_err = member_exceeded_err.is_some();

        if let Some(mut selected) = try_candidates_in_window(
            context,
            &window,
            &nodes,
            mode,
            &mut member_exceeded_err,
            false,
        )? {
            selected._unocclude_guard = guard.take();
            return Ok(selected);
        }

        // 仅当可见列表未匹配到超限同名群时，才使用左上角搜索框执行补充检索
        let saw_name_match_with_exceeded = !had_member_err && member_exceeded_err.is_some();
        if !saw_name_match_with_exceeded {
            if let Some(search_element) = find_search_element(&nodes, window.rect) {
                {
                    let verified = ensure_verified_window(
                        context.verified_cache,
                        &window,
                        context.allow_mock,
                    )?;
                    let _ = clear_search_mode_with_verified(verified, &nodes, window.rect);
                    let _ = uia_core::clear_element_text(search_element);
                    let _ = invoke_element_background_verified(verified, search_element);
                    let _ = type_text_background_verified(
                        verified,
                        search_element,
                        target_name,
                        &TypingDelayRange::new(12, 25),
                    );
                }
                thread::sleep(SEARCH_SETTLE_DELAY);
                let search_nodes = context
                    .session
                    .collect_flat(window.hwnd, INTERNAL_SCAN_DEPTH)?;
                let selected_res = try_candidates_in_window(
                    context,
                    &window,
                    &search_nodes,
                    mode,
                    &mut member_exceeded_err,
                    true,
                );
                if let Ok(verified_after) =
                    ensure_verified_window(context.verified_cache, &window, context.allow_mock)
                {
                    let _ =
                        clear_search_mode_with_verified(verified_after, &search_nodes, window.rect);
                    if let Some(search_elem_after) = find_search_element(&search_nodes, window.rect)
                    {
                        let _ = uia_core::clear_element_text(search_elem_after);
                    }
                }
                if let Some(mut selected) = selected_res? {
                    selected._unocclude_guard = guard.take();
                    return Ok(selected);
                }
            }
        }
    }

    if let Some(err) = member_exceeded_err {
        return Err(err);
    }

    Err(AdapterError::GroupNotFound {
        group: target_name.to_string(),
    })
}
