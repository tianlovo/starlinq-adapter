//! QQNT 桌面富媒体发送编排与原生交互门面模块

pub(crate) mod file_staging;
pub(crate) mod forward_ops;
pub(crate) mod inline_ops;
pub(crate) mod plan;
pub(crate) mod popup_helpers;
pub(crate) mod probe_ops;
pub(crate) mod recall_ops;
pub(crate) mod single_forward_ops;
pub(crate) mod standalone_ops;

use std::collections::HashSet;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use common_types::{
    ChatMessage, ChatTargetKind, InputMode, Result, SendOutcome, SendShortcut, WindowInfo,
};
use uia_core::{TypingDelayRange, UiNodeHandle, VerifiedWindowIdentity};

use crate::constants::{INTERNAL_SCAN_DEPTH, SEND_VERIFY_RETRY_DELAYS_MS};
use crate::driver::outbound_media::OutboundSendPlan;
use crate::driver::runtime_state::SharedInner;
use crate::driver::send_handlers;
use crate::driver::worker_loop::WorkerContext;
use crate::matcher;
use crate::parser::{self, OutboundStubBinding};

pub(crate) use forward_ops::handle_send_forward_messages;
pub(crate) use plan::{build_composer_plan, ComposerExecutionPlan};
pub(crate) use probe_ops::handle_probe_ui_action;
pub(crate) use recall_ops::handle_delete_message;
pub(crate) use single_forward_ops::handle_forward_single_message;

use inline_ops::{execute_rich_inline_steps, RichComposeContext};
use standalone_ops::{execute_standalone_action, StandaloneExecContext};

/// 原生富媒体发送执行与回显验证共享上下文（Parameter Object）
pub(crate) struct NativeSendContext<'a> {
    pub(crate) ctx: &'a WorkerContext,
    pub(crate) shared: &'a Arc<SharedInner>,
    pub(crate) identity: &'a VerifiedWindowIdentity,
    pub(crate) win: &'a WindowInfo,
    pub(crate) active_target: &'a str,
    pub(crate) target_kind: ChatTargetKind,
    pub(crate) detected_member_count: Option<u32>,
    pub(crate) pids: &'a [u32],
    pub(crate) editor: &'a UiNodeHandle,
    pub(crate) send_button: Option<&'a UiNodeHandle>,
    pub(crate) editor_rect: [i32; 4],
    pub(crate) send_rect: Option<[i32; 4]>,
    pub(crate) shortcut: SendShortcut,
    pub(crate) delay_range: TypingDelayRange,
    pub(crate) started: Instant,
}

/// 原生发送前聊天窗口基线摘要，用于在虚拟列表滚动场景下可靠识别新增自发气泡
struct BaselineChatSnapshot {
    total_count: usize,
    last_self_norm: Option<String>,
    baseline_uids: HashSet<String>,
}

fn snapshot_self_messages(native_ctx: &NativeSendContext<'_>) -> Result<(Vec<ChatMessage>, usize)> {
    let (tree, _) = matcher::wake_and_dump_tree_ext(
        &native_ctx.ctx.session,
        native_ctx.win.hwnd,
        INTERNAL_SCAN_DEPTH,
        false,
    )?;
    native_ctx
        .shared
        .register_tree_self_nicknames(&tree, native_ctx.win.rect);
    let mut parsed = parser::extract_messages_from_tree(
        &tree,
        native_ctx.win.rect,
        native_ctx.win.is_standalone_chat,
        native_ctx.active_target,
    );
    for msg in &mut parsed {
        msg.target_kind = native_ctx.target_kind;
    }
    let annotated = {
        let mut tracker = native_ctx.shared.tracker.lock();
        tracker.annotate_only(parsed)
    };
    let notice_count = parser::extract_system_notices_from_tree(
        &tree,
        native_ctx.win.rect,
        native_ctx.win.is_standalone_chat,
        native_ctx.active_target,
    )
    .len();
    Ok((annotated, notice_count))
}

