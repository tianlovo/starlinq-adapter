//! 独占原生桌面交互动作执行器：真实文件粘贴上传（`CF_HDROP`）、魔法猜拳/骰子及私聊窗口抖动

use std::path::Path;
use std::thread;
use std::time::Duration;

use common_types::SendShortcut;
use common_types::{AdapterError, ChatTargetKind, InputMode, Result, UiNodeSnapshot, WindowInfo};
use uia_core::constants::{
    CONTROL_TYPE_BUTTON, CONTROL_TYPE_IMAGE, CONTROL_TYPE_MENU_ITEM, CONTROL_TYPE_TAB_ITEM,
};
use uia_core::{
    paste_clipboard_verified, submit_only_focus_restore_verified, write_files_to_clipboard_hdrop,
    ClipboardPasteSpec, ClipboardRestoreGuard, ComposerSubmitTarget, SimulatedClickKind,
    UiNodeHandle, UiaSession, VerifiedWindowIdentity,
};

use super::plan::StandaloneAction;
use super::popup_helpers::{
    click_matched_target_verified, click_point_verified, click_rect_verified,
    collect_visible_bubbles, composer_append_click_point, dismiss_popups_best_effort,
    find_overlay_targets_in_snapshot, find_popup_clickable_by_keywords, locate_toolbar_button,
    scan_same_pid_popup_trees, MatchedUiTarget,
};
use crate::constants::{CONTROL_TYPE_CUSTOM, INTERNAL_SCAN_DEPTH};
use crate::driver::send_handlers;
use crate::matcher;
use crate::parser;
use crate::profile;

/// 独占原生桌面动作执行上下文（Parameter Object）
pub(crate) struct StandaloneExecContext<'a> {
    pub(crate) session: &'a UiaSession,
    pub(crate) identity: &'a VerifiedWindowIdentity,
    pub(crate) win: &'a WindowInfo,
    pub(crate) active_target: &'a str,
    pub(crate) target_kind: ChatTargetKind,
    pub(crate) pids: &'a [u32],
    pub(crate) editor: &'a UiNodeHandle,
    pub(crate) send_button: Option<&'a UiNodeHandle>,
    pub(crate) editor_rect: [i32; 4],
    pub(crate) shortcut: SendShortcut,
}

/// 生成与目标聊天类型对应的 OneBot 动作名称字符串
fn action_name(target_kind: ChatTargetKind) -> String {
    match target_kind {
        ChatTargetKind::Group => "send_group_msg".to_string(),
        ChatTargetKind::Private => "send_private_msg".to_string(),
    }
}

/// 枚举当前同进程下除主窗口外的可见浮层弹窗句柄及类标题元数据
fn list_visible_popup_windows(
    exec_ctx: &StandaloneExecContext<'_>,
) -> Vec<(isize, String, String)> {
    let main_pid = uia_core::win32::get_window_pid(exec_ctx.win.hwnd);
    let query_pids: &[u32] = if main_pid != 0 {
        std::slice::from_ref(&main_pid)
    } else {
        exec_ctx.pids
    };
    uia_core::win32::enumerate_all_windows_for_pids(query_pids)
        .into_iter()
        .filter(|w| {
            w.hwnd != exec_ctx.win.hwnd
                && w.is_visible
                && !w.is_iconic
                && w.dwm_cloaked == 0
                && (w.rect[2] - w.rect[0]) >= 40
                && (w.rect[3] - w.rect[1]) >= 24
        })
        .map(|w| (w.hwnd, w.class_name, w.title))
        .collect()
}

fn has_separate_popup(exec_ctx: &StandaloneExecContext<'_>) -> bool {
    !list_visible_popup_windows(exec_ctx).is_empty()
}

