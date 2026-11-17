//! UIA 控件树采集与策略回退模块（Strategy + Adapter）：优先使用带属性缓存的 `ControlViewWalker`，并在未唤醒或无语义内容时按序回退到 `RawViewWalker` 与 MSAA (`IAccessible`)

use common_types::constants::processes::CHROME_WIDGET_WINDOW_CLASS;
use common_types::{AdapterError, Result, UiNodeSnapshot};
use uiautomation::core::UICacheRequest;
use uiautomation::patterns::{
    UIInvokePattern, UILegacyIAccessiblePattern, UIScrollPattern, UISelectionItemPattern,
    UITextPattern, UIValuePattern,
};
use uiautomation::types::{ElementMode, Handle, TreeScope, UIProperty};
use uiautomation::variants::Value;
use uiautomation::{UIAutomation, UIElement, UITreeWalker};

use crate::constants::{
    CONTROL_TYPE_UNKNOWN, MAX_TREE_NODES, MIN_POPULATED_TREE_NODES, NON_SEMANTIC_CONTROL_TYPES,
    PATTERN_INVOKE, PATTERN_LEGACY_IACCESSIBLE, PATTERN_SCROLL, PATTERN_SELECTION_ITEM,
    PATTERN_TEXT, PATTERN_VALUE, TEXT_PATTERN_READ_ALL,
};
use crate::msaa::{self, UiNodeHandle};
use crate::win32;

/// 携带序列化快照与底层活动句柄的平铺控件节点适配器（Adapter），供单线程驱动层执行动作
#[derive(Debug, Clone)]
pub struct CollectedNode {
    /// 可跨线程传递的控件静态快照
    pub snapshot: UiNodeSnapshot,
    /// 绑定当前线程的底层活动控件句柄（UIA `UIElement` 或 MSAA `IAccessible`）
    pub element: UiNodeHandle,
}

/// 控件树递归遍历共享上下文参数对象，用于控制递归函数参数规模
struct TreeWalkContext<'a> {
    walker: &'a UITreeWalker,
    cache_request: Option<&'a UICacheRequest>,
    max_depth: usize,
}

/// 构造批量预取 UIA 常用基础属性与模式可用性标志的缓存请求，减少跨进程 COM 往返
fn build_snapshot_cache_request(automation: &UIAutomation) -> Option<UICacheRequest> {
    let request = automation.create_cache_request().ok()?;
    let _ = request.set_element_mode(ElementMode::Full);
    let _ = request.set_tree_scope(TreeScope::Element);
    for property in [
        UIProperty::ControlType,
        UIProperty::LocalizedControlType,
        UIProperty::Name,
        UIProperty::AutomationId,
        UIProperty::ClassName,
        UIProperty::BoundingRectangle,
        UIProperty::IsEnabled,
        UIProperty::IsOffscreen,
        UIProperty::IsInvokePatternAvailable,
        UIProperty::IsValuePatternAvailable,
        UIProperty::IsSelectionItemPatternAvailable,
        UIProperty::IsLegacyIAccessiblePatternAvailable,
        UIProperty::IsTextPatternAvailable,
        UIProperty::IsScrollPatternAvailable,
    ] {
        if request.add_property(property).is_err() {
            return None;
        }
    }
    Some(request)
}

/// 读取指定 UIA 缓存布尔属性值
fn cached_bool_property(element: &UIElement, property: UIProperty) -> bool {
    matches!(
        element
            .get_cached_property_value(property)
            .and_then(|value| value.get_value()),
        Ok(Value::BOOL(true))
    )
}

