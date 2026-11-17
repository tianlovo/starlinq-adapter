//! 嵌套 MSAA / Chromium 控件树的消息容器查找、视图解包与逐行消息解析器

use common_types::constants::{UI_LABEL_MESSAGE_LIST, UI_NOISE_UNKNOWN};
use common_types::UiNodeSnapshot;
use uia_core::constants::{
    CONTROL_TYPE_DOCUMENT, CONTROL_TYPE_GROUP, CONTROL_TYPE_LIST, CONTROL_TYPE_LIST_ITEM,
    CONTROL_TYPE_PANE, CONTROL_TYPE_TABLE, CONTROL_TYPE_TEXT, CONTROL_TYPE_WINDOW,
};

use super::row_cleaner::{
    collect_all_leaf_fragments, collect_leaf_fragments, collect_visible_image_rects,
    contains_badge_subtree, extract_sender_from_header_fragments, fallback_unknown_sender,
    find_avatar_in_row, find_offscreen_avatar_name_in_row, has_content_image_inner,
    has_visible_content_image, is_avatar_node, is_badge_keyword, is_post_bubble_reaction_group,
    is_qqnt_anti_fraud_or_noise_text, is_qqnt_system_notice_text, is_reaction_counter_text,
    is_right_aligned_self, is_timestamp_like, parse_formatted_line, RawParsedMessage,
};
use crate::constants::CONTROL_TYPE_CUSTOM;
use crate::profile;

/// 单行 QQNT 复合消息节点解析结果分类
pub(crate) enum QqntRowParse {
    Parsed(RawParsedMessage),
    Skip,
    NotQqntRow,
}

fn is_sender_header_child_ext(
    node: &UiNodeSnapshot,
    avatar: Option<&(String, [i32; 4])>,
    avatar_name: Option<&str>,
    allow_offscreen: bool,
) -> Option<(Option<String>, Option<String>)> {
    if (!allow_offscreen && node.is_offscreen) || profile::is_system_chrome(node) {
        return None;
    }
    // 限制仅匹配紧凑的横向“昵称+头衔”条带，避免将同时含头像与气泡的复合子行误判为头部条带
    if profile::rect_is_valid(node.rect) && profile::rect_height(node.rect) > 50 {
        return None;
    }
    let has_nested_avatar_or_image = if allow_offscreen {
        find_offscreen_avatar_name_in_row(node, 2).is_some() || has_content_image_inner(node, false)
    } else {
        find_avatar_in_row(node, 2).is_some() || has_visible_content_image(node)
    };
    if has_nested_avatar_or_image {
        return None;
    }
    let has_badges = contains_badge_subtree(node);
    let mut fragments = Vec::new();
    if allow_offscreen {
        collect_all_leaf_fragments(node, &mut fragments);
    } else {
        collect_leaf_fragments(node, &mut fragments);
    }
    if fragments.is_empty() {
        return None;
    }
    if has_badges {
        return extract_sender_from_header_fragments(&fragments);
    }
    if let Some((av_name, av_rect)) = avatar {
        let same_row_rect = profile::rect_is_valid(node.rect)
            && (node.rect[1] - av_rect[1]).abs() <= 16
            && profile::rect_height(node.rect) <= 44
            && (node.rect[0] >= av_rect[2] - 4 || node.rect[2] <= av_rect[0] + 4);
        if (same_row_rect || allow_offscreen)
            && fragments.iter().any(|fragment| fragment.trim() == av_name)
        {
            return extract_sender_from_header_fragments(&fragments);
        }
    }
    if allow_offscreen {
        if let Some(av_name) = avatar_name {
            if fragments.iter().any(|fragment| fragment.trim() == av_name) {
                return extract_sender_from_header_fragments(&fragments);
            }
        }
        if node
            .control_type
            .trim()
            .eq_ignore_ascii_case(CONTROL_TYPE_TEXT)
        {
            let extracted = extract_sender_from_header_fragments(&fragments);
            if let Some((Some(ref sender), _)) = extracted {
                let s = sender.trim();
                if !s.is_empty()
                    && !s.starts_with('[')
                    && !s.starts_with('「')
                    && !is_badge_keyword(s)
                    && !is_reaction_counter_text(s)
                    && !is_qqnt_anti_fraud_or_noise_text(s)
                    && !is_qqnt_system_notice_text(s)
                {
                    return extracted;
                }
            }
        }
    }
    None
}