fn matches_verify_tokens(msg: &ChatMessage, verify_tokens: &[String], has_image: bool) -> bool {
    let actual_norm = parser::normalize_message_text(&msg.content);
    if verify_tokens.is_empty() {
        return !actual_norm.is_empty() && msg.is_self;
    }
    let matched_tokens = verify_tokens
        .iter()
        .filter(|tok| {
            let t = parser::normalize_message_text(tok);
            !t.is_empty() && actual_norm.contains(&t)
        })
        .count();
    if matched_tokens > 0 && (matched_tokens == verify_tokens.len() || has_image) {
        return true;
    }
    if has_image && actual_norm.contains("[图片]") && verify_tokens.len() == 1 {
        return true;
    }
    false
}

fn select_verified_message_from_snapshot(
    messages: &[ChatMessage],
    verify_tokens: &[String],
    has_image: bool,
    baseline: &BaselineChatSnapshot,
) -> Option<ChatMessage> {
    let has_filename_token = verify_tokens.iter().any(|tok| tok.contains('.'));
    let new_matching: Vec<ChatMessage> = messages
        .iter()
        .filter(|m| {
            !baseline.baseline_uids.contains(&m.message_id)
                && matches_verify_tokens(m, verify_tokens, has_image)
        })
        .cloned()
        .collect();
    if let Some(matched) = new_matching.last() {
        return Some(matched.clone());
    }

    // 严禁回落匹配发信前基线中已存在的历史同文件名气泡
    let new_last_self = messages
        .iter()
        .rev()
        .find(|m| m.is_self && !baseline.baseline_uids.contains(&m.message_id));
    if let Some(self_msg) = new_last_self {
        if matches_verify_tokens(self_msg, verify_tokens, has_image) {
            return Some(self_msg.clone());
        }
        let last_self_norm = parser::normalize_message_text(&self_msg.content);
        let tail_self_changed = !last_self_norm.is_empty()
            && baseline.last_self_norm.as_deref() != Some(last_self_norm.as_str());
        if !has_filename_token
            && (verify_tokens.is_empty()
                || (!has_image && (messages.len() > baseline.total_count || tail_self_changed)))
        {
            return Some(self_msg.clone());
        }
    }
    None
}

fn poll_native_verified_message(
    native_ctx: &NativeSendContext<'_>,
    verify_tokens: &[String],
    has_image: bool,
    baseline: &BaselineChatSnapshot,
) -> Option<ChatMessage> {
    for wait_ms in SEND_VERIFY_RETRY_DELAYS_MS {
        thread::sleep(Duration::from_millis(wait_ms));
        let Ok((messages, _notice_count)) = snapshot_self_messages(native_ctx) else {
            continue;
        };
        if let Some(matched) =
            select_verified_message_from_snapshot(&messages, verify_tokens, has_image, baseline)
        {
            return Some(matched);
        }
    }
    None
}

