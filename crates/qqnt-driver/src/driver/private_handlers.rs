//! 私聊白名单安全拦截、好友列表同步以及非群聊目标窗口打开与护栏校验处理器

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use common_types::{
    AdapterError, AdapterStatus, ChatTargetKind, FriendInfo, Result, UiNodeSnapshot, WindowInfo,
};
use storage::Storage;
use uia_core::{restore_no_activate, ForegroundRestoreGuard};

use super::group_handlers::{handle_list_sessions, sync_sessions_from_tree};
use super::runtime_state::{now_utc_ms, SharedInner};
use super::segment_enricher::{self, EnrichContext};
use super::window_discovery::{
    any_window_is_login_prompt, ensure_candidate_windows, login_splash_error,
    refresh_pids_and_check_security,
};
use super::worker_loop::WorkerContext;
use crate::constants::{AUDIT_PRIVATE_WHITELIST_ABORT, INTERNAL_SCAN_DEPTH};
use crate::matcher::{self, GroupSelectContext};
use crate::parser;

/// 严格校检目标好友是否位于 `[target].allowed_private_friends` 白名单，未配置或未命中则审计拦截
pub(crate) fn verify_private_whitelist(shared: &SharedInner, target: &str) -> Result<()> {
    let trimmed = target.trim();
    if shared.config.target.is_private_friend_allowed(trimmed) {
        return Ok(());
    }

    let audit_detail = format!("{{\"target\":{trimmed:?}}}");
    let _ = shared
        .storage
        .append_audit(AUDIT_PRIVATE_WHITELIST_ABORT, &audit_detail);
    Err(AdapterError::PrivateTargetNotAllowed {
        target: trimmed.to_string(),
    })
}

/// 在不发起任何额外 UIA 树扫描的前提下，校验已采集节点仍停留在预期私聊且不存在群特征
pub(crate) fn verify_pre_send_private_guard_snapshots(
    storage: &Storage,
    snapshots: &[&UiNodeSnapshot],
    expected_friend: &str,
    win_rect: [i32; 4],
    is_standalone: bool,
) -> Result<()> {
    let (live_hdr, live_cnt) =
        matcher::read_chat_header_from_snapshots(snapshots, win_rect, is_standalone, None);
    let has_sidebar = matcher::has_group_member_sidebar_in_snapshots(snapshots);

    let header_mismatch = live_hdr
        .as_deref()
        .map(|hdr| !hdr.trim().is_empty() && !matcher::group_title_matches(hdr, expected_friend))
        .unwrap_or(false);

    if header_mismatch || live_cnt.is_some() || has_sidebar {
        let audit_detail = format!(
            "{{\"expected_friend\":{:?},\"live_header\":{:?},\"live_count\":{:?},\"has_sidebar\":{has_sidebar}}}",
            expected_friend, live_hdr, live_cnt
        );
        let _ = storage.append_audit("pre_send_guard_abort", &audit_detail);
        return Err(AdapterError::InputRejected(format!(
            "pre-send private guard aborted: active header={live_hdr:?}, count={live_cnt:?}, sidebar={has_sidebar}, expected='{expected_friend}'"
        )));
    }

    Ok(())
}

/// 打开并安全附着指定私聊好友会话，严格核验无人数后缀且无右侧群成员列表
pub(crate) fn handle_open_private(
    ctx: &mut WorkerContext,
    shared: &Arc<SharedInner>,
    friend_target: &str,
) -> Result<AdapterStatus> {
    let (status, _) = open_private_with_unread(ctx, shared, friend_target, 0)?;
    Ok(status)
}

