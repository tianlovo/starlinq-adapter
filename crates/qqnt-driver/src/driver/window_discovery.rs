//! QQ 进程发现、登录窗体识别、安全风控熔断校验与窗口诊断处理器

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use common_types::{
    AdapterConfig, AdapterError, AdapterEvent, AdapterStatus, ChatTargetKind, Result,
    UiNodeSnapshot, WindowDiagnosticReport, WindowInfo,
};
use uia_core::constants::{CONTROL_TYPE_BUTTON, CONTROL_TYPE_LIST_ITEM, CONTROL_TYPE_TREE_ITEM};
use uia_core::{
    enumerate_candidate_windows, find_pids_by_names, find_windows_with_keywords_ext,
    launch_or_wake_qq, resolve_qq_executable, restore_no_activate, wake_chromium_accessibility,
    wake_chromium_accessibility_ext, win32, CollectedNode, UiaSession,
};

use super::runtime_state::SharedInner;
use crate::constants::{INTERNAL_SCAN_DEPTH, LOGIN_SPLASH_ERROR};
use crate::matcher;
use crate::profile;

/// 刷新本地 QQ 进程列表与可执行路径，并检查是否存在触发安全风控熔断的高危弹窗
pub(crate) fn refresh_pids_and_check_security(shared: &Arc<SharedInner>) -> Result<Vec<u32>> {
    let pids = find_pids_by_names(shared.profile.process_names());
    let resolved = resolve_qq_executable(shared.config.qq_client.executable_path.as_deref(), &pids);
    {
        let mut guard = shared.state.lock();
        guard.qq_pids = pids.clone();
        guard.resolved_qq_path = resolved;
        if guard.security_breaker_tripped {
            let reason = guard
                .security_breaker_reason
                .clone()
                .unwrap_or_else(|| "security verification dialog detected".to_string());
            return Err(AdapterError::SecurityPromptDetected {
                title: "SecurityBreakerTripped".to_string(),
                detail: reason,
            });
        }
    }

    let sec_hits = find_windows_with_keywords_ext(
        &pids,
        shared.profile.security_keywords(),
        shared.allow_mock(),
    );
    if let Some((sec_hwnd, title)) = sec_hits.into_iter().next() {
        let detail = format!(
            "detected security prompt window hwnd=0x{sec_hwnd:x} ('{title}'); automation halted"
        );
        let raw_status = {
            let mut guard = shared.state.lock();
            guard.security_breaker_tripped = true;
            guard.security_breaker_reason = Some(detail.clone());
            guard.to_status(&shared.config)
        };
        let status = shared.enrich_status(raw_status);
        tracing::error!(hwnd = sec_hwnd, title = %title, "触发安全风控熔断机制，停止一切交互操作");
        let _ = shared.storage.append_audit(
            "security_breaker_tripped",
            &format!("{{\"hwnd\":{sec_hwnd},\"title\":{title:?}}}"),
        );
        shared.emit_event(AdapterEvent::SecurityAlert {
            title: title.clone(),
            detail: detail.clone(),
        });
        shared.emit_event(AdapterEvent::StatusChanged(status));
        return Err(AdapterError::SecurityPromptDetected { title, detail });
    }

    Ok(pids)
}

/// 受控重置桌面安全风控熔断器：先只读重扫桌面风控弹窗，仍在则拒绝并审计，消除后复位状态并广播变更
pub(crate) fn handle_reset_security_breaker(shared: &Arc<SharedInner>) -> Result<AdapterStatus> {
    let pids = find_pids_by_names(shared.profile.process_names());
    let resolved = resolve_qq_executable(shared.config.qq_client.executable_path.as_deref(), &pids);
    {
        let mut guard = shared.state.lock();
        guard.qq_pids = pids.clone();
        guard.resolved_qq_path = resolved;
    }

    let sec_hits = find_windows_with_keywords_ext(
        &pids,
        shared.profile.security_keywords(),
        shared.allow_mock(),
    );
    if let Some((sec_hwnd, title)) = sec_hits.into_iter().next() {
        let detail = format!(
            "security prompt window still present hwnd=0x{sec_hwnd:x} ('{title}'); breaker reset denied"
        );
        let _ = shared.storage.append_audit(
            "security_breaker_reset_denied",
            &format!("{{\"hwnd\":{sec_hwnd},\"title\":{title:?}}}"),
        );
        return Err(AdapterError::SecurityPromptDetected { title, detail });
    }

    let (was_tripped, prior_reason, raw_status) = {
        let mut guard = shared.state.lock();
        let was = guard.security_breaker_tripped;
        let prior = guard.security_breaker_reason.take();
        guard.security_breaker_tripped = false;
        (was, prior, guard.to_status(&shared.config))
    };
    let status = shared.enrich_status(raw_status);
    let _ = shared.storage.append_audit(
        "security_breaker_reset",
        &serde_json::json!({
            "was_tripped": was_tripped,
            "prior_reason": prior_reason,
        })
        .to_string(),
    );
    shared.emit_event(AdapterEvent::StatusChanged(status.clone()));
    Ok(status)
}

