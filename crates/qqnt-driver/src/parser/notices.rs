//! QQNT 系统提示横幅（撤回、拍一拍、进退群、禁言等）识别与 OneBot notice 事件转换

use std::collections::HashMap;

use common_types::constants::DEFAULT_ONEBOT_ID_BOUND;
use common_types::{stable_onebot_id, ChatTargetKind, GroupMemberInfo, UiNodeSnapshot};
use serde_json::{json, Value};
use uia_core::constants::CONTROL_TYPE_IMAGE;

use super::compute_message_id;
use super::nested_msaa::{
    collect_container_message_nodes, count_list_items, find_history_containers,
    is_named_message_history_container,
};
use super::row_cleaner::{
    canonical_group_name, collect_leaf_fragments, find_avatar_in_row,
    is_qqnt_anti_fraud_or_noise_text, is_qqnt_system_notice_text, is_timestamp_like,
};
use crate::profile;

/// 从 UIA 消息流中提取的单条系统提示横幅条目
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedSystemNotice {
    pub notice_id: String,
    pub text: String,
}

/// 将系统提示文案映射到 OneBot notice 事件所需的会话上下文
pub struct NoticeContext<'a> {
    pub self_id: i64,
    pub chat_kind: ChatTargetKind,
    pub session_name: &'a str,
    pub group_id: Option<i64>,
    pub peer_user_id: Option<i64>,
    pub members: &'a [GroupMemberInfo],
}

fn clean_actor_name(raw: &str) -> &str {
    let trimmed = raw
        .trim()
        .trim_matches(|ch: char| {
            matches!(
                ch,
                '"' | '\'' | '“' | '”' | '‘' | '’' | '「' | '」' | ':' | '：'
            )
        })
        .trim();
    trimmed
}

fn resolve_actor_id(ctx: &NoticeContext<'_>, raw_actor: &str) -> i64 {
    let actor = clean_actor_name(raw_actor);
    if actor.is_empty() || actor == "你" || actor == "我" {
        return ctx.self_id;
    }
    let canon_session = canonical_group_name(ctx.session_name);
    if ctx.chat_kind == ChatTargetKind::Private && (actor == "对方" || actor == canon_session) {
        if let Some(peer) = ctx.peer_user_id {
            return peer;
        }
    }
    if let Some(member) = ctx
        .members
        .iter()
        .find(|item| item.nickname == actor || (!item.card.is_empty() && item.card == actor))
    {
        return member.real_user_id.unwrap_or(member.user_id);
    }
    if ctx.chat_kind == ChatTargetKind::Private {
        if let Some(peer) = ctx.peer_user_id {
            return peer;
        }
    }
    let seed = if canon_session.is_empty() {
        format!("member:{actor}")
    } else {
        format!("member:{canon_session}:{actor}")
    };
    stable_onebot_id(&seed, DEFAULT_ONEBOT_ID_BOUND)
}

fn is_supported_notice_banner(text: &str) -> bool {
    is_qqnt_system_notice_text(text)
        || text.contains("被禁言")
        || text.contains("解除了禁言")
        || text.contains("全员禁言")
        || text.contains("管理员")
        || text.contains("精华")
        || text.contains("成为好友")
        || text.contains("已是好友")
        || text.contains("添加你为好友")
        || text.contains("手气王")
        || text.contains("运气王")
        || text.contains("回应了你的")
}

