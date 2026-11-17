//! QQNT 9.9.31 弹窗发现、工具栏按钮定位、消息气泡坐标定位及光标安全点击公共工具集

use std::collections::HashSet;
use std::path::Path;
use std::thread;
use std::time::Duration;

use common_types::{AdapterError, MessageSegment, Result, UiNodeSnapshot};
use uia_core::constants::{
    CONTROL_TYPE_BUTTON, CONTROL_TYPE_IMAGE, CONTROL_TYPE_LIST_ITEM, CONTROL_TYPE_MENU_ITEM,
    CONTROL_TYPE_TAB_ITEM, CONTROL_TYPE_TEXT,
};
use uia_core::{
    click_point_cursor_restore_verified, scroll_point_cursor_restore_verified,
    send_escape_key_verified, win32, SimulatedClickKind, UiaSession, VerifiedWindowIdentity,
};

use crate::constants::{CONTROL_TYPE_CUSTOM, INTERNAL_SCAN_DEPTH};
use crate::parser::{self, ParsedTreeMessage};
use crate::profile;

/// 从主窗口或同进程浮层弹窗树中匹配到的可交互 UI 控件定位摘要
#[derive(Debug, Clone)]
pub(crate) struct MatchedUiTarget {
    pub(crate) hwnd: isize,
    pub(crate) name: String,
    pub(crate) control_type: String,
    pub(crate) automation_id: String,
    pub(crate) rect: [i32; 4],
}

/// 在已打开会话的可见聊天消息历史区域中匹配到的一条消息气泡及其坐标锚点
#[derive(Debug, Clone)]
pub(crate) struct VisibleBubbleEntry {
    pub(crate) sender: String,
    pub(crate) content: String,
    pub(crate) is_self: bool,
    pub(crate) bubble_rect: [i32; 4],
    pub(crate) avatar_rect: Option<[i32; 4]>,
}

/// 计算合法屏幕矩形的中心点像素坐标，无效空矩形返回 `None`
pub(crate) fn rect_center(rect: [i32; 4]) -> Option<(i32, i32)> {
    let [l, t, r, b] = rect;
    if r <= l || b <= t {
        return None;
    }
    Some(((l + r) / 2, (t + b) / 2))
}

/// 计算富文本输入栏右下方安全聚焦点击坐标，使插入光标落于已有内联内容末尾而不全选覆盖
pub(crate) fn composer_append_click_point(rect: [i32; 4]) -> Option<(i32, i32)> {
    let [l, t, r, b] = rect;
    if r <= l || b <= t {
        return None;
    }
    let click_x = (r - 24).clamp(l + 8, r - 2);
    let click_y = (b - 14).clamp(t + 6, b - 2);
    Some((click_x, click_y))
}

/// 使用带空闲保护与瞬时恢复的光标模拟策略点击指定像素坐标
pub(crate) fn click_point_verified(
    identity: &VerifiedWindowIdentity,
    screen_x: i32,
    screen_y: i32,
    kind: SimulatedClickKind,
    allow_popup_same_pid: bool,
) -> Result<()> {
    click_point_cursor_restore_verified(identity, screen_x, screen_y, kind, allow_popup_same_pid)
}

/// 使用带空闲保护与瞬时恢复的光标模拟策略点击指定矩形中心点
pub(crate) fn click_rect_verified(
    identity: &VerifiedWindowIdentity,
    rect: [i32; 4],
    kind: SimulatedClickKind,
    allow_popup_same_pid: bool,
) -> Result<()> {
    let (x, y) = rect_center(rect).ok_or_else(|| {
        AdapterError::UiaFailure(format!("cannot click empty or invalid UI rect: {rect:?}"))
    })?;
    click_point_verified(identity, x, y, kind, allow_popup_same_pid)
}

