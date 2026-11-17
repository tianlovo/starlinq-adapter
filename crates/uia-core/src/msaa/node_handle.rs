//! 跨无障碍后端控件句柄适配器（Adapter）与 MSAA 角色映射模块，统一包装 UIA `UIElement` 与 MSAA `IAccessible` 并标准化控件类型

use std::fmt;

use uiautomation::UIElement;
use windows::Win32::System::Variant::{VARIANT, VT_BSTR, VT_I4};
use windows::Win32::UI::Accessibility::{IAccessible, ROLE_SYSTEM_TEXT};

use crate::constants::{
    CONTROL_TYPE_BUTTON, CONTROL_TYPE_CHECK_BOX, CONTROL_TYPE_COMBO_BOX, CONTROL_TYPE_DATA_ITEM,
    CONTROL_TYPE_DIALOG, CONTROL_TYPE_DOCUMENT, CONTROL_TYPE_EDIT, CONTROL_TYPE_GROUP,
    CONTROL_TYPE_HEADER_ITEM, CONTROL_TYPE_IMAGE, CONTROL_TYPE_LINK, CONTROL_TYPE_LIST,
    CONTROL_TYPE_LIST_ITEM, CONTROL_TYPE_MENU, CONTROL_TYPE_MENU_BAR, CONTROL_TYPE_MENU_ITEM,
    CONTROL_TYPE_PANE, CONTROL_TYPE_PROGRESS_BAR, CONTROL_TYPE_RADIO_BUTTON,
    CONTROL_TYPE_SCROLL_BAR, CONTROL_TYPE_SEPARATOR, CONTROL_TYPE_SLIDER, CONTROL_TYPE_SPINNER,
    CONTROL_TYPE_STATUS_BAR, CONTROL_TYPE_TAB, CONTROL_TYPE_TABLE, CONTROL_TYPE_TAB_ITEM,
    CONTROL_TYPE_TEXT, CONTROL_TYPE_THUMB, CONTROL_TYPE_TITLE_BAR, CONTROL_TYPE_TOOL_BAR,
    CONTROL_TYPE_TOOL_TIP, CONTROL_TYPE_TREE, CONTROL_TYPE_TREE_ITEM, CONTROL_TYPE_WINDOW,
    STATE_SYSTEM_READONLY,
};
use crate::win32;

/// 统一表示底层已发现控件节点的句柄适配器，屏蔽 UI Automation 与 MSAA 两种后端差异
#[derive(Clone)]
pub enum UiNodeHandle {
    /// 基于 UI Automation 的原生元素句柄
    Uia(UIElement),
    /// 基于 MSAA (`IAccessible`) 与子项序号的传统无障碍句柄
    Msaa {
        /// 目标节点关联的 COM `IAccessible` 接口指针
        acc: IAccessible,
        /// 子控件索引（主对象自身使用 `CHILDID_SELF` 即 0）
        child_id: i32,
    },
}

impl fmt::Debug for UiNodeHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UiNodeHandle::Uia(element) => formatter.debug_tuple("Uia").field(element).finish(),
            UiNodeHandle::Msaa { child_id, .. } => formatter
                .debug_struct("Msaa")
                .field("child_id", child_id)
                .finish_non_exhaustive(),
        }
    }
}

impl From<UIElement> for UiNodeHandle {
    fn from(element: UIElement) -> Self {
        Self::Uia(element)
    }
}

impl UiNodeHandle {
    /// 若底层句柄为 UIA 元素则返回其引用
    pub fn as_uia(&self) -> Option<&UIElement> {
        match self {
            UiNodeHandle::Uia(element) => Some(element),
            UiNodeHandle::Msaa { .. } => None,
        }
    }

    /// 若底层句柄为 MSAA 节点则返回 `(IAccessible, child_id)` 引用元组
    pub fn as_msaa(&self) -> Option<(&IAccessible, i32)> {
        match self {
            UiNodeHandle::Msaa { acc, child_id } => Some((acc, *child_id)),
            UiNodeHandle::Uia(_) => None,
        }
    }
}

/// 内部解析后的 MSAA 节点角色枚举（涵盖标准整数常量与 Blink 自定义 BSTR 字符串角色）
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MsaaRole {
    System(i32),
    Custom(String),
    Unknown,
}