/// 查询当前驱动状态（在返回前先执行进程刷新与实时窗口标题核对）
pub(crate) fn handle_status(
    session: &UiaSession,
    shared: &Arc<SharedInner>,
) -> Result<AdapterStatus> {
    let _ = refresh_pids_and_check_security(shared);
    let attached = shared.state.lock().attached_window.clone();
    if let Some(mut win) = attached {
        if !win32::is_window_valid(win.hwnd) || !uia_core::refresh_window_info(&mut win) {
            let mut guard = shared.state.lock();
            guard.attached_window = None;
            guard.active_group = None;
            guard.detected_member_count = None;
        } else if !win.is_iconic {
            if let Ok(tree) = session.dump_tree(win.hwnd, INTERNAL_SCAN_DEPTH) {
                reconcile_status_from_tree(shared, win, &tree);
            } else {
                shared.state.lock().attached_window = Some(win);
            }
        } else {
            shared.state.lock().attached_window = Some(win);
        }
    }
    Ok(shared.status_snapshot())
}

/// 基于只读 UIA 树核对当前激活会话标题、会话类型与成员数
pub(crate) fn reconcile_status_from_tree(
    shared: &SharedInner,
    win: WindowInfo,
    tree: &UiNodeSnapshot,
) {
    if !crate::parser::has_message_list_container(tree, win.rect, win.is_standalone_chat) {
        let mut guard = shared.state.lock();
        guard.attached_window = Some(win);
        guard.active_group = None;
        guard.detected_member_count = None;
        return;
    }

    let (live_hdr, live_cnt) =
        matcher::read_chat_header_from_tree(tree, win.rect, win.is_standalone_chat, None);
    let Some(hdr) = live_hdr.filter(|s| !s.trim().is_empty()) else {
        shared.state.lock().attached_window = Some(win);
        return;
    };

    let has_sidebar = matcher::has_group_member_sidebar(tree);
    let canon = super::runtime_state::canonical_storage_group_name(&hdr);
    let resolved_kind = resolve_live_chat_kind(shared, &canon, live_cnt.is_some(), has_sidebar);

    let mut guard = shared.state.lock();
    let live_kind = resolved_kind.unwrap_or(guard.active_chat_kind);
    let prior_matches = guard
        .active_group
        .as_deref()
        .is_some_and(|prev| matcher::group_title_matches(&hdr, prev));
    let prior_count = guard.detected_member_count;
    guard.attached_window = Some(win);
    guard.active_group = Some(hdr);
    guard.active_chat_kind = live_kind;
    guard.detected_member_count = if live_kind == ChatTargetKind::Group {
        live_cnt.or_else(|| prior_matches.then_some(prior_count).flatten())
    } else {
        None
    };
}

/// 根据实时标题与本地存储索引判定当前聊天窗格所属会话类型
fn resolve_live_chat_kind(
    shared: &SharedInner,
    canon: &str,
    has_live_count: bool,
    has_sidebar: bool,
) -> Option<ChatTargetKind> {
    if has_live_count || has_sidebar {
        return Some(ChatTargetKind::Group);
    }
    if shared.config.target.is_private_friend_allowed(canon) {
        return Some(ChatTargetKind::Private);
    }
    if shared
        .storage
        .get_group_by_name(canon)
        .ok()
        .flatten()
        .is_some()
    {
        return Some(ChatTargetKind::Group);
    }
    if shared
        .storage
        .get_friend_by_name(canon)
        .ok()
        .flatten()
        .is_some()
    {
        return Some(ChatTargetKind::Private);
    }
    None
}