/// 使用带空闲保护与瞬时恢复的光标模拟策略点击已定位目标（若目标位于独立弹窗句柄则自动绑定该句柄校验）
pub(crate) fn click_matched_target_verified(
    fallback_identity: &VerifiedWindowIdentity,
    target: &MatchedUiTarget,
    kind: SimulatedClickKind,
    allow_mock: bool,
) -> Result<()> {
    if target.hwnd != 0 && target.hwnd != fallback_identity.hwnd() {
        let popup_identity = VerifiedWindowIdentity::verify_and_bind(target.hwnd, allow_mock)?;
        click_rect_verified(&popup_identity, target.rect, kind, true)
    } else {
        click_rect_verified(fallback_identity, target.rect, kind, true)
    }
}

/// 尽力向当前绑定窗口发送 `Escape` 键以关闭表情、右键菜单或 `@` 浮层弹窗
pub(crate) fn dismiss_popups_best_effort(identity: &VerifiedWindowIdentity) {
    let _ = send_escape_key_verified(identity);
    thread::sleep(Duration::from_millis(80));
}

/// 枚举与主窗口同 UI 进程的可见有效弹窗并采集各自 UIA 快照树（跳过辅助子进程窗口与微尺寸隐藏窗）
pub(crate) fn scan_same_pid_popup_trees(
    session: &UiaSession,
    main_hwnd: isize,
    pids: &[u32],
    max_depth: usize,
) -> Vec<(isize, String, String, UiNodeSnapshot)> {
    let main_pid = win32::get_window_pid(main_hwnd);
    let query_pids: &[u32] = if main_pid != 0 {
        std::slice::from_ref(&main_pid)
    } else {
        pids
    };
    let entries = win32::enumerate_all_windows_for_pids(query_pids);
    let mut popups = Vec::new();
    let mut main_entry = None;

    for entry in entries {
        let [l, t, r, b] = entry.rect;
        if !entry.is_visible || entry.is_iconic || entry.dwm_cloaked != 0 || r <= l || b <= t {
            continue;
        }
        if entry.hwnd == main_hwnd {
            main_entry = Some(entry);
        } else if (r - l) >= 40 && (b - t) >= 24 {
            popups.push(entry);
        }
    }
    if let Some(main_win) = main_entry {
        popups.push(main_win);
    }

    let mut out = Vec::with_capacity(popups.len());
    for win in popups {
        let allow_unocclude = win.hwnd != main_hwnd
            && (win.rect[2] - win.rect[0]) >= 240
            && (win.rect[3] - win.rect[1]) >= 180;
        if let Ok((snapshot, _guard)) =
            crate::matcher::wake_and_dump_tree_ext(session, win.hwnd, max_depth, allow_unocclude)
        {
            out.push((win.hwnd, win.class_name, win.title, snapshot));
        }
    }
    out
}

fn walk_targets_filtered(
    hwnd: isize,
    node: &UiNodeSnapshot,
    pred: &dyn Fn(&UiNodeSnapshot) -> bool,
    skip_static_panes: bool,
    out: &mut Vec<MatchedUiTarget>,
) {
    if node.is_offscreen {
        return;
    }
    if skip_static_panes
        && (node.automation_id.trim().eq_ignore_ascii_case("ml-root")
            || matches!(
                node.name.trim(),
                "消息列表" | "会话列表" | "群成员列表" | "窗口控制区域"
            ))
    {
        return;
    }
    if profile::rect_is_valid(node.rect) && pred(node) {
        out.push(MatchedUiTarget {
            hwnd,
            name: node.name.trim().to_string(),
            control_type: node.control_type.trim().to_string(),
            automation_id: node.automation_id.trim().to_string(),
            rect: node.rect,
        });
    }
    for child in &node.children {
        walk_targets_filtered(hwnd, child, pred, skip_static_panes, out);
    }
}

