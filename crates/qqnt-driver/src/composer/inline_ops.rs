//! 原生富媒体内联编排执行器：依次完成真实右键quote回复、表情弹窗点击、`@` 成员点选、文本输入与剪贴板贴图

use std::thread;
use std::time::Duration;

use common_types::SendShortcut;
use common_types::{AdapterError, ChatTargetKind, InputMode, Result, UiNodeSnapshot, WindowInfo};
use uia_core::constants::CONTROL_TYPE_EDIT;
use uia_core::{
    paste_clipboard_verified, submit_only_focus_restore_verified,
    type_text_focus_restore_in_active_editor, write_bgra_to_clipboard_dib, ClipboardPasteSpec,
    ClipboardRestoreGuard, ComposerSubmitTarget, SimulatedClickKind, TypingDelayRange,
    UiNodeHandle, UiaSession, VerifiedWindowIdentity,
};

use super::plan::InlineStep;
use super::popup_helpers::{
    click_point_verified, click_rect_verified, collect_visible_bubbles,
    composer_append_click_point, dismiss_popups_best_effort, find_overlay_targets_in_snapshot,
    find_popup_clickable_by_keywords, find_targets_in_snapshot, locate_toolbar_button,
    locate_visible_message_bubble, scan_same_pid_popup_trees,
};
use crate::constants::INTERNAL_SCAN_DEPTH;
use crate::driver::send_handlers;
use crate::matcher;
use crate::profile;

/// 富媒体原生内联步骤串行执行上下文（Parameter Object，保持函数参数简洁）
pub(crate) struct RichComposeContext<'a> {
    pub(crate) session: &'a UiaSession,
    pub(crate) identity: &'a VerifiedWindowIdentity,
    pub(crate) win: &'a WindowInfo,
    pub(crate) active_target: &'a str,
    pub(crate) target_kind: ChatTargetKind,
    pub(crate) pids: &'a [u32],
    pub(crate) editor: &'a UiNodeHandle,
    pub(crate) send_button: Option<&'a UiNodeHandle>,
    pub(crate) editor_rect: [i32; 4],
    pub(crate) send_rect: Option<[i32; 4]>,
    pub(crate) shortcut: SendShortcut,
    pub(crate) delay_range: TypingDelayRange,
}

fn action_name(target_kind: ChatTargetKind) -> String {
    match target_kind {
        ChatTargetKind::Group => "send_group_msg".to_string(),
        ChatTargetKind::Private => "send_private_msg".to_string(),
    }
}

fn focus_composer_end(exec_ctx: &RichComposeContext<'_>) -> Result<()> {
    if let Some((x, y)) = composer_append_click_point(exec_ctx.editor_rect) {
        click_point_verified(exec_ctx.identity, x, y, SimulatedClickKind::Left, false)?;
        thread::sleep(Duration::from_millis(70));
    }
    Ok(())
}

fn dump_main_tree(exec_ctx: &RichComposeContext<'_>) -> Result<UiNodeSnapshot> {
    let (tree, _guard) = matcher::wake_and_dump_tree_ext(
        exec_ctx.session,
        exec_ctx.win.hwnd,
        INTERNAL_SCAN_DEPTH,
        false,
    )?;
    Ok(tree)
}

fn execute_attach_reply(
    exec_ctx: &RichComposeContext<'_>,
    sender_name: &str,
    excerpt: &str,
    raw_text: &str,
) -> Result<()> {
    let tree = dump_main_tree(exec_ctx)?;
    let bubbles = collect_visible_bubbles(
        &tree,
        exec_ctx.win.rect,
        exec_ctx.win.is_standalone_chat,
        exec_ctx.active_target,
    );
    let bubble_rect = locate_visible_message_bubble(&bubbles, sender_name, excerpt, raw_text)
        .ok_or_else(|| {
            AdapterError::InputRejected(format!(
                "reply target bubble (sender='{sender_name}', excerpt='{excerpt}') is not visible in current chat viewport"
            ))
        })?;

    exec_ctx.identity.verify_fast()?;
    click_rect_verified(
        exec_ctx.identity,
        bubble_rect,
        SimulatedClickKind::Right,
        false,
    )?;

    for wait_ms in [120u64, 140, 160, 180] {
        thread::sleep(Duration::from_millis(wait_ms));
        if let Some(menu_hit) = find_popup_clickable_by_keywords(
            exec_ctx.session,
            exec_ctx.win.hwnd,
            exec_ctx.pids,
            &["回复", "引用"],
            true,
        ) {
            click_rect_verified(
                exec_ctx.identity,
                menu_hit.rect,
                SimulatedClickKind::Left,
                true,
            )?;
            thread::sleep(Duration::from_millis(140));
            return Ok(());
        }
    }

    dismiss_popups_best_effort(exec_ctx.identity);
    Err(AdapterError::UnsupportedAction {
        action: action_name(exec_ctx.target_kind),
        reason:
            "right-click bubble context menu did not expose a clickable '回复' item in QQNT UIA"
                .to_string(),
    })
}