/// 枚举并按优先级排序所有候选 QQNT 或白名单测试窗口
pub(crate) fn discover_windows(shared: &Arc<SharedInner>, pids: &[u32]) -> Vec<WindowInfo> {
    let mut windows =
        enumerate_candidate_windows(pids, &shared.config.interaction.extra_window_classes);
    if shared.allow_mock() {
        // 启用模拟测试白名单类名时仅保留显式配置的模拟窗口，避免单测或自测触碰宿主机真实 QQ 窗口
        windows.retain(|win| {
            shared
                .config
                .interaction
                .extra_window_classes
                .iter()
                .any(|cls| cls.trim().eq_ignore_ascii_case(&win.class_name))
        });
    }
    matcher::order_main_windows(&windows, &shared.profile)
}

/// 判断窗口尺寸是否符合未登录前的窄幅登录弹窗特征
pub(crate) fn is_narrow_login_window(win: &WindowInfo) -> bool {
    !win.is_iconic && profile::rect_is_valid(win.rect) && profile::rect_width(win.rect) < 500
}

/// 结合节点快照判断窗口是否正停留在登录或扫码引导界面
pub(crate) fn nodes_look_like_login_prompt(
    win: &WindowInfo,
    snapshots: &[&UiNodeSnapshot],
) -> bool {
    if is_narrow_login_window(win) {
        return true;
    }

    let has_login_button = snapshots.iter().any(|node| {
        node.is_enabled
            && !node.is_offscreen
            && node.control_type.eq_ignore_ascii_case(CONTROL_TYPE_BUTTON)
            && node.name.contains("登录")
    });
    if !has_login_button {
        return false;
    }

    let eff_rect = profile::effective_window_rect_from_snapshots(win.rect, snapshots);
    let has_left_list_session = snapshots.iter().any(|node| {
        node.is_enabled
            && !node.is_offscreen
            && (node
                .control_type
                .eq_ignore_ascii_case(CONTROL_TYPE_LIST_ITEM)
                || node
                    .control_type
                    .eq_ignore_ascii_case(CONTROL_TYPE_TREE_ITEM))
            && profile::is_in_left_session_pane(node.rect, eff_rect)
    });
    if has_left_list_session {
        return false;
    }

    let left_sessions = matcher::extract_sessions_from_snapshots(snapshots, eff_rect);
    !left_sessions
        .iter()
        .any(|session| !session.title.contains("登录"))
}

/// 判断已采集平铺节点列表是否呈现为登录引导界面
pub(crate) fn window_looks_like_login_prompt(win: &WindowInfo, nodes: &[CollectedNode]) -> bool {
    let snapshots: Vec<&UiNodeSnapshot> = nodes.iter().map(|node| &node.snapshot).collect();
    nodes_look_like_login_prompt(win, &snapshots)
}

/// 判断全部非独立聊天主窗口是否均仍处于登录界面
pub(crate) fn all_non_standalone_windows_are_login_prompts<F>(
    windows: &[WindowInfo],
    mut inspect_window: F,
) -> bool
where
    F: FnMut(&WindowInfo) -> bool,
{
    let mut main_count = 0usize;
    for win in windows {
        if win.is_standalone_chat {
            continue;
        }
        main_count += 1;
        if !is_narrow_login_window(win) && !inspect_window(win) {
            return false;
        }
    }
    main_count > 0
}

/// 通过即时采集检查候选主窗口是否全部停留在登录提示画面
pub(crate) fn any_window_is_login_prompt(session: &UiaSession, windows: &[WindowInfo]) -> bool {
    all_non_standalone_windows_are_login_prompts(windows, |win| {
        session
            .collect_flat(win.hwnd, INTERNAL_SCAN_DEPTH)
            .map(|nodes| window_looks_like_login_prompt(win, &nodes))
            .unwrap_or(false)
    })
}

/// 判断当前运行环境与配置是否允许自动拉起桌面端 QQ 客户端
pub(crate) fn should_auto_launch_qq(sandbox_isolated: bool, config: &AdapterConfig) -> bool {
    !sandbox_isolated
        && config.qq_client.auto_launch_or_wake
        && config.interaction.extra_window_classes.is_empty()
}