fn collect_candidate_banner_texts(candidate_nodes: &[&UiNodeSnapshot]) -> Vec<String> {
    let mut rows: Vec<String> = Vec::new();
    let mut pending_flat: Option<([i32; 4], Vec<String>)> = None;

    for node in candidate_nodes {
        if node.is_offscreen || profile::is_system_chrome(node) {
            continue;
        }
        let is_flat_inline_leaf = node.children.is_empty()
            && !node
                .control_type
                .trim()
                .eq_ignore_ascii_case(CONTROL_TYPE_IMAGE)
            && profile::rect_is_valid(node.rect)
            && profile::rect_height(node.rect) <= 34;
        if is_flat_inline_leaf {
            let mut frags = Vec::new();
            collect_leaf_fragments(node, &mut frags);
            let clean_frags: Vec<String> = frags
                .into_iter()
                .map(|part| part.trim().to_string())
                .filter(|part| !part.is_empty() && !is_timestamp_like(part))
                .collect();
            if clean_frags.is_empty() {
                continue;
            }
            let [x2, y2, r2, b2] = node.rect;
            let same_band = pending_flat
                .as_ref()
                .is_some_and(|([_px1, py1, pr1, pb1], _)| {
                    (py1 - y2).abs() <= 8 && (pb1 - b2).abs() <= 8 && x2 >= pr1 - 8
                });
            if same_band {
                if let Some((rect, buf)) = pending_flat.as_mut() {
                    *rect = [
                        rect[0].min(x2),
                        rect[1].min(y2),
                        rect[2].max(r2),
                        rect[3].max(b2),
                    ];
                    buf.extend(clean_frags);
                }
            } else {
                if let Some((_rect, buf)) = pending_flat.take() {
                    rows.push(buf.join(" "));
                }
                pending_flat = Some((node.rect, clean_frags));
            }
            continue;
        }

        if let Some((_rect, buf)) = pending_flat.take() {
            rows.push(buf.join(" "));
        }
        // 带发送者头像的常规气泡 row 可能在正文中引用通知词，仅识别无头像或薄型系统提示横幅行
        let is_thin_banner =
            profile::rect_is_valid(node.rect) && profile::rect_height(node.rect) <= 34;
        if !is_thin_banner && find_avatar_in_row(node, 2).is_some() {
            continue;
        }
        let mut fragments = Vec::new();
        collect_leaf_fragments(node, &mut fragments);
        let text = fragments
            .into_iter()
            .map(|part| part.trim().to_string())
            .filter(|part| !part.is_empty() && !is_timestamp_like(part))
            .collect::<Vec<_>>()
            .join(" ");
        if !text.is_empty() {
            rows.push(text);
        }
    }

    if let Some((_rect, buf)) = pending_flat {
        rows.push(buf.join(" "));
    }
    rows
}

/// 从 QQNT 窗口 UI 快照树抽取非气泡形态的系统提示横幅（忽略包含头像与普通气泡的正文讨论）
pub fn extract_system_notices_from_tree(
    root: &UiNodeSnapshot,
    win_rect: [i32; 4],
    is_standalone: bool,
    session_name: &str,
) -> Vec<ParsedSystemNotice> {
    let eff_rect = profile::effective_window_rect_from_tree(win_rect, root);
    let mut containers = Vec::new();
    find_history_containers(root, eff_rect, is_standalone, &mut containers);

    let mut candidate_nodes = Vec::new();
    if let Some(best_container) = containers.into_iter().max_by_key(|candidate| {
        (
            usize::from(is_named_message_history_container(candidate)),
            count_list_items(candidate),
            candidate.children.len(),
        )
    }) {
        collect_container_message_nodes(best_container, &mut candidate_nodes);
    }
    if candidate_nodes.is_empty() {
        return Vec::new();
    }

    let mut occurrences: HashMap<String, usize> = HashMap::new();
    let mut notices = Vec::new();

    for text in collect_candidate_banner_texts(&candidate_nodes) {
        let trimmed = text.trim();
        if trimmed.is_empty()
            || is_qqnt_anti_fraud_or_noise_text(trimmed)
            || !is_supported_notice_banner(trimmed)
        {
            continue;
        }
        let occ = {
            let count = occurrences.entry(trimmed.to_string()).or_insert(0);
            *count += 1;
            *count
        };
        let notice_id = compute_message_id(session_name, "__system_notice__", None, trimmed, occ);
        notices.push(ParsedSystemNotice {
            notice_id,
            text: trimmed.to_string(),
        });
    }

    notices
}