/// 分发并执行 `RichInline` 或 `Standalone` 原生桌面发送计划，校验气泡落地后绑定双形态去重键并持久化
pub(crate) fn execute_native_rich_send(
    native_ctx: &mut NativeSendContext<'_>,
    plan: ComposerExecutionPlan,
) -> Result<SendOutcome> {
    let (canonical_segments, summary_text, verify_tokens, has_image, is_rich, rich_or_standalone) =
        match plan {
            ComposerExecutionPlan::RichInline {
                steps,
                canonical_segments,
                summary_text,
                verify_tokens,
                has_image,
                ..
            } => (
                canonical_segments,
                summary_text,
                verify_tokens,
                has_image,
                true,
                Err(steps),
            ),
            ComposerExecutionPlan::Standalone {
                action,
                canonical_segments,
                summary_text,
                verify_tokens,
            } => (
                canonical_segments,
                summary_text,
                verify_tokens,
                false,
                false,
                Ok(action),
            ),
            ComposerExecutionPlan::PurePlainTextOrImage | ComposerExecutionPlan::DegradedLegacy => {
                unreachable!(
                    "execute_native_rich_send only handles RichInline or Standalone plans"
                );
            }
        };

    let (baseline_msgs, _) = snapshot_self_messages(native_ctx)?;
    let baseline = BaselineChatSnapshot {
        total_count: baseline_msgs.len(),
        last_self_norm: baseline_msgs
            .iter()
            .rev()
            .find(|m| m.is_self)
            .map(|m| parser::normalize_message_text(&m.content))
            .filter(|s| !s.is_empty()),
        baseline_uids: baseline_msgs.iter().map(|m| m.message_id.clone()).collect(),
    };

    let outbound_plan = OutboundSendPlan {
        combined_text: summary_text.clone(),
        images: Vec::new(),
        canonical_segments,
        summary_text,
        verify_norm: parser::normalize_message_text(&verify_tokens.join(" ")),
        degraded_segments: Vec::new(),
    };
    send_handlers::record_plan_outbound(
        native_ctx.shared,
        native_ctx.target_kind,
        native_ctx.active_target,
        &outbound_plan,
    );

    let is_notice_action = matches!(rich_or_standalone, Ok(plan::StandaloneAction::PrivateShake));

    let exec_result = match rich_or_standalone {
        Err(ref steps) => {
            let compose_ctx = RichComposeContext {
                session: &native_ctx.ctx.session,
                identity: native_ctx.identity,
                win: native_ctx.win,
                active_target: native_ctx.active_target,
                target_kind: native_ctx.target_kind,
                pids: native_ctx.pids,
                editor: native_ctx.editor,
                send_button: native_ctx.send_button,
                editor_rect: native_ctx.editor_rect,
                send_rect: native_ctx.send_rect,
                shortcut: native_ctx.shortcut,
                delay_range: native_ctx.delay_range,
            };
            execute_rich_inline_steps(&compose_ctx, steps)
        }
        Ok(ref action) => {
            let standalone_ctx = StandaloneExecContext {
                session: &native_ctx.ctx.session,
                identity: native_ctx.identity,
                win: native_ctx.win,
                active_target: native_ctx.active_target,
                target_kind: native_ctx.target_kind,
                pids: native_ctx.pids,
                editor: native_ctx.editor,
                send_button: native_ctx.send_button,
                editor_rect: native_ctx.editor_rect,
                shortcut: native_ctx.shortcut,
            };
            execute_standalone_action(&standalone_ctx, action)
        }
    };

    let mut live_ui_norm = String::new();
    let send_result: Result<(SendOutcome, Option<ChatMessage>)> = match exec_result {
        Ok((fg_touched, mouse_touched)) => {
            let verified_msg = if is_notice_action {
                None
            } else {
                poll_native_verified_message(native_ctx, &verify_tokens, has_image, &baseline)
            };
            let verified = is_notice_action || verified_msg.is_some();
            if let Some(ref vm) = verified_msg {
                let ui_norm = parser::normalize_message_text(&vm.content);
                if !ui_norm.is_empty() && ui_norm != outbound_plan.verify_norm {
                    native_ctx
                        .shared
                        .tracker
                        .lock()
                        .record_outbound_for_session(
                            native_ctx.target_kind,
                            native_ctx.active_target,
                            &ui_norm,
                        );
                    live_ui_norm = ui_norm;
                }
            }
            let kind_label = if is_rich {
                "rich-inline"
            } else {
                "standalone-native"
            };
            Ok((
                SendOutcome {
                    ok: verified,
                    group: native_ctx.active_target.to_string(),
                    used_mode: InputMode::FocusRestoreUnicode,
                    foreground_touched: fg_touched,
                    mouse_touched,
                    verified_in_chat: verified,
                    elapsed_ms: native_ctx.started.elapsed().as_millis() as u64,
                    detail: if verified {
                        format!("sent via {kind_label} native GUI interaction and verified in chat")
                    } else {
                        format!("executed {kind_label} native GUI interaction but delivery could not be confirmed in chat history")
                    },
                    target_kind: native_ctx.target_kind,
                    ..Default::default()
                },
                verified_msg,
            ))
        }
        Err(err) => Err(err),
    };

    let outcome = send_handlers::finalize_planned_send_result(
        native_ctx.shared,
        (native_ctx.target_kind, native_ctx.active_target),
        &outbound_plan,
        native_ctx.detected_member_count,
        send_result,
    )?;

    if let Some(msg_id) = outcome.onebot_message_id {
        let media_token =
            popup_helpers::extract_media_bubble_tokens(&outbound_plan.canonical_segments)
                .into_iter()
                .next();
        if let Some(stored) = native_ctx
            .shared
            .storage
            .get_message_by_id(msg_id)
            .ok()
            .flatten()
        {
            for candidate_norm in [
                live_ui_norm,
                parser::normalize_message_text(&stored.raw_message),
            ] {
                if !candidate_norm.is_empty() && candidate_norm != outbound_plan.verify_norm {
                    native_ctx
                        .shared
                        .tracker
                        .lock()
                        .register_confirmed_outbound(OutboundStubBinding {
                            onebot_message_id: msg_id,
                            stub_uid: stored.message_uid.clone(),
                            norm_text: candidate_norm,
                            media_token: media_token.clone(),
                            session_key: parser::tracker_session_key(
                                native_ctx.target_kind,
                                native_ctx.active_target,
                            ),
                        });
                }
            }
        }
    }

    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poll_verifier_ignores_historical_baseline_uid_with_same_filename() {
        let old_video = ChatMessage {
            message_id: "09badb9f-old".to_string(),
            group_name: "Demo测试群".to_string(),
            sender: "测试机器人".to_string(),
            content: "[图片]\nproto_v4_live_video.mp4\n7.69\u{2009}KB 已发送\n[图片]".to_string(),
            is_self: true,
            ..Default::default()
        };
        let baseline = BaselineChatSnapshot {
            total_count: 1,
            last_self_norm: Some(parser::normalize_message_text(&old_video.content)),
            baseline_uids: HashSet::from([old_video.message_id.clone()]),
        };
        let tokens = vec!["proto_v4_live_video.mp4".to_string()];

        // 即使群内其它成员发言导致总条数增长，也绝不能回落返回旧基线里的同文件名视频气泡
        let unrelated_peer = ChatMessage {
            message_id: "peer-1".to_string(),
            group_name: "Demo测试群".to_string(),
            sender: "群友A".to_string(),
            content: "hello".to_string(),
            is_self: false,
            ..Default::default()
        };
        assert!(select_verified_message_from_snapshot(
            &[old_video.clone(), unrelated_peer.clone()],
            &tokens,
            false,
            &baseline,
        )
        .is_none());

        // 当带有文件名的校验词未匹配到新增非视频自发消息时，同样拒绝误认
        let unrelated_self = ChatMessage {
            message_id: "self-other-1".to_string(),
            group_name: "Demo测试群".to_string(),
            sender: "测试机器人".to_string(),
            content: "unrelated text".to_string(),
            is_self: true,
            ..Default::default()
        };
        assert!(select_verified_message_from_snapshot(
            &[old_video.clone(), unrelated_self],
            &tokens,
            false,
            &baseline,
        )
        .is_none());

        // 新视频以 0% 进度气泡渲染后，必须精准命中新气泡而非旧 09badb9f-old
        let new_video_0pct = ChatMessage {
            message_id: "9156dd8b-new".to_string(),
            group_name: "Demo测试群".to_string(),
            sender: "测试机器人".to_string(),
            content: "[图片]\nproto_v4_live_video.mp4\n10.97\u{2009}KB\n0%\n[图片]".to_string(),
            is_self: true,
            ..Default::default()
        };
        let verified = select_verified_message_from_snapshot(
            &[old_video, new_video_0pct],
            &tokens,
            false,
            &baseline,
        )
        .expect("new 0% video bubble must verify");
        assert_eq!(verified.message_id, "9156dd8b-new");
    }
}
