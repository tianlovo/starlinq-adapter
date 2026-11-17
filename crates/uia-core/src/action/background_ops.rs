//! 零物理鼠标接管与零前台夺焦的后台交互动作模块，采用责任链模式依次尝试 UIA 模式写值、`WM_CHAR` 投递与后台按钮触发

use std::ffi::c_void;
use std::thread;
use std::time::Duration;

use common_types::constants::processes::CHROME_RENDER_WIDGET_CLASS;
use common_types::{AdapterError, Result, SendShortcut};
use rand::Rng;
use uiautomation::patterns::{
    UIInvokePattern, UILegacyIAccessiblePattern, UISelectionItemPattern, UIValuePattern,
};
use uiautomation::types::ControlType;
use uiautomation::UIElement;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{VK_CONTROL, VK_RETURN};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_CHAR, WM_KEYDOWN, WM_KEYUP};

use super::focus_guard::focus_editor_handle;
use super::models::TypingDelayRange;
use crate::constants::{
    ACTION_METHOD_UIA_INVOKE, ACTION_METHOD_UIA_LEGACY_DEFAULT, ACTION_METHOD_UIA_SELECT,
    CONTROL_TYPE_EDIT, CONTROL_TYPE_GROUP, KEY_DOWN_UP_INTERVAL_MS, POST_CHAR_INPUT_SETTLE_MS,
    POST_VALUE_WRITE_SETTLE_MS, RETRY_INVOKE_DELAY_MS, SCAN_KEYDOWN_CONTROL_LPARAM,
    SCAN_KEYDOWN_RETURN_LPARAM, SCAN_KEYUP_CONTROL_LPARAM, SCAN_KEYUP_RETURN_LPARAM,
};
use crate::msaa::{self, UiNodeHandle};
use crate::tree;
use crate::win32::{self, VerifiedWindowIdentity};

/// 校验证明子窗口句柄依然合法、所属进程 PID 未变更且根祖先窗口严格等于已绑定目标
pub(crate) fn verify_child_hwnd_belongs_to_target(
    identity: &VerifiedWindowIdentity,
    child_hwnd: isize,
    label: &str,
) -> Result<()> {
    if !win32::is_window_valid(child_hwnd) {
        return Err(AdapterError::WindowNotFound(format!(
            "{label} 句柄 HWND 0x{child_hwnd:x} 已失效"
        )));
    }
    let child_pid = win32::get_window_pid(child_hwnd);
    if child_pid != identity.pid() {
        return Err(AdapterError::WindowNotFound(format!(
            "{label} 句柄 HWND 0x{child_hwnd:x} 所属 PID {child_pid} 与已绑定目标 PID {} 不符",
            identity.pid()
        )));
    }
    let root = win32::get_root_ancestor(child_hwnd);
    if root != 0 && root != identity.hwnd() {
        return Err(AdapterError::WindowNotFound(format!(
            "{label} 句柄 HWND 0x{child_hwnd:x} 根窗口 0x{root:x} 与已绑定目标 HWND 0x{:x} 不符",
            identity.hwnd()
        )));
    }
    Ok(())
}

/// 核验已绑定的 QQ 窗口依然存活且指定无障碍控件节点严格归属于同一进程及根窗口
pub fn verify_element_belongs_to_target(
    identity: &VerifiedWindowIdentity,
    element: &UiNodeHandle,
) -> Result<()> {
    identity.verify_fast()?;
    match element {
        UiNodeHandle::Uia(uia_element) => {
            let element_pid = uia_element.get_process_id().map_err(|error| {
                AdapterError::WindowNotFound(format!(
                    "查询 UIA 节点归属 PID 失败 (bound hwnd=0x{:x}): {error}",
                    identity.hwnd()
                ))
            })?;
            if element_pid != identity.pid() {
                return Err(AdapterError::WindowNotFound(format!(
                    "UIA 节点 PID {element_pid} 与已绑定目标 PID {} (hwnd=0x{:x}) 不匹配",
                    identity.pid(),
                    identity.hwnd()
                )));
            }
            if let Ok(native_handle) = uia_element.get_native_window_handle() {
                let raw_hwnd: isize = native_handle.into();
                if raw_hwnd != 0 {
                    verify_child_hwnd_belongs_to_target(identity, raw_hwnd, "UIA 原生子窗口")?;
                }
            }
            Ok(())
        }
        UiNodeHandle::Msaa { acc, .. } => {
            let accessible_hwnd = msaa::window_from_accessible(acc).ok_or_else(|| {
                AdapterError::WindowNotFound(format!(
                    "无法解析 MSAA 节点归属 HWND (bound hwnd=0x{:x})",
                    identity.hwnd()
                ))
            })?;
            verify_child_hwnd_belongs_to_target(identity, accessible_hwnd, "MSAA 子窗口")
        }
    }
}

