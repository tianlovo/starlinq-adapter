//! 聊天历史提取、QQNT 控件树与 MockQQ 消息解析以及自账号昵称被动识别门面模块

mod flat_uia;
mod nested_msaa;
mod notice_enricher;
mod notices;
mod row_cleaner;
mod tracker;

#[cfg(test)]
mod replay_tests;
#[cfg(test)]
mod tests;

use std::collections::HashMap;

use chrono::Utc;
use common_types::constants::UI_LABEL_MESSAGE_LIST;
use common_types::{ChatMessage, UiNodeSnapshot};
use sha2::{Digest, Sha256};
use uia_core::constants::{CONTROL_TYPE_LIST, CONTROL_TYPE_TEXT};

use flat_uia::{is_flat_uia_message_list, parse_flat_uia_message_list};
use nested_msaa::{
    collect_container_message_nodes, count_list_items, find_history_containers,
    is_named_message_history_container, parse_message_node, unwrap_history_viewport,
};
use row_cleaner::strip_avatar_suffix;

pub use notice_enricher::{build_and_enrich_notice_events, enrich_notice_event};
pub use notices::{
    extract_system_notices_from_tree, parse_notice_text_to_onebot_json, NoticeContext,
    ParsedSystemNotice,
};
pub use row_cleaner::{canonical_group_name, normalize_message_text};
pub(crate) use row_cleaner::{extract_reply_preview, is_timestamp_like};
pub use tracker::{tracker_session_key, MessageTracker, OutboundStubBinding};

/// 单条已解析聊天消息及其关联的屏幕可见图片、气泡与头像坐标框列表（供富媒体裁切与桌面交互复用）
#[derive(Debug, Clone, Default)]
pub struct ParsedTreeMessage {
    pub message: ChatMessage,
    pub image_rects: Vec<[i32; 4]>,
    pub bubble_rect: Option<[i32; 4]>,
    pub avatar_rect: Option<[i32; 4]>,
}

/// 顶部栏登录昵称提取递归扫描参数与输出缓冲上下文
struct TopBarScanContext<'a> {
    left: f64,
    top: f64,
    width: f64,
    height: f64,
    avatar_hits: &'a mut Vec<([i32; 4], String)>,
    top_left_texts: &'a mut Vec<([i32; 4], String)>,
}

fn should_skip_top_bar_node(node: &UiNodeSnapshot) -> bool {
    node.is_offscreen
        || crate::profile::is_system_chrome(node)
        || crate::profile::is_excluded_sidebar_container(node)
        || node.name.trim() == UI_LABEL_MESSAGE_LIST
}

fn walk_top_bar(node: &UiNodeSnapshot, context: &mut TopBarScanContext<'_>) {
    if should_skip_top_bar_node(node) {
        return;
    }
    if crate::profile::rect_is_valid(node.rect) {
        let (center_x, center_y) = crate::profile::rect_center(node.rect);
        if center_y >= context.top
            && center_y < context.top + context.height * 0.14
            && center_x >= context.left
            && center_x < context.left + context.width * 0.35
        {
            if let Some(nick) = strip_avatar_suffix(&node.name) {
                context.avatar_hits.push((node.rect, nick.to_string()));
            } else if node
                .control_type
                .trim()
                .eq_ignore_ascii_case(CONTROL_TYPE_TEXT)
            {
                let text = node.name.trim();
                if !text.is_empty() {
                    context.top_left_texts.push((node.rect, text.to_string()));
                }
            }
        }
    }
    for child in &node.children {
        walk_top_bar(child, context);
    }
}

/// 从 QQNT 左上角顶栏（`<nick>的头像` 与相邻昵称文本）被动提取当前登录自账号昵称列表
pub fn extract_self_nicknames_from_tree(root: &UiNodeSnapshot, win_rect: [i32; 4]) -> Vec<String> {
    let eff_rect = crate::profile::effective_window_rect_from_tree(win_rect, root);
    if !crate::profile::rect_is_valid(eff_rect) {
        return Vec::new();
    }
    let left = eff_rect[0] as f64;
    let top = eff_rect[1] as f64;
    let width = crate::profile::rect_width(eff_rect) as f64;
    let height = crate::profile::rect_height(eff_rect) as f64;

    let mut avatar_hits: Vec<([i32; 4], String)> = Vec::new();
    let mut top_left_texts: Vec<([i32; 4], String)> = Vec::new();
    let mut context = TopBarScanContext {
        left,
        top,
        width,
        height,
        avatar_hits: &mut avatar_hits,
        top_left_texts: &mut top_left_texts,
    };
    walk_top_bar(root, &mut context);

    let mut nicks = Vec::new();
    for (av_rect, av_nick) in &avatar_hits {
        if !nicks.iter().any(|existing: &String| existing == av_nick) {
            nicks.push(av_nick.clone());
        }
        let (_, av_center_y) = crate::profile::rect_center(*av_rect);
        for (txt_rect, text) in &top_left_texts {
            let (_, txt_center_y) = crate::profile::rect_center(*txt_rect);
            if txt_rect[0] >= av_rect[2] - 4
                && txt_rect[0] <= av_rect[2] + 220
                && (txt_center_y - av_center_y).abs() <= 45.0
                && !text.starts_with("个性签名:")
                && !nicks.iter().any(|existing: &String| existing == text)
            {
                nicks.push(text.clone());
            }
        }
    }
    nicks
}

