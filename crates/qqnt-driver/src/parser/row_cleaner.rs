//! 消息行文本清洗、格式正则解析、叶子片段归并与头衔噪声过滤工具

use common_types::constants::{
    DEFAULT_BOT_SELF_NAME, IMAGE_PLACEHOLDER_TOKEN, UI_AVATAR_SUFFIX, UI_NOISE_LEVEL_PREFIX,
    UI_NOISE_UNKNOWN, UI_ROLE_ADMIN_LABEL, UI_ROLE_OWNER_LABEL,
};
use common_types::UiNodeSnapshot;
use uia_core::constants::{
    CONTROL_TYPE_BUTTON, CONTROL_TYPE_GROUP, CONTROL_TYPE_IMAGE, CONTROL_TYPE_LINK,
    CONTROL_TYPE_TEXT,
};

use crate::constants::{
    ANTI_FRAUD_NOISE_PHRASES, COLON_MESSAGE_REGEX, DATE_ONLY_REGEX, FILE_BUBBLE_SIZE_REGEX,
    FILE_BUBBLE_STATUS_KEYWORDS, FORMATTED_MESSAGE_REGEX, GROUP_COUNT_SUFFIX_REGEX,
    IGNORED_LINK_URL_PREFIXES, MIN_UNLABELED_IMAGE_DIM, MIN_VISIBLE_IMAGE_DIM,
    REACTION_BOTTOM_TOLERANCE, REACTION_COUNTER_MAX_LEN, REACTION_GROUP_MAX_HEIGHT,
    REPLY_QUOTE_OPEN_CN, REPLY_QUOTE_OPEN_EN, SYSTEM_NOTICE_PHRASES, TIMESTAMP_ONLY_REGEX,
    UI_IMAGE_CONTROL_NAME,
};
use crate::profile;

/// 从单条消息行解析得到的原始发件人、正文、自发送对齐标记、气泡框及头像坐标矩形
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RawParsedMessage {
    pub(crate) sender: String,
    pub(crate) content: String,
    pub(crate) timestamp_hint: Option<String>,
    pub(crate) is_self_aligned: bool,
    pub(crate) image_rects: Vec<[i32; 4]>,
    pub(crate) bubble_rect: Option<[i32; 4]>,
    pub(crate) avatar_rect: Option<[i32; 4]>,
}

/// 归一化消息空白字符以便进行确定性本端回显比对
pub fn normalize_message_text(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 剥离群名末尾的成员人数后缀（如 ` (5)` 或 `（26人）`）以得到规范群名称
pub fn canonical_group_name(name: &str) -> String {
    GROUP_COUNT_SUFFIX_REGEX
        .replace(name.trim(), "")
        .trim()
        .to_string()
}

/// 判断字符串是否仅表示日期分隔线或时间提示戳
pub(crate) fn is_timestamp_like(text: &str) -> bool {
    let trimmed = text.trim();
    !trimmed.is_empty()
        && (TIMESTAMP_ONLY_REGEX.is_match(trimmed) || DATE_ONLY_REGEX.is_match(trimmed))
}

/// 按“昵称 [时间]: 内容”或“昵称: 内容”单行格式解析消息
pub(crate) fn parse_formatted_line(text: &str) -> Option<RawParsedMessage> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(captures) = FORMATTED_MESSAGE_REGEX.captures(trimmed) {
        let sender = captures.name("sender")?.as_str().trim().to_string();
        let time = captures.name("time")?.as_str().trim().to_string();
        let content = captures.name("content")?.as_str().trim().to_string();
        if !sender.is_empty() && !content.is_empty() {
            return Some(RawParsedMessage {
                sender,
                content,
                timestamp_hint: if time.is_empty() { None } else { Some(time) },
                is_self_aligned: false,
                ..Default::default()
            });
        }
    }
    if let Some(captures) = COLON_MESSAGE_REGEX.captures(trimmed) {
        let sender = captures.name("sender")?.as_str().trim();
        let content = captures.name("content")?.as_str().trim();
        let lower_sender = sender.to_ascii_lowercase();
        if !sender.is_empty()
            && !content.is_empty()
            && lower_sender != "http"
            && lower_sender != "https"
        {
            return Some(RawParsedMessage {
                sender: sender.to_string(),
                content: content.to_string(),
                timestamp_hint: None,
                is_self_aligned: false,
                ..Default::default()
            });
        }
    }
    None
}

