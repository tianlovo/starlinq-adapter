//! 桌面多选合并转发执行器：通过右键菜单“多选”->勾选消息->“合并转发”->目标选择器完成真实卡片发送与编号持久化

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use common_types::{
    AdapterError, ChatMessage, ChatTargetKind, ForwardSendOutcome, ForwardSendRequest,
    MessageSegment, Result, StoredMessageRecord, WindowInfo,
};
use serde_json::{json, Value};
use uia_core::constants::{CONTROL_TYPE_CHECK_BOX, CONTROL_TYPE_LIST_ITEM};
use uia_core::{ForegroundRestoreGuard, SimulatedClickKind, UiaSession, VerifiedWindowIdentity};

use super::popup_helpers::{
    click_matched_target_verified, click_point_verified, click_rect_verified,
    collect_visible_bubbles, dismiss_popups_best_effort, find_overlay_targets_in_snapshot,
    find_popup_clickable_by_keywords, find_targets_in_snapshot, locate_visible_message_bubble,
    scan_same_pid_popup_trees, VisibleBubbleEntry,
};
pub(crate) use super::single_forward_ops::{
    complete_forward_picker_modal, has_forward_picker_modal, pick_modal_search_edit,
    poll_forward_picker_modal,
};
use crate::constants::INTERNAL_SCAN_DEPTH;
use crate::driver::group_handlers::{ensure_active_window_for_group, handle_open_group};
use crate::driver::outbound_media::{self, OutboundSendPlan};
use crate::driver::private_handlers::{
    ensure_active_window_for_private, handle_open_private, verify_pre_send_private_guard_snapshots,
    verify_private_whitelist,
};
use crate::driver::runtime_state::{canonical_storage_group_name, SharedInner};
use crate::driver::send_handlers::{verify_pre_send_chat_guard_snapshots, PreSendGuardCheck};
use crate::driver::window_discovery::refresh_pids_and_check_security;
use crate::driver::worker_loop::WorkerContext;
use crate::matcher;
use crate::parser::{self, OutboundStubBinding};

const MAX_NATIVE_FORWARD_MESSAGES: usize = 50;

/// 桌面多选合并转发过程中携带源会话与目标会话信息的内部参数对象
struct ForwardRunSpec<'a> {
    source_name: &'a str,
    source_kind: ChatTargetKind,
    dest_name: &'a str,
    dest_kind: ChatTargetKind,
}

/// 多选合并转发桌面交互成功后的目标会话确认基线上下文
struct ForwardUiOutcome {
    dest_win: WindowInfo,
    dest_active: String,
    dest_member_count: Option<u32>,
    baseline_cards: usize,
    baseline_last_card: Option<String>,
}

pub(super) fn verify_target_guard_in_tree(
    ctx: &mut WorkerContext,
    shared: &Arc<SharedInner>,
    win: &WindowInfo,
    target_name: &str,
    target_kind: ChatTargetKind,
) -> Result<(VerifiedWindowIdentity, Option<u32>)> {
    let identity_ref =
        matcher::ensure_verified_window(&mut ctx.verified_cache, win, shared.allow_mock())?;
    identity_ref.verify_fast()?;
    let identity =
        VerifiedWindowIdentity::verify_and_bind(identity_ref.hwnd(), shared.allow_mock())?;
    let (tree, _guard) =
        matcher::wake_and_dump_tree_ext(&ctx.session, win.hwnd, INTERNAL_SCAN_DEPTH, false)?;
    let mut flat_refs = Vec::new();
    collect_snapshot_refs(&tree, &mut flat_refs);

    let (_hdr, live_cnt) = matcher::read_chat_header_from_snapshots(
        &flat_refs,
        win.rect,
        win.is_standalone_chat,
        None,
    );
    match target_kind {
        ChatTargetKind::Group => {
            let guard_spec = shared.config.target.member_guard_for_group(target_name);
            verify_pre_send_chat_guard_snapshots(
                &shared.storage,
                &flat_refs,
                &PreSendGuardCheck {
                    expected_group: target_name,
                    win_rect: win.rect,
                    is_standalone: win.is_standalone_chat,
                    target_spec: &guard_spec,
                },
            )?;
        }
        ChatTargetKind::Private => {
            verify_pre_send_private_guard_snapshots(
                &shared.storage,
                &flat_refs,
                target_name,
                win.rect,
                win.is_standalone_chat,
            )?;
        }
    }
    Ok((identity, live_cnt))
}

