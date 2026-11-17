//! 真实 QQNT 桌面界面非发信交互探针：用于第一阶段校准搜索复位、表情面板、`@` 列表、气泡右键菜单与多选动作条

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use common_types::{AdapterError, Result, UiNodeSnapshot, WindowInfo};
use serde_json::{json, Value};
use uia_core::{
    current_foreground_hwnd, type_text_focus_restore_in_active_editor, ForegroundRestoreGuard,
    SimulatedClickKind, TypingDelayRange, VerifiedWindowIdentity,
};

use super::forward_ops::{
    click_bubble_multiselect_radio, exit_multiselect_cleanly, has_forward_picker_modal,
    pick_modal_search_edit,
};
use super::popup_helpers::{
    click_matched_target_verified, click_point_verified, click_rect_verified,
    collect_toolbar_buttons, collect_visible_bubbles, composer_append_click_point,
    dismiss_popups_best_effort, find_popup_clickable_by_keywords, find_targets_in_snapshot,
    locate_toolbar_button, scan_same_pid_popup_trees,
};
use crate::constants::INTERNAL_SCAN_DEPTH;
use crate::driver::group_handlers::ensure_active_window_for_group;
use crate::driver::private_handlers::{ensure_active_window_for_private, verify_private_whitelist};
use crate::driver::runtime_state::SharedInner;
use crate::driver::send_handlers::clear_residual_editor_if_needed;
use crate::driver::window_discovery::refresh_pids_and_check_security;
use crate::driver::worker_loop::WorkerContext;
use crate::matcher;

const MAX_PROBE_NODES: usize = 300;
const MAX_POPUP_PROBE_NODES: usize = 800;

/// 合并转发模态框弹窗探针所需的窗口与识别上下文
struct ForwardProbeEnv<'a> {
    session: &'a uia_core::UiaSession,
    identity: &'a VerifiedWindowIdentity,
    win: &'a WindowInfo,
    active_name: &'a str,
    pids: &'a [u32],
    allow_mock: bool,
}

fn collect_popup_summary(
    session: &uia_core::UiaSession,
    main_hwnd: isize,
    pids: &[u32],
    max_depth: usize,
) -> Value {
    let windows = scan_same_pid_popup_trees(session, main_hwnd, pids, max_depth);
    let summaries: Vec<Value> = windows
        .into_iter()
        .map(|(hwnd, class_name, title, snap)| {
            let mut items = find_targets_in_snapshot(hwnd, &snap, &|node| {
                let is_checkbox = node.control_type.eq_ignore_ascii_case("CheckBox");
                let has_label =
                    !node.name.trim().is_empty() || !node.automation_id.trim().is_empty();
                let is_long_text = node.control_type.eq_ignore_ascii_case("Text")
                    && node.name.chars().count() > 24;
                let is_left_session_noise = hwnd == main_hwnd
                    && node.rect[2] <= 640
                    && (120..=670).contains(&node.rect[1])
                    && (node.control_type.eq_ignore_ascii_case("Text")
                        || node.control_type.eq_ignore_ascii_case("Group"));
                !node.is_offscreen
                    && (has_label || is_checkbox)
                    && !is_long_text
                    && !is_left_session_noise
            });
            let node_limit = if hwnd == main_hwnd {
                MAX_PROBE_NODES
            } else {
                MAX_POPUP_PROBE_NODES
            };
            if items.len() > node_limit {
                let drop_count = items.len() - node_limit;
                items.drain(..drop_count);
            }
            let serialized: Vec<Value> = items
                .into_iter()
                .map(|it| {
                    json!({
                        "hwnd": it.hwnd,
                        "name": it.name,
                        "control_type": it.control_type,
                        "automation_id": it.automation_id,
                        "rect": it.rect,
                    })
                })
                .collect();
            json!({
                "hwnd": hwnd,
                "is_main_window": hwnd == main_hwnd,
                "class_name": class_name,
                "title": title,
                "node_samples": serialized,
            })
        })
        .collect();
    json!(summaries)
}