/// 确保桌面存在候选 QQ 窗口，缺失且允许自启时按配置唤醒客户端并等待加载就绪
pub(crate) fn ensure_candidate_windows(shared: &Arc<SharedInner>) -> Result<Vec<WindowInfo>> {
    let mut qq_pids = find_pids_by_names(shared.profile.process_names());
    let mut windows = discover_windows(shared, &qq_pids);

    let sandbox_isolated = {
        let mut guard = shared.state.lock();
        guard.qq_pids = qq_pids.clone();
        guard.resolved_qq_path =
            resolve_qq_executable(shared.config.qq_client.executable_path.as_deref(), &qq_pids);
        guard.sandbox_isolated
    };

    if windows.is_empty() && should_auto_launch_qq(sandbox_isolated, &shared.config) {
        let exe =
            resolve_qq_executable(shared.config.qq_client.executable_path.as_deref(), &qq_pids)
                .ok_or_else(|| {
                    AdapterError::WindowNotFound("QQ.exe executable path not found".to_string())
                })?;

        tracing::info!(exe = %exe, "未发现可见 QQNT 窗口，准备自动启动或唤醒客户端");
        let _spawned_pid = launch_or_wake_qq(&exe, &shared.config.qq_client.launch_args)?;

        let timeout = Duration::from_millis(shared.config.qq_client.startup_timeout_ms.max(300));
        let deadline = Instant::now() + timeout;
        let mut splash_deadline: Option<Instant> = None;
        while Instant::now() < deadline {
            if shared.shutdown.load(Ordering::Acquire) {
                break;
            }
            let poll_sleep_ms = if splash_deadline.is_some() { 350 } else { 300 };
            thread::sleep(Duration::from_millis(poll_sleep_ms));
            qq_pids = find_pids_by_names(shared.profile.process_names());
            windows = discover_windows(shared, &qq_pids);
            if !windows.is_empty() {
                for win in &windows {
                    if !win.is_iconic {
                        wake_chromium_accessibility(win.hwnd);
                    }
                }
                if windows.iter().all(is_narrow_login_window) {
                    let now = Instant::now();
                    let until = splash_deadline
                        .get_or_insert_with(|| (now + Duration::from_millis(4000)).min(deadline));
                    if now < *until {
                        continue;
                    }
                }
                break;
            }
        }

        let mut guard = shared.state.lock();
        guard.qq_pids = qq_pids.clone();
        guard.resolved_qq_path =
            resolve_qq_executable(shared.config.qq_client.executable_path.as_deref(), &qq_pids)
                .or(Some(exe));
    }

    Ok(windows)
}

/// 枚举当前候选 QQ 窗口处理器
pub(crate) fn handle_list_windows(shared: &Arc<SharedInner>) -> Result<Vec<WindowInfo>> {
    let pids = refresh_pids_and_check_security(shared)?;
    Ok(discover_windows(shared, &pids))
}

/// 确保窗口存在并在必要时执行强制唤醒与无障碍子树激活
pub(crate) fn handle_ensure_qq_window(
    shared: &Arc<SharedInner>,
    force_wake: bool,
) -> Result<Vec<WindowInfo>> {
    let mut qq_pids = refresh_pids_and_check_security(shared)?;
    let had_windows_before = !discover_windows(shared, &qq_pids).is_empty();
    let mut windows = ensure_candidate_windows(shared)?;
    let sandbox_isolated = shared.state.lock().sandbox_isolated;

    if force_wake && had_windows_before && should_auto_launch_qq(sandbox_isolated, &shared.config) {
        if let Some(exe) =
            resolve_qq_executable(shared.config.qq_client.executable_path.as_deref(), &qq_pids)
        {
            tracing::info!(exe = %exe, "通过单实例命令行参数强制唤醒主窗口");
            let _ = launch_or_wake_qq(&exe, &shared.config.qq_client.launch_args)?;
            thread::sleep(Duration::from_millis(400));
            qq_pids = find_pids_by_names(shared.profile.process_names());
            windows = discover_windows(shared, &qq_pids);
            let mut guard = shared.state.lock();
            guard.qq_pids = qq_pids.clone();
            guard.resolved_qq_path =
                resolve_qq_executable(shared.config.qq_client.executable_path.as_deref(), &qq_pids)
                    .or(Some(exe));
        }
    }

    for win in &windows {
        if !win.is_iconic {
            if force_wake {
                let _ = wake_chromium_accessibility_ext(win.hwnd, true);
            } else {
                wake_chromium_accessibility(win.hwnd);
            }
        }
    }
    Ok(windows)
}

