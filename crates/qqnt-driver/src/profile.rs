//! QQNT 9.9.31 窗口与控件布局启发式规则、进程/窗口选择器及 `MockQQNTWndClass` 兼容支持

use common_types::constants::{
    CHROME_WIDGET_WINDOW_CLASS, QQ_PROCESS_NAMES, SECURITY_KEYWORDS as COMMON_SECURITY_KEYWORDS,
    TX_GUI_FOUNDATION_CLASS, UI_LABEL_GROUP_MEMBER_LIST, UI_LABEL_MESSAGE_LIST,
    UI_LABEL_SEND_BUTTON, UI_LABEL_SESSION_LIST, UI_LABEL_WINDOW_CONTROL_AREA,
};
use common_types::{InteractionConfig, UiNodeSnapshot};
use uia_core::constants::{
    CONTROL_TYPE_BUTTON, CONTROL_TYPE_COMBO_BOX, CONTROL_TYPE_DOCUMENT, CONTROL_TYPE_EDIT,
    CONTROL_TYPE_GROUP, CONTROL_TYPE_LIST, CONTROL_TYPE_LIST_ITEM, CONTROL_TYPE_MENU_BAR,
    CONTROL_TYPE_PANE, CONTROL_TYPE_TEXT, CONTROL_TYPE_TITLE_BAR, CONTROL_TYPE_TREE_ITEM,
    CONTROL_TYPE_WINDOW,
};
use uia_core::CollectedNode;

use crate::constants::CONTROL_TYPE_CUSTOM;

/// QQNT 桌面端默认可执行进程名列表（复用 `common_types` 单一权威常量）
pub const DEFAULT_PROCESS_NAMES: &[&str] = QQ_PROCESS_NAMES;

/// QQNT 与经典版 QQ 主窗口默认 Win32 窗口类名列表
pub const DEFAULT_WINDOW_CLASSES: &[&str] = &[CHROME_WIDGET_WINDOW_CLASS, TX_GUI_FOUNDATION_CLASS];

/// 触发安全验证熔断的高危窗口标题或文本关键词列表
pub const SECURITY_KEYWORDS: &[&str] = COMMON_SECURITY_KEYWORDS;

/// QQNT 9.9.31 与 `MockQQNTWndClass` 窗口结构启发式画像配置
#[derive(Debug, Clone)]
pub struct QqntProfile {
    process_names: Vec<String>,
    window_classes: Vec<String>,
}

impl QqntProfile {
    /// 结合默认 QQNT 选择器与交互配置中的额外类名/进程名构建画像
    pub fn from_config(interaction_config: &InteractionConfig) -> Self {
        let mut process_names = Vec::new();
        for name in DEFAULT_PROCESS_NAMES.iter().copied().chain(
            interaction_config
                .extra_process_names
                .iter()
                .map(String::as_str),
        ) {
            let trimmed = name.trim();
            if !trimmed.is_empty()
                && !process_names
                    .iter()
                    .any(|existing: &String| existing.eq_ignore_ascii_case(trimmed))
            {
                process_names.push(trimmed.to_string());
            }
        }

        let mut window_classes = Vec::new();
        for class_name in DEFAULT_WINDOW_CLASSES.iter().copied().chain(
            interaction_config
                .extra_window_classes
                .iter()
                .map(String::as_str),
        ) {
            let trimmed = class_name.trim();
            if !trimmed.is_empty()
                && !window_classes
                    .iter()
                    .any(|existing: &String| existing == trimmed)
            {
                window_classes.push(trimmed.to_string());
            }
        }

        Self {
            process_names,
            window_classes,
        }
    }

    /// 返回用于 Win32 进程快照检索的目标进程名列表
    pub fn process_names(&self) -> &[String] {
        &self.process_names
    }

    /// 返回用于枚举候选 QQ 窗口的 Win32 窗口类名列表
    pub fn window_classes(&self) -> &[String] {
        &self.window_classes
    }

    /// 返回触发安全熔断检测的静态关键词集合
    pub fn security_keywords(&self) -> &'static [&'static str] {
        SECURITY_KEYWORDS
    }
}

