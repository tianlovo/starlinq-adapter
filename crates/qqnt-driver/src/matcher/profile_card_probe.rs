//! 策略模式二级兜底探测器：后台触发群标题按钮，跟踪并唤醒独立顶层“资料卡”弹窗或内嵌浮层以提取真实群号并安全关闭弹窗

use std::collections::HashSet;
use std::ffi::c_void;
use std::thread;
use std::time::Duration;

use common_types::constants::{
    MOCK_QQ_MAIN_CLASS, QQ_PROCESS_NAMES, SECURITY_KEYWORDS, UI_LABEL_CLOSE_BUTTON,
    UI_LABEL_PROFILE_CARD,
};
use common_types::{AdapterError, Result, UiNodeSnapshot, WindowInfo};
use uia_core::constants::{CONTROL_TYPE_BUTTON, CONTROL_TYPE_TEXT};
use uia_core::{
    enumerate_candidate_windows, find_pids_by_names, invoke_element_background_verified, win32,
    CollectedNode, ForegroundRestoreGuard, UiaSession, VerifiedWindowIdentity,
};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_CLOSE};

use super::drawer_inspector::{
    clean_node_text, collect_snapshot_candidates, extract_metadata_from_candidates,
    find_header_group_button, merge_candidate_samples, send_background_escape,
    DrawerInspectRequest, ExtractedDrawerMetadata,
};
use crate::constants::{
    DRAWER_CLOSE_BUTTON_KEYWORDS, DRAWER_CLOSE_WAIT_MS, INTERNAL_SCAN_DEPTH,
    IN_WINDOW_POPOVER_MIN_REL_X, POPUP_POLL_ATTEMPTS, POPUP_POLL_INTERVAL_MS,
};
use crate::profile;

/// 独立“资料卡”弹窗或内嵌浮层二级探测的结构化返回结果
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct ProfileCardProbeResult {
    pub(super) real_group_id: Option<i64>,
    pub(super) max_member_count: Option<i32>,
    pub(super) source: &'static str,
    pub(super) candidate_samples: Vec<String>,
}

/// 探测过程中观测并绑定身份的独立顶层“资料卡”窗口记录，用于完成后干净关闭
struct ObservedPopupRecord {
    hwnd: isize,
    identity: VerifiedWindowIdentity,
    nodes: Vec<CollectedNode>,
}

fn find_qq_pids() -> Vec<u32> {
    let names: Vec<String> = QQ_PROCESS_NAMES
        .iter()
        .copied()
        .map(str::to_string)
        .collect();
    find_pids_by_names(&names)
}

fn contains_security_keyword(title: &str) -> Option<&'static str> {
    SECURITY_KEYWORDS
        .iter()
        .copied()
        .find(|keyword| !keyword.is_empty() && title.contains(keyword))
}

fn is_popup_window_candidate(
    win: &WindowInfo,
    main_hwnd: isize,
    before_hwnds: &HashSet<isize>,
) -> bool {
    win.hwnd != main_hwnd
        && (!before_hwnds.contains(&win.hwnd) || win.title.trim() == UI_LABEL_PROFILE_CARD)
}

fn find_popup_close_button(nodes: &[CollectedNode]) -> Option<&CollectedNode> {
    nodes.iter().find(|collected| {
        let snap = &collected.snapshot;
        if !snap.is_enabled || snap.is_offscreen {
            return false;
        }
        let control_type = snap.control_type.trim();
        let is_clickable = control_type.eq_ignore_ascii_case(CONTROL_TYPE_BUTTON)
            || control_type.eq_ignore_ascii_case(CONTROL_TYPE_TEXT);
        if !is_clickable {
            return false;
        }
        let cleaned = clean_node_text(&snap.name);
        cleaned == UI_LABEL_CLOSE_BUTTON
            || DRAWER_CLOSE_BUTTON_KEYWORDS.iter().any(|kw| cleaned == *kw)
    })
}