fn match_face_in_popups(exec_ctx: &RichComposeContext<'_>, face_name: &str) -> Option<[i32; 4]> {
    let slash_form = format!("/{face_name}");
    let bracket_form = format!("[{face_name}]");
    let windows = scan_same_pid_popup_trees(
        exec_ctx.session,
        exec_ctx.win.hwnd,
        exec_ctx.pids,
        INTERNAL_SCAN_DEPTH,
    );
    for (hwnd, _cls, _title, snap) in &windows {
        let exact_hits = find_overlay_targets_in_snapshot(*hwnd, snap, &|node| {
            if !node.is_enabled {
                return false;
            }
            let ct = node.control_type.trim();
            if ct.eq_ignore_ascii_case(CONTROL_TYPE_EDIT) {
                return false;
            }
            if node.rect[1] >= exec_ctx.editor_rect[1] - 4
                && node.rect[3] <= exec_ctx.editor_rect[3] + 4
            {
                return false;
            }
            let width = profile::rect_width(node.rect);
            let height = profile::rect_height(node.rect);
            if width > 140 || height > 140 {
                return false;
            }
            let nm = node.name.trim();
            nm.eq_ignore_ascii_case(face_name)
                || nm.eq_ignore_ascii_case(&slash_form)
                || nm.eq_ignore_ascii_case(&bracket_form)
        });
        if let Some(hit) = exact_hits.into_iter().next() {
            return Some(hit.rect);
        }
    }
    None
}

fn has_separate_popup_window(exec_ctx: &RichComposeContext<'_>) -> bool {
    uia_core::win32::enumerate_all_windows_for_pids(exec_ctx.pids)
        .into_iter()
        .any(|w| {
            w.hwnd != exec_ctx.win.hwnd
                && w.is_visible
                && !w.is_iconic
                && w.dwm_cloaked == 0
                && w.rect[2] > w.rect[0]
                && w.rect[3] > w.rect[1]
        })
}

fn find_matched_face_with_scroll(
    exec_ctx: &RichComposeContext<'_>,
    emoji_btn_rect: [i32; 4],
    face_name: &str,
) -> Option<[i32; 4]> {
    for wait_ms in [180u64, 180, 200, 220] {
        thread::sleep(Duration::from_millis(wait_ms));
        if let Some(face_rect) = match_face_in_popups(exec_ctx, face_name) {
            return Some(face_rect);
        }
    }

    let scroll_x = emoji_btn_rect[0] + 180;
    let scroll_y = (emoji_btn_rect[1] - 150).max(exec_ctx.win.rect[1] + 140);
    let scroll_steps = [
        SimulatedClickKind::WheelUp,
        SimulatedClickKind::WheelDown,
        SimulatedClickKind::WheelDown,
        SimulatedClickKind::WheelDown,
    ];
    for kind in scroll_steps {
        if click_point_verified(exec_ctx.identity, scroll_x, scroll_y, kind, true).is_err() {
            break;
        }
        thread::sleep(Duration::from_millis(160));
        if let Some(face_rect) = match_face_in_popups(exec_ctx, face_name) {
            return Some(face_rect);
        }
    }
    None
}