/// 当 `[l, t, r, b]` 描述非空正面积矩形时返回 `true`
#[inline]
pub fn rect_is_valid(rect: [i32; 4]) -> bool {
    rect[2] > rect[0] && rect[3] > rect[1]
}

/// 计算矩形 `[l, t, r, b]` 的宽度并下限截断为 0
#[inline]
pub fn rect_width(rect: [i32; 4]) -> i32 {
    (rect[2] - rect[0]).max(0)
}

/// 计算矩形 `[l, t, r, b]` 的高度并下限截断为 0
#[inline]
pub fn rect_height(rect: [i32; 4]) -> i32 {
    (rect[3] - rect[1]).max(0)
}

/// 计算矩形 `[l, t, r, b]` 的几何中心坐标 `(x, y)`
#[inline]
pub fn rect_center(rect: [i32; 4]) -> (f64, f64) {
    (
        (rect[0] + rect[2]) as f64 * 0.5,
        (rect[1] + rect[3]) as f64 * 0.5,
    )
}

/// 计算矩形 `[l, t, r, b]` 的像素面积
#[inline]
pub fn rect_area(rect: [i32; 4]) -> i64 {
    i64::from(rect_width(rect)) * i64::from(rect_height(rect))
}

/// 带 3 像素亚像素舍入容差的矩形包含关系判定
#[inline]
pub fn rect_contains(outer: [i32; 4], inner: [i32; 4]) -> bool {
    if !rect_is_valid(outer) || !rect_is_valid(inner) {
        return false;
    }
    let tolerance = 3;
    inner[0] >= outer[0] - tolerance
        && inner[1] >= outer[1] - tolerance
        && inner[2] <= outer[2] + tolerance
        && inner[3] <= outer[3] + tolerance
}

/// 判断节点是否为必须排除在消息历史、输入框与发送按钮检索之外的侧栏或控制区容器
pub fn is_excluded_sidebar_container(node: &UiNodeSnapshot) -> bool {
    let control_type = node.control_type.trim();
    if !(control_type.eq_ignore_ascii_case(CONTROL_TYPE_WINDOW)
        || control_type.eq_ignore_ascii_case(CONTROL_TYPE_LIST)
        || control_type.eq_ignore_ascii_case(CONTROL_TYPE_GROUP))
    {
        return false;
    }
    let name = node.name.trim();
    name == UI_LABEL_SESSION_LIST
        || name == UI_LABEL_GROUP_MEMBER_LIST
        || name == UI_LABEL_WINDOW_CONTROL_AREA
}

/// 判断输入文本是否为空白或 QQNT 内置语音输入引导占位文案
pub fn is_empty_composer_text(text: &str) -> bool {
    let cleaned: String = text
        .chars()
        .filter(|ch| !ch.is_whitespace() && *ch != '\u{200b}' && *ch != '\u{feff}')
        .collect();
    cleaned.is_empty()
        || cleaned == "按住Win+Alt，使用语音输入文字"
        || cleaned == "按住Win+Alt,使用语音输入文字"
        || (cleaned.contains("按住Win+Alt") && cleaned.contains("使用语音输入文字"))
}

fn is_root_window_surface(node: &UiNodeSnapshot) -> bool {
    if node.is_offscreen {
        return false;
    }
    let control_type = node.control_type.trim();
    let is_surface = control_type.eq_ignore_ascii_case(CONTROL_TYPE_PANE)
        || control_type.eq_ignore_ascii_case(CONTROL_TYPE_WINDOW)
        || control_type.eq_ignore_ascii_case(CONTROL_TYPE_DOCUMENT);
    if !is_surface {
        return false;
    }
    if is_excluded_sidebar_container(node) || node.name.trim() == UI_LABEL_MESSAGE_LIST {
        return false;
    }
    rect_is_valid(node.rect) && rect_width(node.rect) >= 300 && rect_height(node.rect) >= 200
}

/// 从平铺采集节点序列解析有效窗口坐标系基准
///
/// 在 125%-200% Windows DPI 缩放下，`GetWindowRect` 可能返回逻辑尺寸而 MSAA 输出物理视口尺寸，
/// 因此优先使用首个尺寸达标的根节点表面矩形以保持相对比例计算准确
pub fn effective_window_rect(window_rect: [i32; 4], nodes: &[CollectedNode]) -> [i32; 4] {
    if let Some(root) = nodes
        .iter()
        .take(16)
        .find(|collected| is_root_window_surface(&collected.snapshot))
    {
        if root.snapshot.rect != window_rect {
            return root.snapshot.rect;
        }
    }
    window_rect
}