fn close_popup_window_safely(
    main_hwnd: isize,
    popup_hwnd: isize,
    popup_identity: &VerifiedWindowIdentity,
    nodes: &[CollectedNode],
) {
    if popup_hwnd == main_hwnd || !win32::is_window_valid(popup_hwnd) {
        return;
    }

    if let Some(close_btn) = find_popup_close_button(nodes) {
        let _ = invoke_element_background_verified(popup_identity, &close_btn.element);
        thread::sleep(Duration::from_millis(DRAWER_CLOSE_WAIT_MS));
    }

    if win32::is_window_valid(popup_hwnd) {
        let _ = send_background_escape(popup_identity);
        thread::sleep(Duration::from_millis(DRAWER_CLOSE_WAIT_MS));
    }

    // 若按钮与 ESC 仍未能关闭独立“资料卡”窗口，则在身份核验通过后投递 `WM_CLOSE`
    if win32::is_window_valid(popup_hwnd) && popup_identity.verify_fast().is_ok() {
        unsafe {
            let _ = PostMessageW(
                Some(HWND(popup_hwnd as *mut c_void)),
                WM_CLOSE,
                WPARAM(0),
                LPARAM(0),
            );
        }
        thread::sleep(Duration::from_millis(DRAWER_CLOSE_WAIT_MS));
    }
}

fn collect_qq_pids_with_hwnd(hwnd: isize) -> Vec<u32> {
    let mut pids = find_qq_pids();
    let pid = win32::get_window_pid(hwnd);
    if pid != 0 && !pids.contains(&pid) {
        pids.push(pid);
    }
    pids
}

fn cleanup_stale_profile_cards(session: &UiaSession, main_hwnd: isize, allow_mock: bool) {
    let qq_pids = collect_qq_pids_with_hwnd(main_hwnd);
    let _fg = ForegroundRestoreGuard::capture(&qq_pids);
    let windows = enumerate_candidate_windows(&qq_pids, &[]);
    for win in windows {
        if win.hwnd != main_hwnd && win.title.trim() == UI_LABEL_PROFILE_CARD {
            if let Ok(popup_identity) =
                VerifiedWindowIdentity::verify_and_bind(win.hwnd, allow_mock)
            {
                let nodes = super::wake_and_collect(session, win.hwnd, INTERNAL_SCAN_DEPTH)
                    .unwrap_or_default();
                close_popup_window_safely(main_hwnd, win.hwnd, &popup_identity, &nodes);
            }
        }
    }
}

fn inspect_popup_window_once(
    session: &UiaSession,
    win: &WindowInfo,
    allow_mock: bool,
) -> Result<
    Option<(
        VerifiedWindowIdentity,
        Vec<CollectedNode>,
        ExtractedDrawerMetadata,
    )>,
> {
    if let Some(sec_kw) = contains_security_keyword(&win.title) {
        return Err(AdapterError::SecurityPromptDetected {
            title: win.title.clone(),
            detail: format!(
                "profile card probe encountered security window keyword '{sec_kw}' on hwnd=0x{:x}",
                win.hwnd
            ),
        });
    }

    let Ok(popup_identity) = VerifiedWindowIdentity::verify_and_bind(win.hwnd, allow_mock) else {
        return Ok(None);
    };

    let Ok((popup_nodes, _guard)) =
        super::wake_and_collect_ext(session, win.hwnd, INTERNAL_SCAN_DEPTH, true)
    else {
        return Ok(None);
    };
    let popup_snaps: Vec<&UiNodeSnapshot> = popup_nodes.iter().map(|n| &n.snapshot).collect();
    let eff_popup_rect = profile::effective_window_rect_from_snapshots(win.rect, &popup_snaps);
    let candidates = collect_snapshot_candidates(&[], &popup_snaps, eff_popup_rect, 0.0);
    let meta = extract_metadata_from_candidates(&candidates);

    Ok(Some((popup_identity, popup_nodes, meta)))
}

fn record_observed_popup(
    observed: &mut Vec<ObservedPopupRecord>,
    hwnd: isize,
    identity: VerifiedWindowIdentity,
    nodes: Vec<CollectedNode>,
) {
    if let Some(existing) = observed.iter_mut().find(|item| item.hwnd == hwnd) {
        existing.identity = identity;
        existing.nodes = nodes;
    } else {
        observed.push(ObservedPopupRecord {
            hwnd,
            identity,
            nodes,
        });
    }
}