/// 在单个快照树中按给定谓词深度收集可见且具有合法坐标框的 UI 目标节点
pub(crate) fn find_targets_in_snapshot(
    hwnd: isize,
    root: &UiNodeSnapshot,
    pred: &dyn Fn(&UiNodeSnapshot) -> bool,
) -> Vec<MatchedUiTarget> {
    let mut out = Vec::new();
    walk_targets_filtered(hwnd, root, pred, false, &mut out);
    out
}

/// 在单个快照树中仅检索叠加浮层/面板候选节点（自动跳过静态聊天消息列表、左侧会话栏、群成员栏与窗口控制栏）
pub(crate) fn find_overlay_targets_in_snapshot(
    hwnd: isize,
    root: &UiNodeSnapshot,
    pred: &dyn Fn(&UiNodeSnapshot) -> bool,
) -> Vec<MatchedUiTarget> {
    let mut out = Vec::new();
    walk_targets_filtered(hwnd, root, pred, true, &mut out);
    out
}

/// 从输入栏正上方工具条带提取全部可见功能按钮（按横坐标从左到右排序）
pub(crate) fn collect_toolbar_buttons(
    tree: &UiNodeSnapshot,
    editor_rect: [i32; 4],
) -> Vec<MatchedUiTarget> {
    let band_top = editor_rect[1] - 110;
    let band_bottom = editor_rect[1] + 24;
    let mut buttons = find_targets_in_snapshot(0, tree, &|node| {
        let ct = node.control_type.trim();
        let is_clickable = ct.eq_ignore_ascii_case(CONTROL_TYPE_BUTTON)
            || ct.eq_ignore_ascii_case(CONTROL_TYPE_MENU_ITEM)
            || ct.eq_ignore_ascii_case(CONTROL_TYPE_CUSTOM)
            || ct.eq_ignore_ascii_case(CONTROL_TYPE_TAB_ITEM)
            || ct.eq_ignore_ascii_case(CONTROL_TYPE_IMAGE);
        if !is_clickable || !node.is_enabled {
            return false;
        }
        let width = profile::rect_width(node.rect);
        let height = profile::rect_height(node.rect);
        let [l, t, r, b] = node.rect;
        l >= editor_rect[0] - 40
            && r <= editor_rect[2] + 40
            && t >= band_top
            && b <= band_bottom
            && (12..=120).contains(&width)
            && (12..=80).contains(&height)
    });
    buttons.sort_by_key(|btn| (btn.rect[0], btn.rect[1]));
    buttons
}

/// 在输入栏上方工具条中按关键词优先定位目标按钮（例如 `表情`、`截图`、`文件`、`抖一抖`）
pub(crate) fn locate_toolbar_button(
    tree: &UiNodeSnapshot,
    editor_rect: [i32; 4],
    keywords: &[&str],
) -> Option<MatchedUiTarget> {
    let buttons = collect_toolbar_buttons(tree, editor_rect);
    for kw in keywords {
        let kw_lower = kw.to_ascii_lowercase();
        if let Some(hit) = buttons.iter().find(|btn| {
            btn.name.to_ascii_lowercase().contains(&kw_lower)
                || btn.automation_id.to_ascii_lowercase().contains(&kw_lower)
        }) {
            return Some(hit.clone());
        }
    }
    None
}

fn compute_clickable_message_viewport(
    tree: &UiNodeSnapshot,
    win_rect: [i32; 4],
    is_standalone: bool,
) -> Option<[i32; 4]> {
    let has_ml_root = !find_targets_in_snapshot(0, tree, &|n| {
        n.automation_id.trim().eq_ignore_ascii_case("ml-root") || n.name.trim() == "消息列表"
    })
    .is_empty();
    let hist_rect = locate_message_list_scroll_point(tree, win_rect, is_standalone)
        .map(|(_, _, rect)| rect)
        .filter(|rect| profile::rect_is_valid(*rect));

    if has_ml_root && profile::rect_is_valid(win_rect) {
        let base = hist_rect.unwrap_or(win_rect);
        let vp = [
            base[0].max(win_rect[0] + 4),
            base[1].max(win_rect[1] + 56) + 4,
            base[2].min(win_rect[2] - 4),
            base[3].min(win_rect[3] - 4) - 4,
        ];
        if profile::rect_is_valid(vp) {
            return Some(vp);
        }
    }

    hist_rect.or_else(|| profile::rect_is_valid(win_rect).then_some(win_rect))
}