/// 基于节点快照切片解析有效窗口坐标系基准
pub fn effective_window_rect_from_snapshots(
    window_rect: [i32; 4],
    snapshots: &[&UiNodeSnapshot],
) -> [i32; 4] {
    if let Some(root) = snapshots
        .iter()
        .copied()
        .take(16)
        .find(|snapshot| is_root_window_surface(snapshot))
    {
        if root.rect != window_rect {
            return root.rect;
        }
    }
    window_rect
}

fn find_root_surface_in_tree(node: &UiNodeSnapshot, remaining: &mut usize) -> Option<[i32; 4]> {
    if *remaining == 0 {
        return None;
    }
    *remaining -= 1;
    if is_root_window_surface(node) {
        return Some(node.rect);
    }
    for child in &node.children {
        if let Some(rect) = find_root_surface_in_tree(child, remaining) {
            return Some(rect);
        }
    }
    None
}

/// 从完整 UI 树根节点提取有效窗口坐标系基准
pub fn effective_window_rect_from_tree(window_rect: [i32; 4], root: &UiNodeSnapshot) -> [i32; 4] {
    let mut budget = 16usize;
    if let Some(rect) = find_root_surface_in_tree(root, &mut budget) {
        if rect != window_rect {
            return rect;
        }
    }
    window_rect
}

/// 判断节点是否为应过滤的操作系统标题栏或最小化/关闭系统按钮
pub fn is_system_chrome(node: &UiNodeSnapshot) -> bool {
    let control_type = node.control_type.trim();
    if control_type.eq_ignore_ascii_case(CONTROL_TYPE_TITLE_BAR)
        || control_type.eq_ignore_ascii_case(CONTROL_TYPE_MENU_BAR)
    {
        return true;
    }
    if control_type.eq_ignore_ascii_case(CONTROL_TYPE_BUTTON) {
        let name = node.name.trim();
        if matches!(
            name,
            "最小化" | "最大化" | "还原" | "关闭" | "Minimize" | "Maximize" | "Restore" | "Close"
        ) {
            return true;
        }
    }
    false
}

/// 判断控件中心是否落在左侧会话列表区域（`x < l + w * 0.42`）
pub fn is_in_left_session_pane(node_rect: [i32; 4], win_rect: [i32; 4]) -> bool {
    if !rect_is_valid(node_rect) || !rect_is_valid(win_rect) {
        return false;
    }
    let (center_x, _) = rect_center(node_rect);
    let left = win_rect[0] as f64;
    let width = rect_width(win_rect) as f64;
    center_x >= left && center_x < left + width * 0.42
}

/// 判断控件中心是否落在左上角搜索栏区域（`x < l + w * 0.42 && y < t + h * 0.16`）
pub fn is_in_top_left_search(node_rect: [i32; 4], win_rect: [i32; 4]) -> bool {
    if !rect_is_valid(node_rect) || !rect_is_valid(win_rect) {
        return false;
    }
    let (center_x, center_y) = rect_center(node_rect);
    let left = win_rect[0] as f64;
    let top = win_rect[1] as f64;
    let width = rect_width(win_rect) as f64;
    let height = rect_height(win_rect) as f64;
    center_x >= left
        && center_x < left + width * 0.42
        && center_y >= top
        && center_y < top + height * 0.16
}

/// 判断控件中心是否落在右侧聊天标题栏区域（`x >= l + w * 0.28 && y < t + h * 0.18`）
pub fn is_in_chat_header(node_rect: [i32; 4], win_rect: [i32; 4], is_standalone: bool) -> bool {
    if !rect_is_valid(node_rect) || !rect_is_valid(win_rect) {
        return false;
    }
    let (center_x, center_y) = rect_center(node_rect);
    let left = win_rect[0] as f64;
    let top = win_rect[1] as f64;
    let width = rect_width(win_rect) as f64;
    let height = rect_height(win_rect) as f64;
    let min_x = if is_standalone {
        left
    } else {
        left + width * 0.28
    };
    center_x >= min_x && center_y >= top && center_y < top + height * 0.18
}