fn is_sender_header_child(
    node: &UiNodeSnapshot,
    avatar: Option<&(String, [i32; 4])>,
) -> Option<(Option<String>, Option<String>)> {
    is_sender_header_child_ext(node, avatar, None, false)
}

fn is_standalone_timestamp_child_ext(
    node: &UiNodeSnapshot,
    allow_offscreen: bool,
) -> Option<String> {
    if (!allow_offscreen && node.is_offscreen) || profile::is_system_chrome(node) {
        return None;
    }
    if profile::rect_is_valid(node.rect) && profile::rect_height(node.rect) > 50 {
        return None;
    }
    let has_nested_avatar_or_image = if allow_offscreen {
        find_offscreen_avatar_name_in_row(node, 2).is_some() || has_content_image_inner(node, false)
    } else {
        find_avatar_in_row(node, 2).is_some() || has_visible_content_image(node)
    };
    if has_nested_avatar_or_image || contains_badge_subtree(node) {
        return None;
    }
    let mut fragments = Vec::new();
    if allow_offscreen {
        collect_all_leaf_fragments(node, &mut fragments);
    } else {
        collect_leaf_fragments(node, &mut fragments);
    }
    if fragments.is_empty() || !fragments.iter().all(|fragment| is_timestamp_like(fragment)) {
        return None;
    }
    Some(fragments.join(" "))
}

fn is_standalone_timestamp_child(node: &UiNodeSnapshot) -> Option<String> {
    is_standalone_timestamp_child_ext(node, false)
}

fn unwrap_row_frame_with_timestamp(mut node: &UiNodeSnapshot) -> (&UiNodeSnapshot, Option<String>) {
    let mut timestamp_hint: Option<String> = None;
    for _ in 0..5 {
        if node.children.len() == 1
            && node.children[0]
                .control_type
                .trim()
                .eq_ignore_ascii_case(CONTROL_TYPE_GROUP)
            && node.children[0].name.trim().is_empty()
        {
            node = &node.children[0];
            continue;
        }
        let mut non_ts_children: Vec<&UiNodeSnapshot> = Vec::new();
        let mut found_ts: Option<String> = None;
        for child in &node.children {
            if let Some(timestamp) = is_standalone_timestamp_child_ext(child, true) {
                if found_ts.is_none() {
                    found_ts = Some(timestamp);
                }
            } else if !child.is_offscreen {
                non_ts_children.push(child);
            }
        }
        if found_ts.is_some()
            && non_ts_children.len() == 1
            && non_ts_children[0]
                .control_type
                .trim()
                .eq_ignore_ascii_case(CONTROL_TYPE_GROUP)
            && non_ts_children[0].name.trim().is_empty()
            && !non_ts_children[0].children.is_empty()
        {
            if timestamp_hint.is_none() {
                timestamp_hint = found_ts;
            }
            node = non_ts_children[0];
        } else {
            break;
        }
    }
    (node, timestamp_hint)
}

fn count_avatars_shallow(node: &UiNodeSnapshot, max_depth: usize) -> usize {
    if profile::is_system_chrome(node) {
        return 0;
    }
    let mut total = 0usize;
    for child in &node.children {
        if profile::is_system_chrome(child) {
            continue;
        }
        let is_flat_av =
            !child.is_offscreen && super::flat_uia::is_flat_avatar_node(child).is_some();
        let is_nested_av = !child.is_offscreen && is_avatar_node(child).is_some();
        if is_flat_av || is_nested_av {
            total += 1;
        } else if max_depth > 1 {
            total += count_avatars_shallow(child, max_depth - 1);
        }
    }
    total
}

