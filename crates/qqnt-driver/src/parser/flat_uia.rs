//! 原生 UIA 扁平兄弟节点流式消息列表解析器（头像节点分段状态机）

use common_types::constants::UI_NOISE_LEVEL_PREFIX;
use common_types::UiNodeSnapshot;
use uia_core::constants::{CONTROL_TYPE_GROUP, CONTROL_TYPE_TEXT};

use super::nested_msaa::is_scrollbar_wrapper;
use super::row_cleaner::{
    collect_all_leaf_fragments, collect_visible_image_rects, contains_badge_subtree,
    extract_sender_from_header_fragments, has_content_image_inner, is_badge_keyword,
    is_post_bubble_reaction_group, is_qqnt_anti_fraud_or_noise_text, is_qqnt_system_notice_text,
    is_reaction_counter_text, is_right_aligned_self, is_timestamp_like, strip_avatar_suffix,
    RawParsedMessage,
};
use crate::constants::REACTION_BOTTOM_TOLERANCE;
use crate::profile;

/// 扁平兄弟节点流式解析过程中暂存的当前消息状态机上下文
struct PendingFlatMessage {
    sender: String,
    avatar_rect: Option<[i32; 4]>,
    timestamp_hint: Option<String>,
    content_fragments: Vec<String>,
    has_image: bool,
    image_rects: Vec<[i32; 4]>,
    largest_bubble_rect: Option<[i32; 4]>,
    has_bubble: bool,
    expect_level_digits: bool,
}

fn extract_flat_avatar_name(node: &UiNodeSnapshot) -> Option<String> {
    if profile::is_system_chrome(node) {
        return None;
    }
    if !node
        .control_type
        .trim()
        .eq_ignore_ascii_case(CONTROL_TYPE_GROUP)
        || !node.children.is_empty()
    {
        return None;
    }
    let raw_name = node.name.trim();
    let name = strip_avatar_suffix(raw_name).unwrap_or(raw_name);
    if name.is_empty()
        || is_badge_keyword(name)
        || is_timestamp_like(name)
        || is_qqnt_anti_fraud_or_noise_text(name)
    {
        return None;
    }
    Some(name.to_string())
}

/// 识别扁平消息流中可见、无子节点、呈方形尺寸且带发送者名称的头像分组节点
pub(crate) fn is_flat_avatar_node(node: &UiNodeSnapshot) -> Option<String> {
    if node.is_offscreen {
        return None;
    }
    let name = extract_flat_avatar_name(node)?;
    let is_square_avatar = profile::rect_is_valid(node.rect) && {
        let width = profile::rect_width(node.rect);
        let height = profile::rect_height(node.rect);
        (20..=96).contains(&width) && (20..=96).contains(&height) && (width - height).abs() <= 18
    };
    is_square_avatar.then_some(name)
}

fn is_flat_uia_avatar_node(node: &UiNodeSnapshot) -> Option<String> {
    if let Some(name) = is_flat_avatar_node(node) {
        return Some(name);
    }
    // 屏外历史扁平消息头像常退化为空矩形，允许在无子节点具名 Group 上分段解析
    if node.is_offscreen {
        return extract_flat_avatar_name(node);
    }
    None
}

/// 检测容器直接子节点列表是否符合 QQNT 扁平 UIA 兄弟流排列特征
pub(crate) fn is_flat_uia_message_list(children: &[UiNodeSnapshot]) -> bool {
    if children.len() < 2 {
        return false;
    }
    let has_flat_avatar = children
        .iter()
        .any(|child| is_flat_avatar_node(child).is_some());
    let has_content_sibling = children.iter().any(|child| {
        if is_flat_avatar_node(child).is_some() || profile::is_system_chrome(child) {
            return false;
        }
        let control_type = child.control_type.trim();
        (control_type.eq_ignore_ascii_case(CONTROL_TYPE_GROUP)
            || control_type.eq_ignore_ascii_case(CONTROL_TYPE_TEXT))
            && (!child.name.trim().is_empty() || !child.children.is_empty())
    });
    has_flat_avatar && has_content_sibling
}

fn flush_flat_message(
    current: &mut Option<PendingFlatMessage>,
    history_rect: Option<[i32; 4]>,
    out: &mut Vec<RawParsedMessage>,
) {
    let Some(msg) = current.take() else {
        return;
    };
    let mut content = msg.content_fragments.join("\n").trim().to_string();
    if content.is_empty() && msg.has_image {
        content = "[图片]".to_string();
    }
    if content.is_empty()
        || is_qqnt_anti_fraud_or_noise_text(&content)
        || is_qqnt_system_notice_text(&content)
        || content.trim() == "立即核实>"
    {
        return;
    }
    let is_self_aligned =
        is_right_aligned_self(history_rect, msg.avatar_rect, msg.largest_bubble_rect);
    out.push(RawParsedMessage {
        sender: msg.sender,
        content,
        timestamp_hint: msg.timestamp_hint,
        is_self_aligned,
        image_rects: msg.image_rects,
        bubble_rect: msg.largest_bubble_rect,
        avatar_rect: msg.avatar_rect,
    });
}