/// 判断节点是否符合右侧消息历史容器位置启发式，同时排除覆满整窗的 Electron WebArea
pub fn is_in_message_history_container(
    node_rect: [i32; 4],
    win_rect: [i32; 4],
    is_standalone: bool,
) -> bool {
    if !rect_is_valid(node_rect) || !rect_is_valid(win_rect) {
        return false;
    }
    let (center_x, center_y) = rect_center(node_rect);
    let left = win_rect[0] as f64;
    let top = win_rect[1] as f64;
    let width = rect_width(win_rect) as f64;
    let height = rect_height(win_rect) as f64;
    if !is_standalone
        && (node_rect[0] < win_rect[0] + (width * 0.18) as i32
            || rect_height(node_rect) > (height * 0.92) as i32)
    {
        return false;
    }
    let min_x = if is_standalone {
        left
    } else {
        left + width * 0.35
    };
    center_x >= min_x && center_y >= top + height * 0.14 && center_y < top + height * 0.78
}

/// 判断节点是否落在右侧聊天消息列纵横范围内（兼容靠近列表顶沿的消息条目）
pub fn is_in_message_history_item(
    node_rect: [i32; 4],
    win_rect: [i32; 4],
    is_standalone: bool,
) -> bool {
    if !rect_is_valid(node_rect) || !rect_is_valid(win_rect) {
        return false;
    }
    let (center_x, center_y) = rect_center(node_rect);
    let left = win_rect[0] as f64;
    let top = win_rect[1] as f64;
    let width = rect_width(win_rect) as f64;
    let height = rect_height(win_rect) as f64;
    let min_x = if is_standalone {
        left
    } else {
        left + width * 0.28
    };
    let max_x = if is_standalone {
        left + width
    } else {
        left + width * 0.86
    };
    center_x >= min_x
        && center_x <= max_x
        && center_y >= top + height * 0.10
        && center_y < top + height * 0.78
}

/// 判断节点是否落在右下角消息编辑框区域（`x >= l + w * 0.28 && y >= t + h * 0.70`）
pub fn is_in_composer_area(node_rect: [i32; 4], win_rect: [i32; 4], is_standalone: bool) -> bool {
    if !rect_is_valid(node_rect) || !rect_is_valid(win_rect) {
        return false;
    }
    let (center_x, center_y) = rect_center(node_rect);
    let left = win_rect[0] as f64;
    let top = win_rect[1] as f64;
    let width = rect_width(win_rect) as f64;
    let height = rect_height(win_rect) as f64;
    let min_x = if is_standalone {
        left
    } else {
        left + width * 0.28
    };
    center_x >= min_x && center_y >= top + height * 0.70
}

/// 判断控件类型是否常见于左侧可选中会话条目
pub fn is_session_list_control_type(control_type: &str) -> bool {
    let trimmed = control_type.trim();
    trimmed.eq_ignore_ascii_case(CONTROL_TYPE_LIST_ITEM)
        || trimmed.eq_ignore_ascii_case(CONTROL_TYPE_TREE_ITEM)
        || trimmed.eq_ignore_ascii_case(CONTROL_TYPE_CUSTOM)
        || trimmed.eq_ignore_ascii_case(CONTROL_TYPE_GROUP)
        || trimmed.eq_ignore_ascii_case(CONTROL_TYPE_BUTTON)
}