/// 从已预取属性缓存的 `UIElement` 中零额外往返地提取节点快照
fn snapshot_cached_element(element: &UIElement) -> UiNodeSnapshot {
    let control_type = element
        .get_cached_control_type()
        .map(|kind| format!("{kind:?}"))
        .unwrap_or_else(|_| CONTROL_TYPE_UNKNOWN.to_string());
    let localized_control_type = element
        .get_cached_localized_control_type()
        .unwrap_or_default();
    let name = element.get_cached_name().unwrap_or_default();
    let automation_id = element.get_cached_automation_id().unwrap_or_default();
    let class_name = element.get_cached_classname().unwrap_or_default();
    let rect = element
        .get_cached_bounding_rectangle()
        .map(|bounds| {
            [
                bounds.get_left(),
                bounds.get_top(),
                bounds.get_right(),
                bounds.get_bottom(),
            ]
        })
        .unwrap_or([0, 0, 0, 0]);
    let is_enabled = element.is_cached_enabled().unwrap_or(false);
    let is_offscreen = element.is_cached_offscreen().unwrap_or(false);

    let mut patterns = Vec::new();
    if cached_bool_property(element, UIProperty::IsInvokePatternAvailable) {
        patterns.push(PATTERN_INVOKE.to_string());
    }
    let has_value_pattern = cached_bool_property(element, UIProperty::IsValuePatternAvailable);
    if has_value_pattern {
        patterns.push(PATTERN_VALUE.to_string());
    }
    if cached_bool_property(element, UIProperty::IsSelectionItemPatternAvailable) {
        patterns.push(PATTERN_SELECTION_ITEM.to_string());
    }
    if cached_bool_property(element, UIProperty::IsLegacyIAccessiblePatternAvailable) {
        patterns.push(PATTERN_LEGACY_IACCESSIBLE.to_string());
    }
    if cached_bool_property(element, UIProperty::IsTextPatternAvailable) {
        patterns.push(PATTERN_TEXT.to_string());
    }
    if cached_bool_property(element, UIProperty::IsScrollPatternAvailable) {
        patterns.push(PATTERN_SCROLL.to_string());
    }

    let value = if has_value_pattern {
        element
            .get_pattern::<UIValuePattern>()
            .ok()
            .and_then(|pattern| pattern.get_value().ok())
    } else {
        None
    };

    UiNodeSnapshot {
        control_type,
        localized_control_type,
        name,
        automation_id,
        class_name,
        rect,
        is_enabled,
        is_offscreen,
        patterns,
        value,
        children: Vec::new(),
    }
}

/// 当属性缓存不可用时通过实时 COM 查询为单个 `UIElement` 构建快照
fn snapshot_single_element(element: &UIElement) -> UiNodeSnapshot {
    let control_type = element
        .get_control_type()
        .map(|kind| format!("{kind:?}"))
        .unwrap_or_else(|_| CONTROL_TYPE_UNKNOWN.to_string());
    let localized_control_type = element.get_localized_control_type().unwrap_or_default();
    let name = element.get_name().unwrap_or_default();
    let automation_id = element.get_automation_id().unwrap_or_default();
    let class_name = element.get_classname().unwrap_or_default();
    let rect = element
        .get_bounding_rectangle()
        .map(|bounds| {
            [
                bounds.get_left(),
                bounds.get_top(),
                bounds.get_right(),
                bounds.get_bottom(),
            ]
        })
        .unwrap_or([0, 0, 0, 0]);
    let is_enabled = element.is_enabled().unwrap_or(false);
    let is_offscreen = element.is_offscreen().unwrap_or(false);

    let mut patterns = Vec::new();
    if element.get_pattern::<UIInvokePattern>().is_ok() {
        patterns.push(PATTERN_INVOKE.to_string());
    }

    let mut value: Option<String> = None;
    if let Ok(value_pattern) = element.get_pattern::<UIValuePattern>() {
        patterns.push(PATTERN_VALUE.to_string());
        if let Ok(current_value) = value_pattern.get_value() {
            value = Some(current_value);
        }
    }

    if element.get_pattern::<UISelectionItemPattern>().is_ok() {
        patterns.push(PATTERN_SELECTION_ITEM.to_string());
    }
    if element.get_pattern::<UILegacyIAccessiblePattern>().is_ok() {
        patterns.push(PATTERN_LEGACY_IACCESSIBLE.to_string());
    }
    if element.get_pattern::<UITextPattern>().is_ok() {
        patterns.push(PATTERN_TEXT.to_string());
    }
    if element.get_pattern::<UIScrollPattern>().is_ok() {
        patterns.push(PATTERN_SCROLL.to_string());
    }

    UiNodeSnapshot {
        control_type,
        localized_control_type,
        name,
        automation_id,
        class_name,
        rect,
        is_enabled,
        is_offscreen,
        patterns,
        value,
        children: Vec::new(),
    }
}