/// 为单条解析得到的消息生成稳定的 SHA-256 去重消息标识
pub fn compute_message_id(
    group_name: &str,
    sender: &str,
    timestamp_hint: Option<&str>,
    content: &str,
    occurrence: usize,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(canonical_group_name(group_name).as_bytes());
    hasher.update(b"\x1f");
    hasher.update(sender.trim().as_bytes());
    hasher.update(b"\x1f");
    hasher.update(timestamp_hint.unwrap_or("").trim().as_bytes());
    hasher.update(b"\x1f");
    hasher.update(content.trim().as_bytes());
    hasher.update(b"\x1f");
    hasher.update(occurrence.to_string().as_bytes());
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// 从 UIA 快照树的消息历史区域提取结构化群聊消息序列
/// 从 UI 快照树抽取所有聊天消息（群聊与私聊通用），返回基础消息模型列表
pub fn extract_messages_from_tree(
    root: &UiNodeSnapshot,
    win_rect: [i32; 4],
    is_standalone: bool,
    group_name: &str,
) -> Vec<ChatMessage> {
    extract_rich_messages_from_tree(root, win_rect, is_standalone, group_name)
        .into_iter()
        .map(|item| item.message)
        .collect()
}

/// 检查快照树右侧聊天区是否存在有效的消息列表历史容器（区分空列表与无聊天面板空白页）
pub fn has_message_list_container(
    root: &UiNodeSnapshot,
    win_rect: [i32; 4],
    is_standalone: bool,
) -> bool {
    let eff_rect = crate::profile::effective_window_rect_from_tree(win_rect, root);
    let mut containers = Vec::new();
    find_history_containers(root, eff_rect, is_standalone, &mut containers);
    !containers.is_empty()
}

/// 从 UI 快照树抽取所有聊天消息并附带气泡内可见图片坐标框集合
pub fn extract_rich_messages_from_tree(
    root: &UiNodeSnapshot,
    win_rect: [i32; 4],
    is_standalone: bool,
    group_name: &str,
) -> Vec<ParsedTreeMessage> {
    let eff_rect = crate::profile::effective_window_rect_from_tree(win_rect, root);
    let self_nicknames = extract_self_nicknames_from_tree(root, eff_rect);
    let mut containers = Vec::new();
    find_history_containers(root, eff_rect, is_standalone, &mut containers);

    let mut raw_messages = Vec::new();
    let mut candidate_nodes = Vec::new();
    let mut history_rect: Option<[i32; 4]> = None;

    if let Some(best_container) = containers.into_iter().max_by_key(|container| {
        let named_bonus = usize::from(is_named_message_history_container(container)) * 10_000;
        let item_count = count_list_items(container);
        let list_bonus = usize::from(
            container
                .control_type
                .trim()
                .eq_ignore_ascii_case(CONTROL_TYPE_LIST),
        ) * 1000;
        let area = crate::profile::rect_area(container.rect).max(0) as usize;
        (named_bonus, item_count, list_bonus, area)
    }) {
        history_rect = Some(best_container.rect);
        let container_w = crate::profile::rect_width(best_container.rect);
        let container_h = crate::profile::rect_height(best_container.rect);
        let rows_parent = unwrap_history_viewport(best_container, container_w, container_h);
        let is_ml_root = rows_parent
            .automation_id
            .trim()
            .eq_ignore_ascii_case("ml-root");
        if (is_named_message_history_container(best_container) || is_ml_root)
            && is_flat_uia_message_list(&rows_parent.children)
        {
            raw_messages =
                parse_flat_uia_message_list(&rows_parent.children, history_rect, container_w);
        } else {
            collect_container_message_nodes(best_container, &mut candidate_nodes);
        }
    }

    if raw_messages.is_empty() && !candidate_nodes.is_empty() {
        for node in candidate_nodes {
            if let Some(raw) = parse_message_node(node, history_rect) {
                raw_messages.push(raw);
            }
        }
    }

    let now_ms = Utc::now().timestamp_millis().max(0) as u64;
    let mut occurrence_map: HashMap<(String, Option<String>, String), usize> = HashMap::new();
    let mut messages = Vec::new();

    for raw in raw_messages {
        let key = (
            raw.sender.clone(),
            raw.timestamp_hint.clone(),
            raw.content.clone(),
        );
        let occurrence = occurrence_map.entry(key).or_insert(0);
        let message_id = compute_message_id(
            group_name,
            &raw.sender,
            raw.timestamp_hint.as_deref(),
            &raw.content,
            *occurrence,
        );
        *occurrence += 1;

        let is_self = raw.is_self_aligned
            || self_nicknames
                .iter()
                .any(|nick| nick.trim() == raw.sender.trim());

        messages.push(ParsedTreeMessage {
            message: ChatMessage {
                message_id,
                group_name: group_name.to_string(),
                sender: raw.sender,
                content: raw.content,
                timestamp_hint: raw.timestamp_hint,
                is_self,
                captured_at_ms: now_ms,
                ..Default::default()
            },
            image_rects: raw.image_rects,
            bubble_rect: raw.bubble_rect,
            avatar_rect: raw.avatar_rect,
        });
    }

    messages
}