fn is_ignored_link_url(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    IGNORED_LINK_URL_PREFIXES
        .iter()
        .any(|prefix| lower.starts_with(prefix))
}

fn push_unique_fragment(out: &mut Vec<String>, text: &str) {
    let trimmed = text.trim();
    if !trimmed.is_empty() && out.last().map(String::as_str) != Some(trimmed) {
        out.push(trimmed.to_string());
    }
}

fn is_file_card_filename_piece(raw: &str) -> bool {
    let trimmed = raw.trim();
    !trimmed.is_empty()
        && trimmed != IMAGE_PLACEHOLDER_TOKEN
        && !trimmed.starts_with('[')
        && !trimmed.starts_with(REPLY_QUOTE_OPEN_CN)
        && !is_badge_keyword(trimmed)
        && !is_timestamp_like(trimmed)
        && !is_qqnt_anti_fraud_or_noise_text(trimmed)
        && !FILE_BUBBLE_STATUS_KEYWORDS
            .iter()
            .any(|kw| trimmed.contains(kw))
}

fn is_file_card_metadata_tail(tail: &[String]) -> bool {
    let mut parts = Vec::with_capacity(tail.len());
    for part in tail {
        let norm = part
            .replace(['\u{2009}', '\u{00A0}', '\u{202F}', '\u{3000}'], " ")
            .trim()
            .to_string();
        if !norm.is_empty() && norm != IMAGE_PLACEHOLDER_TOKEN {
            parts.push(norm);
        }
    }
    if parts.is_empty() {
        return false;
    }
    let meta = parts.join(" ");
    if !FILE_BUBBLE_SIZE_REGEX.is_match(&meta) {
        return false;
    }
    let has_status_or_pct = FILE_BUBBLE_STATUS_KEYWORDS
        .iter()
        .any(|kw| meta.contains(kw))
        || meta.split_whitespace().any(|tok| {
            tok.strip_suffix('%')
                .and_then(|digits| digits.parse::<u16>().ok())
                .is_some_and(|pct| pct <= 100)
        });
    if !has_status_or_pct {
        return false;
    }
    let without_size = FILE_BUBBLE_SIZE_REGEX.replace_all(&meta, " ");
    let mut residual = without_size.into_owned();
    for kw in FILE_BUBBLE_STATUS_KEYWORDS {
        residual = residual.replace(kw, " ");
    }
    for action in ["下载", "打开", "电脑", "手机"] {
        residual = residual.replace(action, " ");
    }
    !residual.chars().any(|ch| {
        !ch.is_ascii_digit()
            && !ch.is_whitespace()
            && !matches!(ch, '|' | '/' | '·' | '-' | '.' | '%')
    })
}

fn canonicalize_file_card_fragments(out: &mut Vec<String>) {
    if out.len() < 3 || out.len() > 8 {
        return;
    }
    let Some(size_idx) = out.iter().position(|part| {
        let norm = part.replace(['\u{2009}', '\u{00A0}', '\u{202F}', '\u{3000}'], " ");
        FILE_BUBBLE_SIZE_REGEX.is_match(&norm)
    }) else {
        return;
    };
    if size_idx < 2 || !is_file_card_metadata_tail(&out[size_idx..]) {
        return;
    }
    let mut start_idx = size_idx;
    while start_idx > 0 && is_file_card_filename_piece(&out[start_idx - 1]) {
        start_idx -= 1;
    }
    if size_idx.saturating_sub(start_idx) < 2 {
        return;
    }
    // 将离屏 `[0,0,0,0]` 或折行拆分的文件/视频卡片文件名片段无空格归一为完整文件名，确保与屏内 UID 完全一致
    let merged: String = out[start_idx..size_idx]
        .iter()
        .map(|s| s.trim())
        .collect::<Vec<_>>()
        .concat();
    out.splice(start_idx..size_idx, std::iter::once(merged));
}