/// 在当前已平铺/嵌套采集到的聊天树中提取带屏幕坐标框的可见消息气泡清单（按可点击视口裁剪）
pub(crate) fn collect_visible_bubbles(
    tree: &UiNodeSnapshot,
    win_rect: [i32; 4],
    is_standalone: bool,
    group_name: &str,
) -> Vec<VisibleBubbleEntry> {
    let vp_rect_opt = compute_clickable_message_viewport(tree, win_rect, is_standalone);
    let parsed: Vec<ParsedTreeMessage> =
        parser::extract_rich_messages_from_tree(tree, win_rect, is_standalone, group_name);
    let mut entries = Vec::with_capacity(parsed.len());
    for item in parsed {
        let Some(raw_rect) = item.bubble_rect.filter(|r| profile::rect_is_valid(*r)) else {
            continue;
        };
        let vis_rect = match vp_rect_opt {
            Some(vp) => [
                raw_rect[0].max(vp[0]),
                raw_rect[1].max(vp[1]),
                raw_rect[2].min(vp[2]),
                raw_rect[3].min(vp[3]),
            ],
            None => raw_rect,
        };
        if !profile::rect_is_valid(vis_rect) {
            continue;
        }
        let raw_w = profile::rect_width(raw_rect);
        let raw_h = profile::rect_height(raw_rect);
        let vis_w = profile::rect_width(vis_rect);
        let vis_h = profile::rect_height(vis_rect);
        let min_vis_h = if raw_h >= 80 { 40 } else { raw_h.min(14) };
        if vis_w < raw_w.min(16) || vis_h < min_vis_h {
            continue;
        }
        entries.push(VisibleBubbleEntry {
            sender: item.message.sender,
            content: item.message.content,
            is_self: item.message.is_self,
            bubble_rect: vis_rect,
            avatar_rect: item.avatar_rect.filter(|r| profile::rect_is_valid(*r)),
        });
    }
    entries
}

/// 按发送者与文本摘录在可见消息气泡中定位最匹配的气泡坐标框（由新到旧检索）
pub(crate) fn locate_visible_message_bubble(
    bubbles: &[VisibleBubbleEntry],
    sender_hint: &str,
    excerpt_hint: &str,
    raw_text_hint: &str,
) -> Option<[i32; 4]> {
    let norm_raw = parser::normalize_message_text(raw_text_hint);
    let norm_excerpt = parser::normalize_message_text(excerpt_hint).replace("...", "");
    let sender_trim = sender_hint.trim();

    if !norm_raw.is_empty() {
        for bubble in bubbles.iter().rev() {
            let bub_norm = parser::normalize_message_text(&bubble.content);
            let sender_ok = sender_trim.is_empty()
                || bubble.sender.trim().contains(sender_trim)
                || sender_trim.contains(bubble.sender.trim());
            if sender_ok && bub_norm == norm_raw {
                return Some(bubble.bubble_rect);
            }
        }
        for bubble in bubbles.iter().rev() {
            let bub_norm = parser::normalize_message_text(&bubble.content);
            if bub_norm == norm_raw {
                return Some(bubble.bubble_rect);
            }
        }
    }

    for bubble in bubbles.iter().rev() {
        let bub_norm = parser::normalize_message_text(&bubble.content);
        let text_hit = (!norm_raw.is_empty()
            && (bub_norm.contains(&norm_raw) || norm_raw.contains(&bub_norm)))
            || (!norm_excerpt.is_empty() && bub_norm.contains(&norm_excerpt));
        if text_hit
            && (sender_trim.is_empty()
                || bubble.sender.trim().contains(sender_trim)
                || sender_trim.contains(bubble.sender.trim()))
        {
            return Some(bubble.bubble_rect);
        }
    }

    // 二次宽松匹配：当发送者别名（群名片/QQ昵称）存在差异时优先以唯一文本内容匹配
    for bubble in bubbles.iter().rev() {
        let bub_norm = parser::normalize_message_text(&bubble.content);
        if (!norm_raw.is_empty() && bub_norm.contains(&norm_raw))
            || (!norm_excerpt.is_empty() && bub_norm.contains(&norm_excerpt))
        {
            return Some(bubble.bubble_rect);
        }
    }
    None
}

