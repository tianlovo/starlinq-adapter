//! 左侧会话列表容器检索、嵌套行去重、会话字段解析与左上角搜索触发控件匹配

use std::collections::HashSet;

use common_types::constants::UI_LABEL_SESSION_LIST;
use common_types::{SessionSummary, UiNodeSnapshot};
use uia_core::constants::{
    CONTROL_TYPE_BUTTON, CONTROL_TYPE_GROUP, CONTROL_TYPE_LIST, CONTROL_TYPE_LIST_ITEM,
    CONTROL_TYPE_TREE_ITEM, CONTROL_TYPE_WINDOW,
};
use uia_core::{CollectedNode, UiNodeHandle};

use super::header_matcher::parse_member_count;
use crate::constants::{
    CONTROL_TYPE_CUSTOM, DATE_ONLY_REGEX, GROUP_PREVIEW_NOTICE_HINTS, GROUP_TITLE_SUFFIX_HINTS,
    SESSION_INLINE_SPLIT_REGEX, TIMESTAMP_ONLY_REGEX, UNREAD_COUNT_REGEX,
};
use crate::profile;

/// 会话标题不得命中的 QQNT 工具栏与系统界面控件名称黑名单
const SESSION_CHROME_DENY_LIST: &[&str] = &[
    "消息",
    "联系人",
    "空间",
    "小程序",
    "腾讯文档",
    "更多",
    "邮箱",
    "我的手机",
    "在线状态",
    "搜索",
    "表情",
    "文件",
    "图片",
    "红包",
    "语音消息",
    "语音输入文字",
    "机器人指令",
    "聊天记录",
    "发送",
    "消息列表",
    "会话列表",
    "群成员列表",
    "窗口控制区域",
    "关闭",
    "最小化",
    "最大化",
    "设置",
    "收藏",
    "游戏",
    "频道",
    "进入综合搜索",
    "综合搜索",
    "群聊",
    "查找用户、群聊等",
    "展开",
    "收起",
    "清空",
    "切换为经典模式",
];

fn is_pure_bracket_placeholder(text: &str) -> bool {
    let Some(inner) = text.strip_prefix('[').and_then(|s| s.strip_suffix(']')) else {
        return false;
    };
    let inner = inner.trim();
    if inner.is_empty() || inner.contains('[') || inner.contains(']') {
        return false;
    }
    inner.starts_with("表情:")
        || inner.starts_with("表情：")
        || matches!(
            inner,
            "图片"
                | "表情"
                | "动画表情"
                | "语音"
                | "视频"
                | "文件"
                | "离线文件"
                | "红包"
                | "QQ红包"
                | "转账"
                | "名片"
                | "推荐名片"
                | "位置"
                | "链接"
                | "小程序"
                | "音乐"
                | "聊天记录"
                | "合并转发"
                | "群收款"
                | "礼物"
                | "戳一戳"
                | "应用消息"
                | "未知消息"
                | "转发"
                | "闪照"
        )
}

fn is_weekday_or_relative_day(token: &str) -> bool {
    matches!(
        token,
        "今天"
            | "昨天"
            | "前天"
            | "星期一"
            | "星期二"
            | "星期三"
            | "星期四"
            | "星期五"
            | "星期六"
            | "星期日"
            | "星期天"
            | "周一"
            | "周二"
            | "周三"
            | "周四"
            | "周五"
            | "周六"
            | "周日"
            | "周天"
    )
}

fn is_numeric_date_token(text: &str) -> bool {
    for sep in ['-', '/'] {
        let parts: Vec<&str> = text.split(sep).collect();
        if parts.len() == 2
            && parts.iter().all(|part| {
                (1..=2).contains(&part.len()) && part.chars().all(|c| c.is_ascii_digit())
            })
        {
            if let (Ok(month), Ok(day)) = (parts[0].parse::<u32>(), parts[1].parse::<u32>()) {
                return (1..=12).contains(&month) && (1..=31).contains(&day);
            }
        }
        if parts.len() == 3
            && parts[0].len() == 4
            && parts[0].chars().all(|c| c.is_ascii_digit())
            && parts[1..].iter().all(|part| {
                (1..=2).contains(&part.len()) && part.chars().all(|c| c.is_ascii_digit())
            })
        {
            if let (Ok(month), Ok(day)) = (parts[1].parse::<u32>(), parts[2].parse::<u32>()) {
                return (1..=12).contains(&month) && (1..=31).contains(&day);
            }
        }
    }
    false
}