fn collect_snapshot_refs<'a>(
    node: &'a common_types::UiNodeSnapshot,
    out: &mut Vec<&'a common_types::UiNodeSnapshot>,
) {
    out.push(node);
    for child in &node.children {
        collect_snapshot_refs(child, out);
    }
}

pub(super) fn attach_chat_window(
    ctx: &mut WorkerContext,
    shared: &Arc<SharedInner>,
    name: &str,
    kind: ChatTargetKind,
) -> Result<(WindowInfo, String)> {
    let pair = match kind {
        ChatTargetKind::Group => ensure_active_window_for_group(ctx, shared, name)?,
        ChatTargetKind::Private => ensure_active_window_for_private(ctx, shared, name)?,
    };
    let (win, active) = pair;
    let Ok((tree, _)) =
        matcher::wake_and_dump_tree_ext(&ctx.session, win.hwnd, INTERNAL_SCAN_DEPTH, false)
    else {
        return Ok((win, active));
    };
    let mut flat_refs = Vec::new();
    collect_snapshot_refs(&tree, &mut flat_refs);
    let (live_hdr, _) = matcher::read_chat_header_from_snapshots(
        &flat_refs,
        win.rect,
        win.is_standalone_chat,
        None,
    );
    let mismatch = live_hdr
        .as_deref()
        .map(|h| !h.trim().is_empty() && !matcher::group_title_matches(h, &active))
        .unwrap_or(false);
    if !mismatch {
        return Ok((win, active));
    }

    match kind {
        ChatTargetKind::Group => {
            let spec = shared.config.target.member_guard_for_group(name);
            handle_open_group(ctx, shared, &spec)?;
            ensure_active_window_for_group(ctx, shared, name)
        }
        ChatTargetKind::Private => {
            handle_open_private(ctx, shared, name)?;
            ensure_active_window_for_private(ctx, shared, name)
        }
    }
}

fn count_forward_cards_in_tree(
    tree: &common_types::UiNodeSnapshot,
    win: &WindowInfo,
    chat_name: &str,
) -> Vec<ChatMessage> {
    parser::extract_messages_from_tree(tree, win.rect, win.is_standalone_chat, chat_name)
        .into_iter()
        .filter(|m| {
            m.content.contains("聊天记录")
                || m.content.contains("转发消息")
                || m.content.contains("合并转发")
        })
        .collect()
}

fn match_bubble_in_candidates(
    candidates: &[VisibleBubbleEntry],
    rec: &StoredMessageRecord,
    segs: &[MessageSegment],
    plain: &str,
    excerpt: &str,
) -> Option<[i32; 4]> {
    if candidates.is_empty() {
        return None;
    }
    if let Some(rect) = locate_visible_message_bubble(candidates, &rec.sender_name, excerpt, plain)
    {
        return Some(rect);
    }

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
    if !pure_text.is_empty() {
        let pure_excerpt = outbound_media::compact_reply_excerpt(&pure_text, 28);
        if let Some(rect) =
            locate_visible_message_bubble(candidates, &rec.sender_name, &pure_excerpt, &pure_text)
        {
            return Some(rect);
        }
    }
    for tok in super::popup_helpers::extract_media_bubble_tokens(segs) {
        if let Some(rect) = locate_visible_message_bubble(candidates, &rec.sender_name, &tok, &tok)
        {
            return Some(rect);
        }
    }
    None
}

pub(super) fn find_bubble_for_record(
    bubbles: &[VisibleBubbleEntry],
    rec: &StoredMessageRecord,
    excluded_rects: &[[i32; 4]],
) -> Option<[i32; 4]> {
    let available: Vec<VisibleBubbleEntry> = bubbles
        .iter()
        .filter(|b| {
            let not_excluded = !excluded_rects.iter().any(|ex| {
                (b.bubble_rect[1] - ex[1]).abs() <= 16 && (b.bubble_rect[3] - ex[3]).abs() <= 16
            });
            // 自发记录严格排除明确 `!b.is_self` 的群友气泡，避免管理员账号误撤回他人消息
            not_excluded && (!rec.is_self || b.is_self)
        })
        .cloned()
        .collect();
    let segs = if !rec.segments.is_empty() {
        rec.segments.clone()
    } else {
        common_types::parse_cq_string_to_segments(&rec.raw_message, true)
    };
    let plain = common_types::segments_to_plain_summary(&segs);
    let excerpt = outbound_media::compact_reply_excerpt(&plain, 28);
    let preferred: Vec<VisibleBubbleEntry> = available
        .iter()
        .filter(|b| b.is_self == rec.is_self)
        .cloned()
        .collect();
    if let Some(rect) = match_bubble_in_candidates(&preferred, rec, &segs, &plain, &excerpt) {
        return Some(rect);
    }
    if rec.is_self || preferred.len() == available.len() {
        return None;
    }
    match_bubble_in_candidates(&available, rec, &segs, &plain, &excerpt)
}

