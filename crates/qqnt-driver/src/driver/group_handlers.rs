//! 会话列表刷新、目标群切换、群消息轮询增量持久化与被动元数据收割处理器

use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use common_types::{
    AdapterError, AdapterEvent, AdapterStatus, ChatMessage, ChatTargetKind, GroupMemberInfo,
    GroupTargetSpec, Result, SessionSummary, UiNodeSnapshot, WindowInfo,
};
use tracing::{debug, warn};
use uia_core::{restore_no_activate, win32, ForegroundRestoreGuard};

use super::private_handlers::{
    ensure_active_window_for_private, handle_open_private, open_private_with_unread,
    switch_active_private_selection, verify_private_whitelist,
};
use super::runtime_state::{canonical_storage_group_name, now_utc_ms, SharedInner};
use super::segment_enricher::{self, EnrichContext};
use super::window_discovery::{
    any_window_is_login_prompt, discover_windows, ensure_candidate_windows, is_narrow_login_window,
    login_splash_error, refresh_pids_and_check_security, window_looks_like_login_prompt,
};
use super::worker_loop::WorkerContext;
use crate::constants::{INTERNAL_SCAN_DEPTH, SIDEBAR_MEMBER_REFRESH_INTERVAL_MS};
use crate::matcher::{self, DrawerInspectRequest, GroupSelectContext};
use crate::parser;

/// 执行群聊消息采集请求时使用的参数对象（Parameter Object）
pub(crate) struct PollRequest<'a> {
    pub(crate) group: &'a str,
    pub(crate) limit: usize,
    pub(crate) allow_unocclude: bool,
}

/// 采集当前已附加或主候选 QQ 窗口左侧的会话列表，并将摘要及未读数同步至 SQLite
pub(crate) fn handle_list_sessions(
    ctx: &mut WorkerContext,
    shared: &Arc<SharedInner>,
) -> Result<Vec<SessionSummary>> {
    let _pids = refresh_pids_and_check_security(shared)?;
    let attached = shared.state.lock().attached_window.clone();

    let mut target = match attached.filter(|win| win32::is_window_valid(win.hwnd)) {
        Some(win) => win,
        None => {
            let windows = ensure_candidate_windows(shared)?;
            windows
                .into_iter()
                .find(|win| !win.is_standalone_chat)
                .ok_or_else(|| {
                    AdapterError::WindowNotFound(
                        "no main QQNT or MockQQ window available".to_string(),
                    )
                })?
        }
    };

    if !uia_core::refresh_window_info(&mut target) {
        return Err(AdapterError::WindowNotFound(
            "attached QQNT or MockQQ window is no longer valid".to_string(),
        ));
    }

    if is_narrow_login_window(&target) {
        return Err(login_splash_error());
    }

    if target.is_iconic {
        restore_no_activate(target.hwnd);
        thread::sleep(Duration::from_millis(120));
        let _ = uia_core::refresh_window_info(&mut target);
    }

    if is_narrow_login_window(&target) {
        return Err(login_splash_error());
    }

    matcher::ensure_verified_window(&mut ctx.verified_cache, &target, shared.allow_mock())?
        .verify_fast()?;

    let (mut nodes, _unocclude_guard) =
        matcher::wake_and_collect_ext(&ctx.session, target.hwnd, INTERNAL_SCAN_DEPTH, true)?;
    if window_looks_like_login_prompt(&target, &nodes) {
        return Err(login_splash_error());
    }

    if matcher::clear_search_mode_if_active(target.hwnd, shared.allow_mock(), &nodes, target.rect) {
        if let Ok((refreshed, _)) =
            matcher::wake_and_collect_ext(&ctx.session, target.hwnd, INTERNAL_SCAN_DEPTH, false)
        {
            nodes = refreshed;
        }
    }

    let sessions = matcher::extract_sessions(&nodes, target.rect);
    if !matcher::is_search_mode_active(&nodes, target.rect) {
        let _ = shared.upsert_session_summaries(&sessions);
    }
    Ok(sessions)
}

/// 打开并安全附着指定群聊：校验人数护栏、播种消息基线、收割侧边栏成员并在必要时触发抽屉探测
pub(crate) fn handle_open_group(
    ctx: &mut WorkerContext,
    shared: &Arc<SharedInner>,
    spec: &GroupTargetSpec,
) -> Result<AdapterStatus> {
    let (status, _) = open_group_with_unread(ctx, shared, spec, 0, false)?;
    Ok(status)
}

