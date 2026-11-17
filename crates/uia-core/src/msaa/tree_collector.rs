//! MSAA (`IAccessible`) 控件树采集与 Chromium 子渲染窗口补采模块

use std::ffi::c_void;

use common_types::constants::processes::CHROME_RENDER_WIDGET_CLASS;
use common_types::{AdapterError, Result, UiNodeSnapshot, WakeReport};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Variant::{VARIANT, VT_DISPATCH, VT_I4};
use windows::Win32::UI::Accessibility::{
    AccessibleChildren, AccessibleObjectFromWindow, IAccessible,
};
use windows::Win32::UI::WindowsAndMessaging::OBJID_CLIENT;
use windows_core::Interface;

use super::node_handle::{map_role, read_location, read_role, read_state, MsaaRole, UiNodeHandle};
use crate::constants::{
    CHILDID_SELF, CONTROL_TYPE_BUTTON, CONTROL_TYPE_CHECK_BOX, CONTROL_TYPE_COMBO_BOX,
    CONTROL_TYPE_DOCUMENT, CONTROL_TYPE_EDIT, CONTROL_TYPE_LINK, CONTROL_TYPE_LIST_ITEM,
    CONTROL_TYPE_MENU_ITEM, CONTROL_TYPE_RADIO_BUTTON, CONTROL_TYPE_TAB_ITEM,
    CONTROL_TYPE_TREE_ITEM, MAX_CHILDREN_PER_CONTAINER, MAX_MSAA_TREE_NODES, PATTERN_INVOKE,
    PATTERN_LEGACY_IACCESSIBLE, PATTERN_SELECTION_ITEM, PATTERN_TEXT, PATTERN_VALUE,
    STATE_SYSTEM_INVISIBLE, STATE_SYSTEM_OFFSCREEN, STATE_SYSTEM_UNAVAILABLE,
};
use crate::tree::{self, CollectedNode};
use crate::win32;

/// MSAA 控件树递归遍历共享上下文参数对象，用于控制递归函数参数规模
struct MsaaWalkContext<'a> {
    max_depth: u32,
    node_count: &'a mut usize,
    flat_output: Option<&'a mut Vec<CollectedNode>>,
}

/// 获取指定窗口句柄的根 `IAccessible`（`OBJID_CLIENT`）并发起 `IAccessible2` / `ISimpleDOMNode` 唤醒探针
pub(crate) fn accessible_from_hwnd(window_hwnd: isize) -> Result<IAccessible> {
    if window_hwnd == 0 {
        return Err(AdapterError::WindowNotFound(
            "目标窗口句柄 hwnd 为 0".to_string(),
        ));
    }
    let mut accessible_pointer: *mut c_void = std::ptr::null_mut();
    unsafe {
        AccessibleObjectFromWindow(
            HWND(window_hwnd as *mut c_void),
            OBJID_CLIENT.0 as u32,
            &IAccessible::IID,
            &mut accessible_pointer,
        )
    }
    .map_err(|error| {
        AdapterError::UiaFailure(format!(
            "AccessibleObjectFromWindow({window_hwnd:#x}) 失败: {error}"
        ))
    })?;

    if accessible_pointer.is_null() {
        return Err(AdapterError::UiaFailure(format!(
            "AccessibleObjectFromWindow({window_hwnd:#x}) 返回空指针"
        )));
    }

    let accessible: IAccessible = unsafe { IAccessible::from_raw(accessible_pointer) };
    let mut first_hresult = None;
    let mut report = WakeReport::default();
    unsafe { win32::query_ia2_interfaces(&accessible, &mut first_hresult, &mut report) };
    Ok(accessible)
}