/// 先执行绑定身份与归属校验，再在后台无鼠标干扰模式下触发指定控件
pub fn invoke_element_background_verified(
    identity: &VerifiedWindowIdentity,
    element: &UiNodeHandle,
) -> Result<&'static str> {
    verify_element_belongs_to_target(identity, element)?;
    invoke_element_background(element)
}

/// 在不移动物理鼠标指针、不调用 `click()` 的前提下通过无障碍后端模式在后台激活控件
pub fn invoke_element_background(element: &UiNodeHandle) -> Result<&'static str> {
    match element {
        UiNodeHandle::Uia(uia_element) => invoke_uia_element_background(uia_element),
        UiNodeHandle::Msaa { acc, child_id } => msaa::invoke_msaa(acc, *child_id),
    }
}

/// 按 `Invoke -> SelectionItem -> LegacyIAccessible` 责任链顺序尝试后台激活单个 UIA 元素
fn invoke_uia_element_background(element: &UIElement) -> Result<&'static str> {
    if let Ok(pattern) = element.get_pattern::<UIInvokePattern>() {
        if pattern.invoke().is_ok() {
            return Ok(ACTION_METHOD_UIA_INVOKE);
        }
    }

    if let Ok(pattern) = element.get_pattern::<UISelectionItemPattern>() {
        if pattern.select().is_ok() {
            return Ok(ACTION_METHOD_UIA_SELECT);
        }
    }

    if let Ok(pattern) = element.get_pattern::<UILegacyIAccessiblePattern>() {
        if pattern.do_default_action().is_ok() {
            return Ok(ACTION_METHOD_UIA_LEGACY_DEFAULT);
        }
    }

    Err(AdapterError::UiaFailure(
        "控件不支持 Invoke、SelectionItem 或 LegacyIAccessible 后台默认动作".to_string(),
    ))
}