/// 打开指定群聊并根据后台累积未读条数将尾部新增消息纳入增量派发队列
fn open_group_with_unread(
    ctx: &mut WorkerContext,
    shared: &Arc<SharedInner>,
    spec: &GroupTargetSpec,
    emit_unread: usize,
    count_switch: bool,
) -> Result<(AdapterStatus, Vec<ChatMessage>)> {
    let qq_pids = refresh_pids_and_check_security(shared)?;
    let _fg = ForegroundRestoreGuard::capture(&qq_pids);
    let windows = ensure_candidate_windows(shared)?;
    let effective_spec = spec.member_guard_for_group(&spec.group_name);

    let mut selected = {
        let mut select_ctx = GroupSelectContext {
            session: &ctx.session,
            profile: &shared.profile,
            allow_mock: shared.allow_mock(),
            verified_cache: &mut ctx.verified_cache,
        };
        match matcher::select_target_group(&mut select_ctx, &windows, &effective_spec) {
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

    shared.register_tree_self_nicknames(&tree, selected.window.rect);
    let mut rich_baseline = parser::extract_rich_messages_from_tree(
        &tree,
        selected.window.rect,
        selected.window.is_standalone_chat,
        &selected.active_title,
    );
    strip_shed_p2_image_rects_on_burst(shared, &mut rich_baseline);

    let canon_group = canonical_storage_group_name(&selected.active_title);

    // 优先尝试双通道探测真实数字群号，使随后写入的基线消息与成员缓存直接绑定真实群号
    if matcher::ensure_verified_window(
        &mut ctx.verified_cache,
        &selected.window,
        shared.allow_mock(),
    )
    .is_ok()
    {
        if let Some(identity) = ctx.verified_cache.as_ref() {
            if identity.verify_fast().is_ok() {
                let inspect_req = DrawerInspectRequest {
                    window: &selected.window,
                    group_name: &canon_group,
                    active_title: &selected.active_title,
                    member_count: selected.detected_member_count,
                    force_refresh: false,
                };
                if let Err(err) = matcher::inspect_group_drawer_if_needed(
                    &ctx.session,
                    identity,
                    &shared.storage,
                    &inspect_req,
                ) {
                    debug!(
                        error = %err,
                        group = %canon_group,
                        "群信息抽屉辅助探测未完成，继续保留基础群信息"
                    );
                }
            }
        }
    }

    let group_id =
        shared.resolve_or_create_group_id(&selected.active_title, selected.detected_member_count);
    persist_sidebar_members_from_tree(shared, &tree, &canon_group, group_id);
    let members = shared
        .storage
        .list_group_members(&canon_group, group_id)
        .unwrap_or_default();
    let bot_login = shared.resolve_login_info();
    let enrich_ctx = EnrichContext {
        storage: &shared.storage,
        session_key: &canon_group,
        chat_kind: ChatTargetKind::Group,
        hwnd: selected.window.hwnd,
        members: &members,
        bot_login: &bot_login,
    };
    segment_enricher::enrich_parsed_messages(&enrich_ctx, &mut rich_baseline);

    let mut baseline: Vec<_> = rich_baseline.into_iter().map(|item| item.message).collect();
    let baseline_notices = parser::extract_system_notices_from_tree(
        &tree,
        selected.window.rect,
        selected.window.is_standalone_chat,
        &selected.active_title,
    );
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

    let mut new_incoming = {
        let mut tracker = shared.tracker.lock();
        tracker.seed_baseline(&baseline[..split_idx]);
        tracker.mark_session_seeded_for(ChatTargetKind::Group, &selected.active_title);
        tracker.seed_notice_baseline(&baseline_notices);
        mark_confirmed_self_replies(&tracker, &mut baseline);
        let res = if tail_incoming.is_empty() {
            Vec::new()
        } else {
            let (_seen, mut fresh) = tracker.observe_poll(tail_incoming);
            mark_confirmed_self_replies(&tracker, &mut fresh);
            fresh
        };
        let visible_uids: Vec<String> = baseline.iter().map(|m| m.message_id.clone()).collect();
        tracker.diff_visible_uids_for(ChatTargetKind::Group, &selected.active_title, &visible_uids);
        res
    };

    if !new_incoming.is_empty() {
        shared.state.lock().messages_observed += new_incoming.len() as u64;
    }

    let _ = shared.persist_messages_for_group(
        &selected.active_title,
        selected.detected_member_count,
        &mut baseline,
    );
    if !new_incoming.is_empty() {
        let _ = shared.persist_messages_for_group(
            &selected.active_title,
            selected.detected_member_count,
            &mut new_incoming,
        );
    }
    if !selected.window.is_standalone_chat {
        sync_sessions_from_tree(shared, &tree, selected.window.rect);
    }

    let active_title_clone = selected.active_title.clone();
    let raw_status = {
        let mut guard = shared.state.lock();
        guard.attached_window = Some(selected.window);
        guard.active_group = Some(selected.active_title);
        guard.active_chat_kind = ChatTargetKind::Group;
        guard.detected_member_count = selected.detected_member_count;
        guard.to_status(&shared.config)
    };
    shared.scheduler.lock().record_switch(
        now_utc_ms(),
        &active_title_clone,
        ChatTargetKind::Group,
        count_switch,
    );

    Ok((shared.enrich_status(raw_status), new_incoming))
}

/// 轻量级切换 UIA 选中项到已建立历史基线的受监控群聊（不重新播种以免吞掉后台未读）
fn switch_active_group_selection(
    ctx: &mut WorkerContext,
    shared: &Arc<SharedInner>,
    spec: &GroupTargetSpec,
) -> Result<String> {
    let qq_pids = refresh_pids_and_check_security(shared)?;
    let _fg = ForegroundRestoreGuard::capture(&qq_pids);
    let windows = ensure_candidate_windows(shared)?;
    let effective_spec = spec.member_guard_for_group(&spec.group_name);
    let mut selected = {
        let mut select_ctx = GroupSelectContext {
            session: &ctx.session,
            profile: &shared.profile,
            allow_mock: shared.allow_mock(),
            verified_cache: &mut ctx.verified_cache,
        };
        matcher::select_target_group(&mut select_ctx, &windows, &effective_spec)?
    };
    let _open_guard = selected._unocclude_guard.take();
    let _ = uia_core::refresh_window_info(&mut selected.window);
    let active_title = selected.active_title.clone();
    let _ = shared.resolve_or_create_group_id(&active_title, selected.detected_member_count);
    {
        let mut guard = shared.state.lock();
        guard.attached_window = Some(selected.window);
        guard.active_group = Some(active_title.clone());
        guard.active_chat_kind = ChatTargetKind::Group;
        guard.detected_member_count = selected.detected_member_count;
    }
    shared
        .scheduler
        .lock()
        .record_switch(now_utc_ms(), &active_title, ChatTargetKind::Group, true);
    Ok(active_title)
}

/// 确认目标群当前已附着到有效窗口句柄；未附着或切群时按 `open_group` 自动重新绑定
pub(crate) fn ensure_active_window_for_group(
    ctx: &mut WorkerContext,
    shared: &Arc<SharedInner>,
    requested_group: &str,
) -> Result<(WindowInfo, String)> {
    let req_trimmed = requested_group.trim();
    let current = {
        let guard = shared.state.lock();
        (
            guard.attached_window.clone(),
            guard.active_group.clone(),
            guard.active_chat_kind,
        )
    };

    if let (Some(mut win), Some(active), ChatTargetKind::Group) = current {
        if (req_trimmed.is_empty() || matcher::group_title_matches(&active, req_trimmed))
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

    let target_name = if req_trimmed.is_empty() {
        shared.config.target.group_name.clone()
    } else {
        req_trimmed.to_string()
    };
    let spec = shared.config.target.member_guard_for_group(&target_name);
    let _ = handle_open_group(ctx, shared, &spec)?;

    let guard = shared.state.lock();
    let win = guard.attached_window.clone().ok_or_else(|| {
        AdapterError::WindowNotFound("failed to attach target window".to_string())
    })?;
    let active = guard
        .active_group
        .clone()
        .ok_or(AdapterError::GroupNotFound {
            group: spec.group_name,
        })?;
    Ok((win, active))
}

/// 检查轮询树快照是否包含消息列表容器以及顶部标题是否与目标会话一致
pub(super) fn inspect_poll_tree_target(
    tree: &UiNodeSnapshot,
    win: &WindowInfo,
    active_target: &str,
) -> (bool, Option<String>, Option<u32>, bool) {
    let missing_container =
        !parser::has_message_list_container(tree, win.rect, win.is_standalone_chat);
    if missing_container {
        return (true, None, None, false);
    }
    let (live_hdr, live_cnt) =
        matcher::read_chat_header_from_tree(tree, win.rect, win.is_standalone_chat, None);
    let header_mismatch = live_hdr
        .as_deref()
        .map(str::trim)
        .is_some_and(|hdr| !hdr.is_empty() && !matcher::group_title_matches(hdr, active_target));
    (false, live_hdr, live_cnt, header_mismatch)
}

/// 当轮询树缺失容器或标题失配时，先把本地运行态与真实窗口同步
pub(super) fn reconcile_poll_tree_mismatch(
    shared: &SharedInner,
    win: &WindowInfo,
    tree: &UiNodeSnapshot,
    missing_container: bool,
) {
    if missing_container {
        let mut guard = shared.state.lock();
        guard.active_group = None;
        guard.detected_member_count = None;
        return;
    }
    super::window_discovery::reconcile_status_from_tree(shared, win.clone(), tree);
}

/// 在显式轮询检测到偏离时按会话类型重开一次目标聊天窗口并刷新目标状态
fn reopen_poll_target(
    ctx: &mut WorkerContext,
    shared: &Arc<SharedInner>,
    kind: ChatTargetKind,
    target: &str,
) -> Result<(WindowInfo, String)> {
    match kind {
        ChatTargetKind::Group => {
            let spec = shared.config.target.member_guard_for_group(target);
            handle_open_group(ctx, shared, &spec)?;
            ensure_active_window_for_group(ctx, shared, target)
        }
        ChatTargetKind::Private => {
            handle_open_private(ctx, shared, target)?;
            ensure_active_window_for_private(ctx, shared, target)
        }
    }
}

/// 采集目标群聊或私聊当前消息列表并执行富媒体解析、增量过滤与 SQLite 持久化
pub(crate) fn handle_poll_messages(
    ctx: &mut WorkerContext,
    shared: &Arc<SharedInner>,
    req: &PollRequest<'_>,
) -> Result<(Vec<ChatMessage>, Vec<ChatMessage>)> {
    let _pids = refresh_pids_and_check_security(shared)?;
    let req_trimmed = req.group.trim();
    let effective_kind = {
        let guard = shared.state.lock();
        if (guard.active_chat_kind == ChatTargetKind::Private
            && (req_trimmed.is_empty()
                || guard
                    .active_group
                    .as_deref()
                    .is_some_and(|active| matcher::group_title_matches(active, req_trimmed))))
            || (!req_trimmed.is_empty()
                && shared
                    .config
                    .target
                    .is_private_friend_allowed(&canonical_storage_group_name(req_trimmed)))
        {
            ChatTargetKind::Private
        } else {
            ChatTargetKind::Group
        }
    };

    let (mut win, mut active_target) = match effective_kind {
        ChatTargetKind::Private => ensure_active_window_for_private(ctx, shared, req.group)?,
        ChatTargetKind::Group => ensure_active_window_for_group(ctx, shared, req.group)?,
    };

    matcher::ensure_verified_window(&mut ctx.verified_cache, &win, shared.allow_mock())?
        .verify_fast()?;

    let (mut tree, mut _unocclude_guard) = matcher::wake_and_dump_tree_ext(
        &ctx.session,
        win.hwnd,
        INTERNAL_SCAN_DEPTH,
        req.allow_unocclude,
    )?;

    if !win.is_standalone_chat {
        sync_sessions_from_tree(shared, &tree, win.rect);
    }

    let (mut missing_container, mut live_hdr, mut live_cnt, mut header_mismatch) =
        inspect_poll_tree_target(&tree, &win, &active_target);

    if missing_container || header_mismatch {
        reconcile_poll_tree_mismatch(shared, &win, &tree, missing_container);
        if req.allow_unocclude && !req_trimmed.is_empty() {
            drop(_unocclude_guard);
            let (reopened_win, reopened_target) =
                reopen_poll_target(ctx, shared, effective_kind, req_trimmed)?;
            win = reopened_win;
            active_target = reopened_target;
            matcher::ensure_verified_window(&mut ctx.verified_cache, &win, shared.allow_mock())?
                .verify_fast()?;
            let (new_tree, new_guard) =
                matcher::wake_and_dump_tree_ext(&ctx.session, win.hwnd, INTERNAL_SCAN_DEPTH, true)?;
            tree = new_tree;
            _unocclude_guard = new_guard;
            if !win.is_standalone_chat {
                sync_sessions_from_tree(shared, &tree, win.rect);
            }
            (missing_container, live_hdr, live_cnt, header_mismatch) =
                inspect_poll_tree_target(&tree, &win, &active_target);
            if missing_container || header_mismatch {
                reconcile_poll_tree_mismatch(shared, &win, &tree, missing_container);
            }
        }
    }

    if missing_container {
        return Err(AdapterError::UiaFailure(
            "当前窗口未处于有效聊天消息面板（缺少消息列表容器）".to_string(),
        ));
    }
    if header_mismatch {
        debug!(
            expected_target = %active_target,
            live_header = ?live_hdr,
            "当前激活聊天视图已切换至其他会话，本轮跳过外部消息事件派发"
        );
        return Ok((Vec::new(), Vec::new()));
    }
    if effective_kind == ChatTargetKind::Private
        && (live_cnt.is_some() || matcher::has_group_member_sidebar(&tree))
    {
        debug!(
            expected_friend = %active_target,
            live_count = ?live_cnt,
            "私聊轮询探测到群特征，为防误判本轮跳过解析"
        );
        return Ok((Vec::new(), Vec::new()));
    }

    let detected_count = live_cnt.or_else(|| shared.state.lock().detected_member_count);
    if effective_kind == ChatTargetKind::Group {
        let mut guard = shared.config.target.member_guard_for_group(&active_target);
        if !req.allow_unocclude {
            guard.require_member_count_verified = false;
        }
        matcher::passes_member_guard(&guard, live_cnt.or(detected_count))?;
    }

    shared.register_tree_self_nicknames(&tree, win.rect);
    let mut rich_parsed = parser::extract_rich_messages_from_tree(
        &tree,
        win.rect,
        win.is_standalone_chat,
        &active_target,
    );
    strip_shed_p2_image_rects_on_burst(shared, &mut rich_parsed);

    let canon_target = canonical_storage_group_name(&active_target);
    let mut group_id_opt: Option<i64> = None;
    let members = if effective_kind == ChatTargetKind::Group {
        let gid = shared.resolve_or_create_group_id(&active_target, detected_count);
        group_id_opt = Some(gid);
        persist_sidebar_members_from_tree(shared, &tree, &canon_target, gid);
        shared
            .storage
            .list_group_members(&canon_target, gid)
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    let bot_login = shared.resolve_login_info();
    let enrich_ctx = EnrichContext {
        storage: &shared.storage,
        session_key: &canon_target,
        chat_kind: effective_kind,
        hwnd: win.hwnd,
        members: &members,
        bot_login: &bot_login,
    };
    segment_enricher::enrich_parsed_messages(&enrich_ctx, &mut rich_parsed);

    let mut parsed: Vec<_> = rich_parsed.into_iter().map(|item| item.message).collect();
    let raw_notices = parser::extract_system_notices_from_tree(
        &tree,
        win.rect,
        win.is_standalone_chat,
        &active_target,
    );
    let (rebound, mut annotated, mut new_incoming, fresh_notices, disappeared_uids) = {
        let mut tracker = shared.tracker.lock();
        let rebound = tracker.reconcile_polled_self_messages(&mut parsed);
        let (mut annotated, mut new_incoming) = tracker.observe_poll(parsed);
        tracker.mark_session_seeded_for(effective_kind, &active_target);
        mark_confirmed_self_replies(&tracker, &mut annotated);
        mark_confirmed_self_replies(&tracker, &mut new_incoming);
        let current_uids: Vec<String> = annotated.iter().map(|m| m.message_id.clone()).collect();
        let disappeared_uids =
            tracker.diff_visible_uids_for(effective_kind, &active_target, &current_uids);
        let fresh_notices = tracker.observe_notices(raw_notices);
        (
            rebound,
            annotated,
            new_incoming,
            fresh_notices,
            disappeared_uids,
        )
    };
    for (onebot_id, ui_uid, ts_hint) in rebound {
        let _ = shared
            .storage
            .reconcile_outbound_stub_uid(onebot_id, &ui_uid, ts_hint.as_deref());
    }

    emit_fresh_notices(
        shared,
        effective_kind,
        &active_target,
        group_id_opt,
        &members,
        (bot_login.user_id, fresh_notices, &disappeared_uids),
    );

    if !new_incoming.is_empty() {
        shared.state.lock().messages_observed += new_incoming.len() as u64;
    }

    match effective_kind {
        ChatTargetKind::Group => {
            let _ =
                shared.persist_messages_for_group(&active_target, detected_count, &mut annotated);
            if !new_incoming.is_empty() {
                let _ = shared.persist_messages_for_group(
                    &active_target,
                    detected_count,
                    &mut new_incoming,
                );
            }
        }
        ChatTargetKind::Private => {
            let _ = shared.persist_messages_for_private(&active_target, &mut annotated);
            if !new_incoming.is_empty() {
                let _ = shared.persist_messages_for_private(&active_target, &mut new_incoming);
            }
        }
    }

    if req.limit > 0 && annotated.len() > req.limit {
        let start = annotated.len() - req.limit;
        annotated = annotated.split_off(start);
    }

    Ok((annotated, new_incoming))
}

/// 后台事件驱动或定时轮询周期内同步会话预览，协调当前会话增量采集与多群未读/紧急 @ 抢占切换
pub(crate) fn handle_sync_and_poll(
    ctx: &mut WorkerContext,
    shared: &Arc<SharedInner>,
    poll_limit: usize,
) -> Result<(Vec<SessionSummary>, Vec<ChatMessage>)> {
    // 在 COM 工作线程执行时刻即时读取最新附着会话，避免排队期快照覆盖刚切换的私聊或群聊
    let (active_group, active_kind, detected_count) = {
        let guard = shared.state.lock();
        (
            guard.active_group.clone(),
            guard.active_chat_kind,
            guard.detected_member_count,
        )
    };
    let mut collected_sessions = Vec::new();
    let mut collected_incoming = Vec::new();

    if let Some(ref group) = active_group {
        let active_authorized = match active_kind {
            ChatTargetKind::Private => shared.config.target.is_private_friend_allowed(group),
            ChatTargetKind::Group => {
                let mut guard_relaxed = shared.config.target.member_guard_for_group(group);
                guard_relaxed.require_member_count_verified = false;
                let eff_count = detected_count.or_else(|| matcher::parse_member_count(group));
                shared.config.target.is_monitored_group(group)
                    && matcher::passes_member_guard(&guard_relaxed, eff_count).is_ok()
            }
        };
        if active_authorized {
            let poll_req = PollRequest {
                group: group.as_str(),
                limit: poll_limit,
                allow_unocclude: false,
            };
            let (_all, new_incoming) = handle_poll_messages(ctx, shared, &poll_req)?;
            collected_incoming.extend(new_incoming);
        }
    } else {
        let pids = refresh_pids_and_check_security(shared)?;
        if pids.is_empty() && !shared.allow_mock() {
            return Ok((Vec::new(), Vec::new()));
        }
        let Some(target) = discover_windows(shared, &pids)
            .into_iter()
            .find(|win| !win.is_standalone_chat && !win.is_iconic && !is_narrow_login_window(win))
        else {
            return Ok((Vec::new(), Vec::new()));
        };
        let Ok(identity) =
            matcher::ensure_verified_window(&mut ctx.verified_cache, &target, shared.allow_mock())
        else {
            return Ok((Vec::new(), Vec::new()));
        };
        if identity.verify_fast().is_err() {
            return Ok((Vec::new(), Vec::new()));
        }
        let Ok((nodes, _guard)) =
            matcher::wake_and_collect_ext(&ctx.session, target.hwnd, INTERNAL_SCAN_DEPTH, false)
        else {
            return Ok((Vec::new(), Vec::new()));
        };
        if window_looks_like_login_prompt(&target, &nodes) {
            return Ok((Vec::new(), Vec::new()));
        }
        if !matcher::is_search_mode_active(&nodes, target.rect) {
            let extracted = matcher::extract_sessions(&nodes, target.rect);
            let _ = shared.upsert_session_summaries(&extracted);
            collected_sessions = extracted;
        }
    }

    let (active_after, kind_after) = {
        let guard = shared.state.lock();
        (guard.active_group.clone(), guard.active_chat_kind)
    };
    let next_target = shared.scheduler.lock().next_switch_target(
        now_utc_ms(),
        active_after.as_deref(),
        kind_after,
        &shared.config.target,
    );
    if let Some(next) = next_target {
        let switched_incoming = execute_harvester_switch(ctx, shared, &next, poll_limit);
        collected_incoming.extend(switched_incoming);
        return Ok((collected_sessions, collected_incoming));
    }

    if active_group.is_none() && !collected_sessions.is_empty() {
        for session in &collected_sessions {
            let unread = session.unread_count.unwrap_or(0) as usize;
            if session.member_count_hint.is_some() || unread == 0 {
                continue;
            }
            let canon = canonical_storage_group_name(&session.title);
            if !shared.config.target.is_private_friend_allowed(&canon) {
                continue;
            }
            if let Ok((_status, fresh)) = open_private_with_unread(ctx, shared, &canon, unread) {
                collected_incoming.extend(fresh);
                break;
            }
        }
    }

    Ok((collected_sessions, collected_incoming))
}

/// 将指定群聊或私聊会话在 UIA 中安全选中以清除未读红点，并同步清空调度器未读记录
pub(crate) fn handle_mark_session_read(
    ctx: &mut WorkerContext,
    shared: &Arc<SharedInner>,
    target: &str,
    target_kind: ChatTargetKind,
) -> Result<()> {
    let trimmed = target.trim();
    if trimmed.is_empty() {
        return Err(AdapterError::InputRejected(
            "mark_session_read target cannot be empty".to_string(),
        ));
    }
    if target_kind == ChatTargetKind::Private {
        verify_private_whitelist(shared, trimmed)?;
    }

    let already_active = {
        let guard = shared.state.lock();
        guard.active_chat_kind == target_kind
            && guard
                .active_group
                .as_deref()
                .is_some_and(|act| matcher::group_title_matches(act, trimmed))
    };

    if !already_active {
        let is_seeded = shared.tracker.lock().is_seeded_for(target_kind, trimmed);
        match target_kind {
            ChatTargetKind::Group => {
                let spec = shared.config.target.member_guard_for_group(trimmed);
                if is_seeded {
                    switch_active_group_selection(ctx, shared, &spec)?;
                } else {
                    let _ = open_group_with_unread(ctx, shared, &spec, 0, true)?;
                }
            }
            ChatTargetKind::Private => {
                if is_seeded {
                    switch_active_private_selection(ctx, shared, trimmed)?;
                } else {
                    let _ = handle_open_private(ctx, shared, trimmed)?;
                }
            }
        }
    } else if target_kind == ChatTargetKind::Group {
        let guard = shared.config.target.member_guard_for_group(trimmed);
        let detected = shared.state.lock().detected_member_count;
        matcher::passes_member_guard(&guard, detected)?;
    }

    shared.scheduler.lock().mark_session_read(trimmed);
    Ok(())
}

/// 按多群采集器选择的下一跳执行会话切换，对已播种会话走增量轮询以免基线重播吞掉未读消息
fn execute_harvester_switch(
    ctx: &mut WorkerContext,
    shared: &Arc<SharedInner>,
    next: &crate::scheduler::session_harvester::HarvestSwitchTarget,
    poll_limit: usize,
) -> Vec<ChatMessage> {
    match next.target_kind {
        ChatTargetKind::Group => {
            let guard_spec = shared.config.target.member_guard_for_group(&next.target);
            let is_seeded = shared
                .tracker
                .lock()
                .is_seeded_for(ChatTargetKind::Group, &next.target);
            if is_seeded {
                if let Ok(selected_title) = switch_active_group_selection(ctx, shared, &guard_spec)
                {
                    let req = PollRequest {
                        group: selected_title.as_str(),
                        limit: poll_limit,
                        allow_unocclude: false,
                    };
                    if let Ok((_all, fresh)) = handle_poll_messages(ctx, shared, &req) {
                        return fresh;
                    }
                }
                Vec::new()
            } else if next.unread_delta > 0 {
                match open_group_with_unread(
                    ctx,
                    shared,
                    &guard_spec,
                    next.unread_delta as usize,
                    true,
                ) {
                    Ok((_status, fresh)) => fresh,
                    Err(err) => {
                        debug!(error = %err, target = %next.target, "切换打开未读监控群失败");
                        Vec::new()
                    }
                }
            } else {
                let _ = open_group_with_unread(ctx, shared, &guard_spec, 0, true);
                Vec::new()
            }
        }
        ChatTargetKind::Private => {
            if verify_private_whitelist(shared, &next.target).is_err() {
                return Vec::new();
            }
            let is_seeded = shared
                .tracker
                .lock()
                .is_seeded_for(ChatTargetKind::Private, &next.target);
            if is_seeded {
                if let Ok(selected_title) =
                    switch_active_private_selection(ctx, shared, &next.target)
                {
                    let req = PollRequest {
                        group: selected_title.as_str(),
                        limit: poll_limit,
                        allow_unocclude: false,
                    };
                    if let Ok((_all, fresh)) = handle_poll_messages(ctx, shared, &req) {
                        return fresh;
                    }
                }
                Vec::new()
            } else {
                open_private_with_unread(ctx, shared, &next.target, next.unread_delta as usize)
                    .map(|(_, fresh)| fresh)
                    .unwrap_or_default()
            }
        }
    }
}

/// 从已采集的完整 UI 节点树提取右侧群成员栏并写入 SQLite 成员缓存表（突发期自动按 10 秒节流）
pub(crate) fn persist_sidebar_members_from_tree(
    shared: &Arc<SharedInner>,
    tree: &UiNodeSnapshot,
    canon_group: &str,
    group_id: i64,
) {
    let within_throttle = {
        let guard = shared.state.lock();
        guard
            .last_member_sync_at
            .get(canon_group)
            .is_some_and(|at| {
                at.elapsed().as_millis() < u128::from(SIDEBAR_MEMBER_REFRESH_INTERVAL_MS)
            })
    };
    if within_throttle
        && !shared
            .storage
            .list_group_members(canon_group, group_id)
            .unwrap_or_default()
            .is_empty()
    {
        return;
    }

    let sidebar_members = matcher::extract_members_from_tree(tree);
    if sidebar_members.is_empty() {
        return;
    }
    if let Err(err) = shared
        .storage
        .upsert_group_members(canon_group, group_id, &sidebar_members)
    {
        warn!(
            error = %err,
            group = %canon_group,
            count = sidebar_members.len(),
            "写入右侧群成员栏提取结果失败"
        );
    } else {
        shared
            .state
            .lock()
            .last_member_sync_at
            .insert(canon_group.to_string(), Instant::now());
        let login = shared.resolve_login_info();
        let _ = shared
            .storage
            .reconcile_member_mapped_ids(Some(&login.nickname), Some(login.user_id));
    }
}

/// 当单次轮询达到突发阈值时，对即将被选择性丢弃的 P2 普通消息提前清空图片裁切矩形以跳过耗时截图
fn strip_shed_p2_image_rects_on_burst(
    shared: &SharedInner,
    rich_items: &mut [parser::ParsedTreeMessage],
) {
    if rich_items.is_empty() {
        return;
    }
    let msg_refs: Vec<&ChatMessage> = rich_items.iter().map(|item| &item.message).collect();
    let (self_id, nicks) = shared.scheduler_self_identity();
    let keep_mask = shared.scheduler.lock().pre_classify_keep_image_mask(
        now_utc_ms(),
        &msg_refs,
        self_id,
        &nicks,
    );
    for (item, keep) in rich_items.iter_mut().zip(keep_mask) {
        if !keep && !item.image_rects.is_empty() {
            item.image_rects.clear();
        }
    }
}

/// 结合消息追踪器中确认的自发 OneBot ID 补充回填 `reply_targets_self` 标记
fn mark_confirmed_self_replies(tracker: &parser::MessageTracker, messages: &mut [ChatMessage]) {
    for msg in messages {
        if !msg.reply_targets_self {
            if let Some(reply_id) = msg.reply_to_message_id {
                if tracker.is_confirmed_self_onebot_id(reply_id) {
                    msg.reply_targets_self = true;
                }
            }
        }
    }
}

/// 从已采集的 UI 节点树被动提取左侧会话列表并更新 SQLite（避免额外 UIA 遍历）
pub(crate) fn sync_sessions_from_tree(
    shared: &Arc<SharedInner>,
    tree: &UiNodeSnapshot,
    _win_rect: [i32; 4],
) {
    if matcher::is_search_mode_active_in_tree(tree) {
        return;
    }
    let sessions = matcher::extract_sessions_from_tree(tree);
    if !sessions.is_empty() {
        let _ = shared.upsert_session_summaries(&sessions);
    }
}

/// 将本轮首次捕捉到的 QQNT 系统提示横幅转换为 OneBot notice 事件并广播（包含撤回真 ID 关联）
fn emit_fresh_notices(
    shared: &Arc<SharedInner>,
    chat_kind: ChatTargetKind,
    session_name: &str,
    group_id: Option<i64>,
    members: &[GroupMemberInfo],
    bot_and_notices: (i64, Vec<parser::ParsedSystemNotice>, &[String]),
) {
    let (self_id, fresh_notices, disappeared_uids) = bot_and_notices;
    if fresh_notices.is_empty() {
        return;
    }
    let peer_user_id = if chat_kind == ChatTargetKind::Private {
        Some(shared.resolve_or_create_friend_id(session_name))
    } else {
        None
    };
    let time_secs = (now_utc_ms() / 1000) as i64;
    let notice_ctx = parser::NoticeContext {
        self_id,
        chat_kind,
        session_name,
        group_id,
        peer_user_id,
        members,
    };
    let events = parser::build_and_enrich_notice_events(
        &shared.storage,
        &notice_ctx,
        &fresh_notices,
        disappeared_uids,
        time_secs,
    );
    for event_json in events {
        shared.emit_event(AdapterEvent::Notice(event_json));
    }
}
