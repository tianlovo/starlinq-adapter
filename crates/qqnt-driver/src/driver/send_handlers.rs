//! 消息发送处理器：集成 Win32 O(1) 目标窗体验证、零额外扫描发送前守卫与双重回显确认

use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use common_types::constants::IMAGE_PLACEHOLDER_TOKEN;
use common_types::{
    AdapterError, ChatMessage, ChatTargetKind, GroupTargetSpec, InputMode, Result, SendOutcome,
    SendRequest, UiNodeSnapshot, WindowInfo,
};
use uia_core::{
    clear_editor_focus_restore_verified, clear_element_text, paste_clipboard_verified,
    read_element_value_only, send_submit_background_for_editor_verified,
    submit_only_focus_restore_verified, type_and_submit_focus_restore_verified,
    type_text_background_verified, type_text_focus_restore_verified, write_bgra_to_clipboard_dib,
    ClipboardPasteSpec, ClipboardRestoreGuard, ComposerSubmitTarget, TypingDelayRange,
    UiNodeHandle, UiaSession, VerifiedWindowIdentity,
};

use super::group_handlers::{ensure_active_window_for_group, handle_open_group};
use super::outbound_media::{self, OutboundSendPlan};
use super::private_handlers::{
    ensure_active_window_for_private, handle_open_private, verify_pre_send_private_guard_snapshots,
    verify_private_whitelist,
};
use super::runtime_state::{canonical_storage_group_name, SharedInner};
use super::window_discovery::refresh_pids_and_check_security;
use super::worker_loop::WorkerContext;
use crate::composer::{self, ComposerExecutionPlan, NativeSendContext};
use crate::constants::{INTERNAL_SCAN_DEPTH, SEND_VERIFY_RETRY_DELAYS_MS};
use crate::matcher;
use crate::parser::{self, OutboundStubBinding};
use crate::profile;

/// 发送前基于已采集 UI 节点执行的群标题与人数护栏校验证据（Parameter Object）
pub(crate) struct PreSendGuardCheck<'a> {
    pub(crate) expected_group: &'a str,
    pub(crate) win_rect: [i32; 4],
    pub(crate) is_standalone: bool,
    pub(crate) target_spec: &'a GroupTargetSpec,
}

/// 后台输入失败或未验证后回退到前台焦点恢复策略时使用的参数对象（Parameter Object）
pub(crate) struct FallbackSubmitParams<'a> {
    pub(crate) target: &'a ComposerSubmitTarget<'a>,
    pub(crate) raw_text: &'a str,
    pub(crate) norm_text: &'a str,
    pub(crate) delay_range: TypingDelayRange,
    pub(crate) retype_reason: &'a str,
}

/// 构造前台焦点恢复模式发送结果载荷时使用的参数对象（Parameter Object）
pub(crate) struct FocusRestoreOutcomeParams<'a> {
    pub(crate) group: &'a str,
    pub(crate) foreground_touched: bool,
    pub(crate) mouse_touched: bool,
    pub(crate) verified: bool,
    pub(crate) elapsed_ms: u64,
    pub(crate) verified_detail: &'a str,
    pub(crate) unverified_detail: &'a str,
}

/// 在不发起任何额外 UIA 树扫描的前提下，校验已采集节点中的当前聊天标题与群人数守卫是否仍然成立
pub(crate) fn verify_pre_send_chat_guard_snapshots(
    storage: &storage::Storage,
    snapshots: &[&UiNodeSnapshot],
    check: &PreSendGuardCheck<'_>,
) -> Result<()> {
    let (live_hdr, live_cnt) = matcher::read_chat_header_from_snapshots(
        snapshots,
        check.win_rect,
        check.is_standalone,
        None,
    );

    let header_mismatch = live_hdr
        .as_deref()
        .map(|hdr| {
            !hdr.trim().is_empty() && !matcher::group_title_matches(hdr, check.expected_group)
        })
        .unwrap_or(false);
    if header_mismatch {
        let audit_detail = format!(
            "{{\"expected_group\":{:?},\"live_header\":{:?},\"live_count\":{:?}}}",
            check.expected_group, live_hdr, live_cnt
        );
        let _ = storage.append_audit("pre_send_guard_abort", &audit_detail);
        return Err(AdapterError::InputRejected(format!(
            "pre-send guard aborted: active chat header {:?} does not match expected group '{}'",
            live_hdr, check.expected_group
        )));
    }

    if let Err(err) = matcher::passes_member_guard(check.target_spec, live_cnt) {
        let audit_detail = format!(
            "{{\"expected_group\":{:?},\"live_header\":{:?},\"live_count\":{:?},\"limit\":{:?}}}",
            check.expected_group, live_hdr, live_cnt, check.target_spec.strict_below_members
        );
        let _ = storage.append_audit("pre_send_guard_abort", &audit_detail);
        return Err(err);
    }

    Ok(())
}