/// 检测主窗口当前是否处于消息多选工具条激活状态，若是则返回“合并转发”按钮矩形
pub(crate) fn is_multiselect_bar_active(
    session: &UiaSession,
    main_hwnd: isize,
    pids: &[u32],
) -> Option<[i32; 4]> {
    find_popup_clickable_by_keywords(session, main_hwnd, pids, &["合并转发", "逐条转发"], true)
        .map(|hit| hit.rect)
}

fn find_multiselect_close_rect(
    session: &UiaSession,
    main_hwnd: isize,
    merge_rect: [i32; 4],
) -> Option<[i32; 4]> {
    let (tree, _) =
        matcher::wake_and_dump_tree_ext(session, main_hwnd, INTERNAL_SCAN_DEPTH, false).ok()?;
    let hits = find_overlay_targets_in_snapshot(main_hwnd, &tree, &|node| {
        node.is_enabled
            && node
                .control_type
                .trim()
                .eq_ignore_ascii_case(CONTROL_TYPE_LIST_ITEM)
            && node.name.trim().is_empty()
            && (12..=40).contains(&(node.rect[2] - node.rect[0]))
            && (12..=40).contains(&(node.rect[3] - node.rect[1]))
            && node.rect[0] > merge_rect[2] + 80
            && (node.rect[1] - (merge_rect[1] - 42)).abs() <= 45
    });
    hits.last().map(|hit| hit.rect)
}

pub(crate) fn exit_multiselect_cleanly(
    session: &UiaSession,
    identity: &VerifiedWindowIdentity,
    main_hwnd: isize,
    pids: &[u32],
    allow_mock: bool,
) {
    let windows = scan_same_pid_popup_trees(session, main_hwnd, pids, 10);
    for (popup_hwnd, _cls, _title, snap) in &windows {
        if *popup_hwnd == main_hwnd {
            continue;
        }
        if let Ok(popup_identity) = VerifiedWindowIdentity::verify_and_bind(*popup_hwnd, allow_mock)
        {
            let close_hits = find_targets_in_snapshot(*popup_hwnd, snap, &|node| {
                let ct = node.control_type.trim();
                let nm = node.name.trim();
                node.is_enabled
                    && (ct.eq_ignore_ascii_case("Button") || ct.eq_ignore_ascii_case("Text"))
                    && (nm == "取消" || nm == "关闭")
            });
            if let Some(btn) = close_hits.last() {
                let _ =
                    click_rect_verified(&popup_identity, btn.rect, SimulatedClickKind::Left, true);
            } else {
                dismiss_popups_best_effort(&popup_identity);
            }
            thread::sleep(Duration::from_millis(180));
        }
    }

    if let Some(cancel) =
        find_popup_clickable_by_keywords(session, main_hwnd, pids, &["取消", "关闭多选"], true)
    {
        let _ = click_rect_verified(identity, cancel.rect, SimulatedClickKind::Left, true);
        thread::sleep(Duration::from_millis(260));
    }

    for _ in 0..3 {
        let Some(merge_rect) = is_multiselect_bar_active(session, main_hwnd, pids) else {
            return;
        };
        if let Some(close_rect) = find_multiselect_close_rect(session, main_hwnd, merge_rect) {
            let _ = click_rect_verified(identity, close_rect, SimulatedClickKind::Left, true);
        } else {
            dismiss_popups_best_effort(identity);
        }
        thread::sleep(Duration::from_millis(220));
    }
}

fn find_message_list_left(
    hwnd: isize,
    tree: &common_types::UiNodeSnapshot,
    bub_rect: [i32; 4],
    win_rect: [i32; 4],
) -> i32 {
    let list_nodes = find_targets_in_snapshot(hwnd, tree, &|node| {
        node.automation_id.trim().eq_ignore_ascii_case("ml-root") || node.name.trim() == "消息列表"
    });
    list_nodes
        .into_iter()
        .next()
        .map(|n| n.rect[0])
        .unwrap_or_else(|| (bub_rect[0] - 42).max(win_rect[0] + 260))
}