fn pick_topmost_popup_hit(hits: Vec<MatchedUiTarget>) -> Option<MatchedUiTarget> {
    if let Some(menu_hit) = hits
        .iter()
        .rev()
        .find(|h| h.control_type.eq_ignore_ascii_case(CONTROL_TYPE_MENU_ITEM))
        .cloned()
    {
        return Some(menu_hit);
    }
    hits.into_iter().last()
}

/// 在叠加浮层及同进程浮层弹窗中检索并筛选文本或控件名称与任一候选关键词匹配的可点击项
pub(crate) fn find_popup_clickable_by_keywords(
    session: &UiaSession,
    main_hwnd: isize,
    pids: &[u32],
    keywords: &[&str],
    exact_first: bool,
) -> Option<MatchedUiTarget> {
    let windows = scan_same_pid_popup_trees(session, main_hwnd, pids, INTERNAL_SCAN_DEPTH);
    if exact_first {
        for (hwnd, _cls, _title, snap) in &windows {
            let hits = find_overlay_targets_in_snapshot(*hwnd, snap, &|node| {
                if !is_clickable_popup_item(node) {
                    return false;
                }
                let nm = node.name.trim();
                keywords.iter().any(|kw| nm.eq_ignore_ascii_case(kw))
            });
            if let Some(picked) = pick_topmost_popup_hit(hits) {
                return Some(picked);
            }
        }
    }

    for (hwnd, _cls, _title, snap) in &windows {
        let hits = find_overlay_targets_in_snapshot(*hwnd, snap, &|node| {
            if !is_clickable_popup_item(node) {
                return false;
            }
            let nm_lower = node.name.trim().to_ascii_lowercase();
            let id_lower = node.automation_id.trim().to_ascii_lowercase();
            keywords.iter().any(|kw| {
                let k = kw.to_ascii_lowercase();
                nm_lower.contains(&k) || id_lower.contains(&k)
            })
        });
        if let Some(picked) = pick_topmost_popup_hit(hits) {
            return Some(picked);
        }
    }
    None
}

fn is_clickable_popup_item(node: &UiNodeSnapshot) -> bool {
    let ct = node.control_type.trim();
    let is_compact_group = ct.eq_ignore_ascii_case("Group")
        && profile::rect_width(node.rect) <= 160
        && profile::rect_height(node.rect) <= 120;
    node.is_enabled
        && (is_compact_group
            || ct.eq_ignore_ascii_case(CONTROL_TYPE_MENU_ITEM)
            || ct.eq_ignore_ascii_case(CONTROL_TYPE_BUTTON)
            || ct.eq_ignore_ascii_case(CONTROL_TYPE_LIST_ITEM)
            || ct.eq_ignore_ascii_case(CONTROL_TYPE_TAB_ITEM)
            || ct.eq_ignore_ascii_case(CONTROL_TYPE_CUSTOM)
            || ct.eq_ignore_ascii_case(CONTROL_TYPE_TEXT)
            || ct.eq_ignore_ascii_case(CONTROL_TYPE_IMAGE))
}