fn is_pure_datetime_title(text: &str) -> bool {
    if TIMESTAMP_ONLY_REGEX.is_match(text)
        || DATE_ONLY_REGEX.is_match(text)
        || is_weekday_or_relative_day(text)
        || is_numeric_date_token(text)
    {
        return true;
    }
    let parts: Vec<&str> = text.split_whitespace().collect();
    parts.len() == 2
        && is_weekday_or_relative_day(parts[0])
        && TIMESTAMP_ONLY_REGEX.is_match(parts[1])
}

/// 校验会话标题是否为合法聊天名称，拦截工具栏控件、头像按钮、快捷键提示、纯占位符及纯时间戳文本
pub fn is_valid_session_title(title: &str) -> bool {
    let trimmed = title.trim();
    if trimmed.is_empty()
        || trimmed.contains('\n')
        || trimmed.contains('\r')
        || trimmed.chars().count() > 36
    {
        return false;
    }
    if SESSION_CHROME_DENY_LIST.contains(&trimmed) {
        return false;
    }
    if trimmed.starts_with("[qq-uia-adapter]")
        || trimmed.ends_with("的头像")
        || trimmed.ends_with("弹出菜单")
        || trimmed.contains("Ctrl +")
        || trimmed.starts_with("个性签名:")
        || trimmed.starts_with("个性签名：")
        || trimmed.starts_with("天气:")
        || trimmed.starts_with("天气：")
    {
        return false;
    }
    !is_pure_bracket_placeholder(trimmed) && !is_pure_datetime_title(trimmed)
}

/// 左侧会话列表或搜索下拉列表中匹配到的单行会话候选节点
#[derive(Debug, Clone)]
pub(crate) struct SessionCandidate<'a> {
    pub(crate) raw_text: String,
    pub(crate) title: String,
    pub(crate) preview: Option<String>,
    pub(crate) unread_count: Option<u32>,
    pub(crate) member_count_hint: Option<u32>,
    pub(crate) is_group_hint: Option<bool>,
    pub(crate) has_at_me_hint: bool,
    pub(crate) has_at_all_hint: bool,
    pub(crate) rect: [i32; 4],
    pub(crate) element: Option<&'a UiNodeHandle>,
    pub(crate) control_priority: u8,
}

/// 判断文本是否包含目标群名（支持精准子串与 ASCII 大小写不敏感匹配）
pub(crate) fn text_contains_target(haystack: &str, target: &str) -> bool {
    let target_trimmed = target.trim();
    if target_trimmed.is_empty() {
        return false;
    }
    if haystack.contains(target_trimmed) {
        return true;
    }
    haystack
        .to_ascii_lowercase()
        .contains(&target_trimmed.to_ascii_lowercase())
}

fn control_priority(control_type: &str) -> u8 {
    let trimmed = control_type.trim();
    if trimmed.eq_ignore_ascii_case(CONTROL_TYPE_LIST_ITEM)
        || trimmed.eq_ignore_ascii_case(CONTROL_TYPE_TREE_ITEM)
    {
        3
    } else if trimmed.eq_ignore_ascii_case(CONTROL_TYPE_GROUP)
        || trimmed.eq_ignore_ascii_case(CONTROL_TYPE_CUSTOM)
    {
        2
    } else {
        1
    }
}

fn gather_text_inside_rect(snapshots: &[&UiNodeSnapshot], container_rect: [i32; 4]) -> Vec<String> {
    let mut parts = Vec::new();
    for snapshot in snapshots {
        if snapshot.is_offscreen || profile::is_system_chrome(snapshot) {
            continue;
        }
        if !profile::rect_contains(container_rect, snapshot.rect) {
            continue;
        }
        for text in [
            snapshot.name.trim(),
            snapshot.value.as_deref().unwrap_or("").trim(),
        ] {
            if !text.is_empty() && parts.last().map(String::as_str) != Some(text) {
                parts.push(text.to_string());
            }
        }
    }
    parts
}