fn execute_send_file(exec_ctx: &StandaloneExecContext<'_>, path: &Path) -> Result<(bool, bool)> {
    exec_ctx.identity.verify_fast()?;
    send_handlers::clear_residual_editor_if_needed(exec_ctx.identity, exec_ctx.editor, true)?;

    if let Some((x, y)) = composer_append_click_point(exec_ctx.editor_rect) {
        click_point_verified(exec_ctx.identity, x, y, SimulatedClickKind::Left, false)?;
        thread::sleep(Duration::from_millis(80));
    }

    {
        let _clip = ClipboardRestoreGuard::capture()?;
        write_files_to_clipboard_hdrop(std::slice::from_ref(&path.to_path_buf()))?;
        paste_clipboard_verified(
            exec_ctx.identity,
            &ClipboardPasteSpec {
                editor: exec_ctx.editor,
                send_button: exec_ctx.send_button,
                mode: InputMode::FocusRestoreUnicode,
                shortcut: exec_ctx.shortcut,
                press_enter: false,
                settle_delay_ms: 350,
            },
        )?;
    }

    thread::sleep(Duration::from_millis(320));
    if let Some(confirm_btn) = find_popup_clickable_by_keywords(
        exec_ctx.session,
        exec_ctx.win.hwnd,
        exec_ctx.pids,
        &["发送(", "发送文件", "确定", "发送"],
        true,
    ) {
        click_rect_verified(
            exec_ctx.identity,
            confirm_btn.rect,
            SimulatedClickKind::Left,
            true,
        )?;
    } else {
        let submit_target = ComposerSubmitTarget {
            window_hwnd: exec_ctx.win.hwnd,
            editor: exec_ctx.editor,
            send_button: exec_ctx.send_button,
            shortcut: exec_ctx.shortcut,
        };
        submit_only_focus_restore_verified(exec_ctx.identity, &submit_target)?;
    }

    Ok((true, true))
}

fn close_emoji_overlay_best_effort(exec_ctx: &StandaloneExecContext<'_>) {
    dismiss_popups_best_effort(exec_ctx.identity);
    if has_separate_popup(exec_ctx) {
        dismiss_popups_best_effort(exec_ctx.identity);
    }
    if let Some((x, y)) = composer_append_click_point(exec_ctx.editor_rect) {
        let _ = click_point_verified(exec_ctx.identity, x, y, SimulatedClickKind::Left, false);
    }
}

type PopupSnapshots = [(isize, String, String, UiNodeSnapshot)];

fn is_emoji_item_candidate(node: &UiNodeSnapshot) -> bool {
    if !node.is_enabled {
        return false;
    }
    let ct = node.control_type.trim();
    let is_compact_group = ct.eq_ignore_ascii_case("Group")
        && profile::rect_width(node.rect) <= 160
        && profile::rect_height(node.rect) <= 120;
    is_compact_group
        || ct.eq_ignore_ascii_case(CONTROL_TYPE_BUTTON)
        || ct.eq_ignore_ascii_case(CONTROL_TYPE_MENU_ITEM)
        || ct.eq_ignore_ascii_case(CONTROL_TYPE_CUSTOM)
        || ct.eq_ignore_ascii_case(CONTROL_TYPE_IMAGE)
}

fn find_emoji_hit_in_windows(
    windows: &PopupSnapshots,
    keywords: &[&str],
) -> Option<MatchedUiTarget> {
    for (hwnd, _cls, _title, snap) in windows {
        let hits = find_overlay_targets_in_snapshot(*hwnd, snap, &|node| {
            if !is_emoji_item_candidate(node) {
                return false;
            }
            let nm_lower = node.name.trim().to_ascii_lowercase();
            let id_lower = node.automation_id.trim().to_ascii_lowercase();
            keywords.iter().any(|kw| {
                let k = kw.to_ascii_lowercase();
                nm_lower.contains(&k) || id_lower.contains(&k)
            })
        });
        if let Some(hit) = hits.into_iter().last() {
            return Some(hit);
        }
    }
    None
}

fn find_magic_category_tab_in_windows(windows: &PopupSnapshots) -> Option<MatchedUiTarget> {
    let tab_keywords = ["魔法表情", "超级表情", "魔法", "互动"];
    for (hwnd, _cls, _title, snap) in windows {
        let hits = find_overlay_targets_in_snapshot(*hwnd, snap, &|node| {
            if !node.is_enabled {
                return false;
            }
            let ct = node.control_type.trim();
            if !ct.eq_ignore_ascii_case(CONTROL_TYPE_BUTTON)
                && !ct.eq_ignore_ascii_case(CONTROL_TYPE_TAB_ITEM)
            {
                return false;
            }
            let nm_lower = node.name.trim().to_ascii_lowercase();
            let id_lower = node.automation_id.trim().to_ascii_lowercase();
            tab_keywords.iter().any(|kw| {
                let k = kw.to_ascii_lowercase();
                nm_lower.contains(&k) || id_lower.contains(&k)
            })
        });
        if let Some(hit) = hits.into_iter().last() {
            return Some(hit);
        }
    }
    None
}