/// 统一读取任意抽象控件句柄的文本内容（依次尝试 `ValuePattern`、`TextPattern`、`LegacyIAccessible`、节点名称或 MSAA 接口）
pub fn read_element_text(element: &UiNodeHandle) -> Option<String> {
    match element {
        UiNodeHandle::Uia(uia_element) => read_uia_element_text(uia_element),
        UiNodeHandle::Msaa { acc, child_id } => msaa::read_msaa_text(acc, *child_id),
    }
}

/// 仅读取可编辑控件的值属性（`ValuePattern` / `LegacyIAccessible::get_value` 或 MSAA `get_accValue`），不回退到容易误命中占位文本的 `Name`
pub fn read_element_value_only(element: &UiNodeHandle) -> Option<String> {
    match element {
        UiNodeHandle::Uia(uia_element) => read_uia_value_only(uia_element),
        UiNodeHandle::Msaa { acc, child_id } => msaa::read_msaa_value_only(acc, *child_id),
    }
}

/// 仅通过 UIA 值模式或传统无障碍接口读取控件值字符串
fn read_uia_value_only(element: &UIElement) -> Option<String> {
    if let Ok(value_pattern) = element.get_pattern::<UIValuePattern>() {
        if let Ok(current_value) = value_pattern.get_value() {
            return Some(current_value);
        }
    }
    if let Ok(legacy) = element.get_pattern::<UILegacyIAccessiblePattern>() {
        if let Ok(current_value) = legacy.get_value() {
            return Some(current_value);
        }
    }
    None
}

/// 清空可编辑控件内容并严格回读验证，确保不存在残留的非占位文本
pub fn clear_element_text(element: &UiNodeHandle) -> Result<()> {
    match element {
        UiNodeHandle::Uia(uia_element) => {
            let value_pattern = uia_element
                .get_pattern::<UIValuePattern>()
                .map_err(|error| {
                    AdapterError::UiaFailure(format!("UIA ValuePattern 不可用于清空操作: {error}"))
                })?;
            value_pattern.set_value("").map_err(|error| {
                AdapterError::UiaFailure(format!("UIA ValuePattern::set_value(\"\") 失败: {error}"))
            })?;
            if let Some(remaining) = read_uia_value_only(uia_element)
                .filter(|value| !msaa::is_known_empty_placeholder(value))
            {
                return Err(AdapterError::UiaFailure(format!(
                    "UIA ValuePattern::set_value(\"\") 后编辑器仍保留非空值 (len={})",
                    remaining.len()
                )));
            }
            Ok(())
        }
        UiNodeHandle::Msaa { acc, child_id } => msaa::clear_msaa_value(acc, *child_id),
    }
}

/// 按多模式责任链读取单个 UIA 元素的文本表示
fn read_uia_element_text(element: &UIElement) -> Option<String> {
    if let Ok(value_pattern) = element.get_pattern::<UIValuePattern>() {
        if let Ok(current_value) = value_pattern.get_value() {
            return Some(current_value);
        }
    }
    if let Ok(text_pattern) = element.get_pattern::<UITextPattern>() {
        if let Ok(range) = text_pattern.get_document_range() {
            if let Ok(full_text) = range.get_text(TEXT_PATTERN_READ_ALL) {
                return Some(full_text);
            }
        }
    }
    if let Ok(legacy) = element.get_pattern::<UILegacyIAccessiblePattern>() {
        if let Ok(current_value) = legacy.get_value() {
            if !current_value.is_empty() {
                return Some(current_value);
            }
        }
        if let Ok(legacy_name) = legacy.get_name() {
            if !legacy_name.is_empty() {
                return Some(legacy_name);
            }
        }
    }
    element.get_name().ok().filter(|text| !text.is_empty())
}