/// 将标准 Win32 `ROLE_SYSTEM_*` 角色整数与状态位掩码映射为与 UIA 兼容的 `control_type` 名称
///
/// Chromium 会同时对静态文本与可编辑输入框发射 `ROLE_SYSTEM_TEXT (42)`，此处结合缺失 `STATE_SYSTEM_READONLY` 的状态区分 `Edit` 与 `Text`
pub fn map_system_role(role: i32, state: u32) -> &'static str {
    match role {
        1 => CONTROL_TYPE_TITLE_BAR,
        2 => CONTROL_TYPE_MENU_BAR,
        3 => CONTROL_TYPE_SCROLL_BAR,
        4 => CONTROL_TYPE_THUMB,
        8 | 9 | 14 => CONTROL_TYPE_WINDOW,
        10 | 16 | 19 | 38 => CONTROL_TYPE_PANE,
        11 => CONTROL_TYPE_MENU,
        12 => CONTROL_TYPE_MENU_ITEM,
        13 | 31 => CONTROL_TYPE_TOOL_TIP,
        15 => CONTROL_TYPE_DOCUMENT,
        18 => CONTROL_TYPE_DIALOG,
        20 => CONTROL_TYPE_GROUP,
        21 => CONTROL_TYPE_SEPARATOR,
        22 => CONTROL_TYPE_TOOL_BAR,
        23 => CONTROL_TYPE_STATUS_BAR,
        24 => CONTROL_TYPE_TABLE,
        25 | 26 => CONTROL_TYPE_HEADER_ITEM,
        27..=29 => CONTROL_TYPE_DATA_ITEM,
        30 => CONTROL_TYPE_LINK,
        33 => CONTROL_TYPE_LIST,
        34 => CONTROL_TYPE_LIST_ITEM,
        35 => CONTROL_TYPE_TREE,
        36 => CONTROL_TYPE_TREE_ITEM,
        37 => CONTROL_TYPE_TAB_ITEM,
        39 | 40 => CONTROL_TYPE_IMAGE,
        41 => CONTROL_TYPE_TEXT,
        42 => {
            if (state & STATE_SYSTEM_READONLY) == 0 {
                CONTROL_TYPE_EDIT
            } else {
                CONTROL_TYPE_TEXT
            }
        }
        43 | 56..=58 | 62 | 64 => CONTROL_TYPE_BUTTON,
        44 => CONTROL_TYPE_CHECK_BOX,
        45 => CONTROL_TYPE_RADIO_BUTTON,
        46 | 47 => CONTROL_TYPE_COMBO_BOX,
        48 => CONTROL_TYPE_PROGRESS_BAR,
        51 => CONTROL_TYPE_SLIDER,
        52 => CONTROL_TYPE_SPINNER,
        60 => CONTROL_TYPE_TAB,
        _ => CONTROL_TYPE_PANE,
    }
}

/// 将 Chromium/Blink 通过 `VT_BSTR` 暴露的 ARIA 自定义角色字符串映射为与 UIA 兼容的 `control_type` 名称
pub fn map_custom_role(role: &str) -> &'static str {
    match role.trim().to_ascii_lowercase().as_str() {
        "textbox" | "searchbox" | "textarea" | "input" => CONTROL_TYPE_EDIT,
        "button" => CONTROL_TYPE_BUTTON,
        "listitem" | "option" | "row" => CONTROL_TYPE_LIST_ITEM,
        "list" | "listbox" => CONTROL_TYPE_LIST,
        "menu" => CONTROL_TYPE_MENU,
        "menuitem" | "menuitemcheckbox" | "menuitemradio" => CONTROL_TYPE_MENU_ITEM,
        "tab" => CONTROL_TYPE_TAB_ITEM,
        "tablist" => CONTROL_TYPE_TAB,
        "tree" => CONTROL_TYPE_TREE,
        "treeitem" => CONTROL_TYPE_TREE_ITEM,
        "checkbox" | "switch" => CONTROL_TYPE_CHECK_BOX,
        "radio" => CONTROL_TYPE_RADIO_BUTTON,
        "combobox" => CONTROL_TYPE_COMBO_BOX,
        "link" => CONTROL_TYPE_LINK,
        "img" | "image" => CONTROL_TYPE_IMAGE,
        "dialog" | "alertdialog" => CONTROL_TYPE_DIALOG,
        "document" | "article" => CONTROL_TYPE_DOCUMENT,
        "group" | "region" => CONTROL_TYPE_GROUP,
        "heading" | "status" | "label" | "paragraph" | "text" => CONTROL_TYPE_TEXT,
        "scrollbar" => CONTROL_TYPE_SCROLL_BAR,
        "progressbar" => CONTROL_TYPE_PROGRESS_BAR,
        "slider" => CONTROL_TYPE_SLIDER,
        "spinbutton" => CONTROL_TYPE_SPINNER,
        "table" | "grid" => CONTROL_TYPE_TABLE,
        "toolbar" => CONTROL_TYPE_TOOL_BAR,
        "tooltip" => CONTROL_TYPE_TOOL_TIP,
        "separator" => CONTROL_TYPE_SEPARATOR,
        _ => CONTROL_TYPE_PANE,
    }
}