/// 为单个 MSAA 节点构造兼容 UIA 字段约定的 `UiNodeSnapshot`
fn build_msaa_snapshot(
    accessible: &IAccessible,
    hwnd_for_root: isize,
    role: &MsaaRole,
    state: u32,
    variant_child: &VARIANT,
) -> UiNodeSnapshot {
    let control_type = map_role(role, state);
    let localized_control_type = match role {
        MsaaRole::Custom(raw) if !raw.trim().is_empty() => raw.trim().to_string(),
        _ => control_type.to_string(),
    };

    let name = unsafe { accessible.get_accName(variant_child) }
        .ok()
        .map(|text| text.to_string())
        .unwrap_or_default();

    let raw_value = unsafe { accessible.get_accValue(variant_child) }
        .ok()
        .map(|text| text.to_string());
    let value = match control_type {
        CONTROL_TYPE_EDIT | CONTROL_TYPE_COMBO_BOX => raw_value,
        _ => raw_value.filter(|text| !text.is_empty()),
    };

    let has_default_action = unsafe { accessible.get_accDefaultAction(variant_child) }
        .map(|text| !text.to_string().trim().is_empty())
        .unwrap_or(false);

    let mut patterns = vec![PATTERN_LEGACY_IACCESSIBLE.to_string()];
    if matches!(control_type, CONTROL_TYPE_EDIT | CONTROL_TYPE_COMBO_BOX) || value.is_some() {
        patterns.push(PATTERN_VALUE.to_string());
    }
    if matches!(control_type, CONTROL_TYPE_EDIT | CONTROL_TYPE_DOCUMENT) {
        patterns.push(PATTERN_TEXT.to_string());
    }
    if has_default_action
        || matches!(
            control_type,
            CONTROL_TYPE_BUTTON
                | CONTROL_TYPE_MENU_ITEM
                | CONTROL_TYPE_LIST_ITEM
                | CONTROL_TYPE_TAB_ITEM
                | CONTROL_TYPE_CHECK_BOX
                | CONTROL_TYPE_RADIO_BUTTON
                | CONTROL_TYPE_LINK
        )
    {
        patterns.push(PATTERN_INVOKE.to_string());
    }
    if matches!(
        control_type,
        CONTROL_TYPE_LIST_ITEM
            | CONTROL_TYPE_TAB_ITEM
            | CONTROL_TYPE_TREE_ITEM
            | CONTROL_TYPE_RADIO_BUTTON
    ) {
        patterns.push(PATTERN_SELECTION_ITEM.to_string());
    }

    let class_name = if hwnd_for_root != 0 {
        win32::get_class_name(hwnd_for_root)
    } else {
        String::new()
    };

    UiNodeSnapshot {
        control_type: control_type.to_string(),
        localized_control_type,
        name,
        automation_id: String::new(),
        class_name,
        rect: read_location(accessible, variant_child),
        is_enabled: (state & STATE_SYSTEM_UNAVAILABLE) == 0,
        is_offscreen: (state & STATE_SYSTEM_OFFSCREEN) != 0,
        patterns,
        value,
        children: Vec::new(),
    }
}