/// 尝试附带属性缓存获取首个子元素；缓存步进失败时安全回退到无缓存查询
fn next_cached_child(
    walker: &UITreeWalker,
    cache_request: Option<&UICacheRequest>,
    parent: &UIElement,
) -> (uiautomation::Result<UIElement>, bool) {
    match cache_request {
        Some(cache) => match walker.get_first_child_build_cache(parent, cache) {
            Ok(child) => (Ok(child), true),
            Err(_) => (walker.get_first_child(parent), false),
        },
        None => (walker.get_first_child(parent), false),
    }
}

/// 尝试附带属性缓存获取下一个同级元素；缓存步进失败时安全回退到无缓存查询
fn next_cached_sibling(
    walker: &UITreeWalker,
    cache_request: Option<&UICacheRequest>,
    child: &UIElement,
) -> (uiautomation::Result<UIElement>, bool) {
    match cache_request {
        Some(cache) => match walker.get_next_sibling_build_cache(child, cache) {
            Ok(next) => (Ok(next), true),
            Err(_) => (walker.get_next_sibling(child), false),
        },
        None => (walker.get_next_sibling(child), false),
    }
}

/// 递归构建带层级子节点结构的 `UiNodeSnapshot`
fn walk_tree_recursive(
    context: &TreeWalkContext<'_>,
    element: &UIElement,
    element_cached: bool,
    depth: usize,
    visited_count: &mut usize,
) -> UiNodeSnapshot {
    *visited_count += 1;
    let mut node = if element_cached {
        snapshot_cached_element(element)
    } else {
        snapshot_single_element(element)
    };

    if depth >= context.max_depth || *visited_count >= MAX_TREE_NODES {
        return node;
    }

    let (first_child, mut child_cached) =
        next_cached_child(context.walker, context.cache_request, element);
    if let Ok(mut child) = first_child {
        loop {
            if *visited_count >= MAX_TREE_NODES {
                break;
            }
            node.children.push(walk_tree_recursive(
                context,
                &child,
                child_cached,
                depth + 1,
                visited_count,
            ));
            let (next_sibling, next_cached) =
                next_cached_sibling(context.walker, context.cache_request, &child);
            match next_sibling {
                Ok(next) => {
                    child = next;
                    child_cached = next_cached;
                }
                Err(_) => break,
            }
        }
    }

    node
}

/// 递归前序收集包含活动句柄的平铺 `CollectedNode` 列表
fn walk_flat_recursive(
    context: &TreeWalkContext<'_>,
    element: &UIElement,
    element_cached: bool,
    depth: usize,
    output: &mut Vec<CollectedNode>,
) {
    if output.len() >= MAX_TREE_NODES {
        return;
    }

    let snapshot = if element_cached {
        snapshot_cached_element(element)
    } else {
        snapshot_single_element(element)
    };
    output.push(CollectedNode {
        snapshot,
        element: UiNodeHandle::Uia(element.clone()),
    });

    if depth >= context.max_depth || output.len() >= MAX_TREE_NODES {
        return;
    }

    let (first_child, mut child_cached) =
        next_cached_child(context.walker, context.cache_request, element);
    if let Ok(mut child) = first_child {
        loop {
            if output.len() >= MAX_TREE_NODES {
                break;
            }
            walk_flat_recursive(context, &child, child_cached, depth + 1, output);
            let (next_sibling, next_cached) =
                next_cached_sibling(context.walker, context.cache_request, &child);
            match next_sibling {
                Ok(next) => {
                    child = next;
                    child_cached = next_cached;
                }
                Err(_) => break,
            }
        }
    }
}