fn collect_leaf_fragments_inner(
    node: &UiNodeSnapshot,
    skip_offscreen: bool,
    out: &mut Vec<String>,
) {
    let mut prev_text_state = None;
    collect_leaf_fragments_with_prev(node, skip_offscreen, out, &mut prev_text_state);
    canonicalize_file_card_fragments(out);
}

fn try_merge_touching_text_leaf(
    node: &UiNodeSnapshot,
    raw_name: &str,
    out: &mut [String],
    prev_text_state: &mut Option<([i32; 4], bool)>,
) -> bool {
    let trimmed = raw_name.trim();
    if trimmed.is_empty()
        || is_badge_keyword(trimmed)
        || is_timestamp_like(trimmed)
        || !profile::rect_is_valid(node.rect)
    {
        return false;
    }
    let Some(([x1, y1, r1, b1], prev_ended_ws)) = *prev_text_state else {
        return false;
    };
    let Some(last) = out.last_mut() else {
        return false;
    };
    if is_badge_keyword(last) || is_timestamp_like(last) || last == trimmed {
        return false;
    }
    let [x2, y2, r2, b2] = node.rect;
    let same_line = (y1 - y2).abs() <= 4 && (b1 - b2).abs() <= 4;
    let touching = (-3..=4).contains(&(x2 - r1));
    if !same_line || !touching {
        return false;
    }

    // 当相邻片段之间存在显式空白分隔（例如 `@成员 [正文]`）时保留单个空格，否则无缝拼接被换行截断的文件名
    if prev_ended_ws || raw_name.starts_with(char::is_whitespace) {
        last.push(' ');
    }
    last.push_str(trimmed);
    *prev_text_state = Some((
        [x1.min(x2), y1.min(y2), r1.max(r2), b1.max(b2)],
        raw_name.ends_with(char::is_whitespace),
    ));
    true
}