fn parse_qqnt_message_row(row: &UiNodeSnapshot, history_rect: [i32; 4]) -> QqntRowParse {
    if row.children.is_empty()
        || !row
            .control_type
            .trim()
            .eq_ignore_ascii_case(CONTROL_TYPE_GROUP)
    {
        return QqntRowParse::NotQqntRow;
    }

    // 入口硬护栏：直接包含 2 个及以上扁平头像节点或高度接近整个历史视口的多头像容器绝不是单条消息行
    let depth1_flat_avatars = row
        .children
        .iter()
        .filter(|child| super::flat_uia::is_flat_avatar_node(child).is_some())
        .count();
    let hist_height = profile::rect_height(history_rect);
    let row_height = profile::rect_height(row.rect);
    let shallow_avatars = count_avatars_shallow(row, 3);
    if depth1_flat_avatars >= 2
        || (hist_height > 0 && row_height >= hist_height * 85 / 100 && shallow_avatars >= 2)
    {
        return QqntRowParse::NotQqntRow;
    }

    let (frame, mut timestamp_hint) = unwrap_row_frame_with_timestamp(row);
    let frame_flat_avatars = frame
        .children
        .iter()
        .filter(|child| super::flat_uia::is_flat_avatar_node(child).is_some())
        .count();
    if frame_flat_avatars >= 2 || count_avatars_shallow(frame, 2) >= 2 {
        return QqntRowParse::NotQqntRow;
    }

    let avatar = find_avatar_in_row(frame, 3);
    let mut offscreen_avatar_name = if avatar.is_none() {
        find_offscreen_avatar_name_in_row(frame, 3)
            .or_else(|| find_offscreen_avatar_name_in_row(row, 3))
    } else {
        None
    };

    let mut all_visible_leaves = Vec::new();
    collect_leaf_fragments(frame, &mut all_visible_leaves);
    let joined_all = all_visible_leaves.join(" ");
    if is_qqnt_anti_fraud_or_noise_text(&joined_all)
        || (avatar.is_none()
            && offscreen_avatar_name.is_none()
            && is_qqnt_system_notice_text(&joined_all))
    {
        return QqntRowParse::Skip;
    }

    let mut header_sender: Option<String> = None;
    let mut content_fragments: Vec<String> = Vec::new();
    let mut largest_bubble_rect: Option<[i32; 4]> = None;
    let mut has_image = false;
    let mut image_rects: Vec<[i32; 4]> = Vec::new();

    for child in &frame.children {
        if let Some((_, av_rect)) = &avatar {
            if child.rect == *av_rect || is_avatar_node(child).is_some() {
                continue;
            }
        }
        if let Some((hdr_sender, hdr_time)) = is_sender_header_child(child, avatar.as_ref()) {
            if header_sender.is_none() {
                header_sender = hdr_sender;
            }
            if timestamp_hint.is_none() {
                timestamp_hint = hdr_time;
            }
            continue;
        }
        if child.is_offscreen {
            continue;
        }

        let mut child_fragments = Vec::new();
        collect_leaf_fragments(child, &mut child_fragments);
        let child_has_image = has_visible_content_image(child);
        if is_post_bubble_reaction_group(
            child.rect,
            largest_bubble_rect,
            largest_bubble_rect.is_some(),
            child_has_image,
            &child_fragments,
        ) {
            continue;
        }
        content_fragments.extend(child_fragments);
        if child_has_image {
            has_image = true;
            collect_visible_image_rects(child, Some(history_rect), &mut image_rects);
        }
        if (!content_fragments.is_empty() || child_has_image) && profile::rect_is_valid(child.rect)
        {
            let replace = largest_bubble_rect
                .map(|best| profile::rect_area(child.rect) > profile::rect_area(best))
                .unwrap_or(true);
            if replace {
                largest_bubble_rect = Some(child.rect);
            }
        }
    }

    // 当可见子节点未解析出发件人、头像或时间戳时，回溯同行的屏外头部子节点补全归属
    if header_sender.is_none()
        || timestamp_hint.is_none()
        || (avatar.is_none() && offscreen_avatar_name.is_none())
    {
        let extra_row_children = if std::ptr::eq(frame, row) {
            &[][..]
        } else {
            row.children.as_slice()
        };
        for child in frame
            .children
            .iter()
            .chain(extra_row_children.iter())
            .filter(|c| c.is_offscreen)
        {
            if avatar.is_none() && offscreen_avatar_name.is_none() {
                offscreen_avatar_name = find_offscreen_avatar_name_in_row(child, 2);
            }
            if timestamp_hint.is_none() {
                if let Some(ts) = is_standalone_timestamp_child_ext(child, true) {
                    timestamp_hint = Some(ts);
                    continue;
                }
            }
            if header_sender.is_none() {
                if let Some((hdr_sender, hdr_time)) = is_sender_header_child_ext(
                    child,
                    avatar.as_ref(),
                    offscreen_avatar_name.as_deref(),
                    true,
                ) {
                    if header_sender.is_none() {
                        header_sender = hdr_sender;
                    }
                    if timestamp_hint.is_none() {
                        timestamp_hint = hdr_time;
                    }
                }
            }
        }
    }

    let has_any_avatar = avatar.is_some() || offscreen_avatar_name.is_some();
    if !has_any_avatar
        && header_sender.is_none()
        && !content_fragments.is_empty()
        && content_fragments
            .iter()
            .all(|fragment| is_timestamp_like(fragment))
    {
        return QqntRowParse::Skip;
    }

    let mut content = content_fragments.join("\n").trim().to_string();
    if content.is_empty() && has_image {
        content = "[图片]".to_string();
    }
    if content.is_empty()
        || is_qqnt_anti_fraud_or_noise_text(&content)
        || (!has_any_avatar && is_qqnt_system_notice_text(&content))
    {
        return QqntRowParse::Skip;
    }

    if !has_any_avatar && header_sender.is_none() && content.trim() == "立即核实>" {
        return QqntRowParse::Skip;
    }

    let sender = header_sender
        .or_else(|| avatar.as_ref().map(|(name, _)| name.clone()))
        .or(offscreen_avatar_name)
        .unwrap_or_else(fallback_unknown_sender);

    // 顶边越出历史视口且回溯屏外头部后仍未归属任何发送者的残片直接过滤，防止生成无主 [CQ:image,file=] 幽灵行
    let top_clipped = profile::rect_is_valid(history_rect)
        && ((profile::rect_is_valid(row.rect) && row.rect[1] < history_rect[1])
            || largest_bubble_rect
                .is_some_and(|bub| profile::rect_is_valid(bub) && bub[1] < history_rect[1]));
    if top_clipped && sender == UI_NOISE_UNKNOWN {
        return QqntRowParse::Skip;
    }

    let is_self_aligned = is_right_aligned_self(
        Some(history_rect),
        avatar.as_ref().map(|(_, rect)| *rect),
        largest_bubble_rect,
    );

    QqntRowParse::Parsed(RawParsedMessage {
        sender,
        content,
        timestamp_hint,
        is_self_aligned,
        image_rects,
        bubble_rect: largest_bubble_rect
            .or_else(|| profile::rect_is_valid(frame.rect).then_some(frame.rect)),
        avatar_rect: avatar.map(|(_, rect)| rect),
    })
}

