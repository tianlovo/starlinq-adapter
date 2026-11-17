//! 原生单条消息桌面转发执行器与合并转发目标选择弹窗共享辅助模块

use std::collections::HashSet;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use common_types::{
    AdapterError, AdapterEvent, ChatMessage, ChatTargetKind, InputMode, MessageSegment, Result,
    SendOutcome, SingleForwardRequest, StoredMessageRecord, UiNodeSnapshot, WindowInfo,
};
use uia_core::constants::{
    CONTROL_TYPE_CHECK_BOX, CONTROL_TYPE_EDIT, CONTROL_TYPE_LIST_ITEM, CONTROL_TYPE_MENU_ITEM,
};
use uia_core::{
    type_text_focus_restore_in_active_editor, ForegroundRestoreGuard, SimulatedClickKind,
    UiaSession, VerifiedWindowIdentity,
};

use super::forward_ops::{
    attach_chat_window, exit_multiselect_cleanly, find_bubble_for_record,
    is_multiselect_bar_active, verify_target_guard_in_tree,
};
use super::popup_helpers::{
    click_matched_target_verified, click_rect_verified, collect_visible_bubbles,
    dismiss_popups_best_effort, extract_media_bubble_tokens, find_overlay_targets_in_snapshot,
    find_popup_clickable_by_keywords, find_targets_in_snapshot, locate_message_list_scroll_point,
    restore_message_list_to_bottom, scan_same_pid_popup_trees, scroll_message_list_wheel,
    MatchedUiTarget,
};
use crate::constants::INTERNAL_SCAN_DEPTH;
use crate::driver::outbound_media::{self, OutboundSendPlan};
use crate::driver::private_handlers::verify_private_whitelist;
use crate::driver::runtime_state::{
    canonical_storage_group_name, lookup_group_record, SharedInner,
};
use crate::driver::window_discovery::refresh_pids_and_check_security;
use crate::driver::worker_loop::WorkerContext;
use crate::matcher;
use crate::parser::{self, OutboundStubBinding};

const MAX_SINGLE_FORWARD_SCROLL_STEPS: usize = 12;

/// 转发前采集的目标会话同类气泡基线快照，用于区分同群同文新增落地气泡与既有历史气泡
struct ForwardBaselineSnapshot {
    match_count: usize,
    uids: HashSet<String>,
}

/// 源消息气泡定位与临时上滚步数记录，用于在转发结束或失败后还原滚动位置
struct LocatedSourceBubble {
    bub_rect: [i32; 4],
    scroll_up_steps: usize,
    scroll_point: Option<(i32, i32)>,
}

/// 打包待单条转发消息的匹配摘要线索（约束函数参数数量 <= 6）
struct ForwardMatchTokens {
    segs: Vec<MessageSegment>,
    plain: String,
    excerpt: String,
    pure_text: String,
    media_tokens: Vec<String>,
}

fn is_modal_search_edit(hwnd: isize, win: &WindowInfo, node: &UiNodeSnapshot) -> bool {
    if !node.is_enabled
        || !node
            .control_type
            .trim()
            .eq_ignore_ascii_case(CONTROL_TYPE_EDIT)
    {
        return false;
    }
    let nm = node.name.trim();
    if nm.contains("留言") {
        return false;
    }
    if hwnd == win.hwnd {
        let is_top_left_search =
            node.rect[0] < win.rect[0] + 290 && node.rect[1] < win.rect[1] + 150;
        let is_bottom_composer =
            node.rect[1] >= win.rect[3] - 210 || (node.rect[3] - node.rect[1]) > 64;
        return !is_top_left_search
            && !is_bottom_composer
            && (nm.contains("搜索")
                || (node.rect[1] > win.rect[1] + 80
                    && node.rect[1] < win.rect[3] - 210
                    && (node.rect[3] - node.rect[1]) <= 64));
    }
    true
}