/// 定位当前聊天区“消息列表”容器并计算用于安全滚轮翻页的中心点坐标与可见区域矩形
pub(crate) fn locate_message_list_scroll_point(
    tree: &UiNodeSnapshot,
    win_rect: [i32; 4],
    is_standalone: bool,
) -> Option<(i32, i32, [i32; 4])> {
    let mut best_rect = find_targets_in_snapshot(0, tree, &|node| {
        node.automation_id.trim().eq_ignore_ascii_case("ml-root") || node.name.trim() == "消息列表"
    })
    .into_iter()
    .map(|target| target.rect)
    .filter(|rect| profile::rect_is_valid(*rect))
    .max_by_key(|rect| profile::rect_area(*rect));

    if best_rect.is_none() && profile::rect_is_valid(win_rect) {
        let win_w = profile::rect_width(win_rect);
        let win_h = profile::rect_height(win_rect);
        let fallback = [
            win_rect[0]
                + if is_standalone {
                    24
                } else {
                    (win_w as f64 * 0.35) as i32
                },
            win_rect[1] + (win_h as f64 * 0.16) as i32,
            win_rect[0]
                + if is_standalone {
                    (win_w - 24).max(32)
                } else {
                    (win_w as f64 * 0.85) as i32
                },
            win_rect[1] + (win_h as f64 * 0.72) as i32,
        ];
        if profile::rect_is_valid(fallback) {
            best_rect = Some(fallback);
        }
    }

    let hist_rect = best_rect?;
    let (cx, cy) = rect_center(hist_rect)?;
    Some((cx, cy, hist_rect))
}

/// 在消息列表坐标上发送指定步数的垂直滚轮消息以向历史顶部或最新底部翻页
pub(crate) fn scroll_message_list_wheel(
    identity: &VerifiedWindowIdentity,
    pt: (i32, i32),
    kind: SimulatedClickKind,
    notches: usize,
) -> Result<()> {
    if notches == 0 {
        return Ok(());
    }
    let delta_per_burst = match kind {
        SimulatedClickKind::WheelUp => 120,
        SimulatedClickKind::WheelDown => -120,
        _ => return Ok(()),
    };
    scroll_point_cursor_restore_verified(identity, pt.0, pt.1, delta_per_burst, notches)?;
    thread::sleep(Duration::from_millis(220));
    Ok(())
}

fn has_ignored_media_uri_prefix(trimmed: &str) -> bool {
    ["base64://", "data:", "http://", "https://"]
        .iter()
        .any(|prefix| {
            trimmed
                .get(..prefix.len())
                .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
        })
}

fn push_unique_media_token(raw: &str, seen: &mut HashSet<String>, out: &mut Vec<String>) {
    let trimmed = raw.trim();
    if trimmed.is_empty() || has_ignored_media_uri_prefix(trimmed) {
        return;
    }
    let base = Path::new(trimmed)
        .file_name()
        .and_then(|s| s.to_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| trimmed.rsplit(['/', '\\']).next().map(str::trim))
        .unwrap_or("")
        .trim();
    if !base.is_empty() && seen.insert(base.to_string()) {
        out.push(base.to_string());
    }
}

/// 从 `File`/`Video` 消息段提取去重后的可匹配文件名词条（自动忽略 `base64://` 与 `data:` 前缀）
pub(crate) fn extract_media_bubble_tokens(segs: &[MessageSegment]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for seg in segs {
        match seg {
            MessageSegment::File { file, name, .. } => {
                if let Some(n) = name.as_deref() {
                    let trimmed = n.trim();
                    if !trimmed.is_empty() && seen.insert(trimmed.to_string()) {
                        out.push(trimmed.to_string());
                    }
                }
                push_unique_media_token(file, &mut seen, &mut out);
            }
            MessageSegment::Video { file, .. } => {
                push_unique_media_token(file, &mut seen, &mut out);
            }
            _ => {}
        }
    }
    out
}