/// 获取指定窗口句柄对应的 UIA 根元素，并优先附带属性缓存预取
fn root_element(
    automation: &UIAutomation,
    window_hwnd: isize,
    cache_request: Option<&UICacheRequest>,
) -> Result<(UIElement, bool)> {
    if !win32::is_window_valid(window_hwnd) {
        return Err(AdapterError::WindowNotFound(format!(
            "无效窗口句柄 HWND 0x{window_hwnd:x}"
        )));
    }
    if let Some(cache) = cache_request {
        if let Ok(element) =
            automation.element_from_handle_build_cache(Handle::from(window_hwnd), cache)
        {
            return Ok((element, true));
        }
    }
    let element = automation
        .element_from_handle(Handle::from(window_hwnd))
        .map_err(|error| {
            AdapterError::UiaFailure(format!("element_from_handle(0x{window_hwnd:x}): {error}"))
        })?;
    Ok((element, false))
}

/// 使用指定的 UIA 树遍历器采集完整树快照
fn dump_tree_with_walker(
    automation: &UIAutomation,
    walker: &UITreeWalker,
    window_hwnd: isize,
    max_depth: usize,
) -> Result<UiNodeSnapshot> {
    let cache_request = build_snapshot_cache_request(automation);
    let (root, root_cached) = root_element(automation, window_hwnd, cache_request.as_ref())?;
    let context = TreeWalkContext {
        walker,
        cache_request: cache_request.as_ref(),
        max_depth,
    };
    let mut visited_count = 0usize;
    Ok(walk_tree_recursive(
        &context,
        &root,
        root_cached,
        0,
        &mut visited_count,
    ))
}

/// 使用指定的 UIA 树遍历器采集平铺节点列表
fn collect_flat_with_walker(
    automation: &UIAutomation,
    walker: &UITreeWalker,
    window_hwnd: isize,
    max_depth: usize,
) -> Result<Vec<CollectedNode>> {
    let cache_request = build_snapshot_cache_request(automation);
    let (root, root_cached) = root_element(automation, window_hwnd, cache_request.as_ref())?;
    let context = TreeWalkContext {
        walker,
        cache_request: cache_request.as_ref(),
        max_depth,
    };
    let mut output = Vec::new();
    walk_flat_recursive(&context, &root, root_cached, 0, &mut output);
    Ok(output)
}

/// 仅通过 UIA `ControlViewWalker` 采集窗口控件树快照
pub(crate) fn dump_tree_control(
    automation: &UIAutomation,
    window_hwnd: isize,
    max_depth: usize,
) -> Result<UiNodeSnapshot> {
    let walker = automation
        .get_control_view_walker()
        .map_err(|error| AdapterError::UiaFailure(format!("get_control_view_walker: {error}")))?;
    dump_tree_with_walker(automation, &walker, window_hwnd, max_depth)
}

/// 仅通过 UIA `RawViewWalker` 采集窗口控件树快照
pub(crate) fn dump_tree_raw(
    automation: &UIAutomation,
    window_hwnd: isize,
    max_depth: usize,
) -> Result<UiNodeSnapshot> {
    let walker = automation
        .get_raw_view_walker()
        .map_err(|error| AdapterError::UiaFailure(format!("get_raw_view_walker: {error}")))?;
    dump_tree_with_walker(automation, &walker, window_hwnd, max_depth)
}