/// 检测主窗口或同进程浮层弹窗中是否已打开转发目标选择模态框
pub(crate) fn has_forward_picker_modal(
    windows: &[(isize, String, String, UiNodeSnapshot)],
    main_win: &WindowInfo,
) -> bool {
    for (hwnd, _cls, _title, snap) in windows {
        if *hwnd != main_win.hwnd {
            let [l, t, r, b] = snap.rect;
            if (r - l) >= 200 && (b - t) >= 160 {
                return true;
            }
            continue;
        }
        let hits = find_overlay_targets_in_snapshot(*hwnd, snap, &|node| {
            if !node.is_enabled {
                return false;
            }
            let ct = node.control_type.trim();
            let nm = node.name.trim();
            let is_top_left_search =
                node.rect[0] < main_win.rect[0] + 290 && node.rect[1] < main_win.rect[1] + 150;
            let is_bottom_composer = node.rect[1] >= main_win.rect[3] - 120;
            let modal_edit = ct.eq_ignore_ascii_case(CONTROL_TYPE_EDIT)
                && !is_top_left_search
                && !is_bottom_composer;
            let modal_label = nm.contains("发送(")
                || nm.contains("给朋友留言")
                || nm.contains("分别发送")
                || nm.contains("转发给")
                || nm.contains("选择发送")
                || nm.contains("选择转发");
            modal_edit || modal_label
        });
        if !hits.is_empty() {
            return true;
        }
    }
    false
}

/// 按给定延时序列轮询等待 QQNT 转发目标选择模态框弹出
pub(crate) fn poll_forward_picker_modal(
    session: &UiaSession,
    main_win: &WindowInfo,
    pids: &[u32],
    delays_ms: &[u64],
) -> bool {
    for &wait_ms in delays_ms {
        thread::sleep(Duration::from_millis(wait_ms));
        let wins = scan_same_pid_popup_trees(session, main_win.hwnd, pids, INTERNAL_SCAN_DEPTH);
        if has_forward_picker_modal(&wins, main_win) {
            return true;
        }
    }
    false
}

fn is_forward_action_label(nm: &str) -> bool {
    matches!(
        nm,
        "发送" | "确定" | "取消" | "关闭" | "完成" | "搜索" | "清空"
    ) || nm.starts_with("发送(")
        || nm.starts_with("确定(")
}

fn forward_entry_type_rank(ct: &str) -> u8 {
    match ct.trim().to_ascii_lowercase().as_str() {
        "listitem" => 0,
        "checkbox" => 1,
        "group" => 2,
        "button" => 3,
        "custom" => 4,
        _ => 5,
    }
}

fn find_modal_targets(
    hwnd: isize,
    main_hwnd: isize,
    snap: &UiNodeSnapshot,
    pred: &dyn Fn(&UiNodeSnapshot) -> bool,
) -> Vec<MatchedUiTarget> {
    if hwnd != main_hwnd {
        find_targets_in_snapshot(hwnd, snap, pred)
    } else {
        find_overlay_targets_in_snapshot(hwnd, snap, pred)
    }
}

/// 在转发目标选择器浮层弹窗中定位搜索编辑框
pub(crate) fn pick_modal_search_edit(
    windows: &[(isize, String, String, UiNodeSnapshot)],
    win: &WindowInfo,
) -> Option<MatchedUiTarget> {
    let ordered_windows = windows
        .iter()
        .filter(|(hwnd, ..)| *hwnd != win.hwnd)
        .chain(windows.iter().filter(|(hwnd, ..)| *hwnd == win.hwnd));

    for (hwnd, _cls, _title, snap) in ordered_windows {
        let mut edits = find_modal_targets(*hwnd, win.hwnd, snap, &|node| {
            is_modal_search_edit(*hwnd, win, node)
        });
        if edits.is_empty() {
            continue;
        }
        edits.sort_by_key(|e| (e.rect[1], e.rect[0]));
        if let Some(search_edit) = edits.iter().find(|e| e.name.contains("搜索")).cloned() {
            return Some(search_edit);
        }
        return edits.into_iter().next();
    }
    None
}

fn match_modal_confirm_in_window(
    hwnd: isize,
    win: &WindowInfo,
    snap: &UiNodeSnapshot,
    keywords: &[&str],
) -> Option<MatchedUiTarget> {
    let hits = find_modal_targets(hwnd, win.hwnd, snap, &|node| {
        if !node.is_enabled {
            return false;
        }
        let ct = node.control_type.trim();
        let clickable = ct.eq_ignore_ascii_case("Button")
            || ct.eq_ignore_ascii_case("MenuItem")
            || ct.eq_ignore_ascii_case("Custom")
            || ct.eq_ignore_ascii_case("Text")
            || (ct.eq_ignore_ascii_case("Group") && (node.rect[3] - node.rect[1]) <= 80);
        let nm = node.name.trim();
        let is_bg_composer_send = hwnd == win.hwnd
            && ct.eq_ignore_ascii_case("Button")
            && nm == "发送"
            && node.rect[1] > win.rect[3] - 68;
        clickable && !is_bg_composer_send && keywords.iter().any(|kw| nm.contains(kw))
    });
    hits.into_iter().last()
}