pub(crate) fn record_plan_outbound(
    shared: &SharedInner,
    target_kind: ChatTargetKind,
    target_name: &str,
    plan: &OutboundSendPlan,
) {
    let mut tracker = shared.tracker.lock();
    tracker.record_outbound_for_session(target_kind, target_name, &plan.verify_norm);
    // 带图混排消息在部分客户端控件中会在图文间插入空格，同时登记空格版本防回显重放
    if !plan.images.is_empty() && !plan.combined_text.trim().is_empty() {
        tracker.record_outbound_for_session(
            target_kind,
            target_name,
            &format!("{} {IMAGE_PLACEHOLDER_TOKEN}", plan.combined_text.trim()),
        );
    }
}

pub(crate) fn rollback_plan_outbound(
    shared: &SharedInner,
    target_kind: ChatTargetKind,
    target_name: &str,
    plan: &OutboundSendPlan,
) {
    let mut tracker = shared.tracker.lock();
    tracker.rollback_outbound_for_session(target_kind, target_name, &plan.verify_norm);
    if !plan.images.is_empty() && !plan.combined_text.trim().is_empty() {
        tracker.rollback_outbound_for_session(
            target_kind,
            target_name,
            &format!("{} {IMAGE_PLACEHOLDER_TOKEN}", plan.combined_text.trim()),
        );
    }
}

fn message_matches_plan(msg: &ChatMessage, plan: &OutboundSendPlan) -> bool {
    let actual_norm = parser::normalize_message_text(&msg.content);
    if plan.images.is_empty() {
        return actual_norm == plan.verify_norm;
    }
    if actual_norm == plan.verify_norm {
        return true;
    }
    if !msg.content.contains(IMAGE_PLACEHOLDER_TOKEN) {
        return false;
    }
    let text_norm = parser::normalize_message_text(&plan.combined_text);
    text_norm.is_empty() || actual_norm.contains(&text_norm)
}

/// 采集当前聊天历史区与待发送执行计划匹配的既有消息切片，用作发送后回显确认基线
fn collect_matching_messages(
    session: &UiaSession,
    shared: &Arc<SharedInner>,
    win: &WindowInfo,
    target: (ChatTargetKind, &str),
    plan: &OutboundSendPlan,
) -> Result<Vec<ChatMessage>> {
    let (tree, _) = matcher::wake_and_dump_tree_ext(session, win.hwnd, INTERNAL_SCAN_DEPTH, false)?;
    shared.register_tree_self_nicknames(&tree, win.rect);
    let mut parsed =
        parser::extract_messages_from_tree(&tree, win.rect, win.is_standalone_chat, target.1);
    for msg in &mut parsed {
        msg.target_kind = target.0;
    }
    let annotated = {
        let mut tracker = shared.tracker.lock();
        tracker.annotate_only(parsed)
    };
    let matching = annotated
        .into_iter()
        .filter(|msg| message_matches_plan(msg, plan))
        .collect();
    Ok(matching)
}

/// 按渐进重试间隔多次检查聊天历史区是否新增一条匹配内容，并返回最新一条命中消息
fn verify_sent_appeared(
    ctx: &WorkerContext,
    shared: &Arc<SharedInner>,
    win: &WindowInfo,
    target: (ChatTargetKind, &str),
    plan: &OutboundSendPlan,
    baseline_count: usize,
) -> Option<ChatMessage> {
    for wait_ms in SEND_VERIFY_RETRY_DELAYS_MS {
        thread::sleep(Duration::from_millis(wait_ms));
        if let Ok(matching) = collect_matching_messages(&ctx.session, shared, win, target, plan) {
            if matching.len() > baseline_count {
                return matching.into_iter().last();
            }
        }
    }
    None
}

/// 判断输入框当前内容是否可视为空白或仅包含系统占位提示
pub(crate) fn is_editor_empty(elem: &UiNodeHandle) -> bool {
    read_element_value_only(elem)
        .map(|text| profile::is_empty_composer_text(&text))
        .unwrap_or(false)
}

/// 判断输入框当前是否存在非空白的草稿残留文本
fn has_nonempty_editor_value(elem: &UiNodeHandle) -> bool {
    read_element_value_only(elem)
        .map(|text| !profile::is_empty_composer_text(&text))
        .unwrap_or(false)
}