/// 采集指定窗口最高深度为 `max_depth` 的控件树快照；遇到未唤醒的 Chromium 骨架或零语义控件时自动依次回退到 `RawViewWalker` 与 MSAA (`IAccessible`)
pub fn dump_tree(
    automation: &UIAutomation,
    window_hwnd: isize,
    max_depth: usize,
) -> Result<UiNodeSnapshot> {
    let mut best_snapshot = dump_tree_control(automation, window_hwnd, max_depth)?;
    if max_depth > 0
        && (snapshot_needs_chromium_wake(&best_snapshot)
            || !has_semantic_descendant(&best_snapshot))
    {
        if let Ok(raw_tree) = dump_tree_raw(automation, window_hwnd, max_depth) {
            if !snapshot_needs_chromium_wake(&raw_tree) && has_semantic_descendant(&raw_tree) {
                return Ok(raw_tree);
            }
            if (!has_semantic_descendant(&best_snapshot) && has_semantic_descendant(&raw_tree))
                || (has_semantic_descendant(&best_snapshot) == has_semantic_descendant(&raw_tree)
                    && count_snapshot_nodes(&raw_tree) > count_snapshot_nodes(&best_snapshot))
            {
                best_snapshot = raw_tree;
            }
        }
        let msaa_depth = max_depth.min(u32::MAX as usize) as u32;
        if let Ok(msaa_tree) = msaa::dump_tree_msaa(window_hwnd, msaa_depth) {
            let msaa_semantic = has_semantic_descendant(&msaa_tree);
            let best_semantic = has_semantic_descendant(&best_snapshot);
            if (!snapshot_needs_chromium_wake(&msaa_tree) && msaa_semantic)
                || (msaa_semantic && !best_semantic)
                || (msaa_semantic == best_semantic
                    && count_snapshot_nodes(&msaa_tree) > count_snapshot_nodes(&best_snapshot))
            {
                return Ok(msaa_tree);
            }
        }
    }
    Ok(best_snapshot)
}

/// 使用 `RawViewWalker` 直接采集前序平铺元素与快照列表
pub fn collect_flat_raw(
    automation: &UIAutomation,
    window_hwnd: isize,
    max_depth: usize,
) -> Result<Vec<CollectedNode>> {
    let walker = automation
        .get_raw_view_walker()
        .map_err(|error| AdapterError::UiaFailure(format!("get_raw_view_walker: {error}")))?;
    collect_flat_with_walker(automation, &walker, window_hwnd, max_depth)
}

/// 统计平铺节点列表中属于语义内容控件的数量
fn flat_semantic_count(nodes: &[CollectedNode]) -> usize {
    nodes
        .iter()
        .filter(|node| is_semantic_content_control(&node.snapshot.control_type))
        .count()
}

/// 采集指定窗口最高深度为 `max_depth` 的前序平铺控件列表；遇到未唤醒的 Chromium 骨架或零语义节点时依次回退至 `RawViewWalker` 与 MSAA
pub fn collect_flat(
    automation: &UIAutomation,
    window_hwnd: isize,
    max_depth: usize,
) -> Result<Vec<CollectedNode>> {
    let walker = automation
        .get_control_view_walker()
        .map_err(|error| AdapterError::UiaFailure(format!("get_control_view_walker: {error}")))?;
    let mut best_nodes = collect_flat_with_walker(automation, &walker, window_hwnd, max_depth)?;
    if max_depth > 0
        && (flat_needs_chromium_wake(&best_nodes) || flat_semantic_count(&best_nodes) == 0)
    {
        if let Ok(raw_nodes) = collect_flat_raw(automation, window_hwnd, max_depth) {
            let raw_semantic = flat_semantic_count(&raw_nodes);
            let best_semantic = flat_semantic_count(&best_nodes);
            if !flat_needs_chromium_wake(&raw_nodes) && raw_semantic > 0 {
                return Ok(raw_nodes);
            }
            if raw_semantic > best_semantic
                || (raw_semantic == best_semantic && raw_nodes.len() > best_nodes.len())
            {
                best_nodes = raw_nodes;
            }
        }
        let msaa_depth = max_depth.min(u32::MAX as usize) as u32;
        if let Ok(msaa_nodes) = msaa::collect_flat_msaa(window_hwnd, msaa_depth) {
            let msaa_semantic = flat_semantic_count(&msaa_nodes);
            let best_semantic = flat_semantic_count(&best_nodes);
            if msaa_semantic > best_semantic
                || (msaa_semantic == best_semantic && msaa_nodes.len() > best_nodes.len())
            {
                return Ok(msaa_nodes);
            }
        }
    }
    Ok(best_nodes)
}