fn find_modal_confirm_button(
    session: &UiaSession,
    win: &WindowInfo,
    pids: &[u32],
) -> Option<MatchedUiTarget> {
    let windows = scan_same_pid_popup_trees(session, win.hwnd, pids, INTERNAL_SCAN_DEPTH);
    let keyword_tiers: [&[&str]; 5] = [&["发送("], &["确定("], &["确定"], &["发送"], &["完成"]];
    for same_main in [false, true] {
        for keywords in keyword_tiers {
            if let Some(hit) = windows
                .iter()
                .filter(|(h, ..)| (*h == win.hwnd) == same_main)
                .find_map(|(hwnd, _, _, snap)| {
                    match_modal_confirm_in_window(*hwnd, win, snap, keywords)
                })
            {
                return Some(hit);
            }
        }
    }
    None
}

/// 在已弹出的转发目标选择器模态框中键入目标会话名称、点选条目并点击“发送”确认按钮
pub(crate) fn complete_forward_picker_modal(
    session: &UiaSession,
    identity: &VerifiedWindowIdentity,
    win: &WindowInfo,
    pids: &[u32],
    dest_name: &str,
    allow_mock: bool,
) -> Result<()> {
    let canon_dest = canonical_storage_group_name(dest_name);
    let windows = scan_same_pid_popup_trees(session, win.hwnd, pids, INTERNAL_SCAN_DEPTH);
    let mut search_context: Option<(isize, [i32; 4])> = None;

    if let Some(edit_box) = pick_modal_search_edit(&windows, win) {
        search_context = Some((edit_box.hwnd, edit_box.rect));
        click_matched_target_verified(identity, &edit_box, SimulatedClickKind::Left, allow_mock)?;
        thread::sleep(Duration::from_millis(100));
        let modal_identity = (edit_box.hwnd != win.hwnd)
            .then(|| VerifiedWindowIdentity::verify_and_bind(edit_box.hwnd, allow_mock))
            .transpose()?;
        type_text_focus_restore_in_active_editor(
            edit_box.hwnd,
            &canon_dest,
            &uia_core::TypingDelayRange {
                min_ms: 15,
                max_ms: 35,
            },
            Some(modal_identity.as_ref().unwrap_or(identity)),
        )?;
        thread::sleep(Duration::from_millis(420));
    }

    let refreshed_windows = scan_same_pid_popup_trees(session, win.hwnd, pids, INTERNAL_SCAN_DEPTH);
    let ordered_windows = refreshed_windows
        .iter()
        .filter(|(hwnd, ..)| *hwnd != win.hwnd)
        .chain(
            refreshed_windows
                .iter()
                .filter(|(hwnd, ..)| *hwnd == win.hwnd),
        );
    let mut picked_target = false;

    for (hwnd, _cls, _title, snap) in ordered_windows {
        let same_modal_search = search_context
            .filter(|(search_hwnd, _)| *search_hwnd == *hwnd)
            .map(|(_, rect)| rect);
        let entry_pred = |node: &UiNodeSnapshot| {
            let ct = node.control_type.trim();
            let valid_type = ct.eq_ignore_ascii_case(CONTROL_TYPE_LIST_ITEM)
                || ct.eq_ignore_ascii_case(CONTROL_TYPE_CHECK_BOX)
                || ct.eq_ignore_ascii_case("Button")
                || ct.eq_ignore_ascii_case("Custom")
                || ct.eq_ignore_ascii_case("Group")
                || ct.eq_ignore_ascii_case("Text");
            let nm = node.name.trim();
            if !valid_type || !node.is_enabled || nm.is_empty() || is_forward_action_label(nm) {
                return false;
            }
            if let Some(s_rect) = same_modal_search {
                if node.rect[1] < s_rect[3] - 6 || node.rect[0] > s_rect[2] + 65 {
                    return false;
                }
            } else if *hwnd == win.hwnd
                && ((node.rect[0] < win.rect[0] + 280 && node.rect[1] < win.rect[1] + 180)
                    || node.rect[1] < win.rect[1] + 110
                    || node.rect[0] >= win.rect[2] - 200
                    || (node.rect[3] - node.rect[1]) > 110)
            {
                return false;
            }
            matcher::group_title_matches(nm, &canon_dest) || nm.contains(&canon_dest)
        };
        let mut candidates = find_modal_targets(*hwnd, win.hwnd, snap, &entry_pred);
        candidates.sort_by_key(|hit| {
            (
                hit.rect[0] / 24,
                hit.rect[1] / 24,
                forward_entry_type_rank(&hit.control_type),
                hit.rect[0],
                hit.rect[1],
            )
        });
        if let Some(target_item) = candidates.into_iter().next() {
            click_matched_target_verified(
                identity,
                &target_item,
                SimulatedClickKind::Left,
                allow_mock,
            )?;
            thread::sleep(Duration::from_millis(240));
            picked_target = true;
            break;
        }
    }

    if !picked_target {
        return Err(AdapterError::UnsupportedAction {
            action: "send_forward_msg".to_string(),
            reason: format!(
                "QQNT merge-forward target picker did not locate selectable entry for '{canon_dest}'"
            ),
        });
    }

    let send_btn = find_modal_confirm_button(session, win, pids).ok_or_else(|| {
        AdapterError::UnsupportedAction {
            action: "send_forward_msg".to_string(),
            reason: "QQNT merge-forward dialog did not expose a clickable '发送'/'确定' confirmation button"
                .to_string(),
        }
    })?;
    click_matched_target_verified(identity, &send_btn, SimulatedClickKind::Left, allow_mock)?;
    Ok(())
}