/// 递归遍历 MSAA `IAccessible` 树并同时服务分层快照树与前序平铺节点采集
fn walk_msaa_recursive(
    accessible: &IAccessible,
    child_id: i32,
    hwnd_for_root: isize,
    depth: u32,
    context: &mut MsaaWalkContext<'_>,
) -> Option<UiNodeSnapshot> {
    if *context.node_count >= MAX_MSAA_TREE_NODES {
        return None;
    }

    let variant_child = win32::variant_i4(child_id);
    let state = read_state(accessible, &variant_child);
    if depth >= 1 && (state & STATE_SYSTEM_INVISIBLE) != 0 {
        return None;
    }

    *context.node_count += 1;
    let role = read_role(accessible, &variant_child);
    let mut snapshot = build_msaa_snapshot(accessible, hwnd_for_root, &role, state, &variant_child);

    if let Some(flat_list) = context.flat_output.as_deref_mut() {
        flat_list.push(CollectedNode {
            snapshot: snapshot.clone(),
            element: UiNodeHandle::Msaa {
                acc: accessible.clone(),
                child_id,
            },
        });
    }

    if child_id == CHILDID_SELF
        && depth < context.max_depth
        && *context.node_count < MAX_MSAA_TREE_NODES
    {
        let child_count = unsafe { accessible.accChildCount() }.unwrap_or(0);
        if child_count > 0 {
            let fetch_count = (child_count as usize).min(MAX_CHILDREN_PER_CONTAINER);
            let mut children_variants =
                win32::VariantVecGuard((0..fetch_count).map(|_| VARIANT::default()).collect());
            let mut obtained: i32 = 0;
            if unsafe { AccessibleChildren(accessible, 0, &mut children_variants.0, &mut obtained) }
                .is_ok()
            {
                let count = (obtained.max(0) as usize).min(children_variants.0.len());
                for variant_item in &children_variants.0[..count] {
                    if *context.node_count >= MAX_MSAA_TREE_NODES {
                        break;
                    }
                    let variant_type = unsafe { variant_item.Anonymous.Anonymous.vt };
                    if variant_type == VT_DISPATCH {
                        let dispatch_opt =
                            unsafe { variant_item.Anonymous.Anonymous.Anonymous.pdispVal.as_ref() };
                        if let Some(Ok(child_accessible)) =
                            dispatch_opt.map(|dispatch| dispatch.cast::<IAccessible>())
                        {
                            if let Some(child_snapshot) = walk_msaa_recursive(
                                &child_accessible,
                                CHILDID_SELF,
                                0,
                                depth + 1,
                                context,
                            ) {
                                if context.flat_output.is_none() {
                                    snapshot.children.push(child_snapshot);
                                }
                            }
                        }
                    } else if variant_type == VT_I4 {
                        let child_element_id =
                            unsafe { variant_item.Anonymous.Anonymous.Anonymous.lVal };
                        if child_element_id > 0 {
                            if let Some(child_snapshot) = walk_msaa_recursive(
                                accessible,
                                child_element_id,
                                0,
                                depth + 1,
                                context,
                            ) {
                                if context.flat_output.is_none() {
                                    snapshot.children.push(child_snapshot);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    Some(snapshot)
}

/// 通过 MSAA (`IAccessible`) 从 `window_hwnd` 采集最高深度为 `max_depth` 的分层无障碍快照树
pub fn dump_tree_msaa(window_hwnd: isize, max_depth: u32) -> Result<UiNodeSnapshot> {
    let root_accessible = accessible_from_hwnd(window_hwnd)?;
    let mut node_count = 0usize;
    let mut root_snapshot = {
        let mut context = MsaaWalkContext {
            max_depth,
            node_count: &mut node_count,
            flat_output: None,
        };
        walk_msaa_recursive(&root_accessible, CHILDID_SELF, window_hwnd, 0, &mut context)
    }
    .ok_or_else(|| AdapterError::UiaFailure(format!("MSAA 根遍历返回空树 ({window_hwnd:#x})")))?;

    if max_depth > 0 && tree::snapshot_needs_chromium_wake(&root_snapshot) {
        for render_hwnd in
            win32::find_child_windows_by_class(window_hwnd, CHROME_RENDER_WIDGET_CLASS)
        {
            if node_count >= MAX_MSAA_TREE_NODES {
                break;
            }
            if let Ok(render_accessible) = accessible_from_hwnd(render_hwnd) {
                let mut context = MsaaWalkContext {
                    max_depth,
                    node_count: &mut node_count,
                    flat_output: None,
                };
                if let Some(render_tree) = walk_msaa_recursive(
                    &render_accessible,
                    CHILDID_SELF,
                    render_hwnd,
                    1,
                    &mut context,
                ) {
                    if !tree::snapshot_needs_chromium_wake(&render_tree) {
                        root_snapshot.children.push(render_tree);
                        break;
                    }
                }
            }
        }
    }

    Ok(root_snapshot)
}

/// 通过 MSAA (`IAccessible`) 从 `window_hwnd` 采集前序平铺无障碍节点与活动句柄列表
pub fn collect_flat_msaa(window_hwnd: isize, max_depth: u32) -> Result<Vec<CollectedNode>> {
    let root_accessible = accessible_from_hwnd(window_hwnd)?;
    let mut node_count = 0usize;
    let mut collected_nodes = Vec::new();
    {
        let mut context = MsaaWalkContext {
            max_depth,
            node_count: &mut node_count,
            flat_output: Some(&mut collected_nodes),
        };
        let _ = walk_msaa_recursive(&root_accessible, CHILDID_SELF, window_hwnd, 0, &mut context);
    }

    if max_depth > 0 && tree::flat_needs_chromium_wake(&collected_nodes) {
        for render_hwnd in
            win32::find_child_windows_by_class(window_hwnd, CHROME_RENDER_WIDGET_CLASS)
        {
            if node_count >= MAX_MSAA_TREE_NODES {
                break;
            }
            if let Ok(render_accessible) = accessible_from_hwnd(render_hwnd) {
                let mut extra_nodes = Vec::new();
                let mut extra_count = node_count;
                {
                    let mut context = MsaaWalkContext {
                        max_depth,
                        node_count: &mut extra_count,
                        flat_output: Some(&mut extra_nodes),
                    };
                    let _ = walk_msaa_recursive(
                        &render_accessible,
                        CHILDID_SELF,
                        render_hwnd,
                        1,
                        &mut context,
                    );
                }
                if !tree::flat_needs_chromium_wake(&extra_nodes) {
                    collected_nodes.extend(extra_nodes);
                    break;
                }
            }
        }
    }

    Ok(collected_nodes)
}