fn parse_session_fields(raw_text: &str) -> (String, Option<String>, Option<u32>, Option<u32>) {
    let lines: Vec<&str> = raw_text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let mut title = lines
        .first()
        .copied()
        .unwrap_or_else(|| raw_text.trim())
        .to_string();
    let mut preview = if lines.len() > 1 {
        Some(lines[1..].join(" "))
    } else {
        None
    };

    // QQNT 会话行常将名称、时间与最新消息拼接成单行文本，按时间预览边界剥离以防污染群名索引
    if lines.len() == 1 {
        let split_parts = SESSION_INLINE_SPLIT_REGEX
            .captures(title.trim())
            .and_then(|captures| {
                let clean_title = captures.name("title")?.as_str().trim().to_string();
                let inline_preview = captures.name("preview")?.as_str().trim().to_string();
                (!clean_title.is_empty() && !inline_preview.is_empty())
                    .then_some((clean_title, inline_preview))
            });
        if let Some((clean_title, inline_preview)) = split_parts {
            title = clean_title;
            preview = Some(inline_preview);
        }
    }

    let unread_count = UNREAD_COUNT_REGEX
        .captures(raw_text)
        .and_then(|captures| captures.get(1))
        .and_then(|matched| matched.as_str().parse::<u32>().ok())
        .or_else(|| {
            if lines.len() >= 4 {
                let tail = lines[lines.len() - 1];
                if (1..=3).contains(&tail.len()) && tail.chars().all(|ch| ch.is_ascii_digit()) {
                    return tail.parse::<u32>().ok().filter(|&count| count > 0);
                }
            }
            None
        });
    let member_count_hint = parse_member_count(&title).or_else(|| parse_member_count(raw_text));
    (title, preview, unread_count, member_count_hint)
}

/// 综合成员数、标题后缀及多行/空格拼接发送者前缀特征推断会话是否为群聊
pub(crate) fn infer_is_group_session(
    title: &str,
    raw_text: &str,
    preview: Option<&str>,
    member_count_hint: Option<u32>,
) -> Option<bool> {
    if member_count_hint.is_some() {
        return Some(true);
    }

    let trimmed_title = title.trim();
    if GROUP_TITLE_SUFFIX_HINTS
        .iter()
        .any(|suffix| trimmed_title.ends_with(suffix))
    {
        return Some(true);
    }

    let preview_str = preview.unwrap_or("");
    if GROUP_PREVIEW_NOTICE_HINTS
        .iter()
        .any(|hint| raw_text.contains(hint) || preview_str.contains(hint))
    {
        return Some(true);
    }

    let lines: Vec<&str> = raw_text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    if lines.len() >= 4 && (lines[2].ends_with('：') || lines.contains(&"回应了你的")) {
        return Some(true);
    }

    if !preview_str.is_empty()
        && (preview_str.contains(" 回应了你的 ")
            || preview_str
                .split_whitespace()
                .skip(1)
                .any(|tok| tok.chars().count() >= 2 && tok.ends_with('：')))
    {
        return Some(true);
    }

    None
}

fn rect_strictly_inside(container: [i32; 4], child: [i32; 4]) -> bool {
    profile::rect_is_valid(container)
        && profile::rect_is_valid(child)
        && child[0] >= container[0]
        && child[1] >= container[1]
        && child[2] <= container[2]
        && child[3] <= container[3]
}

fn find_session_list_container<'a>(
    snapshots: &[&'a UiNodeSnapshot],
    eff_rect: [i32; 4],
) -> Option<(usize, &'a UiNodeSnapshot)> {
    snapshots
        .iter()
        .enumerate()
        .filter(|(_, snapshot)| {
            if snapshot.is_offscreen || !profile::rect_is_valid(snapshot.rect) {
                return false;
            }
            let control_type = snapshot.control_type.trim();
            (control_type.eq_ignore_ascii_case(CONTROL_TYPE_WINDOW)
                || control_type.eq_ignore_ascii_case(CONTROL_TYPE_LIST))
                && snapshot.name.trim() == UI_LABEL_SESSION_LIST
                && profile::rect_width(snapshot.rect) > 0
                && profile::rect_height(snapshot.rect) > 0
                && (!profile::rect_is_valid(eff_rect)
                    || snapshot.rect == eff_rect
                    || profile::is_in_left_session_pane(snapshot.rect, eff_rect))
        })
        .max_by_key(|(_, snapshot)| profile::rect_area(snapshot.rect))
        .map(|(index, snapshot)| (index, *snapshot))
}