/// 将文本中的 `\r\n` 与 `\r` 统一规范化为 `\n`
pub(crate) fn normalize_line_endings(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// 根据控件类型与类名判断元素是否属于原生 Win32 `Edit` 控件（排除 Chromium `Group`/`Document` 与 `contenteditable`）
pub(crate) fn is_native_edit_descriptor(control_type: &str, class_name: Option<&str>) -> bool {
    control_type.trim().eq_ignore_ascii_case(CONTROL_TYPE_EDIT)
        && class_name.is_some_and(|class| class.trim().eq_ignore_ascii_case(CONTROL_TYPE_EDIT))
}

/// 检查给定控件句柄是否为原生 Win32 `Edit` 编辑框，防止向 Chromium 富文本输入区注入会丢失换行的后台消息
pub(crate) fn is_native_win32_edit_control(editor: &UiNodeHandle) -> bool {
    match editor {
        UiNodeHandle::Uia(uia_element) => {
            if uia_element.get_control_type().ok() != Some(ControlType::Edit) {
                return false;
            }
            if let Ok(class_name) = uia_element.get_classname() {
                if is_native_edit_descriptor(CONTROL_TYPE_EDIT, Some(&class_name)) {
                    return true;
                }
            }
            if let Ok(native_handle) = uia_element.get_native_window_handle() {
                let raw_hwnd: isize = native_handle.into();
                if raw_hwnd != 0 {
                    let hwnd_class = win32::get_class_name(raw_hwnd);
                    return is_native_edit_descriptor(CONTROL_TYPE_EDIT, Some(&hwnd_class));
                }
            }
            false
        }
        UiNodeHandle::Msaa { acc, child_id } => {
            let control_type = msaa::msaa_control_type(acc, *child_id);
            if !control_type.eq_ignore_ascii_case(CONTROL_TYPE_EDIT) {
                return false;
            }
            let hwnd_class = msaa::window_from_accessible(acc).map(win32::get_class_name);
            is_native_edit_descriptor(control_type, hwnd_class.as_deref())
        }
    }
}

/// 比较实际输入框文本与期望文本是否精确或去空白后等价（禁止子串命中掩盖双倍重复输入）
pub(crate) fn text_matches_expected(actual: &str, expected: &str) -> bool {
    let normalized_expected = normalize_line_endings(expected);
    if normalized_expected.is_empty() {
        return actual.is_empty() || msaa::is_known_empty_placeholder(actual);
    }
    if msaa::is_known_empty_placeholder(actual) {
        return false;
    }
    let normalized_actual = normalize_line_endings(actual);
    normalized_actual == normalized_expected
        || normalized_actual.trim() == normalized_expected.trim()
}

/// 回读输入框当前内容并核对是否已真实接受预期文本
pub(crate) fn editor_accepted_text(editor: &UiNodeHandle, expected: &str) -> bool {
    let Some(actual) =
        tree::read_element_value_only(editor).or_else(|| tree::read_element_text(editor))
    else {
        return false;
    };
    text_matches_expected(&actual, expected)
}

/// 为跨进程 `PostMessageW` 投递解析最精确的底层接收窗口句柄（优先原生子控件及 Chromium 渲染宿主窗体）
pub(crate) fn resolve_postmessage_target(
    window_hwnd: isize,
    element: Option<&UiNodeHandle>,
) -> isize {
    match element {
        Some(UiNodeHandle::Uia(uia_element)) => {
            resolve_uia_postmessage_target(window_hwnd, Some(uia_element))
        }
        Some(UiNodeHandle::Msaa { acc, .. }) => {
            if let Some(render_hwnd) =
                win32::find_child_window_by_class(window_hwnd, CHROME_RENDER_WIDGET_CLASS)
                    .filter(|&candidate| win32::is_window_valid(candidate))
            {
                return render_hwnd;
            }
            if let Some(object_hwnd) = msaa::window_from_accessible(acc).filter(|&candidate| {
                candidate != 0 && candidate != window_hwnd && win32::is_window_valid(candidate)
            }) {
                return object_hwnd;
            }
            resolve_uia_postmessage_target(window_hwnd, None)
        }
        None => resolve_uia_postmessage_target(window_hwnd, None),
    }
}

/// 为 UIA 元素解析用于投递窗口消息的原生 `HWND`（无效时回退 Chromium 渲染子窗口或顶层宿主）
fn resolve_uia_postmessage_target(window_hwnd: isize, element: Option<&UIElement>) -> isize {
    if let Some(uia_element) = element {
        if let Ok(native_handle) = uia_element.get_native_window_handle() {
            let raw_hwnd: isize = native_handle.into();
            if raw_hwnd != 0 && win32::is_window_valid(raw_hwnd) {
                return raw_hwnd;
            }
        }
    }
    win32::find_child_window_by_class(window_hwnd, CHROME_RENDER_WIDGET_CLASS)
        .unwrap_or(window_hwnd)
}

/// 按传入的 `TypingDelayRange` 随机休眠一定毫秒，模拟自然人工敲击节奏
pub(crate) fn sleep_random_delay(delay_range: &TypingDelayRange) {
    let min_ms = delay_range.min_ms.min(delay_range.max_ms);
    let max_ms = delay_range.min_ms.max(delay_range.max_ms);
    if max_ms == 0 {
        return;
    }
    let delay = if min_ms == max_ms {
        min_ms
    } else {
        rand::thread_rng().gen_range(min_ms..=max_ms)
    };
    if delay > 0 {
        thread::sleep(Duration::from_millis(delay));
    }
}

/// 在发送每条 `PostMessageW` 前即时校验身份存活与接收句柄所属 PID 一致性，阻断窗口销毁重用瞬间的误投递
pub(crate) fn verify_postmessage_target_pid(
    identity: &VerifiedWindowIdentity,
    target_post_hwnd: isize,
) -> Result<()> {
    identity.verify_fast()?;
    let post_pid = win32::get_window_pid(target_post_hwnd);
    if post_pid == 0 || post_pid != identity.pid() {
        return Err(AdapterError::WindowNotFound(format!(
            "PostMessage 目标句柄 HWND 0x{target_post_hwnd:x} PID {post_pid} 与已绑定目标 PID {} 不匹配",
            identity.pid()
        )));
    }
    Ok(())
}

/// 内部实现的逐字符 UTF-16 `WM_CHAR` 后台输入逻辑，支持可选窗口身份保护校验
fn post_chars_background_inner(
    window_hwnd: isize,
    editor: &UiNodeHandle,
    text: &str,
    delay_range: &TypingDelayRange,
    identity: Option<&VerifiedWindowIdentity>,
) -> Result<bool> {
    if let Some(verified) = identity {
        verify_element_belongs_to_target(verified, editor)?;
    }

    let normalized = normalize_line_endings(text);
    // Chromium contenteditable 在纯后台模式下无法同步 Shift 修饰态且会吞掉硬换行，直接拒绝避免污染编辑器或空等校验超时
    if normalized.contains('\n') && !is_native_win32_edit_control(editor) {
        return Ok(false);
    }

    match editor {
        UiNodeHandle::Uia(uia_element) => {
            if let Ok(legacy) = uia_element.get_pattern::<UILegacyIAccessiblePattern>() {
                let _ = legacy.do_default_action();
            }
        }
        UiNodeHandle::Msaa { acc, child_id } => {
            let _ = msaa::focus_msaa(acc, *child_id);
        }
    }

    let target = resolve_postmessage_target(window_hwnd, Some(editor));
    if !win32::is_window_valid(target) {
        return Err(AdapterError::WindowNotFound(format!(
            "目标句柄 HWND 0x{target:x} 不可用于 WM_CHAR 后台投递"
        )));
    }
    if let Some(verified) = identity {
        verify_child_hwnd_belongs_to_target(verified, target, "WM_CHAR 投递目标")?;
    }

    let target_hwnd = HWND(target as *mut c_void);
    for utf16_unit in normalized.encode_utf16() {
        if let Some(verified) = identity {
            verify_postmessage_target_pid(verified, target)?;
        }
        // 原生 Win32 多行 Edit 控件需接收 `\r` 才能插入换行
        let char_code = if utf16_unit == u16::from(b'\n') {
            u16::from(b'\r')
        } else {
            utf16_unit
        };
        unsafe {
            let _ = PostMessageW(
                Some(target_hwnd),
                WM_CHAR,
                WPARAM(char_code as usize),
                LPARAM(1),
            );
        }
        sleep_random_delay(delay_range);
    }

    thread::sleep(Duration::from_millis(POST_CHAR_INPUT_SETTLE_MS));
    Ok(editor_accepted_text(editor, &normalized))
}

/// 通过向编辑框原生句柄或 Chromium 渲染子窗口逐字投递 UTF-16 `WM_CHAR` 消息完成后台输入，并回读确认是否生效
pub fn post_chars_background(
    window_hwnd: isize,
    editor: &UiNodeHandle,
    text: &str,
    delay_range: &TypingDelayRange,
) -> Result<bool> {
    post_chars_background_inner(window_hwnd, editor, text, delay_range, None)
}

/// 判定给定控件类型是否应当跳过直接调用 MSAA `put_accValue`（避免跳过 Chromium JS 输入事件导致发送按钮保持禁用）
pub(crate) fn should_skip_msaa_put_acc_value(control_type: &str) -> bool {
    control_type == CONTROL_TYPE_GROUP
}

/// 内部实现的纯后台写值优先+`WM_CHAR` 回退输入逻辑
fn type_text_background_inner(
    window_hwnd: isize,
    editor: &UiNodeHandle,
    text: &str,
    delay_range: &TypingDelayRange,
    identity: Option<&VerifiedWindowIdentity>,
) -> Result<bool> {
    if let Some(verified) = identity {
        verify_element_belongs_to_target(verified, editor)?;
    }

    let normalized = normalize_line_endings(text);
    // 非原生 Win32 Edit 控件（如 Chromium `Group`/`Document`）在 `set_value` 提交后会剥离换行，直接退让给前台 `Shift+Enter` 路径
    if normalized.contains('\n') && !is_native_win32_edit_control(editor) {
        return Ok(false);
    }

    let value_written = match editor {
        UiNodeHandle::Uia(uia_element) => uia_element
            .get_pattern::<UIValuePattern>()
            .ok()
            .is_some_and(|pattern| pattern.set_value(&normalized).is_ok()),
        UiNodeHandle::Msaa { acc, child_id } => {
            let control_type = msaa::msaa_control_type(acc, *child_id);
            !should_skip_msaa_put_acc_value(control_type)
                && msaa::write_msaa_value(acc, *child_id, &normalized).is_ok()
        }
    };
    if value_written {
        for wait_ms in [POST_VALUE_WRITE_SETTLE_MS, 35, 55, 80] {
            thread::sleep(Duration::from_millis(wait_ms));
            if editor_accepted_text(editor, &normalized) {
                return Ok(true);
            }
        }
        if !is_native_win32_edit_control(editor) {
            return Ok(false);
        }
        let _ = tree::clear_element_text(editor);
    }

    post_chars_background_inner(window_hwnd, editor, &normalized, delay_range, identity)
}

/// 纯后台输入文本（不移动鼠标且不切换前台），优先尝试值接口直接写入，失败或针对 `Group` 富文本框时回退到 `WM_CHAR` 投递
pub fn type_text_background(
    window_hwnd: isize,
    editor: &UiNodeHandle,
    text: &str,
    delay_range: &TypingDelayRange,
) -> Result<bool> {
    type_text_background_inner(window_hwnd, editor, text, delay_range, None)
}

/// [`type_text_background`] 的强制身份保护版本，在每次发送消息前校验证书绑定与目标 PID 一致性
pub fn type_text_background_verified(
    identity: &VerifiedWindowIdentity,
    editor: &UiNodeHandle,
    text: &str,
    delay_range: &TypingDelayRange,
) -> Result<bool> {
    type_text_background_inner(identity.hwnd(), editor, text, delay_range, Some(identity))
}

/// 内部实现的后台提交逻辑：优先触发发送按钮，失败重试后回退至向编辑器窗口投递 `PostMessageW` 按键序列
fn send_submit_background_for_editor_inner(
    window_hwnd: isize,
    editor: Option<&UiNodeHandle>,
    send_button: Option<&UiNodeHandle>,
    shortcut: SendShortcut,
    identity: Option<&VerifiedWindowIdentity>,
) -> Result<()> {
    if let Some(verified) = identity {
        verified.verify_fast()?;
        if let Some(editor_handle) = editor {
            verify_element_belongs_to_target(verified, editor_handle)?;
        }
    }

    if let Some(button_handle) = send_button {
        let first_attempt = if let Some(verified) = identity {
            invoke_element_background_verified(verified, button_handle)
        } else {
            invoke_element_background(button_handle)
        };
        if first_attempt.is_ok() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(RETRY_INVOKE_DELAY_MS));
        let retry_attempt = if let Some(verified) = identity {
            invoke_element_background_verified(verified, button_handle)
        } else {
            invoke_element_background(button_handle)
        };
        if retry_attempt.is_ok() {
            return Ok(());
        }
    }

    if let Some(editor_handle) = editor {
        focus_editor_handle(editor_handle);
    }

    let target = resolve_postmessage_target(window_hwnd, editor);
    if !win32::is_window_valid(target) {
        return Err(AdapterError::WindowNotFound(format!(
            "目标句柄 HWND 0x{target:x} 不可用于后台触发发送"
        )));
    }
    if let Some(verified) = identity {
        verify_child_hwnd_belongs_to_target(verified, target, "后台提交目标")?;
        verify_postmessage_target_pid(verified, target)?;
    }
    let target_hwnd = HWND(target as *mut c_void);

    unsafe {
        match shortcut {
            SendShortcut::Enter => {
                let _ = PostMessageW(
                    Some(target_hwnd),
                    WM_KEYDOWN,
                    WPARAM(VK_RETURN.0 as usize),
                    LPARAM(SCAN_KEYDOWN_RETURN_LPARAM),
                );
                thread::sleep(Duration::from_millis(KEY_DOWN_UP_INTERVAL_MS));
                if let Some(verified) = identity {
                    verify_postmessage_target_pid(verified, target)?;
                }
                let _ = PostMessageW(
                    Some(target_hwnd),
                    WM_KEYUP,
                    WPARAM(VK_RETURN.0 as usize),
                    LPARAM(SCAN_KEYUP_RETURN_LPARAM),
                );
            }
            SendShortcut::CtrlEnter => {
                let _ = PostMessageW(
                    Some(target_hwnd),
                    WM_KEYDOWN,
                    WPARAM(VK_CONTROL.0 as usize),
                    LPARAM(SCAN_KEYDOWN_CONTROL_LPARAM),
                );
                let _ = PostMessageW(
                    Some(target_hwnd),
                    WM_KEYDOWN,
                    WPARAM(VK_RETURN.0 as usize),
                    LPARAM(SCAN_KEYDOWN_RETURN_LPARAM),
                );
                thread::sleep(Duration::from_millis(KEY_DOWN_UP_INTERVAL_MS));
                if let Some(verified) = identity {
                    verify_postmessage_target_pid(verified, target)?;
                }
                let _ = PostMessageW(
                    Some(target_hwnd),
                    WM_KEYUP,
                    WPARAM(VK_RETURN.0 as usize),
                    LPARAM(SCAN_KEYUP_RETURN_LPARAM),
                );
                let _ = PostMessageW(
                    Some(target_hwnd),
                    WM_KEYUP,
                    WPARAM(VK_CONTROL.0 as usize),
                    LPARAM(SCAN_KEYUP_CONTROL_LPARAM),
                );
            }
        }
    }

    Ok(())
}