/// 判断给定控件类型是否代表真实语义内容控件（排除通用容器与窗口装饰元素：`Pane`、`Window`、`TitleBar`、`MenuBar`）
pub fn is_semantic_content_control(control_type: &str) -> bool {
    let trimmed = control_type.trim();
    !trimmed.is_empty()
        && !NON_SEMANTIC_CONTROL_TYPES
            .iter()
            .any(|non_semantic| trimmed.eq_ignore_ascii_case(non_semantic))
}

/// 递归统计树快照中的节点总数
pub(crate) fn count_snapshot_nodes(node: &UiNodeSnapshot) -> usize {
    1 + node
        .children
        .iter()
        .map(count_snapshot_nodes)
        .sum::<usize>()
}

/// 检查快照任意后代中是否包含至少一个语义内容控件
fn has_semantic_descendant(node: &UiNodeSnapshot) -> bool {
    node.children.iter().any(|child| {
        is_semantic_content_control(&child.control_type) || has_semantic_descendant(child)
    })
}

/// 判断层级 UIA 快照是否处于未初始化或 Chromium 懒加载休眠骨架状态（总节点少于 5，或 `Chrome_WidgetWin_1` 下没有任何语义后代）
pub fn snapshot_needs_chromium_wake(root: &UiNodeSnapshot) -> bool {
    if count_snapshot_nodes(root) < MIN_POPULATED_TREE_NODES {
        return true;
    }
    if root.class_name == CHROME_WIDGET_WINDOW_CLASS {
        return !has_semantic_descendant(root);
    }
    false
}