fn try_collect_native_quote_bubble(
    node: &UiNodeSnapshot,
    skip_offscreen: bool,
    out: &mut Vec<String>,
) -> bool {
    if !node
        .control_type
        .trim()
        .eq_ignore_ascii_case(CONTROL_TYPE_GROUP)
        || !node.name.trim().is_empty()
        || !profile::rect_is_valid(node.rect)
        || node.children.len() < 3
    {
        return false;
    }

    let visible_children: Vec<&UiNodeSnapshot> = node
        .children
        .iter()
        .filter(|child| {
            (!skip_offscreen || !child.is_offscreen) && !profile::is_system_chrome(child)
        })
        .collect();
    if visible_children.len() < 3 {
        return false;
    }

    // 原生 QQNT 引用块头部：缩进的引用作者 Text + 同行右侧紧凑 8..=28px 跳转箭头 Image("")
    let author_node = visible_children[0];
    let arrow_node = visible_children[1];
    if !author_node
        .control_type
        .trim()
        .eq_ignore_ascii_case(CONTROL_TYPE_TEXT)
        || !profile::rect_is_valid(author_node.rect)
        || author_node.rect[0] < node.rect[0] + 12
    {
        return false;
    }
    let sender_raw = author_node.name.trim();
    if sender_raw.is_empty()
        || is_badge_keyword(sender_raw)
        || is_timestamp_like(sender_raw)
        || is_qqnt_anti_fraud_or_noise_text(sender_raw)
    {
        return false;
    }
    if !arrow_node
        .control_type
        .trim()
        .eq_ignore_ascii_case(CONTROL_TYPE_IMAGE)
        || !arrow_node.name.trim().is_empty()
        || !profile::rect_is_valid(arrow_node.rect)
    {
        return false;
    }
    let arrow_w = profile::rect_width(arrow_node.rect);
    let arrow_h = profile::rect_height(arrow_node.rect);
    let same_header_line = (arrow_node.rect[1] - author_node.rect[1]).abs() <= 8
        && (arrow_node.rect[3] - author_node.rect[3]).abs() <= 8;
    if !(8..=28).contains(&arrow_w)
        || !(8..=28).contains(&arrow_h)
        || !same_header_line
        || arrow_node.rect[0] < author_node.rect[2] - 2
    {
        return false;
    }

    let quote_left = author_node.rect[0];
    let tail = &visible_children[2..];
    let Some(rel_split) = tail
        .iter()
        .position(|child| profile::rect_is_valid(child.rect) && child.rect[0] <= quote_left - 6)
    else {
        return false;
    };
    if rel_split == 0 {
        return false;
    }

    let excerpt_nodes = &tail[..rel_split];
    let body_nodes = &tail[rel_split..];

    let mut excerpt_frags = Vec::new();
    let mut excerpt_prev = None;
    for child in excerpt_nodes {
        collect_leaf_fragments_with_prev(
            child,
            skip_offscreen,
            &mut excerpt_frags,
            &mut excerpt_prev,
        );
    }
    let excerpt = if excerpt_frags.is_empty()
        && excerpt_nodes.iter().any(|child| {
            child
                .control_type
                .trim()
                .eq_ignore_ascii_case(CONTROL_TYPE_IMAGE)
        }) {
        IMAGE_PLACEHOLDER_TOKEN.to_string()
    } else {
        excerpt_frags.join(" ").trim().to_string()
    };
    if excerpt.is_empty() {
        return false;
    }

    let mut body_frags = Vec::new();
    let mut body_prev = None;
    for child in body_nodes {
        collect_leaf_fragments_with_prev(child, skip_offscreen, &mut body_frags, &mut body_prev);
    }
    canonicalize_file_card_fragments(&mut body_frags);
    if body_frags.is_empty()
        && body_nodes
            .iter()
            .any(|child| has_content_image_inner(child, skip_offscreen))
    {
        body_frags.push(IMAGE_PLACEHOLDER_TOKEN.to_string());
    }
    if body_frags.is_empty() {
        return false;
    }

    let sender_clean = sender_raw.trim_start_matches('@').trim();
    out.push(format!("「回复 @{sender_clean}: {excerpt}」"));
    out.extend(body_frags);
    true
}

fn collect_leaf_fragments_with_prev(
    node: &UiNodeSnapshot,
    skip_offscreen: bool,
    out: &mut Vec<String>,
    prev_text_state: &mut Option<([i32; 4], bool)>,
) {
    if (skip_offscreen && node.is_offscreen) || profile::is_system_chrome(node) {
        return;
    }
    if try_collect_native_quote_bubble(node, skip_offscreen, out) {
        *prev_text_state = None;
        return;
    }
    let control_type = node.control_type.trim();
    let is_link = control_type.eq_ignore_ascii_case(CONTROL_TYPE_LINK);
    let is_text_leaf = control_type.eq_ignore_ascii_case(CONTROL_TYPE_TEXT);
    let raw_name =
        if control_type.eq_ignore_ascii_case(CONTROL_TYPE_IMAGE) && node.name.trim() == "图片" {
            "[图片]"
        } else {
            node.name.as_str()
        };
    let raw_value = node.value.as_deref().unwrap_or("").trim();
    let val = if (is_link && !node.name.trim().is_empty()) || is_ignored_link_url(raw_value) {
        ""
    } else {
        raw_value
    };

    if node.children.is_empty() {
        if is_text_leaf && val.is_empty() {
            if try_merge_touching_text_leaf(node, raw_name, out, prev_text_state) {
                return;
            }
            push_unique_fragment(out, raw_name);
            let trimmed = raw_name.trim();
            if !trimmed.is_empty()
                && !is_badge_keyword(trimmed)
                && !is_timestamp_like(trimmed)
                && profile::rect_is_valid(node.rect)
            {
                *prev_text_state = Some((node.rect, raw_name.ends_with(char::is_whitespace)));
            } else {
                *prev_text_state = None;
            }
            return;
        }
        push_unique_fragment(out, raw_name);
        push_unique_fragment(out, val);
        *prev_text_state = None;
        return;
    }

    let before_len = out.len();
    for child in &node.children {
        collect_leaf_fragments_with_prev(child, skip_offscreen, out, prev_text_state);
    }
    if out.len() == before_len {
        push_unique_fragment(out, raw_name);
        push_unique_fragment(out, val);
        *prev_text_state = None;
    }
}

