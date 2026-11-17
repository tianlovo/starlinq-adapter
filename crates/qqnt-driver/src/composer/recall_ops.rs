//! 原生消息撤回执行器：通过右键目标消息气泡->点击“撤回”菜单项->校验客户端回退与系统横幅完成真实撤回并同步清理数据库

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use common_types::{
    AdapterError, AdapterEvent, BotLoginInfo, ChatTargetKind, Result, StoredMessageRecord,
    UiNodeSnapshot, WindowInfo,
};
use serde_json::{json, Value};
use uia_core::constants::{CONTROL_TYPE_BUTTON, CONTROL_TYPE_MENU_ITEM, CONTROL_TYPE_TEXT};
use uia_core::{ForegroundRestoreGuard, SimulatedClickKind, VerifiedWindowIdentity};

use super::forward_ops::{
    attach_chat_window, exit_multiselect_cleanly, find_bubble_for_record,
    is_multiselect_bar_active, verify_target_guard_in_tree,
};
use super::popup_helpers::{
    click_matched_target_verified, click_rect_verified, collect_visible_bubbles,
    dismiss_popups_best_effort, find_overlay_targets_in_snapshot, find_popup_clickable_by_keywords,
    locate_message_list_scroll_point, restore_message_list_to_bottom, scan_same_pid_popup_trees,
    scroll_message_list_wheel, MatchedUiTarget, VisibleBubbleEntry,
};
use crate::constants::INTERNAL_SCAN_DEPTH;
use crate::driver::private_handlers::verify_private_whitelist;
use crate::driver::runtime_state::{now_utc_ms, SharedInner};
use crate::driver::window_discovery::refresh_pids_and_check_security;
use crate::driver::worker_loop::WorkerContext;
use crate::matcher;
use crate::parser::{self, ParsedSystemNotice};

const MAX_RECALL_SCROLL_STEPS: usize = 24;

/// 已定位待撤回消息气泡及其当前视口基线信息
struct LocatedRecallTarget {
    bub_rect: [i32; 4],
    view_match_count: usize,
    view_recall_notices: usize,
    bottom_recall_notices: usize,
    scroll_up_steps: usize,
    scroll_point: Option<(i32, i32)>,
}

fn is_exact_recall_menu_item(target: &MatchedUiTarget) -> bool {
    let nm = target.name.trim();
    let ct = target.control_type.trim();
    nm == "撤回" && !ct.eq_ignore_ascii_case(CONTROL_TYPE_TEXT)
        || (nm == "撤回" && ct.eq_ignore_ascii_case(CONTROL_TYPE_MENU_ITEM))
}

fn count_matching_bubbles(bubbles: &[VisibleBubbleEntry], record: &StoredMessageRecord) -> usize {
    let mut excluded = Vec::new();
    while excluded.len() < 20 {
        let Some(rect) = find_bubble_for_record(bubbles, record, &excluded) else {
            break;
        };
        excluded.push(rect);
    }
    excluded.len()
}

fn extract_notices_and_recall_count(
    tree: &UiNodeSnapshot,
    win: &WindowInfo,
    active_name: &str,
) -> (Vec<ParsedSystemNotice>, usize) {
    let notices = parser::extract_system_notices_from_tree(
        tree,
        win.rect,
        win.is_standalone_chat,
        active_name,
    );
    let count = notices
        .iter()
        .filter(|n| n.text.contains("撤回了一条消息"))
        .count();
    (notices, count)
}