pub(crate) fn click_bubble_multiselect_radio(
    identity: &VerifiedWindowIdentity,
    win: &WindowInfo,
    tree: &common_types::UiNodeSnapshot,
    bub_rect: [i32; 4],
) -> Result<()> {
    let checkbox_hits = find_targets_in_snapshot(win.hwnd, tree, &|node| {
        node.is_enabled
            && node
                .control_type
                .trim()
                .eq_ignore_ascii_case(CONTROL_TYPE_CHECK_BOX)
            && node.rect[3] >= bub_rect[1] - 12
            && node.rect[1] <= bub_rect[3] + 12
    });
    if let Some(cb) = checkbox_hits.first() {
        click_rect_verified(identity, cb.rect, SimulatedClickKind::Left, false)
    } else {
        let ml_left = find_message_list_left(win.hwnd, tree, bub_rect, win.rect);
        let radio_x = ml_left + 26;
        let radio_y = (bub_rect[1] + bub_rect[3]) / 2;
        click_point_verified(identity, radio_x, radio_y, SimulatedClickKind::Left, false)
    }
}

fn select_additional_bubbles(
    session: &UiaSession,
    identity: &VerifiedWindowIdentity,
    win: &WindowInfo,
    source_name: &str,
    records: &[StoredMessageRecord],
    used_rects: &mut Vec<[i32; 4]>,
) -> Result<()> {
    for rec in records {
        let (tree, _guard) =
            matcher::wake_and_dump_tree_ext(session, win.hwnd, INTERNAL_SCAN_DEPTH, false)?;
        let bubbles = collect_visible_bubbles(&tree, win.rect, win.is_standalone_chat, source_name);
        let Some(bub_rect) = find_bubble_for_record(&bubbles, rec, used_rects) else {
            return Err(AdapterError::InputRejected(format!(
                "forward message_id={} is not visible in current chat viewport for multi-select",
                rec.onebot_message_id
            )));
        };
        used_rects.push(bub_rect);
        click_bubble_multiselect_radio(identity, win, &tree, bub_rect)?;
        thread::sleep(Duration::from_millis(180));
    }
    Ok(())
}

fn serialize_forward_records_json(records: &[StoredMessageRecord]) -> Result<String> {
    let nodes: Vec<Value> = records
        .iter()
        .map(|rec| {
            let content_val = if !rec.segments.is_empty() {
                serde_json::to_value(&rec.segments)
                    .unwrap_or_else(|_| Value::String(rec.raw_message.clone()))
            } else {
                Value::String(rec.raw_message.clone())
            };
            json!({
                "type": "node",
                "data": {
                    "id": rec.onebot_message_id.to_string(),
                    "user_id": rec.user_id.to_string(),
                    "nickname": rec.sender_name,
                    "content": content_val,
                }
            })
        })
        .collect();
    serde_json::to_string(&nodes).map_err(|err| {
        AdapterError::InputRejected(format!("failed to serialize forward nodes: {err}"))
    })
}