fn resolve_probe_window(
    ctx: &mut WorkerContext,
    shared: &Arc<SharedInner>,
    target: Option<&str>,
) -> Result<(WindowInfo, String)> {
    if let Some(name) = target.map(str::trim).filter(|s| !s.is_empty()) {
        if shared.config.target.is_private_friend_allowed(name) {
            return ensure_active_window_for_private(ctx, shared, name);
        }
        return ensure_active_window_for_group(ctx, shared, name);
    }

    let guard_pair = {
        let g = shared.state.lock();
        (
            g.attached_window.clone(),
            g.active_group.clone(),
            g.active_chat_kind,
        )
    };
    if let (Some(win), Some(active), kind) = guard_pair {
        if uia_core::win32::is_window_valid(win.hwnd) {
            if kind == common_types::ChatTargetKind::Private {
                verify_private_whitelist(shared, &active)?;
            }
            return Ok((win, active));
        }
    }
    let default_group = shared.config.target.group_name.clone();
    ensure_active_window_for_group(ctx, shared, &default_group)
}

fn find_editor_rect(tree: &UiNodeSnapshot, win: &WindowInfo) -> Result<([i32; 4], usize)> {
    let mut flat = Vec::new();
    collect_refs(tree, &mut flat);
    let (editor_idx, _) =
        matcher::locate_composer_and_send_indices(&flat, win.rect, win.is_standalone_chat)?;
    Ok((flat[editor_idx].rect, editor_idx))
}

fn collect_refs<'a>(node: &'a UiNodeSnapshot, out: &mut Vec<&'a UiNodeSnapshot>) {
    out.push(node);
    for child in &node.children {
        collect_refs(child, out);
    }
}