/// 判断平铺 UIA 节点序列是否处于未初始化或 Chromium 懒加载休眠骨架状态
pub fn flat_needs_chromium_wake(nodes: &[CollectedNode]) -> bool {
    if nodes.len() < MIN_POPULATED_TREE_NODES {
        return true;
    }
    if nodes.first().map(|node| node.snapshot.class_name.as_str())
        == Some(CHROME_WIDGET_WINDOW_CLASS)
    {
        return !nodes
            .iter()
            .skip(1)
            .any(|node| is_semantic_content_control(&node.snapshot.control_type));
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::{
        CONTROL_TYPE_BUTTON, CONTROL_TYPE_DOCUMENT, CONTROL_TYPE_EDIT, CONTROL_TYPE_GROUP,
        CONTROL_TYPE_LIST, CONTROL_TYPE_LIST_ITEM, CONTROL_TYPE_MENU_BAR, CONTROL_TYPE_PANE,
        CONTROL_TYPE_TEXT, CONTROL_TYPE_TITLE_BAR, CONTROL_TYPE_WINDOW,
    };
    use common_types::constants::processes::{CHROME_RENDER_WIDGET_CLASS, MOCK_QQ_MAIN_CLASS};

    fn make_node(
        control_type: &str,
        class_name: &str,
        children: Vec<UiNodeSnapshot>,
    ) -> UiNodeSnapshot {
        UiNodeSnapshot {
            control_type: control_type.to_string(),
            localized_control_type: String::new(),
            name: String::new(),
            automation_id: String::new(),
            class_name: class_name.to_string(),
            rect: [0, 0, 800, 600],
            is_enabled: true,
            is_offscreen: false,
            patterns: Vec::new(),
            value: None,
            children,
        }
    }

    #[test]
    fn is_semantic_content_control_filters_containers_and_chrome() {
        for non_semantic in [
            "",
            "  ",
            CONTROL_TYPE_PANE,
            "pane",
            CONTROL_TYPE_WINDOW,
            "WINDOW",
            CONTROL_TYPE_TITLE_BAR,
            "titlebar",
            CONTROL_TYPE_MENU_BAR,
            "menubar",
        ] {
            assert!(
                !is_semantic_content_control(non_semantic),
                "{non_semantic:?} 不应被计入语义内容控件"
            );
        }
        for semantic in [
            CONTROL_TYPE_DOCUMENT,
            CONTROL_TYPE_TEXT,
            CONTROL_TYPE_LIST,
            CONTROL_TYPE_LIST_ITEM,
            CONTROL_TYPE_EDIT,
            CONTROL_TYPE_BUTTON,
            CONTROL_TYPE_GROUP,
        ] {
            assert!(
                is_semantic_content_control(semantic),
                "{semantic:?} 应当被识别为语义内容控件"
            );
        }
    }

    #[test]
    fn snapshot_needs_chromium_wake_detects_aura_skeleton_vs_semantic_content_and_mock() {
        // 仅含 9 个 Pane 的 Chrome_WidgetWin_1 Aura 空骨架应当判定为需要无障碍唤醒
        let skeleton = make_node(
            CONTROL_TYPE_PANE,
            CHROME_WIDGET_WINDOW_CLASS,
            vec![
                make_node(CONTROL_TYPE_PANE, "Intermediate D3D Window", vec![]),
                make_node(
                    CONTROL_TYPE_PANE,
                    "RootView",
                    vec![
                        make_node(
                            CONTROL_TYPE_PANE,
                            "",
                            vec![make_node(CONTROL_TYPE_PANE, "", vec![])],
                        ),
                        make_node(
                            CONTROL_TYPE_PANE,
                            "",
                            vec![make_node(CONTROL_TYPE_PANE, "", vec![])],
                        ),
                        make_node(CONTROL_TYPE_TITLE_BAR, "", vec![]),
                        make_node(CONTROL_TYPE_PANE, "", vec![]),
                    ],
                ),
            ],
        );
        assert_eq!(count_snapshot_nodes(&skeleton), 9);
        assert!(snapshot_needs_chromium_wake(&skeleton));

        // 含 Document 与 Text 后代的 Chrome_WidgetWin_1 视为已完成唤醒
        let populated = make_node(
            CONTROL_TYPE_PANE,
            CHROME_WIDGET_WINDOW_CLASS,
            vec![
                make_node(CONTROL_TYPE_PANE, "Intermediate D3D Window", vec![]),
                make_node(
                    CONTROL_TYPE_PANE,
                    "RootView",
                    vec![make_node(
                        CONTROL_TYPE_DOCUMENT,
                        CHROME_RENDER_WIDGET_CLASS,
                        vec![make_node(CONTROL_TYPE_TEXT, "", vec![])],
                    )],
                ),
            ],
        );
        assert!(!snapshot_needs_chromium_wake(&populated));

        // 非 Chromium 宿主窗口（MockQQNTWndClass）达到最小节点阈值即不再触发唤醒
        let mock_window = make_node(
            CONTROL_TYPE_WINDOW,
            MOCK_QQ_MAIN_CLASS,
            vec![
                make_node(CONTROL_TYPE_PANE, "", vec![]),
                make_node(CONTROL_TYPE_PANE, "", vec![]),
                make_node(CONTROL_TYPE_PANE, "", vec![]),
                make_node(CONTROL_TYPE_PANE, "", vec![]),
            ],
        );
        assert!(!snapshot_needs_chromium_wake(&mock_window));

        // 节点总数不足阈值时同样需要触发无障碍唤醒
        let tiny = make_node(
            CONTROL_TYPE_WINDOW,
            MOCK_QQ_MAIN_CLASS,
            vec![make_node(CONTROL_TYPE_TEXT, "", vec![])],
        );
        assert!(snapshot_needs_chromium_wake(&tiny));
    }
}
