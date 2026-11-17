//! 聊天标题栏群名称与人数解析、严格小群人数安全守卫与群标题匹配器

use common_types::constants::UI_LABEL_MESSAGE_LIST;
use common_types::{AdapterError, GroupTargetSpec, Result, UiNodeSnapshot};
use uia_core::constants::{CONTROL_TYPE_BUTTON, CONTROL_TYPE_GROUP, CONTROL_TYPE_TEXT};
use uia_core::CollectedNode;

use crate::constants::{CONTROL_TYPE_CUSTOM, MEMBER_COUNT_PAREN_REGEX, MEMBER_SIDEBAR_COUNT_REGEX};
use crate::parser;
use crate::profile;

/// 聊天标题栏单行控件允许的最大高度（px），用于过滤溢出顶部的多行聊天长气泡
const MAX_CHAT_HEADER_HEIGHT_PX: i32 = 46;

/// 从群名称或标题中提取最后一组括号人数提示（例如 `(5)` 或 `（8人）`）
pub fn parse_member_count(text: &str) -> Option<u32> {
    MEMBER_COUNT_PAREN_REGEX
        .captures_iter(text)
        .last()
        .and_then(|captures| captures.get(1))
        .and_then(|matched| matched.as_str().parse::<u32>().ok())
}

/// 从 QQNT 右侧群成员栏标题（如 `"群聊成员 9"`）解析群成员总数
pub fn parse_member_label_count(text: &str) -> Option<u32> {
    MEMBER_SIDEBAR_COUNT_REGEX
        .captures_iter(text)
        .last()
        .and_then(|captures| captures.get(1))
        .and_then(|matched| matched.as_str().parse::<u32>().ok())
}

/// 强制校验群目标规则中的 `strict_below_members` 与已核验成员数约束
pub fn passes_member_guard(spec: &GroupTargetSpec, detected: Option<u32>) -> Result<()> {
    if let Some(limit) = spec.strict_below_members {
        match detected {
            Some(count) if count >= limit => {
                return Err(AdapterError::MemberCountExceeded {
                    group: spec.group_name.clone(),
                    detected: Some(count),
                    limit,
                });
            }
            None if spec.require_member_count_verified => {
                return Err(AdapterError::MemberCountExceeded {
                    group: spec.group_name.clone(),
                    detected: None,
                    limit,
                });
            }
            _ => {}
        }
    }
    Ok(())
}

/// 对已附着或已展开聊天窗口执行的强一致成员数量安全守卫（存在上限约束时强制要求人数已识别）
pub(crate) fn verify_attached_member_guard(
    spec: &GroupTargetSpec,
    detected: Option<u32>,
) -> Result<()> {
    if spec.strict_below_members.is_some() && !spec.require_member_count_verified {
        let verified_spec = GroupTargetSpec {
            require_member_count_verified: true,
            ..spec.clone()
        };
        passes_member_guard(&verified_spec, detected)
    } else {
        passes_member_guard(spec, detected)
    }
}

/// 判断当前激活聊天标题是否与请求的群名称一致（自动剥离人数后缀对比）
pub fn group_title_matches(active_title: &str, requested_group: &str) -> bool {
    let requested_trimmed = requested_group.trim();
    if requested_trimmed.is_empty() {
        return false;
    }
    let active_trimmed = active_title.trim();
    if active_trimmed == requested_trimmed {
        return true;
    }
    let active_canonical = parser::canonical_group_name(active_trimmed);
    let requested_canonical = parser::canonical_group_name(requested_trimmed);
    !requested_canonical.is_empty() && active_canonical == requested_canonical
}

fn is_header_noise_name(name: &str) -> bool {
    let trimmed = name.trim();
    matches!(
        trimmed,
        "QQ" | "切换为经典模式"
            | "在线状态"
            | "语音通话"
            | "视频通话"
            | "屏幕共享"
            | "群应用"
            | "邀请加群"
            | "更多"
            | "最小化"
            | "最大化"
            | "还原"
            | "关闭"
    ) || trimmed.starts_with("天气:")
        || trimmed.starts_with("个性签名:")
        || trimmed.ends_with("的头像")
}