/// 综合解析后的 MSAA 角色枚举与状态掩码映射为控件类型字符串
pub(crate) fn map_role(role: &MsaaRole, state: u32) -> &'static str {
    match role {
        MsaaRole::System(code) => map_system_role(*code, state),
        MsaaRole::Custom(raw) => map_custom_role(raw),
        MsaaRole::Unknown => CONTROL_TYPE_PANE,
    }
}

/// 判断给定 MSAA 角色与状态位是否代表可输入文本编辑框
pub(crate) fn is_editable_role(role: &MsaaRole, state: u32) -> bool {
    match role {
        MsaaRole::System(code) => {
            *code == ROLE_SYSTEM_TEXT as i32 && (state & STATE_SYSTEM_READONLY) == 0
        }
        MsaaRole::Custom(raw) => matches!(
            raw.trim().to_ascii_lowercase().as_str(),
            "textbox" | "searchbox" | "textarea" | "input"
        ),
        MsaaRole::Unknown => false,
    }
}

/// 从 `IAccessible::get_accRole` 读取并解析节点的角色变体
pub(crate) fn read_role(accessible: &IAccessible, variant_child: &VARIANT) -> MsaaRole {
    let Ok(variant_value) = (unsafe { accessible.get_accRole(variant_child) }) else {
        return MsaaRole::Unknown;
    };
    let guard = win32::SingleVariantGuard(variant_value);
    unsafe {
        let variant_type = guard.0.Anonymous.Anonymous.vt;
        if variant_type == VT_I4 {
            MsaaRole::System(guard.0.Anonymous.Anonymous.Anonymous.lVal)
        } else if variant_type == VT_BSTR {
            MsaaRole::Custom((*guard.0.Anonymous.Anonymous.Anonymous.bstrVal).to_string())
        } else {
            MsaaRole::Unknown
        }
    }
}

/// 从 `IAccessible::get_accState` 读取节点的位掩码状态值
pub(crate) fn read_state(accessible: &IAccessible, variant_child: &VARIANT) -> u32 {
    let Ok(variant_value) = (unsafe { accessible.get_accState(variant_child) }) else {
        return 0;
    };
    let guard = win32::SingleVariantGuard(variant_value);
    unsafe {
        if guard.0.Anonymous.Anonymous.vt == VT_I4 {
            guard.0.Anonymous.Anonymous.Anonymous.lVal as u32
        } else {
            0
        }
    }
}

/// 通过 `IAccessible::accLocation` 读取节点的屏幕外接矩形 `[left, top, right, bottom]`
pub(crate) fn read_location(accessible: &IAccessible, variant_child: &VARIANT) -> [i32; 4] {
    let mut left = 0i32;
    let mut top = 0i32;
    let mut width = 0i32;
    let mut height = 0i32;
    if unsafe {
        accessible.accLocation(&mut left, &mut top, &mut width, &mut height, variant_child)
    }
    .is_ok()
    {
        [
            left,
            top,
            left.saturating_add(width),
            top.saturating_add(height),
        ]
    } else {
        [0, 0, 0, 0]
    }
}