/// 收集节点子树中可见的叶子文本片段（跳过 `is_offscreen` 节点）
pub(crate) fn collect_leaf_fragments(node: &UiNodeSnapshot, out: &mut Vec<String>) {
    collect_leaf_fragments_inner(node, true, out);
}

/// 收集节点子树中的全部叶子文本片段（包含虚拟化屏外历史节点）
pub(crate) fn collect_all_leaf_fragments(node: &UiNodeSnapshot, out: &mut Vec<String>) {
    collect_leaf_fragments_inner(node, false, out);
}

/// 判断文本是否命中 QQNT 安全防诈横幅等系统噪声文案
pub(crate) fn is_qqnt_anti_fraud_or_noise_text(joined: &str) -> bool {
    ANTI_FRAUD_NOISE_PHRASES
        .iter()
        .any(|phrase| joined.contains(phrase))
        || (joined.contains("境外诈骗") && joined.contains("立即核实"))
}

/// 判断文本是否命中进群、退群或撤回等系统提示横幅
pub(crate) fn is_qqnt_system_notice_text(joined: &str) -> bool {
    let compact: String = joined.chars().filter(|ch| !ch.is_whitespace()).collect();
    SYSTEM_NOTICE_PHRASES
        .iter()
        .any(|phrase| compact.contains(phrase))
}

/// 剥离昵称后附带的 `"的头像"` 无障碍标签后缀
pub(crate) fn strip_avatar_suffix(name: &str) -> Option<&str> {
    name.trim()
        .strip_suffix(UI_AVATAR_SUFFIX)
        .map(str::trim)
        .filter(|trimmed| !trimmed.is_empty())
}

/// 判断节点是否为聊天行内的发送者正方形头像控件
pub(crate) fn is_avatar_node(node: &UiNodeSnapshot) -> Option<(String, [i32; 4])> {
    if profile::is_system_chrome(node) || !profile::rect_is_valid(node.rect) {
        return None;
    }
    let control_type = node.control_type.trim();
    if !(control_type.eq_ignore_ascii_case(CONTROL_TYPE_GROUP)
        || control_type.eq_ignore_ascii_case(CONTROL_TYPE_BUTTON)
        || control_type.eq_ignore_ascii_case(CONTROL_TYPE_IMAGE))
    {
        return None;
    }
    let width = profile::rect_width(node.rect);
    let height = profile::rect_height(node.rect);
    if !(24..=96).contains(&width) || !(24..=96).contains(&height) || (width - height).abs() > 18 {
        return None;
    }
    let raw_name = node.name.trim();
    if raw_name.is_empty() {
        return None;
    }
    let sender = strip_avatar_suffix(raw_name).unwrap_or(raw_name);
    if matches!(
        sender,
        "图片" | "表情" | "发送" | "语音" | "更多" | "关闭" | "最小化" | "最大化"
    ) || sender.starts_with(UI_NOISE_LEVEL_PREFIX)
    {
        return None;
    }
    Some((sender.to_string(), node.rect))
}

/// 在限定深度预算内在消息行子树中搜索发送者头像节点
pub(crate) fn find_avatar_in_row(
    node: &UiNodeSnapshot,
    depth_budget: usize,
) -> Option<(String, [i32; 4])> {
    if let Some(avatar) = is_avatar_node(node) {
        return Some(avatar);
    }
    if depth_budget == 0 {
        return None;
    }
    for child in &node.children {
        if let Some(avatar) = find_avatar_in_row(child, depth_budget - 1) {
            return Some(avatar);
        }
    }
    None
}