fn emoji_scroll_tail_signature(
    windows: &PopupSnapshots,
    emoji_btn_rect: [i32; 4],
) -> Vec<(String, [i32; 4])> {
    let mut items = Vec::new();
    for (hwnd, _cls, _title, snap) in windows {
        let hits = find_overlay_targets_in_snapshot(*hwnd, snap, &|node| {
            node.control_type.trim().eq_ignore_ascii_case("Group")
                && !node.name.trim().is_empty()
                && profile::rect_width(node.rect) <= 160
                && profile::rect_height(node.rect) <= 120
                && node.rect[3] <= emoji_btn_rect[1] + 10
        });
        items.extend(hits.into_iter().map(|h| (h.name, h.rect)));
    }
    if items.len() > 3 {
        items.split_off(items.len() - 3)
    } else {
        items
    }
}

fn click_emoji_and_close(
    exec_ctx: &StandaloneExecContext<'_>,
    hit: &MatchedUiTarget,
) -> Result<(bool, bool)> {
    click_matched_target_verified(exec_ctx.identity, hit, SimulatedClickKind::Left, false)?;
    thread::sleep(Duration::from_millis(160));
    close_emoji_overlay_best_effort(exec_ctx);
    Ok((true, true))
}

fn execute_magic_emoji(
    exec_ctx: &StandaloneExecContext<'_>,
    label: &str,
    keywords: &[&str],
) -> Result<(bool, bool)> {
    let (tree, _guard) = matcher::wake_and_dump_tree_ext(
        exec_ctx.session,
        exec_ctx.win.hwnd,
        INTERNAL_SCAN_DEPTH,
        false,
    )?;
    let emoji_btn = locate_toolbar_button(&tree, exec_ctx.editor_rect, &["表情", "emoji", "Emoji"])
        .ok_or_else(|| AdapterError::UnsupportedAction {
            action: action_name(exec_ctx.target_kind),
            reason: format!("could not locate QQNT emoji panel button to trigger native {label}"),
        })?;

    exec_ctx.identity.verify_fast()?;
    click_rect_verified(
        exec_ctx.identity,
        emoji_btn.rect,
        SimulatedClickKind::Left,
        false,
    )?;
    thread::sleep(Duration::from_millis(240));

    let mut windows = scan_same_pid_popup_trees(
        exec_ctx.session,
        exec_ctx.win.hwnd,
        exec_ctx.pids,
        INTERNAL_SCAN_DEPTH,
    );
    if let Some(direct_hit) = find_emoji_hit_in_windows(&windows, keywords) {
        return click_emoji_and_close(exec_ctx, &direct_hit);
    }

    if let Some(tab_hit) = find_magic_category_tab_in_windows(&windows) {
        click_matched_target_verified(
            exec_ctx.identity,
            &tab_hit,
            SimulatedClickKind::Left,
            false,
        )?;
        thread::sleep(Duration::from_millis(240));
        windows = scan_same_pid_popup_trees(
            exec_ctx.session,
            exec_ctx.win.hwnd,
            exec_ctx.pids,
            INTERNAL_SCAN_DEPTH,
        );
        if let Some(magic_hit) = find_emoji_hit_in_windows(&windows, keywords) {
            return click_emoji_and_close(exec_ctx, &magic_hit);
        }
    }

    // 向下滚动默认表情或超级表情列表，并在触底无变化时提前结束避免无效重复扫描
    let scroll_x = emoji_btn.rect[0] + 180;
    let scroll_y = (emoji_btn.rect[1] - 150).max(exec_ctx.win.rect[1] + 140);
    let mut prev_tail = emoji_scroll_tail_signature(&windows, emoji_btn.rect);
    for _ in 0..12 {
        if click_point_verified(
            exec_ctx.identity,
            scroll_x,
            scroll_y,
            SimulatedClickKind::WheelDown,
            true,
        )
        .is_err()
        {
            break;
        }
        thread::sleep(Duration::from_millis(180));
        let scrolled_windows = scan_same_pid_popup_trees(
            exec_ctx.session,
            exec_ctx.win.hwnd,
            exec_ctx.pids,
            INTERNAL_SCAN_DEPTH,
        );
        if let Some(scrolled_hit) = find_emoji_hit_in_windows(&scrolled_windows, keywords) {
            return click_emoji_and_close(exec_ctx, &scrolled_hit);
        }
        let next_tail = emoji_scroll_tail_signature(&scrolled_windows, emoji_btn.rect);
        if !next_tail.is_empty() && next_tail == prev_tail {
            break;
        }
        prev_tail = next_tail;
    }

    close_emoji_overlay_best_effort(exec_ctx);
    Err(AdapterError::UnsupportedAction {
        action: action_name(exec_ctx.target_kind),
        reason: format!(
            "QQNT 9.9.31 desktop emoji panel did not expose a clickable '{label}' control in UIA"
        ),
    })
}