fn locate_recall_bubble(
    ctx: &mut WorkerContext,
    identity: &VerifiedWindowIdentity,
    win: &WindowInfo,
    active_name: &str,
    record: &StoredMessageRecord,
) -> Result<LocatedRecallTarget> {
    let (initial_tree, _guard) =
        matcher::wake_and_dump_tree_ext(&ctx.session, win.hwnd, INTERNAL_SCAN_DEPTH, false)?;
    let initial_bubbles =
        collect_visible_bubbles(&initial_tree, win.rect, win.is_standalone_chat, active_name);
    let (_, bottom_recall_notices) =
        extract_notices_and_recall_count(&initial_tree, win, active_name);
    let scroll_point =
        locate_message_list_scroll_point(&initial_tree, win.rect, win.is_standalone_chat)
            .map(|(x, y, _)| (x, y));

    if let Some(bub_rect) = find_bubble_for_record(&initial_bubbles, record, &[]) {
        return Ok(LocatedRecallTarget {
            bub_rect,
            view_match_count: count_matching_bubbles(&initial_bubbles, record),
            view_recall_notices: bottom_recall_notices,
            bottom_recall_notices,
            scroll_up_steps: 0,
            scroll_point,
        });
    }

    let Some(pt) = scroll_point else {
        return Err(AdapterError::InputRejected(format!(
            "target message_id={} bubble is not visible in current viewport",
            record.onebot_message_id
        )));
    };

    let mut scroll_up_steps = 0usize;
    let mut last_sig: Option<(String, String)> = None;
    let mut stagnant = 0usize;
    for _ in 0..MAX_RECALL_SCROLL_STEPS {
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
            let (_, view_recall_notices) =
                extract_notices_and_recall_count(&scrolled_tree, win, active_name);
            return Ok(LocatedRecallTarget {
                bub_rect,
                view_match_count: count_matching_bubbles(&bubbles, record),
                view_recall_notices,
                bottom_recall_notices,
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
            thread::sleep(Duration::from_millis(450));
        } else {
            stagnant = 0;
            last_sig = sig;
        }
    }

    restore_message_list_to_bottom(identity, pt, scroll_up_steps);
    Err(AdapterError::InputRejected(format!(
        "target message_id={} bubble was not found in recent scrollable chat history",
        record.onebot_message_id
    )))
}

fn confirm_recall_if_modal_open(
    ctx: &WorkerContext,
    main_hwnd: isize,
    pids: &[u32],
    allow_mock: bool,
) -> Result<()> {
    let windows = scan_same_pid_popup_trees(&ctx.session, main_hwnd, pids, 10);
    for (popup_hwnd, _cls, _title, snap) in windows {
        if popup_hwnd == main_hwnd {
            continue;
        }
        let confirm_hits = find_overlay_targets_in_snapshot(popup_hwnd, &snap, &|node| {
            let ct = node.control_type.trim();
            let nm = node.name.trim();
            node.is_enabled
                && (ct.eq_ignore_ascii_case(CONTROL_TYPE_BUTTON)
                    || ct.eq_ignore_ascii_case(CONTROL_TYPE_MENU_ITEM))
                && matches!(nm, "确定" | "撤回" | "确认")
        });
        if let Some(btn) = confirm_hits.first() {
            if let Ok(popup_identity) =
                VerifiedWindowIdentity::verify_and_bind(popup_hwnd, allow_mock)
            {
                click_rect_verified(&popup_identity, btn.rect, SimulatedClickKind::Left, true)?;
                thread::sleep(Duration::from_millis(160));
                break;
            }
        }
    }
    Ok(())
}

fn wait_and_confirm_recall_effect(
    ctx: &WorkerContext,
    identity: &VerifiedWindowIdentity,
    win: &WindowInfo,
    active_name: &str,
    record: &StoredMessageRecord,
    target: &LocatedRecallTarget,
) -> Result<Vec<ParsedSystemNotice>> {
    let mut confirmed = false;
    let mut latest_notices = Vec::new();

    for wait_ms in [220u64, 300, 400] {
        thread::sleep(Duration::from_millis(wait_ms));
        if let Ok((after_tree, _)) =
            matcher::wake_and_dump_tree_ext(&ctx.session, win.hwnd, INTERNAL_SCAN_DEPTH, false)
        {
            let after_bubbles =
                collect_visible_bubbles(&after_tree, win.rect, win.is_standalone_chat, active_name);
            let after_matches = count_matching_bubbles(&after_bubbles, record);
            let (notices, after_recall_notices) =
                extract_notices_and_recall_count(&after_tree, win, active_name);
            latest_notices = notices;
            if after_matches < target.view_match_count
                || after_recall_notices > target.view_recall_notices
            {
                confirmed = true;
                break;
            }
        }
    }

    if let (true, Some(pt)) = (target.scroll_up_steps > 0, target.scroll_point) {
        restore_message_list_to_bottom(identity, pt, target.scroll_up_steps);
        if let Ok((bottom_tree, _)) =
            matcher::wake_and_dump_tree_ext(&ctx.session, win.hwnd, INTERNAL_SCAN_DEPTH, false)
        {
            let (bottom_notices, bottom_after_recall) =
                extract_notices_and_recall_count(&bottom_tree, win, active_name);
            if !confirmed && bottom_after_recall > target.bottom_recall_notices {
                confirmed = true;
            }
            latest_notices = bottom_notices;
        }
    }

    if !confirmed {
        return Err(AdapterError::UiaFailure(format!(
            "clicked QQNT '撤回' for message_id={} but bubble removal or recall notice could not be confirmed",
            record.onebot_message_id
        )));
    }

    Ok(latest_notices)
}

fn build_recall_notice_payload(
    shared: &Arc<SharedInner>,
    record: &StoredMessageRecord,
    active_name: &str,
    target_kind: ChatTargetKind,
    login: &BotLoginInfo,
) -> Value {
    let time_secs = (now_utc_ms() / 1000) as i64;
    let onebot_message_id = record.onebot_message_id;
    match target_kind {
        ChatTargetKind::Group => {
            let group_id = if record.group_id > 0 {
                record.group_id
            } else {
                shared.resolve_or_create_group_id(active_name, None)
            };
            let user_id = if record.user_id > 0 {
                record.user_id
            } else {
                login.user_id
            };
            json!({
                "time": time_secs,
                "self_id": login.user_id,
                "post_type": "notice",
                "notice_type": "group_recall",
                "group_id": group_id,
                "user_id": user_id,
                "operator_id": login.user_id,
                "message_id": onebot_message_id,
            })
        }
        ChatTargetKind::Private => {
            let peer_uid = if record.is_self || record.user_id <= 0 {
                shared.resolve_or_create_friend_id(active_name)
            } else {
                record.user_id
            };
            json!({
                "time": time_secs,
                "self_id": login.user_id,
                "post_type": "notice",
                "notice_type": "friend_recall",
                "user_id": peer_uid,
                "message_id": onebot_message_id,
            })
        }
    }
}

fn video_bottom_filename_click_rect(bub_rect: [i32; 4]) -> [i32; 4] {
    let fx = (bub_rect[0] + 32)
        .min(bub_rect[2].saturating_sub(8))
        .max(bub_rect[0]);
    let fy = (bub_rect[3] - 18).max(bub_rect[1] + 8);
    [fx - 2, fy - 2, fx + 2, fy + 2]
}

fn probe_recall_menu_with_fallback(
    ctx: &WorkerContext,
    identity: &VerifiedWindowIdentity,
    win_hwnd: isize,
    pids: &[u32],
    bub_rect: [i32; 4],
) -> Result<Option<MatchedUiTarget>> {
    identity
        .verify_fast()
        .and_then(|()| click_rect_verified(identity, bub_rect, SimulatedClickKind::Right, false))?;
    thread::sleep(Duration::from_millis(240));
    if let Some(hit) =
        find_popup_clickable_by_keywords(&ctx.session, win_hwnd, pids, &["撤回"], true)
            .filter(is_exact_recall_menu_item)
    {
        return Ok(Some(hit));
    }
    dismiss_popups_best_effort(identity);

    // 高度 >= 120px 的视频卡片中心可能命中播放按钮，回退到底部文件名区再次右键探测撤回菜单
    if bub_rect[3] - bub_rect[1] >= 120 {
        thread::sleep(Duration::from_millis(120));
        let fallback_rect = video_bottom_filename_click_rect(bub_rect);
        identity.verify_fast().and_then(|()| {
            click_rect_verified(identity, fallback_rect, SimulatedClickKind::Right, false)
        })?;
        thread::sleep(Duration::from_millis(240));
        if let Some(hit) =
            find_popup_clickable_by_keywords(&ctx.session, win_hwnd, pids, &["撤回"], true)
                .filter(is_exact_recall_menu_item)
        {
            return Ok(Some(hit));
        }
        dismiss_popups_best_effort(identity);
    }

    Ok(None)
}

/// 在真实 QQNT 窗口中定位消息气泡、通过右键“撤回”菜单撤回并同步清理数据库与广播事件
pub(crate) fn handle_delete_message(
    ctx: &mut WorkerContext,
    shared: &Arc<SharedInner>,
    onebot_message_id: i64,
) -> Result<()> {
    if onebot_message_id <= 0 {
        return Err(AdapterError::InputRejected(
            "message_id must be a positive integer".to_string(),
        ));
    }

    let record = shared
        .storage
        .get_message_by_id(onebot_message_id)?
        .ok_or_else(|| {
            AdapterError::InputRejected(format!("message_id={onebot_message_id} not found"))
        })?;

    let target_kind = record.chat_type;

    let pids = refresh_pids_and_check_security(shared)?;
    if target_kind == ChatTargetKind::Private {
        verify_private_whitelist(shared, &record.group_name)?;
    }

    let _fg_guard = ForegroundRestoreGuard::capture(&pids);
    let (win, active_name) = attach_chat_window(ctx, shared, &record.group_name, target_kind)?;
    let (identity, _live_cnt) =
        verify_target_guard_in_tree(ctx, shared, &win, &active_name, target_kind)?;

    if is_multiselect_bar_active(&ctx.session, win.hwnd, &pids).is_some() {
        exit_multiselect_cleanly(
            &ctx.session,
            &identity,
            win.hwnd,
            &pids,
            shared.allow_mock(),
        );
    }

    let located = locate_recall_bubble(ctx, &identity, &win, &active_name, &record)?;
    let recall_hit = match probe_recall_menu_with_fallback(
        ctx,
        &identity,
        win.hwnd,
        &pids,
        located.bub_rect,
    ) {
        Ok(Some(hit)) => hit,
        Ok(None) => {
            if let (true, Some(pt)) = (located.scroll_up_steps > 0, located.scroll_point) {
                restore_message_list_to_bottom(&identity, pt, located.scroll_up_steps);
            }
            return Err(AdapterError::UnsupportedAction {
                action: "delete_msg".to_string(),
                reason: format!(
                    "QQNT context menu for message_id={onebot_message_id} does not expose '撤回' (message may exceed 2-minute recall window or lack permission)"
                ),
            });
        }
        Err(err) => {
            if let (true, Some(pt)) = (located.scroll_up_steps > 0, located.scroll_point) {
                restore_message_list_to_bottom(&identity, pt, located.scroll_up_steps);
            }
            return Err(err);
        }
    };

    if let Err(err) = click_matched_target_verified(
        &identity,
        &recall_hit,
        SimulatedClickKind::Left,
        shared.allow_mock(),
    ) {
        dismiss_popups_best_effort(&identity);
        if let (true, Some(pt)) = (located.scroll_up_steps > 0, located.scroll_point) {
            restore_message_list_to_bottom(&identity, pt, located.scroll_up_steps);
        }
        return Err(err);
    }

    thread::sleep(Duration::from_millis(200));
    if let Err(err) = confirm_recall_if_modal_open(ctx, win.hwnd, &pids, shared.allow_mock()) {
        dismiss_popups_best_effort(&identity);
        if let (true, Some(pt)) = (located.scroll_up_steps > 0, located.scroll_point) {
            restore_message_list_to_bottom(&identity, pt, located.scroll_up_steps);
        }
        return Err(err);
    }

    let final_notices =
        wait_and_confirm_recall_effect(ctx, &identity, &win, &active_name, &record, &located)?;

    if !final_notices.is_empty() {
        shared.tracker.lock().seed_notice_baseline(&final_notices);
    }

    let _ = shared.storage.delete_message_by_id(onebot_message_id)?;
    let _ = shared.storage.append_audit(
        "message_recalled",
        &json!({
            "message_id": onebot_message_id,
            "chat_type": record.chat_type,
            "target": active_name,
        })
        .to_string(),
    );

    let login = shared.resolve_login_info();
    let notice_payload =
        build_recall_notice_payload(shared, &record, &active_name, target_kind, &login);
    shared.emit_event(AdapterEvent::Notice(notice_payload));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use common_types::AdapterConfig;
    use storage::{Storage, UpsertFriendInput};
    use tokio::sync::broadcast;

    #[test]
    fn exact_recall_menu_filter_rejects_delete_and_notice_text() {
        let recall_menu = MatchedUiTarget {
            hwnd: 1,
            name: "撤回".to_string(),
            control_type: "MenuItem".to_string(),
            automation_id: "menu_recall".to_string(),
            rect: [10, 10, 80, 38],
        };
        assert!(is_exact_recall_menu_item(&recall_menu));

        let delete_menu = MatchedUiTarget {
            name: "删除".to_string(),
            ..recall_menu.clone()
        };
        assert!(!is_exact_recall_menu_item(&delete_menu));

        let notice_text = MatchedUiTarget {
            name: "撤回".to_string(),
            control_type: "Text".to_string(),
            ..recall_menu
        };
        assert!(!is_exact_recall_menu_item(&notice_text));
    }

    #[test]
    fn build_recall_notice_payload_formats_group_and_friend_recall() {
        let storage = Arc::new(Storage::open(":memory:").expect("mem storage"));
        storage
            .upsert_friend(&UpsertFriendInput {
                real_user_id: Some(200000002),
                nickname: "测试好友A".to_string(),
                remark: String::new(),
                last_preview: None,
            })
            .expect("upsert friend");

        let mut cfg = AdapterConfig::default();
        cfg.bot.self_id = 200000003;
        let (tx, _) = broadcast::channel(16);
        let shared = Arc::new(SharedInner::new(cfg, storage, tx));
        let login = shared.resolve_login_info();

        let group_rec = StoredMessageRecord {
            onebot_message_id: 101,
            group_name: "Demo测试群".to_string(),
            group_id: 100000001,
            user_id: 200000003,
            is_self: true,
            chat_type: ChatTargetKind::Group,
            ..Default::default()
        };
        let group_notice = build_recall_notice_payload(
            &shared,
            &group_rec,
            "Demo测试群",
            ChatTargetKind::Group,
            &login,
        );
        assert_eq!(group_notice["notice_type"], "group_recall");
        assert_eq!(group_notice["group_id"], 100000001);
        assert_eq!(group_notice["user_id"], 200_000_003_i64);
        assert_eq!(group_notice["operator_id"], 200_000_003_i64);
        assert_eq!(group_notice["message_id"], 101);

        let priv_rec = StoredMessageRecord {
            onebot_message_id: 102,
            group_name: "测试好友A".to_string(),
            user_id: 200000003,
            is_self: true,
            chat_type: ChatTargetKind::Private,
            ..Default::default()
        };
        let priv_notice = build_recall_notice_payload(
            &shared,
            &priv_rec,
            "测试好友A",
            ChatTargetKind::Private,
            &login,
        );
        assert_eq!(priv_notice["notice_type"], "friend_recall");
        assert_eq!(priv_notice["user_id"], 200000002);
        assert_eq!(priv_notice["message_id"], 102);
    }

    #[test]
    fn find_bubble_for_record_locates_file_and_video_bubbles_and_bottom_fallback_anchor() {
        use common_types::MessageSegment;

        let bubbles = vec![
            VisibleBubbleEntry {
                sender: "测试机器人".to_string(),
                content: "proto_v4_live_verify.txt\n70.00\u{2009}B   已发送".to_string(),
                bubble_rect: [900, 300, 1180, 370],
                avatar_rect: None,
                is_self: true,
            },
            VisibleBubbleEntry {
                sender: "测试机器人".to_string(),
                content: "[图片]\nproto_v4_live_video.mp4\n7.69\u{2009}KB 已发送\n[图片]"
                    .to_string(),
                bubble_rect: [900, 390, 1180, 560],
                avatar_rect: None,
                is_self: true,
            },
        ];

        let file_rec = StoredMessageRecord {
            onebot_message_id: 10166,
            sender_name: "测试机器人".to_string(),
            raw_message: "[文件:proto_v4_live_verify.txt]".to_string(),
            segments: vec![MessageSegment::File {
                file: ".runtime/staging_uploads/abcd/proto_v4_live_verify.txt".to_string(),
                name: Some("proto_v4_live_verify.txt".to_string()),
                size: Some(70),
            }],
            ..Default::default()
        };
        assert_eq!(
            find_bubble_for_record(&bubbles, &file_rec, &[]),
            Some([900, 300, 1180, 370])
        );

        let video_rec = StoredMessageRecord {
            onebot_message_id: 10168,
            sender_name: "测试机器人".to_string(),
            raw_message: "[视频]".to_string(),
            segments: vec![MessageSegment::Video {
                file: "F:\\tmp\\proto_v4_live_video.mp4".to_string(),
                url: None,
            }],
            ..Default::default()
        };
        assert_eq!(
            find_bubble_for_record(&bubbles, &video_rec, &[]),
            Some([900, 390, 1180, 560])
        );

        let fallback = video_bottom_filename_click_rect([900, 390, 1180, 560]);
        assert_eq!(fallback, [930, 540, 934, 544]);
    }

    #[test]
    fn find_bubble_for_record_never_matches_peer_bubble_for_self_record() {
        let peer_bubble = VisibleBubbleEntry {
            sender: "群友A".to_string(),
            content: "同文碰撞".to_string(),
            is_self: false,
            bubble_rect: [500, 200, 780, 250],
            avatar_rect: None,
        };
        let self_bubble = VisibleBubbleEntry {
            sender: "BotSelf".to_string(),
            content: "同文碰撞".to_string(),
            is_self: true,
            bubble_rect: [900, 280, 1180, 330],
            avatar_rect: None,
        };

        let self_rec = StoredMessageRecord {
            onebot_message_id: 20201,
            is_self: true,
            sender_name: "BotSelf".to_string(),
            raw_message: "同文碰撞".to_string(),
            ..Default::default()
        };
        // 即使群友同文气泡排在前面且内容完全一致，自发撤回也必须锁定己方气泡
        assert_eq!(
            find_bubble_for_record(&[peer_bubble.clone(), self_bubble.clone()], &self_rec, &[]),
            Some([900, 280, 1180, 330])
        );
        // 当仅群友气泡可见时，管理员账号严禁误点撤回他人消息气泡
        assert_eq!(
            find_bubble_for_record(std::slice::from_ref(&peer_bubble), &self_rec, &[]),
            None
        );

        let peer_rec = StoredMessageRecord {
            onebot_message_id: 20202,
            is_self: false,
            sender_name: "群友A".to_string(),
            raw_message: "同文碰撞".to_string(),
            ..Default::default()
        };
        assert_eq!(
            find_bubble_for_record(&[self_bubble, peer_bubble], &peer_rec, &[]),
            Some([500, 200, 780, 250])
        );
    }
}