fn execute_forward_ui_sequence(
    ctx: &mut WorkerContext,
    shared: &Arc<SharedInner>,
    spec: &ForwardRunSpec<'_>,
    pids: &[u32],
    records: &[StoredMessageRecord],
) -> Result<ForwardUiOutcome> {
    let (src_win, src_active) =
        attach_chat_window(ctx, shared, spec.source_name, spec.source_kind)?;
    let (src_identity, _) =
        verify_target_guard_in_tree(ctx, shared, &src_win, &src_active, spec.source_kind)?;
    let allow_mock = shared.allow_mock();
    if is_multiselect_bar_active(&ctx.session, src_win.hwnd, pids).is_some() {
        exit_multiselect_cleanly(&ctx.session, &src_identity, src_win.hwnd, pids, allow_mock);
    }

    let (src_tree, _) =
        matcher::wake_and_dump_tree_ext(&ctx.session, src_win.hwnd, INTERNAL_SCAN_DEPTH, false)?;
    let (baseline_cards, baseline_last_card) = if canonical_storage_group_name(spec.source_name)
        == canonical_storage_group_name(spec.dest_name)
        && spec.source_kind == spec.dest_kind
    {
        let existing = count_forward_cards_in_tree(&src_tree, &src_win, &src_active);
        let last = existing.last().map(|c| c.content.clone());
        (existing.len(), last)
    } else {
        (0, None)
    };

    let bubbles = collect_visible_bubbles(
        &src_tree,
        src_win.rect,
        src_win.is_standalone_chat,
        &src_active,
    );
    let first_bub = find_bubble_for_record(&bubbles, &records[0], &[]).ok_or_else(|| {
        AdapterError::InputRejected(format!(
            "first forward message_id={} is not visible in current chat viewport",
            records[0].onebot_message_id
        ))
    })?;
    let mut used_rects = vec![first_bub];

    src_identity.verify_fast()?;
    click_rect_verified(&src_identity, first_bub, SimulatedClickKind::Right, false)?;
    thread::sleep(Duration::from_millis(220));

    let multi_btn =
        find_popup_clickable_by_keywords(&ctx.session, src_win.hwnd, pids, &["多选"], true)
            .ok_or_else(|| {
                dismiss_popups_best_effort(&src_identity);
                AdapterError::UnsupportedAction {
                    action: "send_forward_msg".to_string(),
                    reason: "QQNT message context menu did not expose '多选' item in UIA"
                        .to_string(),
                }
            })?;

    click_matched_target_verified(
        &src_identity,
        &multi_btn,
        SimulatedClickKind::Left,
        allow_mock,
    )?;
    thread::sleep(Duration::from_millis(240));

    let abort_cleanly = |err: AdapterError| -> AdapterError {
        exit_multiselect_cleanly(&ctx.session, &src_identity, src_win.hwnd, pids, allow_mock);
        err
    };

    select_additional_bubbles(
        &ctx.session,
        &src_identity,
        &src_win,
        &src_active,
        &records[1..],
        &mut used_rects,
    )
    .map_err(abort_cleanly)?;

    let merge_btn =
        find_popup_clickable_by_keywords(&ctx.session, src_win.hwnd, pids, &["合并转发"], true)
            .ok_or_else(|| AdapterError::UnsupportedAction {
                action: "send_forward_msg".to_string(),
                reason: "QQNT multi-select bar did not expose '合并转发' button in UIA".to_string(),
            })
            .map_err(abort_cleanly)?;

    click_matched_target_verified(
        &src_identity,
        &merge_btn,
        SimulatedClickKind::Left,
        allow_mock,
    )
    .map_err(abort_cleanly)?;
    if !poll_forward_picker_modal(&ctx.session, &src_win, pids, &[260, 340])
        && merge_btn.rect[1] > src_win.rect[1] + 200
    {
        let icon_x = (merge_btn.rect[0] + merge_btn.rect[2]) / 2;
        let icon_y = merge_btn.rect[1] - 24;
        click_point_verified(
            &src_identity,
            icon_x,
            icon_y,
            SimulatedClickKind::Left,
            true,
        )
        .map_err(abort_cleanly)?;
        let _ = poll_forward_picker_modal(&ctx.session, &src_win, pids, &[280, 380]);
    }

    complete_forward_picker_modal(
        &ctx.session,
        &src_identity,
        &src_win,
        pids,
        spec.dest_name,
        allow_mock,
    )
    .map_err(abort_cleanly)?;
    thread::sleep(Duration::from_millis(380));
    if is_multiselect_bar_active(&ctx.session, src_win.hwnd, pids).is_some() {
        exit_multiselect_cleanly(&ctx.session, &src_identity, src_win.hwnd, pids, allow_mock);
    }

    let (dest_win, dest_active) = attach_chat_window(ctx, shared, spec.dest_name, spec.dest_kind)?;
    let (_, dest_member_count) =
        verify_target_guard_in_tree(ctx, shared, &dest_win, &dest_active, spec.dest_kind)?;

    Ok(ForwardUiOutcome {
        dest_win,
        dest_active,
        dest_member_count,
        baseline_cards,
        baseline_last_card,
    })
}