/// 判断屏外节点是否为自带单行消息格式的可解析 `ListItem` 项（Win32 ListBox 滚动项）
fn is_formatted_offscreen_list_item(node: &UiNodeSnapshot) -> bool {
    node.control_type
        .trim()
        .eq_ignore_ascii_case(CONTROL_TYPE_LIST_ITEM)
        && [node.name.trim(), node.value.as_deref().unwrap_or("").trim()]
            .into_iter()
            .any(|text| !text.is_empty() && parse_formatted_line(text).is_some())
}

/// 解析单个非扁平消息列表节点（支持 MockQQ 单行格式与嵌套 MSAA 行格式）
pub(crate) fn parse_message_node(
    node: &UiNodeSnapshot,
    history_rect: Option<[i32; 4]>,
) -> Option<RawParsedMessage> {
    if profile::is_system_chrome(node) {
        return None;
    }
    let is_list_item = node
        .control_type
        .trim()
        .eq_ignore_ascii_case(CONTROL_TYPE_LIST_ITEM);
    if node.is_offscreen && !is_list_item {
        return None;
    }

    for direct in [node.name.trim(), node.value.as_deref().unwrap_or("").trim()] {
        if let Some(parsed) = parse_formatted_line(direct) {
            return Some(parsed);
        }
    }
    if node.is_offscreen {
        return None;
    }

    if let Some(rect) = history_rect {
        match parse_qqnt_message_row(node, rect) {
            QqntRowParse::Parsed(parsed) => return Some(parsed),
            QqntRowParse::Skip => return None,
            QqntRowParse::NotQqntRow => {}
        }
    }

    let mut fragments = Vec::new();
    collect_leaf_fragments(node, &mut fragments);
    if fragments.is_empty() {
        return None;
    }

    let is_top_clipped_unknown = history_rect.is_some_and(|h| {
        profile::rect_is_valid(h) && profile::rect_is_valid(node.rect) && node.rect[1] < h[1]
    });

    if fragments.len() == 1 {
        let only = fragments[0].trim();
        if let Some(parsed) = parse_formatted_line(only) {
            return Some(parsed);
        }
        if is_timestamp_like(only) || is_top_clipped_unknown {
            return None;
        }
        return Some(RawParsedMessage {
            sender: fallback_unknown_sender(),
            content: only.to_string(),
            timestamp_hint: None,
            is_self_aligned: false,
            ..Default::default()
        });
    }

    if let Some(mut first_parsed) = parse_formatted_line(&fragments[0]) {
        for extra in &fragments[1..] {
            let trimmed = extra.trim();
            if !trimmed.is_empty()
                && !is_timestamp_like(trimmed)
                && !first_parsed.content.contains(trimmed)
            {
                first_parsed.content.push('\n');
                first_parsed.content.push_str(trimmed);
            }
        }
        return Some(first_parsed);
    }

    if is_timestamp_like(&fragments[0]) {
        if fragments.len() >= 3 {
            let content = fragments[2..].join("\n");
            if content.trim().is_empty() {
                return None;
            }
            return Some(RawParsedMessage {
                sender: fragments[1].clone(),
                content,
                timestamp_hint: Some(fragments[0].clone()),
                is_self_aligned: false,
                ..Default::default()
            });
        }
        if is_timestamp_like(&fragments[1]) || is_top_clipped_unknown {
            return None;
        }
        return Some(RawParsedMessage {
            sender: fallback_unknown_sender(),
            content: fragments[1].clone(),
            timestamp_hint: Some(fragments[0].clone()),
            is_self_aligned: false,
            ..Default::default()
        });
    }

    if is_timestamp_like(&fragments[1]) {
        if fragments.len() >= 3 {
            let content = fragments[2..].join("\n");
            if content.trim().is_empty() {
                return None;
            }
            return Some(RawParsedMessage {
                sender: fragments[0].clone(),
                content,
                timestamp_hint: Some(fragments[1].clone()),
                is_self_aligned: false,
                ..Default::default()
            });
        }
        return None;
    }

    if fragments.len() >= 3 && is_timestamp_like(fragments.last().map(String::as_str).unwrap_or(""))
    {
        let last_idx = fragments.len() - 1;
        let content = fragments[1..last_idx].join("\n");
        if content.trim().is_empty() {
            return None;
        }
        return Some(RawParsedMessage {
            sender: fragments[0].clone(),
            content,
            timestamp_hint: Some(fragments[last_idx].clone()),
            is_self_aligned: false,
            ..Default::default()
        });
    }

    let content = fragments[1..].join("\n");
    if content.trim().is_empty() {
        return None;
    }
    Some(RawParsedMessage {
        sender: fragments[0].clone(),
        content,
        timestamp_hint: None,
        is_self_aligned: false,
        ..Default::default()
    })
}