fn execute_insert_face(exec_ctx: &RichComposeContext<'_>, face_name: &str) -> Result<()> {
    let tree = dump_main_tree(exec_ctx)?;
    let emoji_btn = locate_toolbar_button(&tree, exec_ctx.editor_rect, &["表情", "emoji", "Emoji"])
        .ok_or_else(|| AdapterError::UnsupportedAction {
            action: action_name(exec_ctx.target_kind),
            reason: "could not locate QQNT '表情' toolbar button above composer".to_string(),
        })?;

    exec_ctx.identity.verify_fast()?;
    click_rect_verified(
        exec_ctx.identity,
        emoji_btn.rect,
        SimulatedClickKind::Left,
        false,
    )?;

    if let Some(face_rect) = find_matched_face_with_scroll(exec_ctx, emoji_btn.rect, face_name) {
        click_rect_verified(exec_ctx.identity, face_rect, SimulatedClickKind::Left, true)?;
        thread::sleep(Duration::from_millis(120));
        if has_separate_popup_window(exec_ctx) {
            dismiss_popups_best_effort(exec_ctx.identity);
        }
        let _ = focus_composer_end(exec_ctx);
        return Ok(());
    }

    dismiss_popups_best_effort(exec_ctx.identity);
    let _ = focus_composer_end(exec_ctx);
    Err(AdapterError::UnsupportedAction {
        action: action_name(exec_ctx.target_kind),
        reason: format!("QQNT emoji panel did not expose clickable face item '{face_name}' in UIA"),
    })
}

fn find_at_popup_container(node: &UiNodeSnapshot) -> Option<&UiNodeSnapshot> {
    if node.is_offscreen {
        return None;
    }
    if node.control_type.trim().eq_ignore_ascii_case("Window") && node.name.trim() == "成员列表"
    {
        return Some(node);
    }
    for child in &node.children {
        if let Some(found) = find_at_popup_container(child) {
            return Some(found);
        }
    }
    None
}

fn match_at_candidate_in_popups(
    exec_ctx: &RichComposeContext<'_>,
    display_name: &str,
    is_all: bool,
) -> Option<[i32; 4]> {
    let target_lower = if is_all {
        "全体成员".to_string()
    } else {
        display_name.trim().to_ascii_lowercase()
    };
    let windows = scan_same_pid_popup_trees(
        exec_ctx.session,
        exec_ctx.win.hwnd,
        exec_ctx.pids,
        INTERNAL_SCAN_DEPTH,
    );

    // 优先在 QQNT 浮层“成员列表”弹窗容器内按精确匹配、再按子串匹配定位候选群成员
    for (hwnd, _cls, _title, snap) in &windows {
        if let Some(container) = find_at_popup_container(snap) {
            for exact in [true, false] {
                let hits = find_targets_in_snapshot(*hwnd, container, &|node| {
                    let ct = node.control_type.trim();
                    if !node.is_enabled
                        || ct.eq_ignore_ascii_case(CONTROL_TYPE_EDIT)
                        || ct.eq_ignore_ascii_case("Window")
                    {
                        return false;
                    }
                    let width = profile::rect_width(node.rect);
                    let height = profile::rect_height(node.rect);
                    if width > 500 || !(14..=100).contains(&height) {
                        return false;
                    }
                    let nm_lower = node.name.trim().to_ascii_lowercase();
                    if nm_lower.is_empty() {
                        return false;
                    }
                    if exact {
                        nm_lower == target_lower
                    } else {
                        nm_lower.contains(&target_lower)
                    }
                });
                if let Some(hit) = hits.into_iter().next() {
                    return Some(hit.rect);
                }
            }
        }
    }

    // 兼容独立弹窗窗口或单测快照树：在叠层节点中匹配并排除输入框本身及右侧栏区域
    for (hwnd, _cls, _title, snap) in &windows {
        let hits = find_overlay_targets_in_snapshot(*hwnd, snap, &|node| {
            if !node.is_enabled
                || node
                    .control_type
                    .trim()
                    .eq_ignore_ascii_case(CONTROL_TYPE_EDIT)
            {
                return false;
            }
            if node.rect[1] >= exec_ctx.editor_rect[1] && node.rect[3] <= exec_ctx.editor_rect[3] {
                return false;
            }
            if node.rect[0] >= exec_ctx.editor_rect[2] - 8 {
                return false;
            }
            let width = profile::rect_width(node.rect);
            let height = profile::rect_height(node.rect);
            if width > 500 || !(14..=100).contains(&height) {
                return false;
            }
            if node.rect[1] < exec_ctx.editor_rect[1] - 360 {
                return false;
            }
            let nm_lower = node.name.trim().to_ascii_lowercase();
            !nm_lower.is_empty() && nm_lower.contains(&target_lower)
        });
        if let Some(hit) = hits.into_iter().next() {
            return Some(hit.rect);
        }
    }
    None
}