fn extract_offscreen_avatar_name(node: &UiNodeSnapshot) -> Option<String> {
    if profile::is_system_chrome(node) {
        return None;
    }
    if let Some((name, _)) = is_avatar_node(node) {
        return Some(name);
    }
    let control_type = node.control_type.trim();
    if !(control_type.eq_ignore_ascii_case(CONTROL_TYPE_GROUP)
        || control_type.eq_ignore_ascii_case(CONTROL_TYPE_BUTTON)
        || control_type.eq_ignore_ascii_case(CONTROL_TYPE_IMAGE))
    {
        return None;
    }
    let raw_name = node.name.trim();
    if raw_name.is_empty() {
        return None;
    }
    let has_avatar_suffix = strip_avatar_suffix(raw_name).is_some();
    let leaf_or_empty_children =
        node.children.is_empty() || node.children.iter().all(|c| c.name.trim().is_empty());
    // 常规 is_avatar_node 未命中时，仅放行带 `的头像` 后缀或离屏/空矩形头像节点
    if profile::rect_is_valid(node.rect) && !has_avatar_suffix {
        return None;
    }
    if profile::rect_is_valid(node.rect)
        && (profile::rect_width(node.rect) > 96 || profile::rect_height(node.rect) > 96)
    {
        return None;
    }
    if !has_avatar_suffix && !(node.is_offscreen && leaf_or_empty_children) {
        return None;
    }
    let sender = strip_avatar_suffix(raw_name).unwrap_or(raw_name);
    if matches!(
        sender,
        "图片"
            | "表情"
            | "发送"
            | "语音"
            | "更多"
            | "关闭"
            | "最小化"
            | "最大化"
            | "消息列表"
            | "会话列表"
            | "群成员列表"
            | "窗口控制区域"
    ) || sender.starts_with(UI_NOISE_LEVEL_PREFIX)
        || is_badge_keyword(sender)
        || is_timestamp_like(sender)
        || is_reaction_counter_text(sender)
        || is_qqnt_anti_fraud_or_noise_text(sender)
        || is_qqnt_system_notice_text(sender)
    {
        return None;
    }
    Some(sender.to_string())
}

/// 当可见头像查找未命中时，穿透 `is_offscreen` / 空矩形头部节点识别 `<昵称>的头像` 或离屏头像名
pub(crate) fn find_offscreen_avatar_name_in_row(
    node: &UiNodeSnapshot,
    depth_budget: usize,
) -> Option<String> {
    if let Some(name) = extract_offscreen_avatar_name(node) {
        return Some(name);
    }
    if depth_budget == 0 {
        return None;
    }
    for child in &node.children {
        if let Some(name) = find_offscreen_avatar_name_in_row(child, depth_budget - 1) {
            return Some(name);
        }
    }
    None
}

/// 判断文本片段是否为等级或角色头衔徽章关键词
pub(crate) fn is_badge_keyword(text: &str) -> bool {
    let trimmed = text.trim();
    trimmed == UI_ROLE_OWNER_LABEL
        || trimmed == UI_ROLE_ADMIN_LABEL
        || matches!(
            trimmed,
            "LV" | "Lv" | "lv" | "机器人" | "官方" | "头衔" | "精华"
        )
}

/// 检测子树中是否包含角色或等级徽章标签
pub(crate) fn contains_badge_subtree(node: &UiNodeSnapshot) -> bool {
    if is_badge_keyword(node.name.trim()) {
        return true;
    }
    node.children.iter().any(contains_badge_subtree)
}

/// 从带有头衔和时间戳的头部文本片段中剥离噪声并提取发件人昵称与时间提示
pub(crate) fn extract_sender_from_header_fragments(
    fragments: &[String],
) -> Option<(Option<String>, Option<String>)> {
    let mut sender: Option<String> = None;
    let mut timestamp_hint: Option<String> = None;
    let mut skip_next_level_num = false;

    for fragment in fragments {
        let trimmed = fragment.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.eq_ignore_ascii_case(UI_NOISE_LEVEL_PREFIX) {
            skip_next_level_num = true;
            continue;
        }
        if skip_next_level_num
            && trimmed.len() <= 4
            && trimmed.chars().all(|ch| ch.is_ascii_digit())
        {
            skip_next_level_num = false;
            continue;
        }
        skip_next_level_num = false;
        if is_badge_keyword(trimmed) {
            continue;
        }
        if is_timestamp_like(trimmed) {
            if timestamp_hint.is_none() {
                timestamp_hint = Some(trimmed.to_string());
            }
            continue;
        }
        if sender.is_none() {
            sender = Some(trimmed.to_string());
        }
    }

    if sender.is_some() || timestamp_hint.is_some() {
        Some((sender, timestamp_hint))
    } else {
        None
    }
}