/// 判断节点是否为名为 `消息列表` 的 QQNT 消息历史主容器
pub(crate) fn is_named_message_history_container(node: &UiNodeSnapshot) -> bool {
    let control_type = node.control_type.trim();
    (control_type.eq_ignore_ascii_case(CONTROL_TYPE_WINDOW)
        || control_type.eq_ignore_ascii_case(CONTROL_TYPE_LIST)
        || control_type.eq_ignore_ascii_case(CONTROL_TYPE_GROUP))
        && node.name.trim() == UI_LABEL_MESSAGE_LIST
}

/// 递归查找所有落在右栏消息区域内的历史容器候选节点
pub(crate) fn find_history_containers<'a>(
    node: &'a UiNodeSnapshot,
    win_rect: [i32; 4],
    is_standalone: bool,
    out: &mut Vec<&'a UiNodeSnapshot>,
) {
    if node.is_offscreen
        || profile::is_system_chrome(node)
        || profile::is_excluded_sidebar_container(node)
    {
        return;
    }
    let control_type = node.control_type.trim();
    let is_named = is_named_message_history_container(node)
        || node.automation_id.trim().eq_ignore_ascii_case("ml-root");
    let is_container = is_named
        || control_type.eq_ignore_ascii_case(CONTROL_TYPE_LIST)
        || control_type.eq_ignore_ascii_case(CONTROL_TYPE_TABLE);

    if is_container && profile::is_in_message_history_container(node.rect, win_rect, is_standalone)
    {
        out.push(node);
    }

    for child in &node.children {
        find_history_containers(child, win_rect, is_standalone, out);
    }
}