fn execute_probe_forward_modal(env: &ForwardProbeEnv<'_>, query: Option<&str>) -> Result<Value> {
    let (tree, _) =
        matcher::wake_and_dump_tree_ext(env.session, env.win.hwnd, INTERNAL_SCAN_DEPTH, true)?;
    let bubbles = collect_visible_bubbles(
        &tree,
        env.win.rect,
        env.win.is_standalone_chat,
        env.active_name,
    );
    let first_bub = bubbles
        .len()
        .checked_sub(2)
        .and_then(|idx| bubbles.get(idx))
        .or_else(|| bubbles.last())
        .ok_or_else(|| {
            AdapterError::InputRejected("no visible message bubble for forward modal probe".into())
        })?;
    let last_bub = bubbles.last().unwrap_or(first_bub);

    click_rect_verified(
        env.identity,
        first_bub.bubble_rect,
        SimulatedClickKind::Right,
        false,
    )?;
    thread::sleep(Duration::from_millis(220));

    let Some(multi_hit) =
        find_popup_clickable_by_keywords(env.session, env.win.hwnd, env.pids, &["多选"], true)
    else {
        dismiss_popups_best_effort(env.identity);
        return Err(AdapterError::UnsupportedAction {
            action: "probe_forward_modal".to_string(),
            reason: "context menu did not expose '多选' item".to_string(),
        });
    };
    click_rect_verified(env.identity, multi_hit.rect, SimulatedClickKind::Left, true)?;
    thread::sleep(Duration::from_millis(260));

    let abort_cleanly = |err: AdapterError| -> AdapterError {
        exit_multiselect_cleanly(
            env.session,
            env.identity,
            env.win.hwnd,
            env.pids,
            env.allow_mock,
        );
        err
    };

    if bubbles.len() >= 2 {
        let (multi_tree, _) =
            matcher::wake_and_dump_tree_ext(env.session, env.win.hwnd, INTERNAL_SCAN_DEPTH, false)
                .map_err(abort_cleanly)?;
        let refreshed_last = collect_visible_bubbles(
            &multi_tree,
            env.win.rect,
            env.win.is_standalone_chat,
            env.active_name,
        )
        .into_iter()
        .last()
        .map(|b| b.bubble_rect)
        .unwrap_or(last_bub.bubble_rect);
        click_bubble_multiselect_radio(env.identity, env.win, &multi_tree, refreshed_last)
            .map_err(abort_cleanly)?;
        thread::sleep(Duration::from_millis(180));
    }

    let merge_hit =
        find_popup_clickable_by_keywords(env.session, env.win.hwnd, env.pids, &["合并转发"], true)
            .ok_or_else(|| AdapterError::UnsupportedAction {
                action: "probe_forward_modal".to_string(),
                reason: "multi-select bar did not expose '合并转发'".to_string(),
            })
            .map_err(abort_cleanly)?;

    click_rect_verified(env.identity, merge_hit.rect, SimulatedClickKind::Left, true)
        .map_err(abort_cleanly)?;
    thread::sleep(Duration::from_millis(360));
    let after_label_click = collect_popup_summary(env.session, env.win.hwnd, env.pids, 18);

    let wins_after_label = scan_same_pid_popup_trees(env.session, env.win.hwnd, env.pids, 18);
    let mut after_icon_click: Option<Value> = None;
    if !has_forward_picker_modal(&wins_after_label, env.win)
        && merge_hit.rect[1] > env.win.rect[1] + 200
    {
        let icon_x = (merge_hit.rect[0] + merge_hit.rect[2]) / 2;
        let icon_y = merge_hit.rect[1] - 24;
        click_point_verified(env.identity, icon_x, icon_y, SimulatedClickKind::Left, true)
            .map_err(abort_cleanly)?;
        thread::sleep(Duration::from_millis(380));
        after_icon_click = Some(collect_popup_summary(
            env.session,
            env.win.hwnd,
            env.pids,
            18,
        ));
    }

    let mut after_query_popups: Option<Value> = None;
    if let Some(q_text) = query.map(str::trim).filter(|s| !s.is_empty()) {
        let latest_wins = scan_same_pid_popup_trees(env.session, env.win.hwnd, env.pids, 18);
        if let Some(edit_box) = pick_modal_search_edit(&latest_wins, env.win) {
            if click_matched_target_verified(
                env.identity,
                &edit_box,
                SimulatedClickKind::Left,
                env.allow_mock,
            )
            .is_ok()
            {
                thread::sleep(Duration::from_millis(100));
                let modal_identity = (edit_box.hwnd != env.win.hwnd)
                    .then(|| VerifiedWindowIdentity::verify_and_bind(edit_box.hwnd, env.allow_mock))
                    .transpose()
                    .ok()
                    .flatten();
                let _ = type_text_focus_restore_in_active_editor(
                    edit_box.hwnd,
                    q_text,
                    &TypingDelayRange {
                        min_ms: 15,
                        max_ms: 35,
                    },
                    Some(modal_identity.as_ref().unwrap_or(env.identity)),
                );
                thread::sleep(Duration::from_millis(420));
                after_query_popups = Some(collect_popup_summary(
                    env.session,
                    env.win.hwnd,
                    env.pids,
                    18,
                ));
            }
        }
    }

    exit_multiselect_cleanly(
        env.session,
        env.identity,
        env.win.hwnd,
        env.pids,
        env.allow_mock,
    );
    Ok(json!({
        "action": "probe_forward_modal",
        "merge_rect": merge_hit.rect,
        "after_label_click": after_label_click,
        "after_icon_click": after_icon_click,
        "after_query_popups": after_query_popups,
    }))
}