/// 通过点击聊天顶栏群标题唤醒独立“资料卡”弹窗或内嵌浮层并提取真实群号，无论成功与否均负责清理关闭弹窗
pub(super) fn probe_profile_card_group_id(
    session: &UiaSession,
    identity: &VerifiedWindowIdentity,
    request: &DrawerInspectRequest<'_>,
    before_nodes: &[CollectedNode],
) -> Result<ProfileCardProbeResult> {
    let allow_mock = request.window.class_name == MOCK_QQ_MAIN_CLASS;
    let qq_pids = collect_qq_pids_with_hwnd(request.window.hwnd);
    let _fg = ForegroundRestoreGuard::capture(&qq_pids);
    cleanup_stale_profile_cards(session, request.window.hwnd, allow_mock);

    let before_hwnds: HashSet<isize> = enumerate_candidate_windows(&qq_pids, &[])
        .into_iter()
        .map(|win| win.hwnd)
        .collect();

    let before_snaps: Vec<&UiNodeSnapshot> = before_nodes.iter().map(|n| &n.snapshot).collect();
    let eff_rect =
        profile::effective_window_rect_from_snapshots(request.window.rect, &before_snaps);

    let Some(header_button) = find_header_group_button(
        before_nodes,
        eff_rect,
        request.window.is_standalone_chat,
        request.group_name,
    ) else {
        return Ok(ProfileCardProbeResult {
            source: "profile_card_button_missing",
            ..Default::default()
        });
    };

    identity.verify_fast()?;
    if invoke_element_background_verified(identity, &header_button.element).is_err() {
        return Ok(ProfileCardProbeResult {
            source: "profile_card_open_failed",
            ..Default::default()
        });
    }

    let mut result = ProfileCardProbeResult {
        source: "profile_card_miss",
        ..Default::default()
    };
    let mut observed_popups: Vec<ObservedPopupRecord> = Vec::new();
    let mut main_had_popover = false;

    for _ in 0..POPUP_POLL_ATTEMPTS {
        thread::sleep(Duration::from_millis(POPUP_POLL_INTERVAL_MS));

        let cur_pids = find_qq_pids();
        let cur_windows = enumerate_candidate_windows(&cur_pids, &[]);
        for win in cur_windows
            .into_iter()
            .filter(|win| is_popup_window_candidate(win, request.window.hwnd, &before_hwnds))
        {
            if let Some((popup_identity, popup_nodes, meta)) =
                inspect_popup_window_once(session, &win, allow_mock)?
            {
                record_observed_popup(&mut observed_popups, win.hwnd, popup_identity, popup_nodes);
                if result.max_member_count.is_none() {
                    result.max_member_count = meta.max_member_count;
                }
                merge_candidate_samples(&mut result.candidate_samples, &meta.candidate_samples);
                if meta.real_group_id.is_some() {
                    result.real_group_id = meta.real_group_id;
                    result.source = "profile_card_window";
                    break;
                }
            }
        }

        if result.real_group_id.is_some() {
            break;
        }

        // 若未产生独立顶层弹窗，同时检查主窗口内是否弹出群资料浮层
        if let Ok(after_main) =
            super::wake_and_collect(session, request.window.hwnd, INTERNAL_SCAN_DEPTH)
        {
            let after_snaps: Vec<&UiNodeSnapshot> =
                after_main.iter().map(|n| &n.snapshot).collect();
            let in_win_candidates = collect_snapshot_candidates(
                &before_snaps,
                &after_snaps,
                request.window.rect,
                IN_WINDOW_POPOVER_MIN_REL_X,
            );
            if !in_win_candidates.is_empty() {
                main_had_popover = true;
            }
            let meta = extract_metadata_from_candidates(&in_win_candidates);
            if result.max_member_count.is_none() {
                result.max_member_count = meta.max_member_count;
            }
            merge_candidate_samples(&mut result.candidate_samples, &meta.candidate_samples);
            if meta.real_group_id.is_some() {
                result.real_group_id = meta.real_group_id;
                result.source = "profile_card_popover";
                break;
            }
        }
    }

    for popup in &observed_popups {
        close_popup_window_safely(
            request.window.hwnd,
            popup.hwnd,
            &popup.identity,
            &popup.nodes,
        );
    }
    if main_had_popover {
        let _ = send_background_escape(identity);
        thread::sleep(Duration::from_millis(DRAWER_CLOSE_WAIT_MS));
    }

    Ok(result)
}