/// 将 QQNT 原生扁平 UIA 消息列表按头像节点分段解析为结构化消息序列
pub(crate) fn parse_flat_uia_message_list(
    children: &[UiNodeSnapshot],
    history_rect: Option<[i32; 4]>,
    container_w: i32,
) -> Vec<RawParsedMessage> {
    let mut out = Vec::new();
    let mut current: Option<PendingFlatMessage> = None;
    let mut pending_timestamp: Option<String> = None;

    for child in children {
        if profile::is_system_chrome(child) || is_scrollbar_wrapper(child, container_w) {
            continue;
        }
        if let Some(sender) = is_flat_uia_avatar_node(child) {
            flush_flat_message(&mut current, history_rect, &mut out);
            current = Some(PendingFlatMessage {
                sender,
                avatar_rect: profile::rect_is_valid(child.rect).then_some(child.rect),
                timestamp_hint: pending_timestamp.take(),
                content_fragments: Vec::new(),
                has_image: false,
                image_rects: Vec::new(),
                largest_bubble_rect: None,
                has_bubble: false,
                expect_level_digits: false,
            });
            continue;
        }

        let control_type = child.control_type.trim();
        if control_type.eq_ignore_ascii_case(CONTROL_TYPE_TEXT) {
            let text = child.name.trim();
            if text.is_empty() {
                continue;
            }
            if is_timestamp_like(text) {
                flush_flat_message(&mut current, history_rect, &mut out);
                pending_timestamp = Some(text.to_string());
                continue;
            }
            if let Some(pending) = current.as_mut() {
                let is_header_meta = !pending.has_bubble
                    && (text == pending.sender
                        || is_badge_keyword(text)
                        || (pending.expect_level_digits
                            && text.len() <= 4
                            && text.chars().all(|ch| ch.is_ascii_digit())));
                let is_post_bubble_reaction = pending.has_bubble
                    && is_reaction_counter_text(text)
                    && (!profile::rect_is_valid(child.rect)
                        || pending
                            .largest_bubble_rect
                            .map(|bub| child.rect[1] >= bub[3] - REACTION_BOTTOM_TOLERANCE)
                            .unwrap_or(false));
                if is_post_bubble_reaction {
                    continue;
                }
                if is_header_meta {
                    pending.expect_level_digits = text.eq_ignore_ascii_case(UI_NOISE_LEVEL_PREFIX);
                    continue;
                }
                pending.expect_level_digits = false;
                if !is_qqnt_anti_fraud_or_noise_text(text) && !is_qqnt_system_notice_text(text) {
                    pending.content_fragments.push(text.to_string());
                }
            }
            continue;
        }

        let mut fragments = Vec::new();
        collect_all_leaf_fragments(child, &mut fragments);
        let has_image = has_content_image_inner(child, false);
        if fragments.is_empty() && !has_image {
            continue;
        }
        if !has_image && fragments.iter().all(|fragment| is_timestamp_like(fragment)) {
            flush_flat_message(&mut current, history_rect, &mut out);
            pending_timestamp = Some(fragments.join(" "));
            continue;
        }

        let joined = fragments.join("\n").trim().to_string();
        if is_qqnt_anti_fraud_or_noise_text(&joined)
            || is_qqnt_system_notice_text(&joined)
            || joined == "立即核实>"
        {
            flush_flat_message(&mut current, history_rect, &mut out);
            continue;
        }

        if let Some(pending) = current.as_mut() {
            if is_post_bubble_reaction_group(
                child.rect,
                pending.largest_bubble_rect,
                pending.has_bubble,
                has_image,
                &fragments,
            ) {
                continue;
            }
            let is_header_badge_group = !pending.has_bubble
                && !has_image
                && (!profile::rect_is_valid(child.rect) || profile::rect_height(child.rect) <= 50)
                && contains_badge_subtree(child)
                && extract_sender_from_header_fragments(&fragments).is_some();
            if is_header_badge_group {
                continue;
            }
            if !joined.is_empty() {
                pending.content_fragments.push(joined);
            }
            if has_image {
                pending.has_image = true;
                collect_visible_image_rects(child, history_rect, &mut pending.image_rects);
            }
            pending.has_bubble = true;
            pending.expect_level_digits = false;
            if profile::rect_is_valid(child.rect) {
                let replace = pending
                    .largest_bubble_rect
                    .map(|best| profile::rect_area(child.rect) > profile::rect_area(best))
                    .unwrap_or(true);
                if replace {
                    pending.largest_bubble_rect = Some(child.rect);
                }
            }
        }
    }

    flush_flat_message(&mut current, history_rect, &mut out);
    out
}