/// 执行真实桌面端多选合并转发并校验目标会话新 `[聊天记录]` 气泡落地后再写入数据库
pub(crate) fn handle_send_forward_messages(
    ctx: &mut WorkerContext,
    shared: &Arc<SharedInner>,
    req: &ForwardSendRequest,
) -> Result<ForwardSendOutcome> {
    if req.message_ids.is_empty() {
        return Err(AdapterError::InputRejected(
            "forward message_ids cannot be empty".to_string(),
        ));
    }
    if req.message_ids.len() > MAX_NATIVE_FORWARD_MESSAGES {
        return Err(AdapterError::InputRejected(format!(
            "forward message_ids count {} exceeds limit {MAX_NATIVE_FORWARD_MESSAGES}",
            req.message_ids.len()
        )));
    }
    if req.target_kind == ChatTargetKind::Private {
        verify_private_whitelist(shared, &req.target)?;
    }

    let mut records = Vec::with_capacity(req.message_ids.len());
    for id in &req.message_ids {
        let rec = shared.storage.get_message_by_id(*id)?.ok_or_else(|| {
            AdapterError::InputRejected(format!(
                "forward referenced message_id={id} not found in SQLite"
            ))
        })?;
        records.push(rec);
    }

    let source_name = canonical_storage_group_name(&records[0].group_name);
    let source_kind = records[0].chat_type;
    if source_kind == ChatTargetKind::Private {
        verify_private_whitelist(shared, &source_name)?;
    }
    for rec in &records[1..] {
        if canonical_storage_group_name(&rec.group_name) != source_name
            || rec.chat_type != source_kind
        {
            return Err(AdapterError::InputRejected(
                "native multi-select forward requires all referenced message_ids to belong to the same chat session"
                    .to_string(),
            ));
        }
    }

    let pids = refresh_pids_and_check_security(shared)?;
    let _outer_fg = ForegroundRestoreGuard::capture(&pids);

    let spec = ForwardRunSpec {
        source_name: &source_name,
        source_kind,
        dest_name: &req.target,
        dest_kind: req.target_kind,
    };
    let ui_out = execute_forward_ui_sequence(ctx, shared, &spec, &pids, &records)?;
    let ForwardUiOutcome {
        dest_win,
        dest_active,
        dest_member_count,
        baseline_cards,
        baseline_last_card,
    } = ui_out;

    let mut confirmed_msg: Option<ChatMessage> = None;
    for wait_ms in [260u64, 340, 420, 520, 650, 800] {
        thread::sleep(Duration::from_millis(wait_ms));
        if let Ok((dest_tree, _)) =
            matcher::wake_and_dump_tree_ext(&ctx.session, dest_win.hwnd, INTERNAL_SCAN_DEPTH, false)
        {
            let cards = count_forward_cards_in_tree(&dest_tree, &dest_win, &dest_active);
            let tail_changed = cards
                .last()
                .map(|c| Some(&c.content) != baseline_last_card.as_ref())
                .unwrap_or(false);
            if cards.len() > baseline_cards || tail_changed {
                confirmed_msg = cards.into_iter().last();
                break;
            }
        }
    }

    let verified_card = confirmed_msg.ok_or_else(|| {
        AdapterError::UiaFailure(
            "merge-forward was submitted but no new '[聊天记录]' bubble appeared in target chat"
                .to_string(),
        )
    })?;

    let nodes_json = serialize_forward_records_json(&records)?;
    let forward_id = shared.storage.allocate_forward_nodes(&nodes_json)?;
    let summary_text = verified_card.content.clone();
    let verify_norm = parser::normalize_message_text(&summary_text);
    let plan = OutboundSendPlan {
        combined_text: summary_text.clone(),
        images: Vec::new(),
        canonical_segments: vec![MessageSegment::Forward {
            id: forward_id.clone(),
        }],
        summary_text,
        verify_norm: verify_norm.clone(),
        degraded_segments: Vec::new(),
    };

    shared.state.lock().messages_sent += 1;
    let (onebot_msg_id_opt, entity_id, stub_uid) = shared.persist_confirmed_sent_plan(
        &dest_active,
        req.target_kind,
        &plan,
        Some(&verified_card),
        dest_member_count,
    );
    let message_id = onebot_msg_id_opt.ok_or_else(|| {
        AdapterError::UiaFailure(
            "failed to persist confirmed merge-forward message record".to_string(),
        )
    })?;
    let dest_session_key = parser::tracker_session_key(req.target_kind, &dest_active);
    shared
        .tracker
        .lock()
        .register_confirmed_outbound(OutboundStubBinding {
            onebot_message_id: message_id,
            stub_uid,
            norm_text: verify_norm,
            media_token: None,
            session_key: dest_session_key,
        });

    Ok(ForwardSendOutcome {
        message_id,
        forward_id,
        target: dest_active,
        target_kind: req.target_kind,
        group_id: (req.target_kind == ChatTargetKind::Group).then_some(entity_id),
        user_id: (req.target_kind == ChatTargetKind::Private).then_some(entity_id),
    })
}