/// 检测子树中是否包含消息正文图片节点（可选是否跳过屏外节点）
pub(crate) fn has_content_image_inner(node: &UiNodeSnapshot, skip_offscreen: bool) -> bool {
    if (skip_offscreen && node.is_offscreen) || profile::is_system_chrome(node) {
        return false;
    }
    if node
        .control_type
        .trim()
        .eq_ignore_ascii_case(CONTROL_TYPE_IMAGE)
        && (node.name.trim() == "图片"
            || (profile::rect_is_valid(node.rect)
                && profile::rect_width(node.rect) >= 24
                && profile::rect_height(node.rect) >= 24))
    {
        return true;
    }
    node.children
        .iter()
        .any(|child| has_content_image_inner(child, skip_offscreen))
}

/// 检测子树中是否存在可见的消息图片内容
pub(crate) fn has_visible_content_image(node: &UiNodeSnapshot) -> bool {
    has_content_image_inner(node, true)
}

/// 根据头像或气泡相对消息历史区域中轴线的右对齐位置判断是否属于本端发送
pub(crate) fn is_right_aligned_self(
    history_rect: Option<[i32; 4]>,
    avatar_rect: Option<[i32; 4]>,
    bubble_rect: Option<[i32; 4]>,
) -> bool {
    let Some(rect) = history_rect.filter(|candidate| profile::rect_is_valid(*candidate)) else {
        return false;
    };
    let history_mid_x = (rect[0] + rect[2]) as f64 * 0.5;
    let history_width = profile::rect_width(rect) as f64;
    if let Some(av_rect) = avatar_rect.filter(|candidate| profile::rect_is_valid(*candidate)) {
        let (av_center_x, _) = profile::rect_center(av_rect);
        return av_center_x > history_mid_x;
    }
    if let Some(bub_rect) = bubble_rect.filter(|candidate| profile::rect_is_valid(*candidate)) {
        let (bubble_center_x, _) = profile::rect_center(bub_rect);
        return bubble_center_x > history_mid_x + history_width * 0.08
            && bub_rect[0] as f64 > rect[0] as f64 + history_width * 0.18;
    }
    false
}

/// 获取未知发送者时的统一兜底展示名称
pub(crate) fn fallback_unknown_sender() -> String {
    UI_NOISE_UNKNOWN.to_string()
}

/// 返回内建默认自账号昵称标识
pub(crate) fn default_bot_self_name() -> &'static str {
    DEFAULT_BOT_SELF_NAME
}

/// 判断文本是否为气泡底部表情回应的数字计数角标（例如 `1`、`+5`、`99+`）
pub(crate) fn is_reaction_counter_text(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed.len() > REACTION_COUNTER_MAX_LEN {
        return false;
    }
    let body = trimmed
        .strip_prefix('+')
        .or_else(|| trimmed.strip_suffix('+'))
        .unwrap_or(trimmed);
    !body.is_empty() && body.chars().all(|ch| ch.is_ascii_digit())
}

/// 判断节点是否位于主气泡底部下方且仅含表情反应计数或微缩图标
pub(crate) fn is_post_bubble_reaction_group(
    node_rect: [i32; 4],
    largest_bubble_rect: Option<[i32; 4]>,
    has_bubble: bool,
    has_content_image: bool,
    fragments: &[String],
) -> bool {
    if !has_bubble || has_content_image || !profile::rect_is_valid(node_rect) {
        return false;
    }
    if profile::rect_height(node_rect) > REACTION_GROUP_MAX_HEIGHT {
        return false;
    }
    let below_bubble = largest_bubble_rect
        .map(|bub| node_rect[1] >= bub[3] - REACTION_BOTTOM_TOLERANCE)
        .unwrap_or(false);
    below_bubble
        && (fragments.is_empty()
            || fragments
                .iter()
                .all(|fragment| is_reaction_counter_text(fragment)))
}