/// 判断读取到的输入框文本是否在过滤占位符后与目标规范文本完全一致
pub(crate) fn editor_value_matches_norm(raw: &str, norm_text: &str) -> bool {
    !norm_text.is_empty()
        && !profile::is_empty_composer_text(raw)
        && parser::normalize_message_text(raw) == norm_text
}

/// 读取输入框实时值并校验其是否已准确填入待发送内容
fn editor_contains_target_text(elem: &UiNodeHandle, norm_text: &str) -> bool {
    if norm_text.is_empty() {
        return false;
    }
    read_element_value_only(elem)
        .map(|text| editor_value_matches_norm(&text, norm_text))
        .unwrap_or(false)
}

/// 在执行新一轮文本键入前，清理输入框中残余的未发送草稿（优先无焦点清理）
pub(crate) fn clear_residual_editor_if_needed(
    identity: &VerifiedWindowIdentity,
    editor: &UiNodeHandle,
    allow_focus_fallback: bool,
) -> Result<()> {
    if !has_nonempty_editor_value(editor) {
        return Ok(());
    }
    identity.verify_fast()?;
    if clear_element_text(editor).is_ok() && !has_nonempty_editor_value(editor) {
        return Ok(());
    }
    if allow_focus_fallback {
        clear_editor_focus_restore_verified(identity, editor)?;
    }
    Ok(())
}

/// 当后台 UIA 提交未确认落地时，决定是仅补发提交热键/点击，还是清空后重新焦点键入
fn fallback_submit_or_retype(
    identity: &VerifiedWindowIdentity,
    params: &FallbackSubmitParams<'_>,
) -> Result<(bool, bool, String)> {
    identity.verify_fast()?;
    if editor_contains_target_text(params.target.editor, params.norm_text) {
        let (fg_touched, mouse_touched) =
            submit_only_focus_restore_verified(identity, params.target)?;
        return Ok((
            fg_touched,
            mouse_touched,
            "background UIA typed text; fell back to focus-restore submit".to_string(),
        ));
    }

    clear_residual_editor_if_needed(identity, params.target.editor, true)?;
    let (fg_touched, mouse_touched) = type_and_submit_focus_restore_verified(
        identity,
        params.target,
        params.raw_text,
        &params.delay_range,
    )?;
    Ok((fg_touched, mouse_touched, params.retype_reason.to_string()))
}

/// 执行群聊消息发送，涵盖群标题及小群人数双阶段安全守卫
pub(crate) fn handle_send_group_message(
    ctx: &mut WorkerContext,
    shared: &Arc<SharedInner>,
    req: &SendRequest,
) -> Result<SendOutcome> {
    handle_send_message_impl(ctx, shared, req, ChatTargetKind::Group)
}

/// 执行私聊消息发送，首要核验白名单并确认不存在群成员栏与群人数后缀
pub(crate) fn handle_send_private_message(
    ctx: &mut WorkerContext,
    shared: &Arc<SharedInner>,
    req: &SendRequest,
) -> Result<SendOutcome> {
    verify_private_whitelist(shared, &req.group)?;
    handle_send_message_impl(ctx, shared, req, ChatTargetKind::Private)
}