/// 对真实 QQNT 窗口执行只读且可自动复原的界面弹窗探针并返回真实控件布局 JSON
pub(crate) fn handle_probe_ui_action(
    ctx: &mut WorkerContext,
    shared: &Arc<SharedInner>,
    action: &str,
    target: Option<&str>,
    query: Option<&str>,
) -> Result<Value> {
    let pids = refresh_pids_and_check_security(shared)?;
    let fg_before = current_foreground_hwnd();
    let _fg_guard = ForegroundRestoreGuard::capture(&pids);

    let (mut win, active_name) = resolve_probe_window(ctx, shared, target)?;
    let _ = uia_core::refresh_window_info(&mut win);
    let identity_ref =
        matcher::ensure_verified_window(&mut ctx.verified_cache, &win, shared.allow_mock())?;
    identity_ref.verify_fast()?;
    let identity =
        VerifiedWindowIdentity::verify_and_bind(identity_ref.hwnd(), shared.allow_mock())?;

    match action.trim() {
        "clear_search" => {
            let (before_nodes, _) =
                matcher::wake_and_collect_ext(&ctx.session, win.hwnd, INTERNAL_SCAN_DEPTH, true)?;
            let was_active = matcher::is_search_mode_active(&before_nodes, win.rect);
            let cleared = matcher::clear_search_mode_if_active(
                win.hwnd,
                shared.allow_mock(),
                &before_nodes,
                win.rect,
            );
            let (after_nodes, _) =
                matcher::wake_and_collect_ext(&ctx.session, win.hwnd, INTERNAL_SCAN_DEPTH, false)?;
            let still_active = matcher::is_search_mode_active(&after_nodes, win.rect);
            Ok(json!({
                "action": "clear_search",
                "active_name": active_name,
                "was_search_active": was_active,
                "cleared_invoked": cleared,
                "after_search_active": still_active,
            }))
        }
        "selfcheck_state" => {
            let (tree, _) =
                matcher::wake_and_dump_tree_ext(&ctx.session, win.hwnd, INTERNAL_SCAN_DEPTH, true)?;
            let header = matcher::read_chat_header_from_tree(&tree, win.rect, win.is_standalone_chat, None);
            let (editor_rect, _) = find_editor_rect(&tree, &win)?;
            let toolbar: Vec<Value> = collect_toolbar_buttons(&tree, editor_rect)
                .into_iter()
                .map(|b| json!({"name": b.name, "control_type": b.control_type, "automation_id": b.automation_id, "rect": b.rect}))
                .collect();
            let bubbles: Vec<Value> = collect_visible_bubbles(&tree, win.rect, win.is_standalone_chat, &active_name)
                .into_iter()
                .rev()
                .take(8)
                .map(|b| json!({"sender": b.sender, "content": b.content, "is_self": b.is_self, "bubble_rect": b.bubble_rect, "avatar_rect": b.avatar_rect}))
                .collect();
            Ok(json!({
                "action": "selfcheck_state",
                "hwnd": win.hwnd,
                "active_name": active_name,
                "foreground_before": fg_before,
                "header": header,
                "editor_rect": editor_rect,
                "toolbar_buttons": toolbar,
                "recent_bubbles": bubbles,
            }))
        }
        "probe_emoji" => {
            let (tree, _) =
                matcher::wake_and_dump_tree_ext(&ctx.session, win.hwnd, INTERNAL_SCAN_DEPTH, true)?;
            let (editor_rect, _) = find_editor_rect(&tree, &win)?;
            let emoji_btn = locate_toolbar_button(&tree, editor_rect, &["表情", "emoji", "Emoji"])
                .ok_or_else(|| AdapterError::UiaFailure("emoji toolbar button not found".to_string()))?;
            click_rect_verified(&identity, emoji_btn.rect, SimulatedClickKind::Left, false)?;
            thread::sleep(Duration::from_millis(320));
            let popups = collect_popup_summary(&ctx.session, win.hwnd, &pids, 14);
            let scrolled_popups = if query == Some("scroll") {
                let scroll_x = emoji_btn.rect[0] + 180;
                let scroll_y = (emoji_btn.rect[1] - 150).max(win.rect[1] + 140);
                for _ in 0..6 {
                    let _ = click_point_verified(
                        &identity,
                        scroll_x,
                        scroll_y,
                        SimulatedClickKind::WheelDown,
                        true,
                    );
                    thread::sleep(Duration::from_millis(200));
                }
                Some(collect_popup_summary(&ctx.session, win.hwnd, &pids, 14))
            } else {
                None
            };
            dismiss_popups_best_effort(&identity);
            Ok(json!({
                "action": "probe_emoji",
                "emoji_button": {"name": emoji_btn.name, "rect": emoji_btn.rect},
                "popups": popups,
                "scrolled_popups": scrolled_popups,
            }))
        }
        "probe_at" => {
            let (nodes, _) =
                matcher::wake_and_collect_ext(&ctx.session, win.hwnd, INTERNAL_SCAN_DEPTH, true)?;
            let (editor_idx, _) =
                matcher::locate_composer_and_send_button(&nodes, win.rect, win.is_standalone_chat)?;
            let editor = &nodes[editor_idx].element;
            let editor_rect = nodes[editor_idx].snapshot.rect;
            clear_residual_editor_if_needed(&identity, editor, true)?;
            if let Some((x, y)) = composer_append_click_point(editor_rect) {
                click_point_verified(&identity, x, y, SimulatedClickKind::Left, false)?;
                thread::sleep(Duration::from_millis(80));
            }
            let suffix = query.map(str::trim).unwrap_or("");
            let input = format!("@{suffix}");
            type_text_focus_restore_in_active_editor(
                win.hwnd,
                &input,
                &TypingDelayRange { min_ms: 20, max_ms: 40 },
                Some(&identity),
            )?;
            thread::sleep(Duration::from_millis(320));
            let popups = collect_popup_summary(&ctx.session, win.hwnd, &pids, 14);
            dismiss_popups_best_effort(&identity);
            let _ = clear_residual_editor_if_needed(&identity, editor, true);
            Ok(json!({
                "action": "probe_at",
                "typed": input,
                "popups": popups,
            }))
        }
        "probe_bubble_menu" => {
            let (tree, _) =
                matcher::wake_and_dump_tree_ext(&ctx.session, win.hwnd, INTERNAL_SCAN_DEPTH, true)?;
            let bubbles = collect_visible_bubbles(&tree, win.rect, win.is_standalone_chat, &active_name);
            let target_bub = if let Some(q) = query.map(str::trim).filter(|s| !s.is_empty()) {
                bubbles.iter().rev().find(|b| b.content.contains(q) || b.sender.contains(q))
            } else {
                bubbles.last()
            }
            .ok_or_else(|| AdapterError::InputRejected("no visible message bubble to probe".to_string()))?;
            click_rect_verified(&identity, target_bub.bubble_rect, SimulatedClickKind::Right, false)?;
            thread::sleep(Duration::from_millis(260));
            let popups = collect_popup_summary(&ctx.session, win.hwnd, &pids, 10);
            dismiss_popups_best_effort(&identity);
            Ok(json!({
                "action": "probe_bubble_menu",
                "target_bubble": {"sender": target_bub.sender, "content": target_bub.content, "rect": target_bub.bubble_rect},
                "popups": popups,
            }))
        }
        "probe_multiselect" => {
            let (tree, _) =
                matcher::wake_and_dump_tree_ext(&ctx.session, win.hwnd, INTERNAL_SCAN_DEPTH, true)?;
            let bubbles = collect_visible_bubbles(&tree, win.rect, win.is_standalone_chat, &active_name);
            let target_bub = bubbles
                .last()
                .ok_or_else(|| AdapterError::InputRejected("no visible message bubble for multiselect probe".to_string()))?;
            click_rect_verified(&identity, target_bub.bubble_rect, SimulatedClickKind::Right, false)?;
            thread::sleep(Duration::from_millis(220));
            let multi_hit =
                find_popup_clickable_by_keywords(&ctx.session, win.hwnd, &pids, &["多选"], true);
            let multi_popups = if let Some(hit) = multi_hit {
                click_rect_verified(&identity, hit.rect, SimulatedClickKind::Left, true)?;
                thread::sleep(Duration::from_millis(320));
                let summary = collect_popup_summary(&ctx.session, win.hwnd, &pids, 14);
                if let Some(cancel) =
                    find_popup_clickable_by_keywords(&ctx.session, win.hwnd, &pids, &["取消", "关闭多选"], true)
                {
                    let _ = click_rect_verified(&identity, cancel.rect, SimulatedClickKind::Left, true);
                }
                dismiss_popups_best_effort(&identity);
                summary
            } else {
                let menu_summary = collect_popup_summary(&ctx.session, win.hwnd, &pids, 10);
                dismiss_popups_best_effort(&identity);
                return Ok(json!({
                    "action": "probe_multiselect",
                    "multiselect_item_found": false,
                    "context_menu": menu_summary,
                }));
            };
            Ok(json!({
                "action": "probe_multiselect",
                "multiselect_item_found": true,
                "popups": multi_popups,
            }))
        }
        "probe_forward_modal" => {
            let env = ForwardProbeEnv {
                session: &ctx.session,
                identity: &identity,
                win: &win,
                active_name: &active_name,
                pids: &pids,
                allow_mock: shared.allow_mock(),
            };
            execute_probe_forward_modal(&env, query)
        }
        other => Err(AdapterError::InputRejected(format!(
            "unsupported diag probe action '{other}'; expected clear_search|selfcheck_state|probe_emoji|probe_at|probe_bubble_menu|probe_multiselect|probe_forward_modal"
        ))),
    }
}