fn prune_nested_session_wrappers(
    candidates: Vec<SessionCandidate<'_>>,
) -> Vec<SessionCandidate<'_>> {
    // 剔除覆盖 2 个及以上纵向上界明显分离候选行的多行滚动包装容器
    let rects: Vec<[i32; 4]> = candidates.iter().map(|cand| cand.rect).collect();
    let without_wrappers: Vec<SessionCandidate<'_>> = candidates
        .into_iter()
        .enumerate()
        .filter(|(outer_idx, candidate)| {
            let mut contained_tops: Vec<i32> = Vec::new();
            for (inner_idx, other_rect) in rects.iter().enumerate() {
                if *outer_idx == inner_idx || *other_rect == candidate.rect {
                    continue;
                }
                if profile::rect_contains(candidate.rect, *other_rect)
                    && !contained_tops
                        .iter()
                        .any(|top_y| (*top_y - other_rect[1]).abs() < 20)
                {
                    contained_tops.push(other_rect[1]);
                }
            }
            contained_tops.len() < 2
        })
        .map(|(_, candidate)| candidate)
        .collect();

    // 在保留下的单行候选节点中仅保留最外层完整行矩形
    let kept_rects: Vec<[i32; 4]> = without_wrappers.iter().map(|cand| cand.rect).collect();
    without_wrappers
        .into_iter()
        .enumerate()
        .filter(|(idx, candidate)| {
            !kept_rects.iter().enumerate().any(|(other_idx, outer)| {
                *idx != other_idx
                    && *outer != candidate.rect
                    && profile::rect_contains(*outer, candidate.rect)
            })
        })
        .map(|(_, candidate)| candidate)
        .collect()
}