/// 在向上翻找历史气泡完成后，按余量一次性向下滚轮将消息列表恢复至最新消息底部
pub(crate) fn restore_message_list_to_bottom(
    identity: &VerifiedWindowIdentity,
    pt: (i32, i32),
    scroll_up_steps: usize,
) {
    if scroll_up_steps == 0 {
        return;
    }
    let _ = scroll_point_cursor_restore_verified(identity, pt.0, pt.1, -480, scroll_up_steps + 4);
    thread::sleep(Duration::from_millis(120));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_node(
        control_type: &str,
        name: &str,
        automation_id: &str,
        rect: [i32; 4],
        children: Vec<UiNodeSnapshot>,
    ) -> UiNodeSnapshot {
        UiNodeSnapshot {
            control_type: control_type.to_string(),
            localized_control_type: String::new(),
            name: name.to_string(),
            automation_id: automation_id.to_string(),
            class_name: String::new(),
            rect,
            is_enabled: true,
            is_offscreen: false,
            patterns: Vec::new(),
            value: None,
            children,
        }
    }

    fn build_top_clipped_video_tree(video_rect: [i32; 4]) -> UiNodeSnapshot {
        let win_rect = [342, 100, 1446, 880];
        let ml_rect = [648, 160, 1262, 668];
        let children = vec![
            make_node(
                "Group",
                "测试机器人",
                "",
                [1210, 165, 1242, 197],
                Vec::new(),
            ),
            make_node(
                "Group",
                "",
                "",
                video_rect,
                vec![make_node(
                    "Text",
                    "proto_v4_live_video.mp4\n7.69\u{2009}KB 已发送",
                    "",
                    video_rect,
                    Vec::new(),
                )],
            ),
        ];
        make_node(
            "Pane",
            "QQ",
            "",
            win_rect,
            vec![make_node("Group", "消息列表", "ml-root", ml_rect, children)],
        )
    }

    #[test]
    fn collect_visible_bubbles_skips_tiny_top_sliver_and_clips_partially_exposed_video_card() {
        let win_rect = [342, 100, 1446, 880];

        // 仅露出顶部边缘 11px（175 - 164 = 11 < 40）的高视频卡必须跳过，促使继续向上滚屏
        let sliver_tree = build_top_clipped_video_tree([882, -66, 1202, 175]);
        let sliver_bubbles = collect_visible_bubbles(&sliver_tree, win_rect, false, "Demo测试群");
        assert!(
            sliver_bubbles.is_empty(),
            "11px sliver of 241px video card must be skipped"
        );

        // 滚出 57px（221 - 164 = 57 >= 40）后裁剪为 [882, 164, 1202, 221]，中心点 (1042, 192) 严格位于窗口与消息列表内部
        let exposed_tree = build_top_clipped_video_tree([882, -20, 1202, 221]);
        let exposed_bubbles = collect_visible_bubbles(&exposed_tree, win_rect, false, "Demo测试群");
        assert_eq!(exposed_bubbles.len(), 1);
        assert_eq!(exposed_bubbles[0].bubble_rect, [882, 164, 1202, 221]);
        assert_eq!(
            rect_center(exposed_bubbles[0].bubble_rect),
            Some((1042, 192))
        );
    }

    #[test]
    fn extract_media_bubble_tokens_skips_case_insensitive_base64_and_data_uris() {
        let segs = vec![
            MessageSegment::File {
                file: "base64://AAAABBBBCCCC".to_string(),
                name: None,
                size: None,
            },
            MessageSegment::Video {
                file: "BASE64://DDDDEEEEFFFF".to_string(),
                url: None,
            },
            MessageSegment::File {
                file: "Data:application/octet-stream;base64,AAAA".to_string(),
                name: Some("   ".to_string()),
                size: None,
            },
            MessageSegment::File {
                file: r"C:\staging\Report.PDF".to_string(),
                name: Some("Report.PDF".to_string()),
                size: None,
            },
        ];
        // base64:// 与 data: 均须跳过，仅保留合法文件名分词
        let tokens = extract_media_bubble_tokens(&segs);
        assert_eq!(tokens, vec!["Report.PDF".to_string()]);
    }
}
