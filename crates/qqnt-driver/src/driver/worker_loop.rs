//! 专用 STA COM UIA 工作线程事件分发主循环及本地窗口身份护栏缓存

use std::sync::Arc;

use uia_core::{UiaSession, VerifiedWindowIdentity};

use super::commands::{DriverCommand, PriorityCommandQueue};
use super::group_handlers::{
    handle_list_sessions, handle_mark_session_read, handle_open_group, handle_poll_messages,
    handle_sync_and_poll, PollRequest,
};
use super::onebot_queries::{
    handle_get_group_info, handle_get_group_list, handle_get_group_member_list,
};
use super::private_handlers::{handle_get_friend_list, handle_open_private};
use super::runtime_state::SharedInner;
use super::send_handlers::{handle_send_group_message, handle_send_private_message};
use super::window_discovery::{
    handle_capture_screenshot, handle_diagnose_window, handle_ensure_qq_window,
    handle_inspect_tree, handle_list_windows, handle_reset_security_breaker, handle_status,
    refresh_pids_and_check_security,
};
use crate::composer::{
    handle_forward_single_message, handle_probe_ui_action, handle_send_forward_messages,
};

/// 仅生存于单线程 COM/UIA 工作线程内部的会画句柄与目标窗口不可变身份缓存
pub(crate) struct WorkerContext {
    pub(crate) session: UiaSession,
    pub(crate) verified_cache: Option<VerifiedWindowIdentity>,
}

/// 在专用 MTA/COM 工作线程上串行消费异步控制命令直至收到关停信号
pub(crate) fn run_worker_loop(
    mut ctx: WorkerContext,
    shared: &Arc<SharedInner>,
    cmd_queue: Arc<PriorityCommandQueue>,
) {
    while let Some(cmd) = cmd_queue.pop_blocking() {
        match cmd {
            DriverCommand::Status(reply) => {
                let _ = reply.send(handle_status(&ctx.session, shared));
            }
            DriverCommand::CheckSecurity(reply) => {
                let _ = reply.send(refresh_pids_and_check_security(shared).map(|_| ()));
            }
            DriverCommand::ListWindows(reply) => {
                let _ = reply.send(handle_list_windows(shared));
            }
            DriverCommand::EnsureQqWindow { force_wake, resp } => {
                let _ = resp.send(handle_ensure_qq_window(shared, force_wake));
            }
            DriverCommand::DiagnoseWindow { hwnd, resp } => {
                let _ = resp.send(handle_diagnose_window(&ctx.session, shared, hwnd));
            }
            DriverCommand::InspectTree {
                hwnd,
                max_depth,
                resp,
            } => {
                let _ = resp.send(handle_inspect_tree(&ctx.session, shared, hwnd, max_depth));
            }
            DriverCommand::ListSessions(reply) => {
                let _ = reply.send(handle_list_sessions(&mut ctx, shared));
            }
            DriverCommand::OpenGroup { spec, resp } => {
                let _ = resp.send(handle_open_group(&mut ctx, shared, &spec));
            }
            DriverCommand::OpenPrivate { target, resp } => {
                let _ = resp.send(handle_open_private(&mut ctx, shared, &target));
            }
            DriverCommand::PollMessages {
                group,
                limit,
                allow_unocclude,
                resp,
            } => {
                let poll_req = PollRequest {
                    group: &group,
                    limit,
                    allow_unocclude,
                };
                let _ = resp.send(handle_poll_messages(&mut ctx, shared, &poll_req));
            }
            DriverCommand::SendGroupMessage { req, resp } => {
                let _ = resp.send(handle_send_group_message(&mut ctx, shared, &req));
            }
            DriverCommand::SendPrivateMessage { req, resp } => {
                let _ = resp.send(handle_send_private_message(&mut ctx, shared, &req));
            }
            DriverCommand::SyncAndPoll { poll_limit, resp } => {
                let _ = resp.send(handle_sync_and_poll(&mut ctx, shared, poll_limit));
            }
            DriverCommand::GetGroupList { no_cache, resp } => {
                let _ = resp.send(handle_get_group_list(&mut ctx, shared, no_cache));
            }
            DriverCommand::GetFriendList { no_cache, resp } => {
                let _ = resp.send(handle_get_friend_list(&mut ctx, shared, no_cache));
            }
            DriverCommand::GetGroupInfo {
                group_id,
                group_name,
                no_cache,
                resp,
            } => {
                let _ = resp.send(handle_get_group_info(
                    &mut ctx,
                    shared,
                    group_id,
                    group_name.as_deref(),
                    no_cache,
                ));
            }
            DriverCommand::GetGroupMemberList {
                group_id,
                group_name,
                no_cache,
                resp,
            } => {
                let _ = resp.send(handle_get_group_member_list(
                    &mut ctx,
                    shared,
                    group_id,
                    group_name.as_deref(),
                    no_cache,
                ));
            }
            DriverCommand::SendForwardMessages { req, resp } => {
                let _ = resp.send(handle_send_forward_messages(&mut ctx, shared, &req));
            }
            DriverCommand::ForwardSingleMessage { req, resp } => {
                let _ = resp.send(handle_forward_single_message(&mut ctx, shared, &req));
            }
            DriverCommand::ResetSecurityBreaker(reply) => {
                let _ = reply.send(handle_reset_security_breaker(shared));
            }
            DriverCommand::DeleteMessage {
                onebot_message_id,
                resp,
            } => {
                let _ = resp.send(crate::composer::handle_delete_message(
                    &mut ctx,
                    shared,
                    onebot_message_id,
                ));
            }
            DriverCommand::CaptureScreenshot { hwnd, resp } => {
                let _ = resp.send(handle_capture_screenshot(shared, hwnd));
            }
            DriverCommand::ProbeUiAction {
                action,
                target,
                query,
                resp,
            } => {
                let _ = resp.send(handle_probe_ui_action(
                    &mut ctx,
                    shared,
                    &action,
                    target.as_deref(),
                    query.as_deref(),
                ));
            }
            DriverCommand::MarkSessionRead {
                target,
                target_kind,
                resp,
            } => {
                let _ = resp.send(handle_mark_session_read(
                    &mut ctx,
                    shared,
                    &target,
                    target_kind,
                ));
            }
            DriverCommand::Shutdown => {
                break;
            }
        }
    }
}