fn is_valid_header_title_candidate(text: &str) -> bool {
    let trimmed = text.trim();
    !trimmed.is_empty()
        && !is_header_noise_name(trimmed)
        && !trimmed.contains('\n')
        && !trimmed.contains('\r')
        && !trimmed.starts_with('[')
        && trimmed.chars().count() <= 48
}

/// 从当前激活窗口的平铺节点集合中读取聊天标题与群人数提示
pub fn read_chat_header(
    nodes: &[CollectedNode],
    win_rect: [i32; 4],
    is_standalone: bool,
    fallback_title: Option<&str>,
) -> (Option<String>, Option<u32>) {
    let snapshots: Vec<&UiNodeSnapshot> = nodes.iter().map(|node| &node.snapshot).collect();
    read_chat_header_from_snapshots(&snapshots, win_rect, is_standalone, fallback_title)
}

/// 直接基于快照切片从顶部聊天头与右侧成员统计提取活跃群名称与群成员人数
pub fn read_chat_header_from_snapshots(
    snapshots: &[&UiNodeSnapshot],
    win_rect: [i32; 4],
    is_standalone: bool,
    fallback_title: Option<&str>,
) -> (Option<String>, Option<u32>) {
    let eff_rect = profile::effective_window_rect_from_snapshots(win_rect, snapshots);
    let left = eff_rect[0] as f64;
    let top = eff_rect[1] as f64;
    let width = profile::rect_width(eff_rect) as f64;
    let height = profile::rect_height(eff_rect) as f64;
    let msg_list_rect = snapshots
        .iter()
        .copied()
        .find(|snap| {
            !snap.is_offscreen
                && profile::rect_is_valid(snap.rect)
                && (snap.name.trim() == UI_LABEL_MESSAGE_LIST
                    || snap.automation_id.trim().eq_ignore_ascii_case("ml-root"))
        })
        .map(|snap| snap.rect);

    let mut header_texts: Vec<(i32, i32, String)> = Vec::new();
    for snapshot in snapshots {
        let control_type = snapshot.control_type.trim();
        let automation_id = snapshot.automation_id.trim();
        if snapshot.is_offscreen
            || profile::is_system_chrome(snapshot)
            || profile::is_excluded_sidebar_container(snapshot)
            || snapshot.name.trim() == UI_LABEL_MESSAGE_LIST
            || automation_id.eq_ignore_ascii_case("ml-root")
            || control_type.eq_ignore_ascii_case("Document")
            || starts_with_ignore_ascii_case(automation_id, "msg-")
            || starts_with_ignore_ascii_case(automation_id, "ml-")
        {
            continue;
        }
        let is_header_control = control_type.eq_ignore_ascii_case(CONTROL_TYPE_TEXT)
            || control_type.eq_ignore_ascii_case("Heading")
            || control_type.eq_ignore_ascii_case(CONTROL_TYPE_BUTTON)
            || control_type.eq_ignore_ascii_case(CONTROL_TYPE_GROUP)
            || control_type.eq_ignore_ascii_case(CONTROL_TYPE_CUSTOM);
        if !is_header_control {
            continue;
        }
        if !profile::is_in_chat_header(snapshot.rect, eff_rect, is_standalone) {
            continue;
        }
        if profile::rect_height(snapshot.rect) > MAX_CHAT_HEADER_HEIGHT_PX {
            continue;
        }
        if let Some(ml_rect) = msg_list_rect {
            let overlaps_ml_horizontally =
                snapshot.rect[2] > ml_rect[0] + 4 && snapshot.rect[0] < ml_rect[2] - 4;
            if overlaps_ml_horizontally
                && (snapshot.rect[3] > ml_rect[1] + 2 || snapshot.rect[1] >= ml_rect[1] - 6)
            {
                continue;
            }
        }
        let (center_x, _) = profile::rect_center(snapshot.rect);
        if !is_standalone && center_x > left + width * 0.78 {
            continue;
        }
        let text = snapshot.name.trim();
        let is_pure_count =
            parse_member_count(text).is_some() && parser::canonical_group_name(text).is_empty();
        if is_pure_count || is_valid_header_title_candidate(text) {
            header_texts.push((snapshot.rect[1], snapshot.rect[0], text.to_string()));
        }
    }

    header_texts.sort_by_key(|(top_y, left_x, _)| (*top_y, *left_x));

    let mut direct_title: Option<String> = None;
    let mut direct_count: Option<u32> = None;

    for (_, _, text) in &header_texts {
        if let Some(count) = parse_member_count(text) {
            if !parser::canonical_group_name(text).is_empty()
                && is_valid_header_title_candidate(text)
            {
                direct_count = Some(count);
                direct_title = Some(text.clone());
                break;
            }
        }
    }

    if direct_title.is_none() || direct_count.is_none() {
        for (_, _, text) in &header_texts {
            if let Some(count) = parse_member_count(text) {
                if direct_count.is_none() {
                    direct_count = Some(count);
                }
                if direct_title.is_none()
                    && !parser::canonical_group_name(text).is_empty()
                    && is_valid_header_title_candidate(text)
                {
                    direct_title = Some(text.clone());
                }
            } else if direct_title.is_none() && is_valid_header_title_candidate(text) {
                direct_title = Some(text.clone());
            }
        }
    }

    if direct_count.is_none() && !header_texts.is_empty() {
        let joined = header_texts
            .iter()
            .map(|(_, _, text)| text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        direct_count = parse_member_count(&joined);
    }

    // 当标题栏未携带成员数量或拆分渲染时，回退读取右侧边栏中的 `"群聊成员 9"` 统计标签
    if direct_count.is_none() {
        for snapshot in snapshots {
            if snapshot.is_offscreen
                || profile::is_system_chrome(snapshot)
                || !profile::rect_is_valid(snapshot.rect)
            {
                continue;
            }
            let control_type = snapshot.control_type.trim();
            if !(control_type.eq_ignore_ascii_case(CONTROL_TYPE_TEXT)
                || control_type.eq_ignore_ascii_case("Heading")
                || control_type.eq_ignore_ascii_case(CONTROL_TYPE_GROUP))
            {
                continue;
            }
            let (center_x, center_y) = profile::rect_center(snapshot.rect);
            let min_x = if is_standalone {
                left
            } else {
                left + width * 0.28
            };
            if center_x >= min_x && center_y >= top && center_y < top + height * 0.40 {
                if let Some(count) = parse_member_label_count(snapshot.name.trim()) {
                    direct_count = Some(count);
                    break;
                }
            }
        }
    }

    if let Some(fallback) = fallback_title {
        let trimmed = fallback.trim();
        if !trimmed.is_empty() {
            if direct_title.is_none() {
                direct_title = Some(trimmed.to_string());
            }
            if direct_count.is_none() {
                direct_count = parse_member_count(trimmed);
            }
        }
    }

    (direct_title, direct_count)
}

fn collect_snapshot_refs<'a>(node: &'a UiNodeSnapshot, out: &mut Vec<&'a UiNodeSnapshot>) {
    out.push(node);
    if node.name.trim() == UI_LABEL_MESSAGE_LIST
        || node.automation_id.trim().eq_ignore_ascii_case("ml-root")
        || profile::is_excluded_sidebar_container(node)
    {
        return;
    }
    for child in &node.children {
        collect_snapshot_refs(child, out);
    }
}

/// 判断文本是否以指定 ASCII 前缀开头（忽略 ASCII 大小写并保证 UTF-8 边界安全）
fn starts_with_ignore_ascii_case(text: &str, prefix: &str) -> bool {
    text.len() >= prefix.len()
        && text.is_char_boundary(prefix.len())
        && text[..prefix.len()].eq_ignore_ascii_case(prefix)
}

/// 基于完整 UI 快照树读取当前聊天窗口标题与成员人数
pub fn read_chat_header_from_tree(
    root: &UiNodeSnapshot,
    win_rect: [i32; 4],
    is_standalone: bool,
    fallback_title: Option<&str>,
) -> (Option<String>, Option<u32>) {
    let mut snapshots = Vec::new();
    collect_snapshot_refs(root, &mut snapshots);
    read_chat_header_from_snapshots(&snapshots, win_rect, is_standalone, fallback_title)
}