fn execute_private_shake(exec_ctx: &StandaloneExecContext<'_>) -> Result<(bool, bool)> {
    let (tree, _guard) = matcher::wake_and_dump_tree_ext(
        exec_ctx.session,
        exec_ctx.win.hwnd,
        INTERNAL_SCAN_DEPTH,
        false,
    )?;
    let before_notices = parser::extract_system_notices_from_tree(
        &tree,
        exec_ctx.win.rect,
        exec_ctx.win.is_standalone_chat,
        exec_ctx.active_target,
    );
    let before_last_self = collect_visible_bubbles(
        &tree,
        exec_ctx.win.rect,
        exec_ctx.win.is_standalone_chat,
        exec_ctx.active_target,
    )
    .into_iter()
    .rev()
    .find(|b| b.is_self)
    .map(|b| b.content);
    let btn = locate_toolbar_button(&tree, exec_ctx.editor_rect, &["窗口抖动", "抖一抖", "抖动"])
        .ok_or_else(|| AdapterError::UnsupportedAction {
        action: action_name(exec_ctx.target_kind),
        reason:
            "QQNT 9.9.31 desktop private chat toolbar does not expose a native Window Shake control"
                .to_string(),
    })?;

    exec_ctx.identity.verify_fast()?;
    click_rect_verified(exec_ctx.identity, btn.rect, SimulatedClickKind::Left, false)?;

    for wait_ms in [240u64, 300, 360] {
        thread::sleep(Duration::from_millis(wait_ms));
        if let Ok((after_tree, _)) = matcher::wake_and_dump_tree_ext(
            exec_ctx.session,
            exec_ctx.win.hwnd,
            INTERNAL_SCAN_DEPTH,
            false,
        ) {
            let after_notices = parser::extract_system_notices_from_tree(
                &after_tree,
                exec_ctx.win.rect,
                exec_ctx.win.is_standalone_chat,
                exec_ctx.active_target,
            );
            let last_notice = after_notices.last().map(|n| n.text.as_str()).unwrap_or("");
            let prev_notice = before_notices.last().map(|n| n.text.as_str()).unwrap_or("");
            if (after_notices.len() > before_notices.len() || last_notice != prev_notice)
                && (last_notice.contains("频繁") || last_notice.contains("失败"))
            {
                return Err(AdapterError::InputRejected(
                    "QQNT rate-limited private Window Shake".to_string(),
                ));
            }
            let after_last_self = collect_visible_bubbles(
                &after_tree,
                exec_ctx.win.rect,
                exec_ctx.win.is_standalone_chat,
                exec_ctx.active_target,
            )
            .into_iter()
            .rev()
            .find(|b| b.is_self)
            .map(|b| b.content);
            if after_notices.len() > before_notices.len()
                || (last_notice != prev_notice && last_notice.contains("抖"))
                || (after_last_self.is_some() && after_last_self != before_last_self)
            {
                return Ok((true, true));
            }
        }
    }

    Ok((true, true))
}

/// 执行独占原生桌面交互动作（文件、视频、魔法表情、窗口抖动），禁止任何降级文字替代
pub(crate) fn execute_standalone_action(
    exec_ctx: &StandaloneExecContext<'_>,
    action: &StandaloneAction,
) -> Result<(bool, bool)> {
    match action {
        StandaloneAction::File { path } | StandaloneAction::Video { path } => {
            execute_send_file(exec_ctx, path)
        }
        StandaloneAction::Rps => {
            execute_magic_emoji(exec_ctx, "猜拳", &["猜拳", "石头剪刀布", "剪刀石头布"])
        }
        StandaloneAction::Dice => {
            execute_magic_emoji(exec_ctx, "掷骰子", &["随机骰子", "掷骰子", "骰子", "色子"])
        }
        StandaloneAction::PrivateShake => execute_private_shake(exec_ctx),
    }
}