/// 统计容器子树下的 `ListItem` 节点数量以辅助最优容器排序
pub(crate) fn count_list_items(node: &UiNodeSnapshot) -> usize {
    let own = usize::from(
        node.control_type
            .trim()
            .eq_ignore_ascii_case(CONTROL_TYPE_LIST_ITEM),
    );
    own + node.children.iter().map(count_list_items).sum::<usize>()
}

fn collect_list_item_descendants<'a>(
    node: &'a UiNodeSnapshot,
    out: &mut Vec<&'a UiNodeSnapshot>,
) -> bool {
    if profile::is_system_chrome(node)
        || (node.is_offscreen && !is_formatted_offscreen_list_item(node))
    {
        return false;
    }
    if node
        .control_type
        .trim()
        .eq_ignore_ascii_case(CONTROL_TYPE_LIST_ITEM)
    {
        out.push(node);
        return true;
    }
    let mut found_any = false;
    for child in &node.children {
        if collect_list_item_descendants(child, out) {
            found_any = true;
        }
    }
    found_any
}

/// 判断节点是否为右侧窄滚动条包装容器
pub(crate) fn is_scrollbar_wrapper(node: &UiNodeSnapshot, container_w: i32) -> bool {
    node.name.trim().is_empty()
        && profile::rect_is_valid(node.rect)
        && profile::rect_width(node.rect) <= 28.min((container_w / 10).max(16))
        && node
            .children
            .iter()
            .all(|child| child.name.trim().is_empty())
}

/// 解包 `消息列表` 外层最多两层的全视口包装容器，定位真实消息流父节点
pub(crate) fn unwrap_history_viewport(
    mut node: &UiNodeSnapshot,
    container_w: i32,
    container_h: i32,
) -> &UiNodeSnapshot {
    let min_height = if container_h > 0 {
        (container_h * 85 / 100).max(120)
    } else {
        0
    };
    for _ in 0..2 {
        let significant: Vec<&UiNodeSnapshot> = node
            .children
            .iter()
            .filter(|child| !child.is_offscreen && !is_scrollbar_wrapper(child, container_w))
            .collect();
        if significant.len() != 1 {
            break;
        }
        let single = significant[0];
        let is_unnamed_group = single
            .control_type
            .trim()
            .eq_ignore_ascii_case(CONTROL_TYPE_GROUP)
            && single.name.trim().is_empty()
            && !single.children.is_empty();
        let is_large_viewport = profile::rect_width(single.rect) >= (container_w * 7 / 10).max(120)
            && profile::rect_height(single.rect) >= min_height;
        let is_ml_root = single.automation_id.trim().eq_ignore_ascii_case("ml-root");
        let is_flat_wrapper =
            is_unnamed_group && super::flat_uia::is_flat_uia_message_list(&single.children);
        // 优先识别 `ml-root` 或已具备扁平兄弟流特征的大视口组，在头像/时间戳启发式检查前直接解包
        if (is_ml_root && !single.children.is_empty()) || (is_large_viewport && is_flat_wrapper) {
            node = single;
            continue;
        }
        if is_unnamed_group
            && is_large_viewport
            && find_avatar_in_row(single, 2).is_none()
            && !single
                .children
                .iter()
                .any(|child| is_standalone_timestamp_child(child).is_some())
        {
            node = single;
        } else {
            break;
        }
    }
    node
}