/// 面向编辑器上下文的后台提交接口，优先尝试无障碍点击 `send_button`，失败则投递快捷键消息至目标句柄
pub fn send_submit_background_for_editor(
    window_hwnd: isize,
    editor: Option<&UiNodeHandle>,
    send_button: Option<&UiNodeHandle>,
    shortcut: SendShortcut,
) -> Result<()> {
    send_submit_background_for_editor_inner(window_hwnd, editor, send_button, shortcut, None)
}

/// [`send_submit_background_for_editor`] 的强制身份保护版本，在触发按钮及投递按键消息前校验证书绑定归属
pub fn send_submit_background_for_editor_verified(
    identity: &VerifiedWindowIdentity,
    editor: Option<&UiNodeHandle>,
    send_button: Option<&UiNodeHandle>,
    shortcut: SendShortcut,
) -> Result<()> {
    send_submit_background_for_editor_inner(
        identity.hwnd(),
        editor,
        send_button,
        shortcut,
        Some(identity),
    )
}

/// 不依赖显式编辑器引用的后台提交入口，优先点击 `send_button` 并回退至顶层或渲染子窗口投递快捷键
pub fn send_submit_background(
    window_hwnd: isize,
    send_button: Option<&UiNodeHandle>,
    shortcut: SendShortcut,
) -> Result<()> {
    send_submit_background_for_editor(window_hwnd, None, send_button, shortcut)
}