/// 在已确认目标窗口并清框后执行纯文本或富媒体图文粘贴发送动作
fn execute_image_paste_submit(
    identity: &VerifiedWindowIdentity,
    submit_target: &ComposerSubmitTarget<'_>,
    requested_mode: InputMode,
    allow_focus_fallback: bool,
    plan: &OutboundSendPlan,
    delay_range: TypingDelayRange,
) -> Result<(InputMode, bool, String)> {
    let mut used_mode = InputMode::BackgroundUia;
    let mut fg_touched = false;

    if !plan.combined_text.trim().is_empty() {
        identity.verify_fast()?;
        let has_newline = plan.combined_text.contains('\n');
        match requested_mode {
            InputMode::BackgroundUia => {
                let typed = type_text_background_verified(
                    identity,
                    submit_target.editor,
                    &plan.combined_text,
                    &delay_range,
                )?;
                if !typed {
                    if has_newline {
                        return Err(AdapterError::InputRejected(
                            "multi-line text on rich-text composer requires InputMode::Auto or FocusRestoreUnicode"
                                .to_string(),
                        ));
                    }
                    return Err(AdapterError::InputRejected(
                        "background UIA typing did not populate the prefix text before image paste"
                            .to_string(),
                    ));
                }
            }
            InputMode::FocusRestoreUnicode => {
                type_text_focus_restore_verified(
                    identity,
                    submit_target.editor,
                    &plan.combined_text,
                    &delay_range,
                )?;
                used_mode = InputMode::FocusRestoreUnicode;
                fg_touched = true;
            }
            InputMode::Auto => {
                if has_newline && allow_focus_fallback {
                    clear_residual_editor_if_needed(identity, submit_target.editor, true)?;
                    type_text_focus_restore_verified(
                        identity,
                        submit_target.editor,
                        &plan.combined_text,
                        &delay_range,
                    )?;
                    used_mode = InputMode::FocusRestoreUnicode;
                    fg_touched = true;
                } else {
                    let typed = type_text_background_verified(
                        identity,
                        submit_target.editor,
                        &plan.combined_text,
                        &delay_range,
                    )?;
                    if !typed {
                        if !allow_focus_fallback {
                            return Err(AdapterError::InputRejected(
                                "background UIA typing did not populate the prefix text before image paste"
                                    .to_string(),
                            ));
                        }
                        clear_residual_editor_if_needed(identity, submit_target.editor, true)?;
                        type_text_focus_restore_verified(
                            identity,
                            submit_target.editor,
                            &plan.combined_text,
                            &delay_range,
                        )?;
                        used_mode = InputMode::FocusRestoreUnicode;
                        fg_touched = true;
                    }
                }
            }
        }
    }

    // 在最小临界区内捕获剪贴板快照 -> 顺序写入 CF_DIB 与粘贴 -> Drop 时自动恢复宿主机原有剪贴板
    {
        let _clip_guard = ClipboardRestoreGuard::capture()?;
        for (idx, img) in plan.images.iter().enumerate() {
            identity.verify_fast()?;
            write_bgra_to_clipboard_dib(&img.bitmap)?;
            let is_last = idx + 1 == plan.images.len();
            let (paste_mode, paste_fg) = paste_clipboard_verified(
                identity,
                &ClipboardPasteSpec {
                    editor: submit_target.editor,
                    send_button: submit_target.send_button,
                    mode: requested_mode,
                    shortcut: submit_target.shortcut,
                    press_enter: is_last,
                    settle_delay_ms: 150,
                },
            )?;
            if used_mode != InputMode::FocusRestoreUnicode {
                used_mode = paste_mode;
            }
            fg_touched |= paste_fg;
        }
    }

    Ok((
        used_mode,
        fg_touched,
        format!(
            "pasted {} image(s) via clipboard ({used_mode:?})",
            plan.images.len()
        ),
    ))
}