/// 判断节点是否可能是左上角全局搜索输入框或触发按钮
pub fn is_search_input_candidate(node: &UiNodeSnapshot, win_rect: [i32; 4]) -> bool {
    if !node.is_enabled || node.is_offscreen || is_system_chrome(node) {
        return false;
    }
    if !rect_is_valid(node.rect) || !rect_is_valid(win_rect) {
        return false;
    }
    let control_type = node.control_type.trim();
    let is_input_type = control_type.eq_ignore_ascii_case(CONTROL_TYPE_EDIT)
        || control_type.eq_ignore_ascii_case(CONTROL_TYPE_DOCUMENT)
        || control_type.eq_ignore_ascii_case(CONTROL_TYPE_COMBO_BOX)
        || control_type.eq_ignore_ascii_case(CONTROL_TYPE_BUTTON);
    if !is_input_type {
        return false;
    }

    let name_lower = node.name.to_ascii_lowercase();
    let id_lower = node.automation_id.to_ascii_lowercase();
    let has_search_hint =
        node.name.contains("搜索") || name_lower.contains("search") || id_lower.contains("search");

    if is_in_top_left_search(node.rect, win_rect) {
        return control_type.eq_ignore_ascii_case(CONTROL_TYPE_EDIT)
            || control_type.eq_ignore_ascii_case(CONTROL_TYPE_DOCUMENT)
            || has_search_hint;
    }

    let (center_x, center_y) = rect_center(node.rect);
    let left = win_rect[0] as f64;
    let top = win_rect[1] as f64;
    let width = rect_width(win_rect) as f64;
    let height = rect_height(win_rect) as f64;
    has_search_hint && center_x < left + width * 0.42 && center_y < top + height * 0.24
}

/// 计算节点作为右下角聊天输入编辑框的匹配评分，不符合条件时返回 `None`
pub fn composer_score(
    node: &UiNodeSnapshot,
    win_rect: [i32; 4],
    is_standalone: bool,
) -> Option<i32> {
    if !node.is_enabled || node.is_offscreen || is_system_chrome(node) {
        return None;
    }
    if !rect_is_valid(node.rect) || !rect_is_valid(win_rect) {
        return None;
    }
    let width = rect_width(node.rect);
    let height = rect_height(node.rect);
    // 剔除群成员列表中 30x27 等微型内联 Edit 伪目标控件
    if width < 80 || height < 24 {
        return None;
    }

    let (center_x, center_y) = rect_center(node.rect);
    let left = win_rect[0] as f64;
    let top = win_rect[1] as f64;
    let win_width = rect_width(win_rect) as f64;
    let win_height = rect_height(win_rect) as f64;
    let left_ratio = (node.rect[0] as f64 - left) / win_width;
    let top_ratio = (node.rect[1] as f64 - top) / win_height;

    if !is_standalone
        && (left_ratio < 0.26 || left_ratio >= 0.78 || height > (win_height * 0.60) as i32)
    {
        return None;
    }

    let name_trimmed = node.name.trim();
    let name_lower = name_trimmed.to_ascii_lowercase();
    if top_ratio < 0.25 && (name_trimmed == "搜索" || name_lower == "search") {
        return None;
    }

    let min_x = if is_standalone {
        left
    } else {
        left + win_width * 0.28
    };
    if center_x < min_x || center_y < top + win_height * 0.55 {
        return None;
    }

    let control_type = node.control_type.trim();
    let is_native_editor = control_type.eq_ignore_ascii_case(CONTROL_TYPE_EDIT)
        || control_type.eq_ignore_ascii_case(CONTROL_TYPE_DOCUMENT);
    let has_composer_pattern = node.patterns.iter().any(|pattern| {
        pattern.eq_ignore_ascii_case("Value") || pattern.eq_ignore_ascii_case("Text")
    });
    let has_voice_placeholder = node
        .value
        .as_deref()
        .map(|val| is_empty_composer_text(val) && val.contains("按住 Win + Alt"))
        .unwrap_or(false);
    let is_rich_composer = (control_type.eq_ignore_ascii_case(CONTROL_TYPE_GROUP)
        || control_type.eq_ignore_ascii_case(CONTROL_TYPE_CUSTOM))
        && (has_composer_pattern || has_voice_placeholder)
        && (is_standalone || width >= (win_width * 0.25) as i32)
        && height >= 60
        && (top_ratio >= 0.65 || is_in_composer_area(node.rect, win_rect, is_standalone));

    if !is_native_editor && !is_rich_composer {
        return None;
    }

    let mut score = 10;
    if is_native_editor {
        score += 40;
    }
    if is_in_composer_area(node.rect, win_rect, is_standalone) {
        score += 100;
    }
    if node
        .patterns
        .iter()
        .any(|pattern| pattern.eq_ignore_ascii_case("Value"))
    {
        score += 30;
    }
    if height >= 36 {
        score += 20;
    }
    if has_voice_placeholder {
        score += 25;
    }
    let id_lower = node.automation_id.to_ascii_lowercase();
    if node.name.contains("输入")
        || node.name.contains("消息")
        || name_lower.contains("input")
        || name_lower.contains("editor")
        || id_lower.contains("input")
        || id_lower.contains("editor")
        || id_lower.contains("chat")
    {
        score += 15;
    }
    Some(score)
}