fn parse_ban_duration_secs(text: &str) -> i64 {
    let digits: String = text
        .chars()
        .skip_while(|ch| !ch.is_ascii_digit())
        .take_while(|ch| ch.is_ascii_digit())
        .collect();
    let Ok(val) = digits.parse::<i64>() else {
        return 0;
    };
    if text.contains('天') {
        val.saturating_mul(86_400)
    } else if text.contains("小时") || text.contains('时') {
        val.saturating_mul(3_600)
    } else if text.contains("分钟") || text.contains('分') {
        val.saturating_mul(60)
    } else {
        val
    }
}

fn resolved_group_id(ctx: &NoticeContext<'_>) -> i64 {
    ctx.group_id.unwrap_or_else(|| {
        let canon = canonical_group_name(ctx.session_name);
        let seed = if canon.is_empty() {
            ctx.session_name
        } else {
            canon.as_str()
        };
        stable_onebot_id(seed, DEFAULT_ONEBOT_ID_BOUND)
    })
}

fn clean_poke_target(raw_tail: &str) -> &str {
    let without_end = raw_tail.trim().trim_end_matches(['。', '.', '!', '！']);
    let head = without_end
        .split_once('的')
        .map(|(prefix, _)| prefix)
        .unwrap_or(without_end);
    clean_actor_name(head)
}