/// 打开指定私聊会话并根据未读数选择将尾部新入站消息交由追踪器增量触发 `PrivateMessage`
pub(crate) fn open_private_with_unread(
    ctx: &mut WorkerContext,
    shared: &Arc<SharedInner>,
    friend_target: &str,
    emit_unread: usize,
) -> Result<(AdapterStatus, Vec<common_types::ChatMessage>)> {
    verify_private_whitelist(shared, friend_target)?;
    let qq_pids = refresh_pids_and_check_security(shared)?;
    let _fg = ForegroundRestoreGuard::capture(&qq_pids);
    let windows = ensure_candidate_windows(shared)?;

    let mut selected = {
        let mut select_ctx = GroupSelectContext {
            session: &ctx.session,
            profile: &shared.profile,
            allow_mock: shared.allow_mock(),
            verified_cache: &mut ctx.verified_cache,
        };
        match matcher::select_target_private(&mut select_ctx, &windows, friend_target) {
            Ok(selected) => selected,
            Err(AdapterError::GroupNotFound { .. })
                if any_window_is_login_prompt(&ctx.session, &windows) =>
            {
                return Err(login_splash_error());
            }
            Err(err) => return Err(err),
        }
    };

    let _open_guard = selected._unocclude_guard.take();
    let _ = uia_core::refresh_window_info(&mut selected.window);

    let (tree, _dump_guard) = matcher::wake_and_dump_tree_ext(
        &ctx.session,
        selected.window.hwnd,
        INTERNAL_SCAN_DEPTH,
        true,
    )?;

    matcher::ensure_verified_window(
        &mut ctx.verified_cache,
        &selected.window,
        shared.allow_mock(),
    )?
    .verify_fast()?;

    let (live_hdr, live_cnt) = matcher::read_chat_header_from_tree(
        &tree,
        selected.window.rect,
        selected.window.is_standalone_chat,
        None,
    );
    if live_cnt.is_some() || matcher::has_group_member_sidebar(&tree) {
        return Err(AdapterError::InputRejected(format!(
            "target '{}' resolved to a group chat view (header={live_hdr:?}, count={live_cnt:?})",
            friend_target
        )));
    }

    shared.register_tree_self_nicknames(&tree, selected.window.rect);
    let mut rich_baseline = parser::extract_rich_messages_from_tree(
        &tree,
        selected.window.rect,
        selected.window.is_standalone_chat,
        &selected.active_title,
    );
    let bot_login = shared.resolve_login_info();
    let canon_target = super::runtime_state::canonical_storage_group_name(&selected.active_title);
    let enrich_ctx = EnrichContext {
        storage: &shared.storage,
        session_key: &canon_target,
        chat_kind: ChatTargetKind::Private,
        hwnd: selected.window.hwnd,
        members: &[],
        bot_login: &bot_login,
    };
    segment_enricher::enrich_parsed_messages(&enrich_ctx, &mut rich_baseline);
    let mut baseline: Vec<_> = rich_baseline.into_iter().map(|item| item.message).collect();
    let rebound = {
        let mut tracker = shared.tracker.lock();
        tracker.reconcile_polled_self_messages(&mut baseline)
    };
    for (onebot_id, ui_uid, ts_hint) in rebound {
        let _ = shared
            .storage
            .reconcile_outbound_stub_uid(onebot_id, &ui_uid, ts_hint.as_deref());
    }

    let split_idx = if emit_unread > 0 && !baseline.is_empty() {
        baseline.len().saturating_sub(emit_unread)
    } else {
        baseline.len()
    };
    let tail_incoming: Vec<_> = baseline
        .iter()
        .skip(split_idx)
        .filter(|msg| !msg.is_self)
        .cloned()
        .collect();

    let baseline_notices = parser::extract_system_notices_from_tree(
        &tree,
        selected.window.rect,
        selected.window.is_standalone_chat,
        &selected.active_title,
    );
    let mut new_incoming = {
        let mut tracker = shared.tracker.lock();
        tracker.seed_baseline(&baseline[..split_idx]);
        tracker.mark_session_seeded_for(ChatTargetKind::Private, &selected.active_title);
        tracker.seed_notice_baseline(&baseline_notices);
        let res = if tail_incoming.is_empty() {
            Vec::new()
        } else {
            let (_seen, mut fresh) = tracker.observe_poll(tail_incoming);
            for msg in &mut fresh {
                if !msg.reply_targets_self {
                    if let Some(reply_id) = msg.reply_to_message_id {
                        if tracker.is_confirmed_self_onebot_id(reply_id) {
                            msg.reply_targets_self = true;
                        }
                    }
                }
            }
            fresh
        };
        let visible_uids: Vec<String> = baseline.iter().map(|m| m.message_id.clone()).collect();
        tracker.diff_visible_uids_for(
            ChatTargetKind::Private,
            &selected.active_title,
            &visible_uids,
        );
        res
    };

    if !new_incoming.is_empty() {
        shared.state.lock().messages_observed += new_incoming.len() as u64;
    }

    shared.persist_messages_for_private(&selected.active_title, &mut baseline);
    if !new_incoming.is_empty() {
        shared.persist_messages_for_private(&selected.active_title, &mut new_incoming);
    }
    if !selected.window.is_standalone_chat {
        sync_sessions_from_tree(shared, &tree, selected.window.rect);
    }

    let active_title_clone = selected.active_title.clone();
    let raw_status = {
        let mut guard = shared.state.lock();
        guard.attached_window = Some(selected.window);
        guard.active_group = Some(selected.active_title);
        guard.active_chat_kind = ChatTargetKind::Private;
        guard.detected_member_count = None;
        guard.to_status(&shared.config)
    };
    shared.scheduler.lock().record_switch(
        now_utc_ms(),
        &active_title_clone,
        ChatTargetKind::Private,
        emit_unread > 0,
    );

    Ok((shared.enrich_status(raw_status), new_incoming))
}