/// 执行目标窗口的 Win32、MSAA 与 UIA 综合深度诊断
pub(crate) fn handle_diagnose_window(
    session: &UiaSession,
    shared: &Arc<SharedInner>,
    hwnd: Option<isize>,
) -> Result<WindowDiagnosticReport> {
    let pids = refresh_pids_and_check_security(shared)?;
    let target_hwnd = match hwnd {
        Some(handle) => handle,
        None => {
            let attached = shared.state.lock().attached_window.clone();
            attached
                .filter(|win| win32::is_window_valid(win.hwnd))
                .map(|win| win.hwnd)
                .or_else(|| {
                    discover_windows(shared, &pids)
                        .into_iter()
                        .next()
                        .map(|win| win.hwnd)
                })
                .ok_or_else(|| {
                    AdapterError::WindowNotFound(
                        "no candidate QQNT or MockQQ window found to diagnose".to_string(),
                    )
                })?
        }
    };

    let mut info = WindowInfo {
        hwnd: target_hwnd,
        pid: 0,
        process_name: String::new(),
        class_name: String::new(),
        title: String::new(),
        is_visible: true,
        is_iconic: false,
        rect: [0; 4],
        is_standalone_chat: false,
    };
    if uia_core::refresh_window_info(&mut info) && info.is_iconic {
        restore_no_activate(target_hwnd);
        thread::sleep(Duration::from_millis(120));
    }

    session.diagnose_hwnd(target_hwnd, &pids, true)
}

/// 导出指定或主候选 QQ 窗口的完整语义元素快照树
pub(crate) fn handle_inspect_tree(
    session: &UiaSession,
    shared: &Arc<SharedInner>,
    hwnd: Option<isize>,
    max_depth: usize,
) -> Result<UiNodeSnapshot> {
    let _pids = refresh_pids_and_check_security(shared)?;
    let mut target_win = match hwnd {
        Some(handle) => {
            let mut info = WindowInfo {
                hwnd: handle,
                pid: 0,
                process_name: String::new(),
                class_name: String::new(),
                title: String::new(),
                is_visible: true,
                is_iconic: false,
                rect: [0; 4],
                is_standalone_chat: false,
            };
            let _ = uia_core::refresh_window_info(&mut info);
            info
        }
        None => {
            let windows = ensure_candidate_windows(shared)?;
            let mut first = windows.into_iter().next().ok_or_else(|| {
                AdapterError::WindowNotFound(
                    "no candidate QQNT or MockQQ window found to inspect".to_string(),
                )
            })?;
            let _ = uia_core::refresh_window_info(&mut first);
            first
        }
    };

    if target_win.is_iconic {
        restore_no_activate(target_win.hwnd);
        thread::sleep(Duration::from_millis(120));
        let _ = uia_core::refresh_window_info(&mut target_win);
    }

    let (tree, _unocclude_guard) =
        matcher::wake_and_dump_tree_ext(session, target_win.hwnd, max_depth, true)?;
    Ok(tree)
}

/// 抓取指定或主候选 QQ 窗口的完整无边框 `PrintWindow` PNG 截图（只读不切群）
pub(crate) fn handle_capture_screenshot(
    shared: &Arc<SharedInner>,
    hwnd: Option<isize>,
) -> Result<Vec<u8>> {
    let pids = refresh_pids_and_check_security(shared)?;
    let target_hwnd = match hwnd {
        Some(handle) => handle,
        None => {
            let attached = shared.state.lock().attached_window.clone();
            attached
                .filter(|win| win32::is_window_valid(win.hwnd))
                .map(|win| win.hwnd)
                .or_else(|| {
                    discover_windows(shared, &pids)
                        .into_iter()
                        .next()
                        .map(|win| win.hwnd)
                })
                .ok_or_else(|| {
                    AdapterError::WindowNotFound(
                        "no candidate QQNT or MockQQ window found to capture screenshot"
                            .to_string(),
                    )
                })?
        }
    };

    uia_core::VerifiedWindowIdentity::verify_and_bind(target_hwnd, shared.allow_mock())?
        .verify_fast()?;

    let mut info = WindowInfo {
        hwnd: target_hwnd,
        pid: 0,
        process_name: String::new(),
        class_name: String::new(),
        title: String::new(),
        is_visible: true,
        is_iconic: false,
        rect: [0; 4],
        is_standalone_chat: false,
    };
    if uia_core::refresh_window_info(&mut info) && info.is_iconic {
        restore_no_activate(target_hwnd);
        thread::sleep(Duration::from_millis(120));
    }

    uia_core::capture_window_full_png(target_hwnd)
}

/// 当主窗口尚停留在登录窗口时返回统一引导提示错误文本
pub(crate) fn login_splash_error() -> AdapterError {
    AdapterError::WindowNotFound(LOGIN_SPLASH_ERROR.to_string())
}