type NoticeClassifierFn = fn(&NoticeContext<'_>, &str, i64) -> Option<Value>;

const NOTICE_CLASSIFIER_CHAIN: &[NoticeClassifierFn] = &[
    classify_recall_notice,
    classify_emoji_like_notice,
    classify_poke_notice,
    classify_group_join_notice,
    classify_group_kick_notice,
    classify_group_leave_notice,
    classify_group_ban_notice,
    classify_group_admin_notice,
    classify_essence_notice,
    classify_friend_add_notice,
    classify_lucky_king_notice,
];

fn classify_recall_notice(ctx: &NoticeContext<'_>, compact: &str, time_secs: i64) -> Option<Value> {
    let (actor_raw, _) = compact.split_once("撤回了一条消息")?;
    let user_id = resolve_actor_id(ctx, actor_raw);
    Some(match ctx.chat_kind {
        ChatTargetKind::Group => json!({
            "time": time_secs,
            "self_id": ctx.self_id,
            "post_type": "notice",
            "notice_type": "group_recall",
            "group_id": resolved_group_id(ctx),
            "user_id": user_id,
            "operator_id": user_id,
            "message_id": 0,
            "raw_notice": compact,
        }),
        ChatTargetKind::Private => json!({
            "time": time_secs,
            "self_id": ctx.self_id,
            "post_type": "notice",
            "notice_type": "friend_recall",
            "user_id": user_id,
            "message_id": 0,
            "raw_notice": compact,
        }),
    })
}

fn classify_poke_notice(ctx: &NoticeContext<'_>, compact: &str, time_secs: i64) -> Option<Value> {
    let (sender_raw, target_tail) = compact
        .split_once("拍了拍")
        .or_else(|| compact.split_once("戳了戳"))?;
    let sender_id = resolve_actor_id(ctx, sender_raw);
    let target_clean = clean_poke_target(target_tail);
    let target_id = if target_clean.starts_with("自己") {
        sender_id
    } else {
        resolve_actor_id(ctx, target_clean)
    };
    let mut payload = json!({
        "time": time_secs,
        "self_id": ctx.self_id,
        "post_type": "notice",
        "notice_type": "notify",
        "sub_type": "poke",
        "user_id": target_id,
        "sender_id": sender_id,
        "target_id": target_id,
        "raw_notice": compact,
    });
    if ctx.chat_kind == ChatTargetKind::Group {
        if let Some(obj) = payload.as_object_mut() {
            obj.insert("group_id".to_string(), json!(resolved_group_id(ctx)));
        }
    }
    Some(payload)
}

fn classify_group_join_notice(
    ctx: &NoticeContext<'_>,
    compact: &str,
    time_secs: i64,
) -> Option<Value> {
    let (before_join, _) = compact.split_once("加入了群聊")?;
    let (sub_type, operator_id, user_id) =
        if let Some((inviter, invitee)) = before_join.split_once("邀请") {
            (
                "invite",
                resolve_actor_id(ctx, inviter),
                resolve_actor_id(ctx, invitee),
            )
        } else {
            ("approve", 0, resolve_actor_id(ctx, before_join))
        };
    Some(json!({
        "time": time_secs,
        "self_id": ctx.self_id,
        "post_type": "notice",
        "notice_type": "group_increase",
        "sub_type": sub_type,
        "group_id": resolved_group_id(ctx),
        "operator_id": operator_id,
        "user_id": user_id,
        "raw_notice": compact,
    }))
}

fn classify_group_kick_notice(
    ctx: &NoticeContext<'_>,
    compact: &str,
    time_secs: i64,
) -> Option<Value> {
    let (before_kick, _) = compact.split_once("移出了群聊")?;
    let (operator_id, user_id) = if let Some((op, victim)) = before_kick.split_once("将") {
        (resolve_actor_id(ctx, op), resolve_actor_id(ctx, victim))
    } else if let Some((victim, op)) = before_kick.split_once("被") {
        (resolve_actor_id(ctx, op), resolve_actor_id(ctx, victim))
    } else {
        (0, resolve_actor_id(ctx, before_kick))
    };
    Some(json!({
        "time": time_secs,
        "self_id": ctx.self_id,
        "post_type": "notice",
        "notice_type": "group_decrease",
        "sub_type": "kick",
        "group_id": resolved_group_id(ctx),
        "operator_id": operator_id,
        "user_id": user_id,
        "raw_notice": compact,
    }))
}

fn classify_group_leave_notice(
    ctx: &NoticeContext<'_>,
    compact: &str,
    time_secs: i64,
) -> Option<Value> {
    let (leaver, _) = compact.split_once("退出了群聊")?;
    let user_id = resolve_actor_id(ctx, leaver);
    Some(json!({
        "time": time_secs,
        "self_id": ctx.self_id,
        "post_type": "notice",
        "notice_type": "group_decrease",
        "sub_type": "leave",
        "group_id": resolved_group_id(ctx),
        "operator_id": user_id,
        "user_id": user_id,
        "raw_notice": compact,
    }))
}

fn resolve_optional_actor_id(ctx: &NoticeContext<'_>, raw_actor: &str) -> i64 {
    if clean_actor_name(raw_actor).is_empty() {
        0
    } else {
        resolve_actor_id(ctx, raw_actor)
    }
}

fn classify_emoji_like_notice(
    ctx: &NoticeContext<'_>,
    compact: &str,
    time_secs: i64,
) -> Option<Value> {
    let (actor_raw, _) = compact.split_once("回应了你的")?;
    let user_id = resolve_actor_id(ctx, actor_raw);
    Some(json!({
        "time": time_secs,
        "self_id": ctx.self_id,
        "post_type": "notice",
        "notice_type": "group_msg_emoji_like",
        "group_id": resolved_group_id(ctx),
        "user_id": user_id,
        "message_id": 0,
        "likes": [{
            "emoji_id": "0",
            "count": 1,
        }],
        "raw_notice": compact,
    }))
}

fn classify_group_ban_notice(
    ctx: &NoticeContext<'_>,
    compact: &str,
    time_secs: i64,
) -> Option<Value> {
    if let Some((left, _)) = compact.split_once("开启了全员禁言") {
        return Some(json!({
            "time": time_secs,
            "self_id": ctx.self_id,
            "post_type": "notice",
            "notice_type": "group_ban",
            "sub_type": "ban",
            "group_id": resolved_group_id(ctx),
            "operator_id": resolve_optional_actor_id(ctx, left),
            "user_id": 0,
            "duration": -1,
            "raw_notice": compact,
        }));
    }
    if let Some((left, _)) = compact.split_once("关闭了全员禁言") {
        return Some(json!({
            "time": time_secs,
            "self_id": ctx.self_id,
            "post_type": "notice",
            "notice_type": "group_ban",
            "sub_type": "lift_ban",
            "group_id": resolved_group_id(ctx),
            "operator_id": resolve_optional_actor_id(ctx, left),
            "user_id": 0,
            "duration": 0,
            "raw_notice": compact,
        }));
    }
    if !compact.contains("被禁言") && !compact.contains("解除了禁言") {
        return None;
    }
    let (sub_type, user_raw, duration) = if let Some((left, _)) = compact.split_once("解除了禁言")
    {
        ("lift_ban", left.trim_end_matches('被').trim(), 0)
    } else if let Some((left, right)) = compact.split_once("被禁言") {
        ("ban", left, parse_ban_duration_secs(right))
    } else {
        ("ban", "", 0)
    };
    Some(json!({
        "time": time_secs,
        "self_id": ctx.self_id,
        "post_type": "notice",
        "notice_type": "group_ban",
        "sub_type": sub_type,
        "group_id": resolved_group_id(ctx),
        "operator_id": 0,
        "user_id": resolve_actor_id(ctx, user_raw),
        "duration": duration,
        "raw_notice": compact,
    }))
}

fn classify_group_admin_notice(
    ctx: &NoticeContext<'_>,
    compact: &str,
    time_secs: i64,
) -> Option<Value> {
    let (sub_type, left) = [
        "已被取消管理员",
        "被取消管理员",
        "取消了管理员",
        "取消管理员",
    ]
    .iter()
    .find_map(|pat| compact.split_once(pat).map(|(l, _)| ("unset", l)))
    .or_else(|| {
        [
            "已被设为管理员",
            "被设为管理员",
            "已被设置为管理员",
            "被设置为管理员",
            "设为管理员",
            "设置为管理员",
        ]
        .iter()
        .find_map(|pat| compact.split_once(pat).map(|(l, _)| ("set", l)))
    })?;
    let stripped = left
        .rsplit_once('将')
        .or_else(|| left.rsplit_once('把'))
        .map(|(_, target)| target)
        .unwrap_or(left)
        .trim()
        .trim_end_matches(['已', '被'])
        .trim();
    Some(json!({
        "time": time_secs,
        "self_id": ctx.self_id,
        "post_type": "notice",
        "notice_type": "group_admin",
        "sub_type": sub_type,
        "group_id": resolved_group_id(ctx),
        "user_id": resolve_optional_actor_id(ctx, stripped),
        "raw_notice": compact,
    }))
}

fn classify_essence_notice(
    ctx: &NoticeContext<'_>,
    compact: &str,
    time_secs: i64,
) -> Option<Value> {
    let (sub_type, left) = [
        "移出了精华消息",
        "移出精华消息",
        "取消了精华消息",
        "取消精华消息",
        "取消群精华",
        "移出群精华",
    ]
    .iter()
    .find_map(|pat| compact.split_once(pat).map(|(l, _)| ("delete", l)))
    .or_else(|| {
        ["设为精华消息", "设置为精华消息", "设为群精华", "设置群精华"]
            .iter()
            .find_map(|pat| compact.split_once(pat).map(|(l, _)| ("add", l)))
    })?;

    let trimmed = left.trim();
    let (operator_id, sender_id) = if let Some((before, op)) = trimmed.split_once("的消息被") {
        let sender_raw = before.trim();
        let s_id = if sender_raw.contains('你') {
            ctx.self_id
        } else {
            resolve_optional_actor_id(ctx, sender_raw)
        };
        (resolve_optional_actor_id(ctx, op), s_id)
    } else if let Some((op_raw, target_tail)) = trimmed.split_once('将') {
        let op_id = resolve_optional_actor_id(ctx, op_raw);
        let sender_part = target_tail
            .split_once("的消息")
            .map(|(s, _)| s.trim())
            .unwrap_or("");
        let clean_sender = clean_actor_name(sender_part);
        let s_id = if clean_sender.is_empty() || matches!(clean_sender, "一条" | "该" | "这条")
        {
            0
        } else {
            resolve_actor_id(ctx, clean_sender)
        };
        (op_id, s_id)
    } else {
        let op_raw = trimmed.trim_end_matches(['已', '被']).trim();
        (resolve_optional_actor_id(ctx, op_raw), 0)
    };

    Some(json!({
        "time": time_secs,
        "self_id": ctx.self_id,
        "post_type": "notice",
        "notice_type": "essence",
        "sub_type": sub_type,
        "group_id": resolved_group_id(ctx),
        "sender_id": sender_id,
        "operator_id": operator_id,
        "message_id": 0,
        "raw_notice": compact,
    }))
}

fn classify_friend_add_notice(
    ctx: &NoticeContext<'_>,
    compact: &str,
    time_secs: i64,
) -> Option<Value> {
    let raw_left = if let Some((left, _)) = compact.split_once("已添加你为好友") {
        left
    } else if let Some((left, _)) = compact
        .split_once("已经成为好友")
        .or_else(|| compact.split_once("已经是好友"))
    {
        left
    } else {
        return None;
    };
    let mut actor = clean_actor_name(raw_left);
    for prefix in ["你与", "你和", "你们"] {
        if let Some(rest) = actor.strip_prefix(prefix) {
            actor = rest.trim();
        }
    }
    for suffix in ["与你", "和你"] {
        if let Some(rest) = actor.strip_suffix(suffix) {
            actor = rest.trim();
        }
    }
    let user_id = if actor.is_empty() {
        ctx.peer_user_id.unwrap_or(0)
    } else {
        resolve_actor_id(ctx, actor)
    };
    Some(json!({
        "time": time_secs,
        "self_id": ctx.self_id,
        "post_type": "notice",
        "notice_type": "friend_add",
        "user_id": user_id,
        "raw_notice": compact,
    }))
}

fn classify_lucky_king_notice(
    ctx: &NoticeContext<'_>,
    compact: &str,
    time_secs: i64,
) -> Option<Value> {
    if !compact.contains("手气王") && !compact.contains("运气王") {
        return None;
    }
    let winner_raw = ["领取", "领了", "抢了", "获得", "成为", "是"]
        .iter()
        .find_map(|sep| compact.split_once(sep).map(|(l, _)| l.trim()))
        .unwrap_or("");
    let target_id = resolve_optional_actor_id(ctx, winner_raw);
    let sender_id = if compact.contains("你的红包") || compact.contains("你发的红包") {
        ctx.self_id
    } else if let Some((before_hb, _)) = compact.split_once("的红包") {
        let owner_raw = before_hb
            .rsplit_once('在')
            .or_else(|| before_hb.rsplit_once("领了"))
            .or_else(|| before_hb.rsplit_once("领取了"))
            .map(|(_, right)| right.trim())
            .unwrap_or("");
        resolve_optional_actor_id(ctx, owner_raw)
    } else {
        0
    };
    Some(json!({
        "time": time_secs,
        "self_id": ctx.self_id,
        "post_type": "notice",
        "notice_type": "notify",
        "sub_type": "lucky_king",
        "group_id": resolved_group_id(ctx),
        "user_id": sender_id,
        "target_id": target_id,
        "raw_notice": compact,
    }))
}

fn build_system_tip_fallback(ctx: &NoticeContext<'_>, compact: &str, time_secs: i64) -> Value {
    let mut fallback = json!({
        "time": time_secs,
        "self_id": ctx.self_id,
        "post_type": "notice",
        "notice_type": "notify",
        "sub_type": "system_tip",
        "user_id": ctx.peer_user_id.unwrap_or(ctx.self_id),
        "raw_notice": compact,
    });
    if ctx.chat_kind == ChatTargetKind::Group {
        if let Some(obj) = fallback.as_object_mut() {
            obj.insert("group_id".to_string(), json!(resolved_group_id(ctx)));
        }
    }
    fallback
}

/// 通过责任链管道将单条系统提示文案分类并转换为标准 OneBot v11 notice 事件 JSON
pub fn parse_notice_text_to_onebot_json(
    ctx: &NoticeContext<'_>,
    text: &str,
    time_secs: i64,
) -> Value {
    let compact = text.trim();
    NOTICE_CLASSIFIER_CHAIN
        .iter()
        .find_map(|classifier| classifier(ctx, compact, time_secs))
        .unwrap_or_else(|| build_system_tip_fallback(ctx, compact, time_secs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_emoji_like_admin_essence_friend_add_lucky_king_and_whole_group_ban() {
        let members = vec![
            GroupMemberInfo {
                group_id: 999888,
                user_id: 222001,
                real_user_id: Some(222001),
                nickname: "群友甲".to_string(),
                card: String::new(),
                role: "member".to_string(),
            },
            GroupMemberInfo {
                group_id: 999888,
                user_id: 222002,
                real_user_id: Some(222002),
                nickname: "群主乙".to_string(),
                card: String::new(),
                role: "owner".to_string(),
            },
        ];
        let ctx = NoticeContext {
            self_id: 10001,
            chat_kind: ChatTargetKind::Group,
            session_name: "Demo测试群",
            group_id: Some(999888),
            peer_user_id: None,
            members: &members,
        };

        let emoji_like = parse_notice_text_to_onebot_json(&ctx, "群友甲 回应了你的 消息:", 100);
        assert_eq!(emoji_like["notice_type"], "group_msg_emoji_like");
        assert_eq!(emoji_like["user_id"], 222001);
        assert_eq!(emoji_like["likes"][0]["emoji_id"], "0");
        assert_eq!(emoji_like["likes"][0]["count"], 1);

        let admin_set = parse_notice_text_to_onebot_json(&ctx, "群友甲 被设为管理员", 101);
        assert_eq!(admin_set["notice_type"], "group_admin");
        assert_eq!(admin_set["sub_type"], "set");
        assert_eq!(admin_set["user_id"], 222001);

        let admin_unset = parse_notice_text_to_onebot_json(&ctx, "群友甲 被取消管理员", 102);
        assert_eq!(admin_unset["notice_type"], "group_admin");
        assert_eq!(admin_unset["sub_type"], "unset");
        assert_eq!(admin_unset["user_id"], 222001);

        let ess_add =
            parse_notice_text_to_onebot_json(&ctx, "群主乙 将 群友甲 的消息设为精华消息", 103);
        assert_eq!(ess_add["notice_type"], "essence");
        assert_eq!(ess_add["sub_type"], "add");
        assert_eq!(ess_add["operator_id"], 222002);
        assert_eq!(ess_add["sender_id"], 222001);

        let ess_del =
            parse_notice_text_to_onebot_json(&ctx, "群主乙 将一条消息移出了精华消息", 104);
        assert_eq!(ess_del["notice_type"], "essence");
        assert_eq!(ess_del["sub_type"], "delete");
        assert_eq!(ess_del["operator_id"], 222002);

        let lucky =
            parse_notice_text_to_onebot_json(&ctx, "群友甲 领了 群主乙 的红包，获得了手气王", 105);
        assert_eq!(lucky["notice_type"], "notify");
        assert_eq!(lucky["sub_type"], "lucky_king");
        assert_eq!(lucky["target_id"], 222001);
        assert_eq!(lucky["user_id"], 222002);

        let ban_all = parse_notice_text_to_onebot_json(&ctx, "群主乙 开启了全员禁言", 106);
        assert_eq!(ban_all["notice_type"], "group_ban");
        assert_eq!(ban_all["sub_type"], "ban");
        assert_eq!(ban_all["user_id"], 0);
        assert_eq!(ban_all["duration"], -1);
        assert_eq!(ban_all["operator_id"], 222002);

        let lift_all = parse_notice_text_to_onebot_json(&ctx, "群主乙 关闭了全员禁言", 107);
        assert_eq!(lift_all["notice_type"], "group_ban");
        assert_eq!(lift_all["sub_type"], "lift_ban");
        assert_eq!(lift_all["user_id"], 0);
        assert_eq!(lift_all["duration"], 0);

        let priv_ctx = NoticeContext {
            self_id: 10001,
            chat_kind: ChatTargetKind::Private,
            session_name: "新好友",
            group_id: None,
            peer_user_id: Some(778899),
            members: &[],
        };
        let friend_add = parse_notice_text_to_onebot_json(
            &priv_ctx,
            "你们已经成为好友，现在可以开始聊天了",
            108,
        );
        assert_eq!(friend_add["notice_type"], "friend_add");
        assert_eq!(friend_add["user_id"], 778899);
    }
}
