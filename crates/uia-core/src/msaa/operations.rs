//! 基于 MSAA (`IAccessible`) 的后台值读取、写入清空、默认动作触发、焦点定位与宿主句柄反查操作模块

use common_types::{AdapterError, Result};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Accessibility::{IAccessible, WindowFromAccessibleObject};
use windows_core::BSTR;

use super::node_handle::{is_editable_role, map_role, read_role, read_state};
use crate::constants::{
    ACTION_METHOD_MSAA_DEFAULT, ACTION_METHOD_MSAA_SELECT, CONTROL_TYPE_BUTTON, SELFLAG_TAKEFOCUS,
    SELFLAG_TAKESELECTION, STATE_SYSTEM_UNAVAILABLE, VOICE_INPUT_PLACEHOLDER_PREFIX,
};
use crate::win32;

/// 查询 MSAA 节点对应的 UIA 兼容控件类型字符串（如 `"Group"`、`"Edit"` 等）
pub(crate) fn msaa_control_type(accessible: &IAccessible, child_id: i32) -> &'static str {
    let variant_child = win32::variant_i4(child_id);
    let role = read_role(accessible, &variant_child);
    let state = read_state(accessible, &variant_child);
    map_role(&role, state)
}

/// 判断给定文本是否属于已知的空值或 Windows 语音输入提示占位文本
pub(crate) fn is_known_empty_placeholder(value: &str) -> bool {
    let trimmed = value.trim();
    trimmed.is_empty() || trimmed.starts_with(VOICE_INPUT_PLACEHOLDER_PREFIX)
}

/// 根据控件可编辑性、`accValue` 与 `accName` 解析有效文本，避免编辑框占位符或群名误充当草稿正文
pub(crate) fn resolve_msaa_text(
    is_editable: bool,
    acc_value: Option<&str>,
    acc_name: Option<&str>,
) -> Option<String> {
    if let Some(value) = acc_value {
        return (!value.is_empty()).then(|| value.to_string());
    }
    if is_editable {
        return None;
    }
    acc_name
        .filter(|name| !name.is_empty())
        .map(ToString::to_string)
}

/// 仅读取 MSAA `get_accValue` 原始字符串而不回退到 `get_accName`；成功为空返回 `Some("")`，不支持返回 `None`
pub fn read_msaa_value_only(accessible: &IAccessible, child_id: i32) -> Option<String> {
    let variant_child = win32::variant_i4(child_id);
    unsafe { accessible.get_accValue(&variant_child) }
        .ok()
        .map(|text| text.to_string())
}

/// 安全读取 MSAA 节点文本：当 `get_accValue` 返回空串或编辑框读取失败时绝不回退到 `get_accName`
pub fn read_msaa_text(accessible: &IAccessible, child_id: i32) -> Option<String> {
    let variant_child = win32::variant_i4(child_id);
    let role = read_role(accessible, &variant_child);
    let state = read_state(accessible, &variant_child);
    let editable = is_editable_role(&role, state);

    let acc_value = unsafe { accessible.get_accValue(&variant_child) }
        .ok()
        .map(|text| text.to_string());
    let acc_name = if !editable && acc_value.is_none() {
        unsafe { accessible.get_accName(&variant_child) }
            .ok()
            .map(|text| text.to_string())
    } else {
        None
    };

    resolve_msaa_text(editable, acc_value.as_deref(), acc_name.as_deref())
}

/// 通过 `IAccessible::put_accValue` 向指定 MSAA 节点写入文本
pub fn write_msaa_value(accessible: &IAccessible, child_id: i32, text: &str) -> Result<()> {
    let variant_child = win32::variant_i4(child_id);
    let bstr_value = BSTR::from(text);
    unsafe { accessible.put_accValue(&variant_child, &bstr_value) }.map_err(|error| {
        AdapterError::UiaFailure(format!(
            "MSAA put_accValue(child_id={child_id}) 失败: {error}"
        ))
    })
}

/// 通过 `IAccessible::put_accValue("")` 清空节点内容并校验无残留的非占位用户文本
pub fn clear_msaa_value(accessible: &IAccessible, child_id: i32) -> Result<()> {
    write_msaa_value(accessible, child_id, "")?;
    if let Some(remaining) = read_msaa_value_only(accessible, child_id) {
        if !is_known_empty_placeholder(&remaining) {
            return Err(AdapterError::UiaFailure(format!(
                "MSAA put_accValue(\"\") 后仍残留非空编辑值 (len={})",
                remaining.len()
            )));
        }
    }
    Ok(())
}

/// 在不移动物理鼠标的前提下后台激活 MSAA 节点：按钮仅走 `accDoDefaultAction`，非按钮可回退 `accSelect`
pub fn invoke_msaa(accessible: &IAccessible, child_id: i32) -> Result<&'static str> {
    let variant_child = win32::variant_i4(child_id);
    let state = read_state(accessible, &variant_child);
    if (state & STATE_SYSTEM_UNAVAILABLE) != 0 {
        return Err(AdapterError::UiaFailure(format!(
            "MSAA 元素 (child_id={child_id}) 处于禁用状态"
        )));
    }
    match unsafe { accessible.accDoDefaultAction(&variant_child) } {
        Ok(()) => Ok(ACTION_METHOD_MSAA_DEFAULT),
        Err(default_error) => {
            let role = read_role(accessible, &variant_child);
            if map_role(&role, state) == CONTROL_TYPE_BUTTON {
                return Err(AdapterError::UiaFailure(format!(
                    "MSAA 按钮执行 accDoDefaultAction 失败: {default_error}"
                )));
            }
            match unsafe {
                accessible.accSelect(SELFLAG_TAKEFOCUS | SELFLAG_TAKESELECTION, &variant_child)
            } {
                Ok(()) => Ok(ACTION_METHOD_MSAA_SELECT),
                Err(select_error) => Err(AdapterError::UiaFailure(format!(
                    "MSAA 后台触发失败 (default_action: {default_error}; select: {select_error})"
                ))),
            }
        }
    }
}

/// 通过 `IAccessible::accSelect(SELFLAG_TAKEFOCUS)` 请求焦点至 MSAA 节点
pub fn focus_msaa(accessible: &IAccessible, child_id: i32) -> Result<()> {
    let variant_child = win32::variant_i4(child_id);
    unsafe { accessible.accSelect(SELFLAG_TAKEFOCUS, &variant_child) }.map_err(|error| {
        AdapterError::UiaFailure(format!(
            "MSAA accSelect(SELFLAG_TAKEFOCUS, child_id={child_id}) 失败: {error}"
        ))
    })
}

/// 通过 `WindowFromAccessibleObject` 反查 MSAA 节点所属的底层 Win32 窗口句柄
pub fn window_from_accessible(accessible: &IAccessible) -> Option<isize> {
    let mut output_hwnd = HWND::default();
    if unsafe { WindowFromAccessibleObject(accessible, Some(&mut output_hwnd)) }.is_ok() {
        let raw_hwnd = output_hwnd.0 as isize;
        if raw_hwnd != 0 {
            return Some(raw_hwnd);
        }
    }
    None
}