fn build_forward_match_tokens(record: &StoredMessageRecord) -> ForwardMatchTokens {
    let segs = if !record.segments.is_empty() {
        record.segments.clone()
    } else {
        common_types::parse_cq_string_to_segments(&record.raw_message, true)
    };
    let plain = common_types::segments_to_plain_summary(&segs);
    let excerpt = outbound_media::compact_reply_excerpt(&plain, 28);
    let pure_text = segs
        .iter()
        .filter_map(|seg| match seg {
            MessageSegment::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
        .trim()
        .to_string();
    let media_tokens = extract_media_bubble_tokens(&segs);
    ForwardMatchTokens {
        segs,
        plain,
        excerpt,
        pure_text,
        media_tokens,
    }
}

fn message_matches_forward_source(msg: &ChatMessage, tokens: &ForwardMatchTokens) -> bool {
    if !msg.is_self {
        return false;
    }
    let actual_norm = parser::normalize_message_text(&msg.content);
    if actual_norm.is_empty() {
        return false;
    }
    let plain_norm = parser::normalize_message_text(&tokens.plain);
    if !plain_norm.is_empty()
        && (actual_norm == plain_norm
            || actual_norm.contains(&plain_norm)
            || plain_norm.contains(&actual_norm))
    {
        return true;
    }
    let excerpt_norm = parser::normalize_message_text(&tokens.excerpt).replace("...", "");
    if !excerpt_norm.is_empty() && actual_norm.contains(&excerpt_norm) {
        return true;
    }
    let pure_norm = parser::normalize_message_text(&tokens.pure_text);
    if !pure_norm.is_empty() && actual_norm.contains(&pure_norm) {
        return true;
    }
    if tokens
        .media_tokens
        .iter()
        .any(|tok| !tok.is_empty() && actual_norm.contains(tok.as_str()))
    {
        return true;
    }
    let has_image = tokens
        .segs
        .iter()
        .any(|seg| matches!(seg, MessageSegment::Image { .. }));
    if has_image && actual_norm.contains("[图片]") {
        return true;
    }
    let has_forward = tokens
        .segs
        .iter()
        .any(|seg| matches!(seg, MessageSegment::Forward { .. }));
    has_forward
        && (actual_norm.contains("聊天记录")
            || actual_norm.contains("转发消息")
            || actual_norm.contains("合并转发"))
}

fn collect_dest_matching_self_messages(
    shared: &Arc<SharedInner>,
    tree: &UiNodeSnapshot,
    win: &WindowInfo,
    dest_target: (ChatTargetKind, &str),
    tokens: &ForwardMatchTokens,
) -> Vec<ChatMessage> {
    shared.register_tree_self_nicknames(tree, win.rect);
    let mut parsed =
        parser::extract_messages_from_tree(tree, win.rect, win.is_standalone_chat, dest_target.1);
    for msg in &mut parsed {
        msg.target_kind = dest_target.0;
    }
    let annotated = {
        let mut tracker = shared.tracker.lock();
        tracker.annotate_only(parsed)
    };
    annotated
        .into_iter()
        .filter(|msg| message_matches_forward_source(msg, tokens))
        .collect()
}

fn capture_baseline_from_tree(
    shared: &Arc<SharedInner>,
    tree: &UiNodeSnapshot,
    win: &WindowInfo,
    dest_target: (ChatTargetKind, &str),
    tokens: &ForwardMatchTokens,
) -> ForwardBaselineSnapshot {
    let matching = collect_dest_matching_self_messages(shared, tree, win, dest_target, tokens);
    let uids = matching.iter().map(|m| m.message_id.clone()).collect();
    ForwardBaselineSnapshot {
        match_count: matching.len(),
        uids,
    }
}

fn is_exact_forward_menu_item(target: &MatchedUiTarget) -> bool {
    let nm = target.name.trim();
    let ct = target.control_type.trim();
    nm == "转发" && ct.eq_ignore_ascii_case(CONTROL_TYPE_MENU_ITEM)
}

fn video_bottom_click_rect(bub_rect: [i32; 4]) -> [i32; 4] {
    let fx = (bub_rect[0] + 32)
        .min(bub_rect[2].saturating_sub(8))
        .max(bub_rect[0]);
    let fy = (bub_rect[3] - 18).max(bub_rect[1] + 8);
    [fx - 2, fy - 2, fx + 2, fy + 2]
}

fn locate_source_bubble(
    ctx: &mut WorkerContext,
    identity: &VerifiedWindowIdentity,
    win: &WindowInfo,
    active_name: &str,
    record: &StoredMessageRecord,
) -> Result<LocatedSourceBubble> {
    let (initial_tree, _guard) =
        matcher::wake_and_dump_tree_ext(&ctx.session, win.hwnd, INTERNAL_SCAN_DEPTH, false)?;
    let initial_bubbles =
        collect_visible_bubbles(&initial_tree, win.rect, win.is_standalone_chat, active_name);
    let scroll_point =
        locate_message_list_scroll_point(&initial_tree, win.rect, win.is_standalone_chat)
            .map(|(x, y, _)| (x, y));
    if let Some(bub_rect) = find_bubble_for_record(&initial_bubbles, record, &[]) {
        return Ok(LocatedSourceBubble {
            bub_rect,
            scroll_up_steps: 0,
            scroll_point,
        });
    }

    let Some(pt) = scroll_point else {
        return Err(AdapterError::InputRejected(format!(
            "forward message_id={} bubble is not visible in current chat viewport",
            record.onebot_message_id
        )));
    };

    let mut scroll_up_steps = 0usize;
    let mut last_sig: Option<(String, String)> = None;
    let mut stagnant = 0usize;
    for _ in 0..MAX_SINGLE_FORWARD_SCROLL_STEPS {
        if let Err(err) = identity
            .verify_fast()
            .and_then(|()| scroll_message_list_wheel(identity, pt, SimulatedClickKind::WheelUp, 2))
        {
            restore_message_list_to_bottom(identity, pt, scroll_up_steps);
            return Err(err);
        }
        scroll_up_steps += 1;
        let Ok((scrolled_tree, _)) =
            matcher::wake_and_dump_tree_ext(&ctx.session, win.hwnd, INTERNAL_SCAN_DEPTH, false)
        else {
            continue;
        };
        let bubbles = collect_visible_bubbles(
            &scrolled_tree,
            win.rect,
            win.is_standalone_chat,
            active_name,
        );
        if let Some(bub_rect) = find_bubble_for_record(&bubbles, record, &[]) {
            return Ok(LocatedSourceBubble {
                bub_rect,
                scroll_up_steps,
                scroll_point,
            });
        }
        let sig = bubbles
            .first()
            .map(|b| (b.sender.clone(), b.content.clone()));
        if sig.is_some() && sig == last_sig {
            stagnant += 1;
            if stagnant >= 3 {
                break;
            }
            thread::sleep(Duration::from_millis(260));
        } else {
            stagnant = 0;
            last_sig = sig;
        }
    }

    restore_message_list_to_bottom(identity, pt, scroll_up_steps);
    Err(AdapterError::InputRejected(format!(
        "forward message_id={} bubble was not found in recent scrollable chat history",
        record.onebot_message_id
    )))
}

fn probe_forward_menu_with_fallback(
    ctx: &WorkerContext,
    identity: &VerifiedWindowIdentity,
    win_hwnd: isize,
    pids: &[u32],
    bub_rect: [i32; 4],
) -> Result<Option<MatchedUiTarget>> {
    identity
        .verify_fast()
        .and_then(|()| click_rect_verified(identity, bub_rect, SimulatedClickKind::Right, false))?;
    thread::sleep(Duration::from_millis(220));
    if let Some(hit) =
        find_popup_clickable_by_keywords(&ctx.session, win_hwnd, pids, &["转发"], true)
            .filter(is_exact_forward_menu_item)
    {
        return Ok(Some(hit));
    }
    dismiss_popups_best_effort(identity);

    // 高视频卡片中心点击可能命中播放按钮，回退到底栏文件名安全区再试一次右键“转发”
    if bub_rect[3] - bub_rect[1] >= 120 {
        thread::sleep(Duration::from_millis(120));
        let fallback_rect = video_bottom_click_rect(bub_rect);
        identity.verify_fast().and_then(|()| {
            click_rect_verified(identity, fallback_rect, SimulatedClickKind::Right, false)
        })?;
        thread::sleep(Duration::from_millis(240));
        if let Some(hit) =
            find_popup_clickable_by_keywords(&ctx.session, win_hwnd, pids, &["转发"], true)
                .filter(is_exact_forward_menu_item)
        {
            return Ok(Some(hit));
        }
        dismiss_popups_best_effort(identity);
    }

    Ok(None)
}

fn verify_dest_group_precheck(shared: &SharedInner, dest_target: &str) -> Result<()> {
    let mut guard_spec = shared.config.target.member_guard_for_group(dest_target);
    let known_count = matcher::parse_member_count(dest_target).or_else(|| {
        lookup_group_record(&shared.storage, dest_target)
            .and_then(|g| g.member_count.and_then(|c| u32::try_from(c).ok()))
    });
    if known_count.is_some() {
        guard_spec.require_member_count_verified = false;
        matcher::passes_member_guard(&guard_spec, known_count)?;
    }
    Ok(())
}

/// 通过桌面气泡右键菜单“转发”与目标选择器将单条已缓存消息原生转发至目标群聊或好友私聊
pub(crate) fn handle_forward_single_message(
    ctx: &mut WorkerContext,
    shared: &Arc<SharedInner>,
    req: &SingleForwardRequest,
) -> Result<SendOutcome> {
    let started = Instant::now();
    shared.check_outbound_deadline(&req.target, req.deadline_unix_ms)?;

    if req.message_id <= 0 {
        return Err(AdapterError::InputRejected(
            "forward message_id must be a positive integer".to_string(),
        ));
    }
    let dest_target = req.target.trim();
    if dest_target.is_empty() {
        return Err(AdapterError::InputRejected(
            "forward target cannot be empty".to_string(),
        ));
    }
    if req.target_kind == ChatTargetKind::Private {
        verify_private_whitelist(shared, dest_target)?;
    } else {
        verify_dest_group_precheck(shared, dest_target)?;
    }

    let record = shared
        .storage
        .get_message_by_id(req.message_id)?
        .ok_or_else(|| {
            AdapterError::InputRejected(format!(
                "forward message_id={} not found in SQLite",
                req.message_id
            ))
        })?;
    let source_name = canonical_storage_group_name(&record.group_name);
    let source_kind = record.chat_type;
    if source_kind == ChatTargetKind::Private {
        verify_private_whitelist(shared, &source_name)?;
    }

    let pids = refresh_pids_and_check_security(shared)?;
    let _fg_guard = ForegroundRestoreGuard::capture(&pids);
    let allow_mock = shared.allow_mock();
    let match_tokens = build_forward_match_tokens(&record);
    let same_session =
        source_name == canonical_storage_group_name(dest_target) && source_kind == req.target_kind;

    let baseline = if !same_session {
        let (dest_win, dest_active) =
            attach_chat_window(ctx, shared, dest_target, req.target_kind)?;
        let (_, _) =
            verify_target_guard_in_tree(ctx, shared, &dest_win, &dest_active, req.target_kind)?;
        let (dest_tree, _) = matcher::wake_and_dump_tree_ext(
            &ctx.session,
            dest_win.hwnd,
            INTERNAL_SCAN_DEPTH,
            false,
        )?;
        capture_baseline_from_tree(
            shared,
            &dest_tree,
            &dest_win,
            (req.target_kind, &dest_active),
            &match_tokens,
        )
    } else {
        ForwardBaselineSnapshot {
            match_count: 0,
            uids: HashSet::new(),
        }
    };

    shared.check_outbound_deadline(&req.target, req.deadline_unix_ms)?;

    let (src_win, src_active) = attach_chat_window(ctx, shared, &source_name, source_kind)?;
    let (src_identity, _) =
        verify_target_guard_in_tree(ctx, shared, &src_win, &src_active, source_kind)?;
    if is_multiselect_bar_active(&ctx.session, src_win.hwnd, &pids).is_some() {
        exit_multiselect_cleanly(&ctx.session, &src_identity, src_win.hwnd, &pids, allow_mock);
    }

    let baseline = if same_session {
        let (src_tree, _) = matcher::wake_and_dump_tree_ext(
            &ctx.session,
            src_win.hwnd,
            INTERNAL_SCAN_DEPTH,
            false,
        )?;
        capture_baseline_from_tree(
            shared,
            &src_tree,
            &src_win,
            (req.target_kind, &src_active),
            &match_tokens,
        )
    } else {
        baseline
    };

    let located = locate_source_bubble(ctx, &src_identity, &src_win, &src_active, &record)?;
    let restore_if_scrolled = |identity: &VerifiedWindowIdentity, loc: &LocatedSourceBubble| {
        if let (true, Some(pt)) = (loc.scroll_up_steps > 0, loc.scroll_point) {
            restore_message_list_to_bottom(identity, pt, loc.scroll_up_steps);
        }
    };

    if let Err(err) = shared.check_outbound_deadline(&req.target, req.deadline_unix_ms) {
        restore_if_scrolled(&src_identity, &located);
        return Err(err);
    }

    let forward_hit = match probe_forward_menu_with_fallback(
        ctx,
        &src_identity,
        src_win.hwnd,
        &pids,
        located.bub_rect,
    ) {
        Ok(Some(hit)) => hit,
        Ok(None) => {
            restore_if_scrolled(&src_identity, &located);
            return Err(AdapterError::UnsupportedAction {
                action: "forward_single_message".to_string(),
                reason: "QQNT message context menu did not expose '转发' item in UIA".to_string(),
            });
        }
        Err(err) => {
            restore_if_scrolled(&src_identity, &located);
            return Err(err);
        }
    };

    if let Err(err) = click_matched_target_verified(
        &src_identity,
        &forward_hit,
        SimulatedClickKind::Left,
        allow_mock,
    ) {
        dismiss_popups_best_effort(&src_identity);
        restore_if_scrolled(&src_identity, &located);
        return Err(err);
    }

    if !poll_forward_picker_modal(&ctx.session, &src_win, &pids, &[220, 300, 380]) {
        dismiss_popups_best_effort(&src_identity);
        restore_if_scrolled(&src_identity, &located);
        return Err(AdapterError::UnsupportedAction {
            action: "forward_single_message".to_string(),
            reason: "QQNT forward target picker modal did not appear after clicking '转发'"
                .to_string(),
        });
    }

    if let Err(err) = complete_forward_picker_modal(
        &ctx.session,
        &src_identity,
        &src_win,
        &pids,
        dest_target,
        allow_mock,
    ) {
        dismiss_popups_best_effort(&src_identity);
        restore_if_scrolled(&src_identity, &located);
        return Err(err);
    }

    thread::sleep(Duration::from_millis(320));
    if same_session {
        restore_if_scrolled(&src_identity, &located);
    }

    let (dest_win, dest_active) = attach_chat_window(ctx, shared, dest_target, req.target_kind)?;
    let (_, dest_member_count) =
        verify_target_guard_in_tree(ctx, shared, &dest_win, &dest_active, req.target_kind)?;

    let mut confirmed_msg: Option<ChatMessage> = None;
    for wait_ms in [220u64, 300, 380, 480, 620] {
        thread::sleep(Duration::from_millis(wait_ms));
        let Ok((dest_tree, _)) = matcher::wake_and_dump_tree_ext(
            &ctx.session,
            dest_win.hwnd,
            INTERNAL_SCAN_DEPTH,
            false,
        ) else {
            continue;
        };
        let matching = collect_dest_matching_self_messages(
            shared,
            &dest_tree,
            &dest_win,
            (req.target_kind, &dest_active),
            &match_tokens,
        );
        if let Some(new_msg) = matching
            .iter()
            .rfind(|m| !baseline.uids.contains(&m.message_id))
            .cloned()
        {
            confirmed_msg = Some(new_msg);
            break;
        }
        if matching.len() > baseline.match_count {
            confirmed_msg = matching.into_iter().last();
            break;
        }
    }

    let verified_msg = confirmed_msg.ok_or_else(|| {
        AdapterError::UiaFailure(format!(
            "single-message forward for message_id={} was submitted but no new bubble appeared in '{}'",
            req.message_id, dest_active
        ))
    })?;

    let summary_text = verified_msg.content.clone();
    let verify_norm = parser::normalize_message_text(&summary_text);
    if !verify_norm.is_empty() {
        shared.tracker.lock().record_outbound_for_session(
            req.target_kind,
            &dest_active,
            &verify_norm,
        );
    }
    let plan = OutboundSendPlan {
        combined_text: summary_text.clone(),
        images: Vec::new(),
        canonical_segments: match_tokens.segs,
        summary_text,
        verify_norm: verify_norm.clone(),
        degraded_segments: Vec::new(),
    };

    shared.state.lock().messages_sent += 1;
    let (onebot_msg_id, entity_id, stub_uid) = shared.persist_confirmed_sent_plan(
        &dest_active,
        req.target_kind,
        &plan,
        Some(&verified_msg),
        dest_member_count,
    );
    if let Some(msg_id) = onebot_msg_id {
        let media_token = extract_media_bubble_tokens(&plan.canonical_segments)
            .into_iter()
            .next();
        let dest_session_key = parser::tracker_session_key(req.target_kind, &dest_active);
        shared
            .tracker
            .lock()
            .register_confirmed_outbound(OutboundStubBinding {
                onebot_message_id: msg_id,
                stub_uid,
                norm_text: verify_norm,
                media_token,
                session_key: dest_session_key,
            });
    }

    let outcome = SendOutcome {
        ok: true,
        group: dest_active,
        used_mode: InputMode::FocusRestoreUnicode,
        foreground_touched: true,
        mouse_touched: true,
        verified_in_chat: true,
        elapsed_ms: started.elapsed().as_millis() as u64,
        detail: format!(
            "forwarded single message_id={} via native context menu",
            req.message_id
        ),
        onebot_message_id: onebot_msg_id,
        group_id: (req.target_kind == ChatTargetKind::Group).then_some(entity_id),
        target_kind: req.target_kind,
        user_id: (req.target_kind == ChatTargetKind::Private).then_some(entity_id),
        degraded_segments: Vec::new(),
    };
    if outcome.onebot_message_id.is_some() {
        shared.emit_event(AdapterEvent::MessageSent(outcome.clone()));
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use common_types::AdapterConfig;
    use storage::Storage;
    use tokio::sync::broadcast;

    #[test]
    fn forward_menu_filter_bottom_fallback_and_same_group_baseline_diff() {
        let forward_item = MatchedUiTarget {
            hwnd: 1,
            name: "转发".to_string(),
            control_type: "MenuItem".to_string(),
            automation_id: "menu_fwd".to_string(),
            rect: [965, 665, 1037, 694],
        };
        assert!(is_exact_forward_menu_item(&forward_item));
        assert!(!is_exact_forward_menu_item(&MatchedUiTarget {
            name: "合并转发".to_string(),
            ..forward_item.clone()
        }));
        assert!(!is_exact_forward_menu_item(&MatchedUiTarget {
            control_type: "Text".to_string(),
            ..forward_item
        }));
        assert_eq!(
            video_bottom_click_rect([900, 390, 1180, 560]),
            [930, 540, 934, 544]
        );

        let storage = Arc::new(Storage::open(":memory:").expect("mem db"));
        let (tx, _) = broadcast::channel(8);
        let shared = Arc::new(SharedInner::new(AdapterConfig::default(), storage, tx));
        let rec = StoredMessageRecord {
            onebot_message_id: 1001,
            sender_name: "群友A".to_string(),
            raw_message: "同群同文单条转发".to_string(),
            is_self: false,
            ..Default::default()
        };
        let tokens = build_forward_match_tokens(&rec);
        let peer_msg = ChatMessage {
            message_id: "uid-peer-same".to_string(),
            sender: "群友A".to_string(),
            content: "同群同文单条转发".to_string(),
            is_self: false,
            ..Default::default()
        };
        let self_old = ChatMessage {
            message_id: "uid-self-old".to_string(),
            sender: "测试机器人".to_string(),
            content: "同群同文单条转发".to_string(),
            is_self: true,
            ..Default::default()
        };
        assert!(!message_matches_forward_source(&peer_msg, &tokens));
        assert!(message_matches_forward_source(&self_old, &tokens));

        // 基线已有 1 条己方同文气泡，转发后新增第 2 条不同 UID 的己方气泡必须精准识别
        let baseline = ForwardBaselineSnapshot {
            match_count: 1,
            uids: HashSet::from([self_old.message_id.clone()]),
        };
        let self_new = ChatMessage {
            message_id: "uid-self-new".to_string(),
            ..self_old
        };
        assert!(!baseline.uids.contains(&self_new.message_id));
        assert_eq!(baseline.match_count, 1);
        assert!(shared.status_snapshot().messages_sent == 0);
    }
}