/// 轻量级切换 UIA 选中项到已建立基线的白名单私聊会话（不重新播种历史基准以保留增量）
pub(crate) fn switch_active_private_selection(
    ctx: &mut WorkerContext,
    shared: &Arc<SharedInner>,
    friend_target: &str,
) -> Result<String> {
    verify_private_whitelist(shared, friend_target)?;
    let qq_pids = refresh_pids_and_check_security(shared)?;
    let _fg = ForegroundRestoreGuard::capture(&qq_pids);
    let windows = ensure_candidate_windows(shared)?;
    let mut select_ctx = GroupSelectContext {
        session: &ctx.session,
        profile: &shared.profile,
        allow_mock: shared.allow_mock(),
        verified_cache: &mut ctx.verified_cache,
    };
    let mut selected = matcher::select_target_private(&mut select_ctx, &windows, friend_target)?;
    let _open_guard = selected._unocclude_guard.take();
    let _ = uia_core::refresh_window_info(&mut selected.window);
    let active_title = selected.active_title.clone();
    {
        let mut guard = shared.state.lock();
        guard.attached_window = Some(selected.window);
        guard.active_group = Some(active_title.clone());
        guard.active_chat_kind = ChatTargetKind::Private;
        guard.detected_member_count = None;
    }
    shared.scheduler.lock().record_switch(
        now_utc_ms(),
        &active_title,
        ChatTargetKind::Private,
        true,
    );
    Ok(active_title)
}

/// 确认当前已附着窗口正处于目标私聊好友会话，未命中或类型切换时自动执行 `handle_open_private`
pub(crate) fn ensure_active_window_for_private(
    ctx: &mut WorkerContext,
    shared: &Arc<SharedInner>,
    requested_friend: &str,
) -> Result<(WindowInfo, String)> {
    verify_private_whitelist(shared, requested_friend)?;
    let req_trimmed = requested_friend.trim();
    if req_trimmed.is_empty() {
        return Err(AdapterError::InputRejected(
            "private target friend name cannot be empty".to_string(),
        ));
    }

    let current = {
        let guard = shared.state.lock();
        (
            guard.attached_window.clone(),
            guard.active_group.clone(),
            guard.active_chat_kind,
        )
    };

    if let (Some(mut win), Some(active), ChatTargetKind::Private) = current {
        if matcher::group_title_matches(&active, req_trimmed)
            && uia_core::refresh_window_info(&mut win)
        {
            if win.is_iconic {
                restore_no_activate(win.hwnd);
                thread::sleep(Duration::from_millis(120));
                let _ = uia_core::refresh_window_info(&mut win);
            }
            shared.state.lock().attached_window = Some(win.clone());
            return Ok((win, active));
        }
    }

    let _ = handle_open_private(ctx, shared, req_trimmed)?;
    let guard = shared.state.lock();
    let win = guard.attached_window.clone().ok_or_else(|| {
        AdapterError::WindowNotFound("failed to attach target private chat window".to_string())
    })?;
    let active = guard
        .active_group
        .clone()
        .ok_or_else(|| AdapterError::GroupNotFound {
            group: req_trimmed.to_string(),
        })?;
    Ok((win, active))
}

/// 查询嵌入式好友列表缓存；当明确要求 `no_cache` 或本地缓存为空时尽力刷新左侧会话列表
pub(crate) fn handle_get_friend_list(
    ctx: &mut WorkerContext,
    shared: &Arc<SharedInner>,
    no_cache: bool,
) -> Result<Vec<FriendInfo>> {
    if no_cache {
        let _ = shared
            .storage
            .purge_invalid_friends(matcher::is_valid_session_title);
    }
    let initial = shared.storage.list_friends()?;
    if no_cache || initial.is_empty() {
        let _ = handle_list_sessions(ctx, shared);
        if no_cache {
            let _ = shared
                .storage
                .purge_invalid_friends(matcher::is_valid_session_title);
        }
        let login = shared.resolve_login_info();
        let _ = shared
            .storage
            .reconcile_member_mapped_ids(Some(&login.nickname), Some(login.user_id));
    }
    shared.storage.list_friends()
}