fn execute_insert_at(
    exec_ctx: &RichComposeContext<'_>,
    display_name: &str,
    is_all: bool,
) -> Result<()> {
    exec_ctx.identity.verify_fast()?;
    focus_composer_end(exec_ctx)?;
    type_text_focus_restore_in_active_editor(
        exec_ctx.win.hwnd,
        "@",
        &exec_ctx.delay_range,
        Some(exec_ctx.identity),
    )?;
    thread::sleep(Duration::from_millis(260));

    let mut matched = match_at_candidate_in_popups(exec_ctx, display_name, is_all);
    if matched.is_none() && !is_all {
        let filter_prefix: String = display_name.chars().take(6).collect();
        if !filter_prefix.is_empty() {
            type_text_focus_restore_in_active_editor(
                exec_ctx.win.hwnd,
                &filter_prefix,
                &exec_ctx.delay_range,
                Some(exec_ctx.identity),
            )?;
            thread::sleep(Duration::from_millis(240));
            matched = match_at_candidate_in_popups(exec_ctx, display_name, false);
        }
    }

    if let Some(rect) = matched {
        click_rect_verified(exec_ctx.identity, rect, SimulatedClickKind::Left, true)?;
        thread::sleep(Duration::from_millis(120));
        return Ok(());
    }

    dismiss_popups_best_effort(exec_ctx.identity);
    let _ =
        send_handlers::clear_residual_editor_if_needed(exec_ctx.identity, exec_ctx.editor, true);
    Err(AdapterError::UnsupportedAction {
        action: action_name(exec_ctx.target_kind),
        reason: format!(
            "QQNT @mention popup did not match member '{display_name}'; aborted without sending plain text '@'"
        ),
    })
}

/// 按严格顺序执行全部内联步骤，并在所有项落入编辑器后执行唯一一次提交动作
pub(crate) fn execute_rich_inline_steps(
    exec_ctx: &RichComposeContext<'_>,
    steps: &[InlineStep],
) -> Result<(bool, bool)> {
    exec_ctx.identity.verify_fast()?;
    send_handlers::clear_residual_editor_if_needed(exec_ctx.identity, exec_ctx.editor, true)?;

    for step in steps {
        match step {
            InlineStep::AttachReply {
                sender_name,
                excerpt,
                raw_text,
            } => {
                execute_attach_reply(exec_ctx, sender_name, excerpt, raw_text)?;
            }
            InlineStep::InsertFace { face_name } => {
                execute_insert_face(exec_ctx, face_name)?;
            }
            InlineStep::InsertAt {
                display_name,
                is_all,
            } => {
                execute_insert_at(exec_ctx, display_name, *is_all)?;
            }
            InlineStep::TypeText { text } => {
                exec_ctx.identity.verify_fast()?;
                focus_composer_end(exec_ctx)?;
                type_text_focus_restore_in_active_editor(
                    exec_ctx.win.hwnd,
                    text,
                    &exec_ctx.delay_range,
                    Some(exec_ctx.identity),
                )?;
            }
            InlineStep::PasteImage { bitmap } => {
                exec_ctx.identity.verify_fast()?;
                focus_composer_end(exec_ctx)?;
                let _clip = ClipboardRestoreGuard::capture()?;
                write_bgra_to_clipboard_dib(bitmap)?;
                paste_clipboard_verified(
                    exec_ctx.identity,
                    &ClipboardPasteSpec {
                        editor: exec_ctx.editor,
                        send_button: exec_ctx.send_button,
                        mode: InputMode::FocusRestoreUnicode,
                        shortcut: exec_ctx.shortcut,
                        press_enter: false,
                        settle_delay_ms: 180,
                    },
                )?;
            }
        }
    }

    thread::sleep(Duration::from_millis(140));
    exec_ctx.identity.verify_fast()?;
    if let Some(btn_rect) = exec_ctx.send_rect {
        click_rect_verified(exec_ctx.identity, btn_rect, SimulatedClickKind::Left, false)?;
    } else {
        let target = ComposerSubmitTarget {
            window_hwnd: exec_ctx.win.hwnd,
            editor: exec_ctx.editor,
            send_button: exec_ctx.send_button,
            shortcut: exec_ctx.shortcut,
        };
        submit_only_focus_restore_verified(exec_ctx.identity, &target)?;
    }

    Ok((true, true))
}