/// 判断节点是否可作为聊天输入框候选控件
pub fn is_composer_editor_candidate(
    node: &UiNodeSnapshot,
    win_rect: [i32; 4],
    is_standalone: bool,
) -> bool {
    composer_score(node, win_rect, is_standalone).is_some()
}

/// 计算节点作为右下角发送按钮的匹配评分，不符合条件时返回 `None`
pub fn send_button_score(
    node: &UiNodeSnapshot,
    win_rect: [i32; 4],
    is_standalone: bool,
) -> Option<i32> {
    if node.is_offscreen || is_system_chrome(node) {
        return None;
    }
    if !rect_is_valid(node.rect) || !rect_is_valid(win_rect) {
        return None;
    }
    let control_type = node.control_type.trim();
    let is_button = control_type.eq_ignore_ascii_case(CONTROL_TYPE_BUTTON);
    let is_compact_alt = (control_type.eq_ignore_ascii_case(CONTROL_TYPE_GROUP)
        || control_type.eq_ignore_ascii_case(CONTROL_TYPE_CUSTOM)
        || control_type.eq_ignore_ascii_case(CONTROL_TYPE_TEXT))
        && rect_width(node.rect) <= 180
        && rect_height(node.rect) <= 72;
    if !is_button && !is_compact_alt {
        return None;
    }

    let (center_x, center_y) = rect_center(node.rect);
    let left = win_rect[0] as f64;
    let top = win_rect[1] as f64;
    let win_width = rect_width(win_rect) as f64;
    let win_height = rect_height(win_rect) as f64;
    let min_x = if is_standalone {
        left
    } else {
        left + win_width * 0.28
    };
    if center_x < min_x || center_y < top + win_height * 0.60 {
        return None;
    }

    let name_trimmed = node.name.trim();
    let name_lower = name_trimmed.to_ascii_lowercase();
    let id_lower = node.automation_id.to_ascii_lowercase();
    let exact_send = name_trimmed == UI_LABEL_SEND_BUTTON
        || name_trimmed.starts_with("发送(")
        || name_lower == "send";

    // QQNT 在输入框为空时会将底栏“发送”按钮置为禁用态，输入前置识别需允许捕获该精确底部按钮并优先偏好启用态
    if !node.is_enabled {
        let in_bottom_bar = center_y >= top + win_height * 0.78
            && (is_standalone
                || (center_x >= left + win_width * 0.26 && center_x <= left + win_width * 0.96));
        if !(is_button && exact_send && in_bottom_bar) {
            return None;
        }
    }

    let matches_send = if is_button {
        name_trimmed.contains(UI_LABEL_SEND_BUTTON)
            || name_lower.contains("send")
            || id_lower.contains("send")
    } else {
        exact_send
    };
    if !matches_send {
        return None;
    }

    let mut score = 50;
    if node.is_enabled {
        score += 60;
    }
    if is_button {
        score += 30;
    }
    if exact_send {
        score += 100;
    }
    if center_y >= top + win_height * 0.70 {
        score += 30;
    }
    if name_trimmed.contains("快捷键")
        || name_trimmed.contains("设置")
        || name_trimmed.contains("选项")
        || name_lower.contains("option")
        || name_lower.contains("setting")
    {
        score -= 80;
    }
    Some(score)
}