/// 将图片矩形框裁剪到消息历史视口范围内，拦截过度越界或仅剩极窄条带的半截图像
pub(crate) fn clip_rect_to_history(
    rect: [i32; 4],
    history_rect: Option<[i32; 4]>,
) -> Option<[i32; 4]> {
    if !profile::rect_is_valid(rect) {
        return None;
    }
    let Some(hist) = history_rect.filter(|candidate| profile::rect_is_valid(*candidate)) else {
        return Some(rect);
    };
    let orig_height = profile::rect_height(rect);
    let clipped = [
        rect[0].max(hist[0]),
        rect[1].max(hist[1]),
        rect[2].min(hist[2]),
        rect[3].min(hist[3]),
    ];
    let inter_width = clipped[2] - clipped[0];
    let inter_height = clipped[3] - clipped[1];
    if inter_width < 24 || inter_height < 24 || inter_height < (orig_height / 2) {
        None
    } else {
        Some(clipped)
    }
}

/// 递归收集消息气泡子树内所有可见且落入历史视口的有效图片坐标框，供 `PrintWindow` 局部截切使用
pub(crate) fn collect_visible_image_rects(
    node: &UiNodeSnapshot,
    history_rect: Option<[i32; 4]>,
    out: &mut Vec<[i32; 4]>,
) {
    if node.is_offscreen || profile::is_system_chrome(node) {
        return;
    }
    if node
        .control_type
        .trim()
        .eq_ignore_ascii_case(CONTROL_TYPE_IMAGE)
        && profile::rect_is_valid(node.rect)
    {
        let width = profile::rect_width(node.rect);
        let height = profile::rect_height(node.rect);
        let is_content_img = width > MIN_VISIBLE_IMAGE_DIM
            && height > MIN_VISIBLE_IMAGE_DIM
            && (node.name.trim() == UI_IMAGE_CONTROL_NAME
                || (width >= MIN_UNLABELED_IMAGE_DIM && height >= MIN_UNLABELED_IMAGE_DIM));
        if is_content_img {
            if let Some(clipped) = clip_rect_to_history(node.rect, history_rect) {
                if !out.contains(&clipped) {
                    out.push(clipped);
                }
            }
        }
    }
    for child in &node.children {
        collect_visible_image_rects(child, history_rect, out);
    }
}

/// 从消息文本开头剥离引用回复头部（`「回复 ...」` 或 `[回复 ...]`），返回 `(回复线索, 剩余正文)`
pub(crate) fn extract_reply_preview(text: &str) -> (Option<(Option<String>, String)>, &str) {
    let trimmed = text.trim_start();
    let (inner_raw, after) = if let Some(rest) = trimmed.strip_prefix(REPLY_QUOTE_OPEN_CN) {
        let Some(end) = rest.find('」') else {
            return (None, text);
        };
        (&rest[..end], rest[end + '」'.len_utf8()..].trim_start())
    } else if let Some(rest) = trimmed.strip_prefix(REPLY_QUOTE_OPEN_EN) {
        let Some(end) = rest.find(']') else {
            return (None, text);
        };
        (&rest[..end], rest[end + 1..].trim_start())
    } else {
        return (None, text);
    };

    let split = inner_raw
        .find(':')
        .map(|idx| (idx, 1))
        .or_else(|| inner_raw.find('：').map(|idx| (idx, '：'.len_utf8())));
    let preview = match split {
        Some((idx, sep_len)) => {
            let sender = inner_raw[..idx].trim().trim_start_matches('@').trim();
            let excerpt = inner_raw[idx + sep_len..].trim().to_string();
            let sender_opt = (!sender.is_empty()).then(|| sender.to_string());
            (sender_opt, excerpt)
        }
        None => (None, inner_raw.trim().to_string()),
    };
    (Some(preview), after)
}