/// 从容器节点收集非扁平结构下的候选消息节点列表
pub(crate) fn collect_container_message_nodes<'a>(
    container: &'a UiNodeSnapshot,
    out: &mut Vec<&'a UiNodeSnapshot>,
) {
    if is_named_message_history_container(container) {
        let container_w = profile::rect_width(container.rect);
        let container_h = profile::rect_height(container.rect);
        let rows_parent = unwrap_history_viewport(container, container_w, container_h);
        for child in &rows_parent.children {
            if profile::is_system_chrome(child)
                || (child.is_offscreen && !is_formatted_offscreen_list_item(child))
            {
                continue;
            }
            if is_scrollbar_wrapper(child, container_w) {
                continue;
            }
            out.push(child);
        }
        return;
    }

    for child in &container.children {
        if profile::is_system_chrome(child)
            || (child.is_offscreen && !is_formatted_offscreen_list_item(child))
        {
            continue;
        }
        let control_type = child.control_type.trim();
        let is_wrapper = control_type.eq_ignore_ascii_case(CONTROL_TYPE_GROUP)
            || control_type.eq_ignore_ascii_case(CONTROL_TYPE_CUSTOM)
            || control_type.eq_ignore_ascii_case(CONTROL_TYPE_PANE)
            || control_type.eq_ignore_ascii_case(CONTROL_TYPE_DOCUMENT);
        if control_type.eq_ignore_ascii_case(CONTROL_TYPE_LIST_ITEM)
            || control_type.eq_ignore_ascii_case(CONTROL_TYPE_TEXT)
            || (is_wrapper && !collect_list_item_descendants(child, out))
        {
            out.push(child);
        }
    }
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn parse_qqnt_message_row_rejects_multi_avatar_container_even_with_notice_child() {
        let history_rect = [580, 188, 1194, 665];
        let mega_container = UiNodeSnapshot {
            control_type: CONTROL_TYPE_GROUP.to_string(),
            rect: history_rect,
            is_enabled: true,
            children: vec![
                UiNodeSnapshot {
                    control_type: CONTROL_TYPE_GROUP.to_string(),
                    name: "测试好友B".to_string(),
                    rect: [600, 195, 632, 228],
                    is_enabled: true,
                    ..Default::default()
                },
                UiNodeSnapshot {
                    control_type: CONTROL_TYPE_GROUP.to_string(),
                    rect: [640, 210, 820, 250],
                    is_enabled: true,
                    children: vec![UiNodeSnapshot {
                        control_type: CONTROL_TYPE_TEXT.to_string(),
                        name: "第一条消息".to_string(),
                        rect: [650, 218, 810, 240],
                        is_enabled: true,
                        ..Default::default()
                    }],
                    ..Default::default()
                },
                UiNodeSnapshot {
                    control_type: CONTROL_TYPE_GROUP.to_string(),
                    rect: [580, 260, 1194, 290],
                    is_enabled: true,
                    children: vec![UiNodeSnapshot {
                        control_type: CONTROL_TYPE_TEXT.to_string(),
                        name: "测试好友B 回应了你的 消息:".to_string(),
                        rect: [800, 265, 980, 285],
                        is_enabled: true,
                        ..Default::default()
                    }],
                    ..Default::default()
                },
                UiNodeSnapshot {
                    control_type: CONTROL_TYPE_GROUP.to_string(),
                    name: "测试机器人".to_string(),
                    rect: [1142, 310, 1174, 343],
                    is_enabled: true,
                    ..Default::default()
                },
                UiNodeSnapshot {
                    control_type: CONTROL_TYPE_GROUP.to_string(),
                    rect: [900, 325, 1134, 365],
                    is_enabled: true,
                    children: vec![UiNodeSnapshot {
                        control_type: CONTROL_TYPE_TEXT.to_string(),
                        name: "第二条消息".to_string(),
                        rect: [910, 333, 1120, 355],
                        is_enabled: true,
                        ..Default::default()
                    }],
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        assert!(matches!(
            parse_qqnt_message_row(&mega_container, history_rect),
            QqntRowParse::NotQqntRow
        ));
    }

    #[test]
    fn collects_and_parses_offscreen_formatted_listbox_items_only() {
        let offscreen_item = UiNodeSnapshot {
            control_type: CONTROL_TYPE_LIST_ITEM.to_string(),
            name: "水群路人甲 [09:00:01]: @qq-bot P0触发A".to_string(),
            is_enabled: true,
            is_offscreen: true,
            ..Default::default()
        };
        let offscreen_noise_group = UiNodeSnapshot {
            control_type: CONTROL_TYPE_GROUP.to_string(),
            name: "水群路人乙 [09:00:02]: 隐藏噪音".to_string(),
            is_enabled: true,
            is_offscreen: true,
            ..Default::default()
        };
        let container = UiNodeSnapshot {
            control_type: CONTROL_TYPE_LIST.to_string(),
            name: UI_LABEL_MESSAGE_LIST.to_string(),
            rect: [0, 0, 500, 400],
            is_enabled: true,
            children: vec![offscreen_item, offscreen_noise_group],
            ..Default::default()
        };
        let mut nodes = Vec::new();
        collect_container_message_nodes(&container, &mut nodes);
        assert_eq!(nodes.len(), 1);
        let parsed =
            parse_message_node(nodes[0], Some(container.rect)).expect("offscreen list item");
        assert_eq!(parsed.sender, "水群路人甲");
        assert!(parsed.content.contains("@qq-bot P0触发A"));
        assert!(parse_message_node(&container.children[1], Some(container.rect)).is_none());
    }
}