/// 判断节点是否可作为右下角发送按钮候选控件
pub fn is_send_button_candidate(
    node: &UiNodeSnapshot,
    win_rect: [i32; 4],
    is_standalone: bool,
) -> bool {
    send_button_score(node, win_rect, is_standalone).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(control_type: &str, name: &str, rect: [i32; 4]) -> UiNodeSnapshot {
        UiNodeSnapshot {
            control_type: control_type.to_string(),
            name: name.to_string(),
            rect,
            is_enabled: true,
            is_offscreen: false,
            patterns: vec!["Value".to_string(), "Invoke".to_string()],
            ..Default::default()
        }
    }

    #[test]
    fn profile_deduplicates_extra_names_and_classes() {
        let interaction_config = InteractionConfig {
            extra_process_names: vec!["qq.exe".into(), "MockQQ.exe".into()],
            extra_window_classes: vec!["Chrome_WidgetWin_1".into(), "MockQQNTWndClass".into()],
            ..Default::default()
        };
        let profile = QqntProfile::from_config(&interaction_config);
        assert_eq!(
            profile.process_names(),
            &[
                DEFAULT_PROCESS_NAMES[0],
                DEFAULT_PROCESS_NAMES[1],
                "MockQQ.exe"
            ]
        );
        assert_eq!(
            profile.window_classes(),
            &[
                DEFAULT_WINDOW_CLASSES[0],
                DEFAULT_WINDOW_CLASSES[1],
                "MockQQNTWndClass"
            ]
        );
    }

    #[test]
    fn mock_qq_regions_align_with_heuristics() {
        let win = [100, 100, 916, 779]; // 模拟 800x640 客户区对应的 816x679 外框矩形
        assert!(is_in_left_session_pane([110, 181, 335, 721], win));
        assert!(is_in_chat_header([350, 143, 875, 177], win, false));
        assert!(is_in_message_history_container(
            [350, 183, 875, 561],
            win,
            false
        ));
        assert!(is_in_message_history_item([355, 185, 860, 205], win, false));

        let editor = node("Edit", "", [350, 571, 875, 666]);
        assert!(is_composer_editor_candidate(&editor, win, false));

        let send_button = node("Button", "发送", [780, 676, 875, 716]);
        assert!(is_send_button_candidate(&send_button, win, false));

        let close_button = node("Button", "关闭", [870, 102, 910, 132]);
        assert!(is_system_chrome(&close_button));
        assert!(!is_send_button_candidate(&close_button, win, false));
    }

    #[test]
    fn electron_web_area_and_rich_controls_score_accurately() {
        let win = [100, 100, 916, 779]; // 816x679 外框尺寸

        // 覆盖全窗口的 Electron WebArea Document 必须被聊天历史容器与输入框同时拒绝
        assert!(!is_in_message_history_container(win, win, false));
        let web_area_below_titlebar = [100, 136, 916, 779];
        assert!(!is_in_message_history_container(
            web_area_below_titlebar,
            win,
            false
        ));
        let web_area_doc = node("Document", "QQ", win);
        assert_eq!(composer_score(&web_area_doc, win, false), None);

        // 右侧消息历史容器应正常通过区域校验
        assert!(is_in_message_history_container(
            [350, 183, 875, 561],
            win,
            false
        ));

        // 处于输入区域的 Group/Custom 控件得分应低于原生 Edit/Document（差值至少 40 分）
        let editor_rect = [350, 590, 875, 670];
        let native_edit = node("Edit", "", editor_rect);
        let rich_group = UiNodeSnapshot {
            patterns: vec!["Text".to_string()],
            ..node("Group", "", editor_rect)
        };
        let rich_group_val = node("Group", "", editor_rect);
        let plain_group = UiNodeSnapshot {
            patterns: vec!["Invoke".to_string()],
            ..node("Group", "", editor_rect)
        };
        let native_score = composer_score(&native_edit, win, false).expect("native Edit score");
        let rich_val_score =
            composer_score(&rich_group_val, win, false).expect("Group(Value) score");
        let rich_text_score = composer_score(&rich_group, win, false).expect("Group(Text) score");
        assert!(native_score > rich_val_score);
        assert!(native_score >= rich_val_score + 40);
        assert!(rich_text_score > 0);
        assert_eq!(composer_score(&plain_group, win, false), None);

        // 位于底栏阈值上方区域的 Group/Custom 不得被误判为输入框
        let high_group = UiNodeSnapshot {
            patterns: vec!["Text".to_string()],
            ..node("Custom", "", [350, 480, 875, 520])
        };
        assert_eq!(composer_score(&high_group, win, false), None);

        // 紧凑发送控件与 Button 发送按钮评分对照及过宽干扰气泡剔除验证
        let send_rect = [790, 680, 880, 718]; // 宽 90 高 38 像素矩形
        let button_send = node("Button", "发送", send_rect);
        let group_send = node("Group", "发送", send_rect);
        let custom_send = node("Custom", "发送(S)", send_rect);
        let text_send = node("Text", "Send", send_rect);
        let oversized_group = node("Group", "发送", [640, 680, 880, 718]); // 宽度 240 超出 180 上限
        let chat_bubble = node("Text", "发送消息测试", send_rect);

        let button_score = send_button_score(&button_send, win, false).expect("Button send score");
        let group_score = send_button_score(&group_send, win, false).expect("Group send score");
        assert!(button_score > group_score);
        assert!(button_score >= group_score + 30);
        assert!(send_button_score(&custom_send, win, false).is_some());
        assert!(send_button_score(&text_send, win, false).is_some());
        assert_eq!(send_button_score(&oversized_group, win, false), None);
        assert_eq!(send_button_score(&chat_bubble, win, false), None);
    }

    #[test]
    fn qqnt_150_dpi_composer_decoys_and_disabled_send_button() {
        let logical_win = [451, 84, 1519, 863];
        let physical_root = UiNodeSnapshot {
            control_type: "Pane".to_string(),
            name: "QQ".to_string(),
            rect: [675, 126, 2277, 1295],
            is_enabled: true,
            ..Default::default()
        };
        let effective = effective_window_rect_from_tree(logical_win, &physical_root);
        assert_eq!(effective, [675, 126, 2277, 1295]);
        assert_eq!(
            effective_window_rect_from_snapshots(logical_win, &[&physical_root]),
            [675, 126, 2277, 1295]
        );

        let left_item = node("Group", "Demo测试群", [759, 552, 1134, 648]);
        assert_eq!(
            effective_window_rect_from_snapshots(logical_win, &[&left_item]),
            logical_win
        );

        // 1602px 物理宽度下中心比例 0.346 的顶部群标题文本应命中聊天头区域
        let header_text = node("Text", "Demo测试群", [1161, 211, 1298, 242]);
        assert!(is_in_chat_header(header_text.rect, effective, false));

        // 左上角搜索框与右侧群成员栏 30x27 微型 Edit 伪目标必须被输入框评分剔除
        let search_edit = node("Edit", "搜索", [815, 210, 1060, 240]);
        assert_eq!(composer_score(&search_edit, effective, false), None);
        let member_decoy_top = node("Edit", "", [2220, 687, 2250, 714]);
        let member_decoy_bottom = node("Edit", "", [2220, 1080, 2250, 1107]);
        assert_eq!(composer_score(&member_decoy_top, effective, false), None);
        assert_eq!(composer_score(&member_decoy_bottom, effective, false), None);

        // 真实 QQNT 带语音占位文案的 Group 输入框应获得正分数
        let live_composer = UiNodeSnapshot {
            control_type: "Group".to_string(),
            name: "Demo测试群".to_string(),
            value: Some("按住 Win + Alt，使用语音输入文字\n".to_string()),
            rect: [1134, 1038, 2001, 1205],
            is_enabled: true,
            is_offscreen: false,
            patterns: vec!["LegacyIAccessible".into(), "Value".into(), "Invoke".into()],
            ..Default::default()
        };
        assert!(composer_score(&live_composer, effective, false).is_some());
        assert!(is_empty_composer_text(
            live_composer.value.as_deref().unwrap_or("")
        ));
        assert!(is_empty_composer_text("  \n\r "));
        assert!(!is_empty_composer_text("hello"));

        // 未输入文字前的禁用态“发送”按钮可被捕获，但启用态按钮得分严格更高
        let disabled_send = UiNodeSnapshot {
            control_type: "Button".to_string(),
            name: "发送".to_string(),
            rect: [1854, 1226, 1896, 1268],
            is_enabled: false,
            is_offscreen: false,
            patterns: vec!["LegacyIAccessible".into(), "Invoke".into()],
            ..Default::default()
        };
        let enabled_send = UiNodeSnapshot {
            is_enabled: true,
            ..disabled_send.clone()
        };
        let disabled_score =
            send_button_score(&disabled_send, effective, false).expect("disabled pre-typing send");
        let enabled_score =
            send_button_score(&enabled_send, effective, false).expect("enabled send button");
        assert!(enabled_score > disabled_score);
    }
}