/// 根据平铺节点快照构建去重排序后的左侧会话候选清单
pub(crate) fn build_session_candidates<'a>(
    snapshots: &[&'a UiNodeSnapshot],
    elements: Option<&[&'a UiNodeHandle]>,
    win_rect: [i32; 4],
    allow_unscoped_search_fallback: bool,
) -> Vec<SessionCandidate<'a>> {
    let eff_rect = profile::effective_window_rect_from_snapshots(win_rect, snapshots);
    let scoped_container = find_session_list_container(snapshots, eff_rect);
    let mut candidates = Vec::new();

    if let Some((container_idx, container)) =
        scoped_container.filter(|_| !allow_unscoped_search_fallback)
    {
        let container_width = profile::rect_width(container.rect);
        let min_row_width = (container_width * 72 / 100).max(80);
        for (index, snapshot) in snapshots.iter().enumerate() {
            if index == container_idx
                || snapshot.is_offscreen
                || profile::is_system_chrome(snapshot)
            {
                continue;
            }
            if !profile::is_session_list_control_type(&snapshot.control_type) {
                continue;
            }
            if !rect_strictly_inside(container.rect, snapshot.rect) {
                continue;
            }
            let row_width = profile::rect_width(snapshot.rect);
            let row_height = profile::rect_height(snapshot.rect);
            if row_width < min_row_width || !(28..=220).contains(&row_height) {
                continue;
            }

            let mut raw_text = snapshot.name.trim().to_string();
            if raw_text.is_empty() {
                raw_text = gather_text_inside_rect(snapshots, snapshot.rect).join("\n");
            }
            if raw_text.trim().is_empty() {
                continue;
            }
            let (title, preview, unread_count, member_count_hint) = parse_session_fields(&raw_text);
            if !is_valid_session_title(&title) {
                continue;
            }
            let is_group_hint =
                infer_is_group_session(&title, &raw_text, preview.as_deref(), member_count_hint);
            let (has_at_me_hint, has_at_all_hint) =
                detect_session_at_hints(&raw_text, preview.as_deref());

            candidates.push(SessionCandidate {
                raw_text,
                title,
                preview,
                unread_count,
                member_count_hint,
                is_group_hint,
                has_at_me_hint,
                has_at_all_hint,
                rect: snapshot.rect,
                element: elements.and_then(|els| els.get(index).copied()),
                control_priority: control_priority(&snapshot.control_type),
            });
        }
    } else if allow_unscoped_search_fallback {
        let top = eff_rect[1] as f64;
        let height = profile::rect_height(eff_rect) as f64;
        for (index, snapshot) in snapshots.iter().enumerate() {
            if snapshot.is_offscreen
                || profile::is_system_chrome(snapshot)
                || profile::is_excluded_sidebar_container(snapshot)
            {
                continue;
            }
            if !profile::is_session_list_control_type(&snapshot.control_type) {
                continue;
            }
            if !profile::is_in_left_session_pane(snapshot.rect, eff_rect) {
                continue;
            }
            let (_, center_y) = profile::rect_center(snapshot.rect);
            if center_y < top + height * 0.14 || profile::rect_width(snapshot.rect) < 100 {
                continue;
            }

            let mut raw_text = snapshot.name.trim().to_string();
            if raw_text.is_empty() {
                raw_text = gather_text_inside_rect(snapshots, snapshot.rect).join("\n");
            }
            if raw_text.trim().is_empty() {
                continue;
            }

            let (title, preview, unread_count, member_count_hint) = parse_session_fields(&raw_text);
            if !is_valid_session_title(&title) {
                continue;
            }
            let is_group_hint =
                infer_is_group_session(&title, &raw_text, preview.as_deref(), member_count_hint);
            let (has_at_me_hint, has_at_all_hint) =
                detect_session_at_hints(&raw_text, preview.as_deref());

            candidates.push(SessionCandidate {
                raw_text,
                title,
                preview,
                unread_count,
                member_count_hint,
                is_group_hint,
                has_at_me_hint,
                has_at_all_hint,
                rect: snapshot.rect,
                element: elements.and_then(|els| els.get(index).copied()),
                control_priority: control_priority(&snapshot.control_type),
            });
        }
    }

    // 针对同一区域与相同标题的重叠嵌套节点去重，保留优先级更高的行节点
    candidates.sort_by_key(|cand| std::cmp::Reverse(cand.control_priority));
    let mut seen_keys: HashSet<(String, [i32; 4])> = HashSet::new();
    let mut deduped = Vec::new();
    for candidate in candidates {
        let key = (candidate.title.clone(), candidate.rect);
        if seen_keys.insert(key) {
            deduped.push(candidate);
        }
    }

    if scoped_container.is_some() && !allow_unscoped_search_fallback {
        deduped = prune_nested_session_wrappers(deduped);
    }

    deduped.sort_by_key(|cand| (cand.rect[1], cand.rect[0]));
    deduped
}

/// 在平铺节点中检索左上角搜索框句柄以备会话列表未命中时执行搜索回退
pub(crate) fn find_search_element(
    nodes: &[CollectedNode],
    win_rect: [i32; 4],
) -> Option<&UiNodeHandle> {
    let eff_rect = profile::effective_window_rect(win_rect, nodes);
    nodes
        .iter()
        .find(|collected| profile::is_search_input_candidate(&collected.snapshot, eff_rect))
        .map(|collected| &collected.element)
}

fn is_clear_search_button_snapshot(snapshot: &UiNodeSnapshot, eff_rect: [i32; 4]) -> bool {
    !snapshot.is_offscreen
        && snapshot.is_enabled
        && snapshot
            .control_type
            .trim()
            .eq_ignore_ascii_case(CONTROL_TYPE_BUTTON)
        && snapshot.name.trim() == "清空"
        && profile::is_in_top_left_search(snapshot.rect, eff_rect)
}

/// 在平铺节点中检索左上角搜索栏右侧的 `清空` 按钮句柄
pub(crate) fn find_clear_search_element(
    nodes: &[CollectedNode],
    win_rect: [i32; 4],
) -> Option<&UiNodeHandle> {
    let eff_rect = profile::effective_window_rect(win_rect, nodes);
    nodes
        .iter()
        .find(|collected| is_clear_search_button_snapshot(&collected.snapshot, eff_rect))
        .map(|collected| &collected.element)
}