fn handle_send_message_impl(
    ctx: &mut WorkerContext,
    shared: &Arc<SharedInner>,
    req: &SendRequest,
    target_kind: ChatTargetKind,
) -> Result<SendOutcome> {
    shared.check_outbound_deadline(&req.group, req.deadline_unix_ms)?;
    let started = Instant::now();
    let pids = refresh_pids_and_check_security(shared)?;
    let _outer_fg_guard = uia_core::ForegroundRestoreGuard::capture(&pids);
    let (mut win, mut active_target) = match target_kind {
        ChatTargetKind::Group => ensure_active_window_for_group(ctx, shared, &req.group)?,
        ChatTargetKind::Private => ensure_active_window_for_private(ctx, shared, &req.group)?,
    };

    let mut nodes_and_guard =
        matcher::wake_and_collect_ext(&ctx.session, win.hwnd, INTERNAL_SCAN_DEPTH, true)?;
    let target_name = if req.group.trim().is_empty() && target_kind == ChatTargetKind::Group {
        shared.config.target.group_name.clone()
    } else {
        req.group.trim().to_string()
    };
    let mut live_guard_spec = shared.config.target.member_guard_for_group(&target_name);
    live_guard_spec.require_member_count_verified = false;

    let (live_hdr, live_cnt) =
        matcher::read_chat_header(&nodes_and_guard.0, win.rect, win.is_standalone_chat, None);
    let initial_mismatch = live_hdr
        .as_deref()
        .map(|hdr| !hdr.trim().is_empty() && !matcher::group_title_matches(hdr, &active_target))
        .unwrap_or(false);
    let initial_guard_bad = match target_kind {
        ChatTargetKind::Group => matcher::passes_member_guard(&live_guard_spec, live_cnt).is_err(),
        ChatTargetKind::Private => {
            let snap_refs: Vec<&UiNodeSnapshot> = nodes_and_guard
                .0
                .iter()
                .map(|node| &node.snapshot)
                .collect();
            live_cnt.is_some() || matcher::has_group_member_sidebar_in_snapshots(&snap_refs)
        }
    };

    if (initial_mismatch || initial_guard_bad) && !req.group.trim().is_empty() {
        tracing::info!(
            active_target = %active_target,
            target_kind = ?target_kind,
            live_hdr = ?live_hdr,
            live_cnt = ?live_cnt,
            "发送前初次探测到聊天窗口标题或类型特征变化，尝试重新定位目标会话"
        );
        let reopened = match target_kind {
            ChatTargetKind::Group => {
                let reopen_spec = shared.config.target.member_guard_for_group(&target_name);
                handle_open_group(ctx, shared, &reopen_spec).and_then(|_| {
                    ensure_active_window_for_group(ctx, shared, &reopen_spec.group_name)
                })
            }
            ChatTargetKind::Private => handle_open_private(ctx, shared, &target_name)
                .and_then(|_| ensure_active_window_for_private(ctx, shared, &target_name)),
        };
        if let Ok((reopened_win, reopened_target)) = reopened {
            win = reopened_win;
            active_target = reopened_target;
            let _ = uia_core::refresh_window_info(&mut win);
            nodes_and_guard =
                matcher::wake_and_collect_ext(&ctx.session, win.hwnd, INTERNAL_SCAN_DEPTH, true)?;
        }
    }

    let (nodes, _unocclude_guard) = nodes_and_guard;
    let snapshots: Vec<&UiNodeSnapshot> = nodes.iter().map(|node| &node.snapshot).collect();

    // 发送前零额外树扫描复核：结合 VerifiedWindowIdentity 与当前节点标题/群或私聊特征完成最终放行判定
    let identity_ref =
        matcher::ensure_verified_window(&mut ctx.verified_cache, &win, shared.allow_mock())?;
    identity_ref.verify_fast()?;
    let identity =
        VerifiedWindowIdentity::verify_and_bind(identity_ref.hwnd(), shared.allow_mock())?;
    match target_kind {
        ChatTargetKind::Group => {
            let pre_send_check = PreSendGuardCheck {
                expected_group: &active_target,
                win_rect: win.rect,
                is_standalone: win.is_standalone_chat,
                target_spec: &live_guard_spec,
            };
            verify_pre_send_chat_guard_snapshots(&shared.storage, &snapshots, &pre_send_check)?;
        }
        ChatTargetKind::Private => {
            verify_pre_send_private_guard_snapshots(
                &shared.storage,
                &snapshots,
                &active_target,
                win.rect,
                win.is_standalone_chat,
            )?;
        }
    }

    let (editor_idx, send_idx) =
        matcher::locate_composer_and_send_button(&nodes, win.rect, win.is_standalone_chat)?;
    let editor = &nodes[editor_idx].element;
    let send_btn = send_idx.map(|idx| &nodes[idx].element);
    let editor_rect = nodes[editor_idx].snapshot.rect;
    let send_rect = send_idx.map(|idx| nodes[idx].snapshot.rect);

    let raw_segments = outbound_media::extract_effective_segments_checked(
        req,
        shared.config.onebot.allow_degraded_media_summary,
    )?;
    let detected_count = match target_kind {
        ChatTargetKind::Group => live_cnt.or_else(|| shared.state.lock().detected_member_count),
        ChatTargetKind::Private => None,
    };
    let members = if target_kind == ChatTargetKind::Group {
        let canon = canonical_storage_group_name(&active_target);
        let gid = shared.resolve_or_create_group_id(&active_target, detected_count);
        shared
            .storage
            .list_group_members(&canon, gid)
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let delay_range = {
        let humanizer = shared.humanizer.lock();
        humanizer.typing_delay_range()
    };
    let shortcut = shared.config.interaction.send_shortcut;

    let exec_plan = composer::build_composer_plan(
        &shared.storage,
        &members,
        target_kind,
        (&active_target, target_kind),
        &raw_segments,
        shared.config.onebot.allow_degraded_media_summary,
    )?;
    if matches!(
        exec_plan,
        ComposerExecutionPlan::RichInline { .. } | ComposerExecutionPlan::Standalone { .. }
    ) {
        let mut native_ctx = NativeSendContext {
            ctx,
            shared,
            identity: &identity,
            win: &win,
            active_target: &active_target,
            target_kind,
            detected_member_count: detected_count,
            pids: &pids,
            editor,
            send_button: send_btn,
            editor_rect,
            send_rect,
            shortcut,
            delay_range,
            started,
        };
        return composer::execute_native_rich_send(&mut native_ctx, exec_plan);
    }

    let plan = outbound_media::build_outbound_send_plan(
        &shared.storage,
        &members,
        (&active_target, target_kind),
        &raw_segments,
    )?;

    let baseline_count = collect_matching_messages(
        &ctx.session,
        shared,
        &win,
        (target_kind, &active_target),
        &plan,
    )?
    .len();
    record_plan_outbound(shared, target_kind, &active_target, &plan);

    let requested_mode = req.mode.unwrap_or(shared.config.interaction.default_mode);
    let allow_focus_for_clear = requested_mode == InputMode::FocusRestoreUnicode
        || (requested_mode == InputMode::Auto && shared.config.interaction.allow_focus_fallback);

    identity.verify_fast()?;
    clear_residual_editor_if_needed(&identity, editor, allow_focus_for_clear)?;

    let submit_target = ComposerSubmitTarget {
        window_hwnd: win.hwnd,
        editor,
        send_button: send_btn,
        shortcut,
    };

    let send_result: Result<(SendOutcome, Option<ChatMessage>)> = if !plan.images.is_empty() {
        let (used_mode, fg_touched, detail) = execute_image_paste_submit(
            &identity,
            &submit_target,
            requested_mode,
            shared.config.interaction.allow_focus_fallback,
            &plan,
            delay_range,
        )?;
        let verified_msg = verify_sent_appeared(
            ctx,
            shared,
            &win,
            (target_kind, &active_target),
            &plan,
            baseline_count,
        );
        let verified = verified_msg.is_some();
        Ok((
            SendOutcome {
                ok: verified,
                group: active_target.clone(),
                used_mode,
                foreground_touched: fg_touched,
                mouse_touched: false,
                verified_in_chat: verified,
                elapsed_ms: started.elapsed().as_millis() as u64,
                detail: if verified {
                    format!("{detail}; verified in chat history")
                } else {
                    format!("{detail}; could not be verified in chat history")
                },
                target_kind,
                degraded_segments: plan.degraded_segments.clone(),
                ..Default::default()
            },
            verified_msg,
        ))
    } else {
        match requested_mode {
            InputMode::BackgroundUia => {
                identity.verify_fast()?;
                let typed = type_text_background_verified(
                    &identity,
                    editor,
                    &plan.combined_text,
                    &delay_range,
                )?;
                if !typed {
                    if plan.combined_text.contains('\n') {
                        Err(AdapterError::InputRejected(
                            "multi-line text on rich-text composer requires InputMode::Auto or FocusRestoreUnicode"
                                .to_string(),
                        ))
                    } else {
                        Err(AdapterError::InputRejected(
                            "background UIA typing did not populate the composer editor"
                                .to_string(),
                        ))
                    }
                } else {
                    identity.verify_fast()?;
                    send_submit_background_for_editor_verified(
                        &identity,
                        Some(editor),
                        send_btn,
                        shortcut,
                    )?;
                    let verified_msg = verify_sent_appeared(
                        ctx,
                        shared,
                        &win,
                        (target_kind, &active_target),
                        &plan,
                        baseline_count,
                    );
                    let verified = verified_msg.is_some();
                    Ok((
                        SendOutcome {
                            ok: verified,
                            group: active_target.clone(),
                            used_mode: InputMode::BackgroundUia,
                            foreground_touched: false,
                            mouse_touched: false,
                            verified_in_chat: verified,
                            elapsed_ms: started.elapsed().as_millis() as u64,
                            detail: if verified {
                                "sent via background UIA and verified in chat history".to_string()
                            } else {
                                "submitted via background UIA without history confirmation"
                                    .to_string()
                            },
                            target_kind,
                            degraded_segments: plan.degraded_segments.clone(),
                            ..Default::default()
                        },
                        verified_msg,
                    ))
                }
            }
            InputMode::FocusRestoreUnicode => {
                identity.verify_fast()?;
                let (fg_touched, mouse_touched) = type_and_submit_focus_restore_verified(
                    &identity,
                    &submit_target,
                    &plan.combined_text,
                    &delay_range,
                )?;
                let verified_msg = verify_sent_appeared(
                    ctx,
                    shared,
                    &win,
                    (target_kind, &active_target),
                    &plan,
                    baseline_count,
                );
                let mut outcome = build_focus_restore_outcome(&FocusRestoreOutcomeParams {
                    group: &active_target,
                    foreground_touched: fg_touched,
                    mouse_touched,
                    verified: verified_msg.is_some(),
                    elapsed_ms: started.elapsed().as_millis() as u64,
                    verified_detail:
                        "sent via focus-restore Unicode SendInput and verified in chat history",
                    unverified_detail:
                        "focus-restore Unicode SendInput could not be verified in chat history",
                });
                outcome.target_kind = target_kind;
                outcome.degraded_segments = plan.degraded_segments.clone();
                Ok((outcome, verified_msg))
            }
            InputMode::Auto => {
                identity.verify_fast()?;
                let bg_attempt = type_text_background_verified(
                    &identity,
                    editor,
                    &plan.combined_text,
                    &delay_range,
                );
                match bg_attempt {
                    Ok(true) => {
                        identity.verify_fast()?;
                        send_submit_background_for_editor_verified(
                            &identity,
                            Some(editor),
                            send_btn,
                            shortcut,
                        )?;
                        let verified_msg = verify_sent_appeared(
                            ctx,
                            shared,
                            &win,
                            (target_kind, &active_target),
                            &plan,
                            baseline_count,
                        );
                        let verified = verified_msg.is_some();
                        let cleared = is_editor_empty(editor);
                        if verified || cleared || !shared.config.interaction.allow_focus_fallback {
                            Ok((
                                SendOutcome {
                                    ok: verified,
                                    group: active_target.clone(),
                                    used_mode: InputMode::BackgroundUia,
                                    foreground_touched: false,
                                    mouse_touched: false,
                                    verified_in_chat: verified,
                                    elapsed_ms: started.elapsed().as_millis() as u64,
                                    detail: if verified {
                                        "sent via background UIA and verified in chat history"
                                            .to_string()
                                    } else if cleared {
                                        "submitted via background UIA (composer cleared) but message could not be verified in chat history"
                                            .to_string()
                                    } else {
                                        "background UIA send could not be verified (fallback disabled)"
                                            .to_string()
                                    },
                                    target_kind,
                                    degraded_segments: plan.degraded_segments.clone(),
                                    ..Default::default()
                                },
                                verified_msg,
                            ))
                        } else {
                            let fallback_params = FallbackSubmitParams {
                                target: &submit_target,
                                raw_text: &plan.combined_text,
                                norm_text: &plan.verify_norm,
                                delay_range,
                                retype_reason:
                                    "background UIA unverified; fell back to focus-restore Unicode SendInput",
                            };
                            let (fg_touched, mouse_touched, detail) =
                                fallback_submit_or_retype(&identity, &fallback_params)?;
                            let verified_fb_msg = verify_sent_appeared(
                                ctx,
                                shared,
                                &win,
                                (target_kind, &active_target),
                                &plan,
                                baseline_count,
                            );
                            let unverified_detail =
                                format!("{detail}; delivery could not be verified in chat history");
                            let mut outcome =
                                build_focus_restore_outcome(&FocusRestoreOutcomeParams {
                                    group: &active_target,
                                    foreground_touched: fg_touched,
                                    mouse_touched,
                                    verified: verified_fb_msg.is_some(),
                                    elapsed_ms: started.elapsed().as_millis() as u64,
                                    verified_detail: &detail,
                                    unverified_detail: &unverified_detail,
                                });
                            outcome.target_kind = target_kind;
                            outcome.degraded_segments = plan.degraded_segments.clone();
                            Ok((outcome, verified_fb_msg))
                        }
                    }
                    Ok(false) | Err(_) if shared.config.interaction.allow_focus_fallback => {
                        let fallback_params = FallbackSubmitParams {
                            target: &submit_target,
                            raw_text: &plan.combined_text,
                            norm_text: &plan.verify_norm,
                            delay_range,
                            retype_reason:
                                "background UIA rejected text; fell back to focus-restore Unicode SendInput",
                        };
                        let (fg_touched, mouse_touched, detail) =
                            fallback_submit_or_retype(&identity, &fallback_params)?;
                        let verified_fb_msg = verify_sent_appeared(
                            ctx,
                            shared,
                            &win,
                            (target_kind, &active_target),
                            &plan,
                            baseline_count,
                        );
                        let unverified_detail =
                            format!("{detail}; delivery could not be verified in chat history");
                        let mut outcome = build_focus_restore_outcome(&FocusRestoreOutcomeParams {
                            group: &active_target,
                            foreground_touched: fg_touched,
                            mouse_touched,
                            verified: verified_fb_msg.is_some(),
                            elapsed_ms: started.elapsed().as_millis() as u64,
                            verified_detail: &detail,
                            unverified_detail: &unverified_detail,
                        });
                        outcome.target_kind = target_kind;
                        outcome.degraded_segments = plan.degraded_segments.clone();
                        Ok((outcome, verified_fb_msg))
                    }
                    Ok(false) => Err(AdapterError::InputRejected(
                        "background UIA typing failed and focus fallback is disabled".to_string(),
                    )),
                    Err(err) => Err(err),
                }
            }
        }
    };

    finalize_planned_send_result(
        shared,
        (target_kind, &active_target),
        &plan,
        detected_count,
        send_result,
    )
}

/// 根据焦点恢复输入尝试的执行细节构建诚实的 `SendOutcome` 返回结构
pub(crate) fn build_focus_restore_outcome(params: &FocusRestoreOutcomeParams<'_>) -> SendOutcome {
    SendOutcome {
        ok: params.verified,
        group: params.group.to_string(),
        used_mode: InputMode::FocusRestoreUnicode,
        foreground_touched: params.foreground_touched,
        mouse_touched: params.mouse_touched,
        verified_in_chat: params.verified,
        elapsed_ms: params.elapsed_ms,
        detail: if params.verified {
            params.verified_detail.to_string()
        } else {
            params.unverified_detail.to_string()
        },
        ..Default::default()
    }
}

/// 基于执行计划统一维护发送统计计数、回滚未证实发信并写入结构化消息段记录
pub(crate) fn finalize_planned_send_result(
    shared: &Arc<SharedInner>,
    target: (ChatTargetKind, &str),
    plan: &OutboundSendPlan,
    detected_member_count: Option<u32>,
    send_result: Result<(SendOutcome, Option<ChatMessage>)>,
) -> Result<SendOutcome> {
    let (target_kind, target_name) = target;
    match send_result {
        Ok((mut outcome, verified_msg)) => {
            outcome.target_kind = target_kind;
            outcome.degraded_segments = plan.degraded_segments.clone();
            if outcome.ok {
                shared.state.lock().messages_sent += 1;
                let (onebot_msg_id, entity_id, stub_uid) = shared.persist_confirmed_sent_plan(
                    &outcome.group,
                    target_kind,
                    plan,
                    verified_msg.as_ref(),
                    detected_member_count,
                );
                if let Some(msg_id) = onebot_msg_id {
                    let media_token = crate::composer::popup_helpers::extract_media_bubble_tokens(
                        &plan.canonical_segments,
                    )
                    .into_iter()
                    .next();
                    let session_name = if outcome.group.trim().is_empty() {
                        target_name
                    } else {
                        outcome.group.as_str()
                    };
                    shared
                        .tracker
                        .lock()
                        .register_confirmed_outbound(OutboundStubBinding {
                            onebot_message_id: msg_id,
                            stub_uid,
                            norm_text: plan.verify_norm.clone(),
                            media_token,
                            session_key: parser::tracker_session_key(target_kind, session_name),
                        });
                }
                outcome.onebot_message_id = outcome.onebot_message_id.or(onebot_msg_id);
                match target_kind {
                    ChatTargetKind::Group => {
                        outcome.group_id = outcome.group_id.or(Some(entity_id))
                    }
                    ChatTargetKind::Private => {
                        outcome.user_id = outcome.user_id.or(Some(entity_id))
                    }
                }
            } else {
                rollback_plan_outbound(shared, target_kind, target_name, plan);
            }
            Ok(outcome)
        }
        Err(err) => {
            rollback_plan_outbound(shared, target_kind, target_name, plan);
            Err(err)
        }
    }
}

/// 兼容单测调用的发送收尾桥接入口
#[cfg(test)]
pub(crate) fn finalize_send_result(
    shared: &Arc<SharedInner>,
    req: &SendRequest,
    detected_member_count: Option<u32>,
    send_result: Result<SendOutcome>,
) -> Result<SendOutcome> {
    let target_kind = req.target_kind;
    let segments = outbound_media::extract_effective_segments(req).unwrap_or_default();
    let summary_text = if segments.is_empty() {
        req.text.clone()
    } else {
        common_types::segments_to_plain_summary(&segments)
    };
    let verify_norm = parser::normalize_message_text(&req.text);
    let plan = OutboundSendPlan {
        combined_text: req.text.clone(),
        images: Vec::new(),
        canonical_segments: segments,
        summary_text,
        verify_norm,
        degraded_segments: Vec::new(),
    };
    finalize_planned_send_result(
        shared,
        (target_kind, &req.group),
        &plan,
        detected_member_count,
        send_result.map(|outcome| (outcome, None)),
    )
}