/// 检查左上角搜索栏是否处于激活筛选状态，避免将搜索结果条目写入常规群/好友缓存表
pub fn is_search_mode_active_snapshots(snapshots: &[&UiNodeSnapshot], win_rect: [i32; 4]) -> bool {
    let eff_rect = profile::effective_window_rect_from_snapshots(win_rect, snapshots);
    for snapshot in snapshots {
        if snapshot.is_offscreen || profile::is_system_chrome(snapshot) {
            continue;
        }
        if is_clear_search_button_snapshot(snapshot, eff_rect) {
            return true;
        }
        if profile::is_search_input_candidate(snapshot, eff_rect) {
            if let Some(val) = snapshot.value.as_deref() {
                let cleaned = val.trim();
                if !cleaned.is_empty() && cleaned != "搜索" && cleaned != "Search" {
                    return true;
                }
            }
        }
        if profile::is_in_left_session_pane(snapshot.rect, eff_rect)
            && matches!(snapshot.name.trim(), "进入综合搜索" | "查找用户、群聊等")
        {
            return true;
        }
    }
    false
}

/// 检查平铺采集节点中左上角搜索栏是否处于激活筛选状态
pub fn is_search_mode_active(nodes: &[CollectedNode], win_rect: [i32; 4]) -> bool {
    let snapshots: Vec<&UiNodeSnapshot> = nodes.iter().map(|node| &node.snapshot).collect();
    is_search_mode_active_snapshots(&snapshots, win_rect)
}

/// 检查完整 UI 快照树中左上角搜索栏是否处于激活筛选状态
pub fn is_search_mode_active_in_tree(root: &UiNodeSnapshot) -> bool {
    let mut snapshots = Vec::new();
    collect_tree_snapshots(root, &mut snapshots);
    is_search_mode_active_snapshots(&snapshots, root.rect)
}

/// 从平铺采集的 UIA/MSAA 节点列表中提取左侧可见会话列表摘要
pub fn extract_sessions(nodes: &[CollectedNode], win_rect: [i32; 4]) -> Vec<SessionSummary> {
    let snapshots: Vec<&UiNodeSnapshot> = nodes.iter().map(|node| &node.snapshot).collect();
    extract_sessions_from_snapshots(&snapshots, win_rect)
}

fn collect_tree_snapshots<'a>(node: &'a UiNodeSnapshot, out: &mut Vec<&'a UiNodeSnapshot>) {
    out.push(node);
    for child in &node.children {
        collect_tree_snapshots(child, out);
    }
}

/// 从完整 UI 节点树中严格定位 `会话列表` 容器并提取有效会话摘要列表
pub fn extract_sessions_from_tree(root: &UiNodeSnapshot) -> Vec<SessionSummary> {
    let mut snapshots = Vec::new();
    collect_tree_snapshots(root, &mut snapshots);
    extract_sessions_from_snapshots(&snapshots, root.rect)
}

/// 直接基于快照切片提取左侧会话摘要（无 COM 句柄依赖，供单元测试与纯分析场景使用）
pub fn extract_sessions_from_snapshots(
    snapshots: &[&UiNodeSnapshot],
    win_rect: [i32; 4],
) -> Vec<SessionSummary> {
    build_session_candidates(snapshots, None, win_rect, false)
        .into_iter()
        .map(|candidate| SessionSummary {
            title: candidate.title,
            raw_name: candidate.raw_text,
            preview: candidate.preview,
            unread_count: candidate.unread_count,
            member_count_hint: candidate.member_count_hint,
            rect: candidate.rect,
            is_group_hint: candidate.is_group_hint,
            has_at_me_hint: candidate.has_at_me_hint,
            has_at_all_hint: candidate.has_at_all_hint,
        })
        .collect()
}

/// 识别左侧会话原始标题与预览文本中的 `[有人@我]`、`[有全体消息]` 等紧急提示标识
fn detect_session_at_hints(raw_text: &str, preview: Option<&str>) -> (bool, bool) {
    let preview_str = preview.unwrap_or("");
    let has_at_me = [raw_text, preview_str]
        .iter()
        .any(|s| s.contains("[有人@我]") || s.contains("[特别关心]") || s.contains("@我"));
    let has_at_all = [raw_text, preview_str]
        .iter()
        .any(|s| s.contains("[有全体消息]") || s.contains("[全体成员]") || s.contains("@全体成员"));
    (has_at_me, has_at_all)
}
