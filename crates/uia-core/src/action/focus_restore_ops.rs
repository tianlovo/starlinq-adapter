//! 前台聚焦安全恢复（Focus-Restore）输入与提交动作模块，在仅靠纯后台通道不可行时配合鼠标退让与 RAII 焦点复原安全使用 Unicode `SendInput`

use std::ffi::c_void;
use std::mem::size_of;
use std::thread;
use std::time::Duration;

use common_types::{AdapterError, Result, SendShortcut};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE,
    VIRTUAL_KEY, VK_A, VK_BACK, VK_CONTROL, VK_ESCAPE, VK_RETURN, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;

use super::background_ops::{
    invoke_element_background, invoke_element_background_verified, normalize_line_endings,
    sleep_random_delay,
};
use super::focus_guard::{ensure_foreground_target_intact, enter_window_editor_foreground_inner};
use super::models::{ComposerSubmitTarget, TargetIdentityScope, TypingDelayRange};
use crate::constants::{
    NEWLINE_CHORD_SETTLE_MS, POST_BACKSPACE_DELAY_MS, POST_SELECT_ALL_DELAY_MS,
    PRE_SUBMIT_FOREGROUND_DELAY_MS,
};
use crate::msaa::UiNodeHandle;
use crate::win32::VerifiedWindowIdentity;

/// 向当前前台焦点发送单个 UTF-16 码元的 Unicode 按下与抬起合成键盘事件（全程不触碰剪贴板或鼠标）
fn send_unicode_unit(unit: u16) -> Result<()> {
    let inputs = [
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(0),
                    wScan: unit,
                    dwFlags: KEYEVENTF_UNICODE,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        },
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(0),
                    wScan: unit,
                    dwFlags: KEYEVENTF_UNICODE | KEYEVENTF_KEYUP,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        },
    ];

    let injected = unsafe { SendInput(&inputs, size_of::<INPUT>() as i32) };
    if injected != inputs.len() as u32 {
        return Err(AdapterError::InputRejected(format!(
            "SendInput(KEYEVENTF_UNICODE) 仅成功注入 {injected}/{} 个事件",
            inputs.len()
        )));
    }
    Ok(())
}

/// 通过 `SendInput` 按序发射虚拟按键按下/抬起事件切片
pub(crate) fn send_vk_events(events: &[(VIRTUAL_KEY, bool)], label: &str) -> Result<()> {
    let inputs: Vec<INPUT> = events
        .iter()
        .map(|&(virtual_key, is_key_up)| INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: virtual_key,
                    wScan: 0,
                    dwFlags: if is_key_up {
                        KEYEVENTF_KEYUP
                    } else {
                        Default::default()
                    },
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        })
        .collect();

    let injected = unsafe { SendInput(&inputs, size_of::<INPUT>() as i32) };
    if injected != inputs.len() as u32 {
        return Err(AdapterError::InputRejected(format!(
            "SendInput({label}) 仅成功注入 {injected}/{} 个事件",
            inputs.len()
        )));
    }
    Ok(())
}

/// 使用全局虚拟按键 `SendInput` 发送 `Enter` 或 `Ctrl+Enter` 提交快捷键
fn send_vk_submit(shortcut: SendShortcut) -> Result<()> {
    match shortcut {
        SendShortcut::Enter => send_vk_events(&[(VK_RETURN, false), (VK_RETURN, true)], "submit"),
        SendShortcut::CtrlEnter => send_vk_events(
            &[
                (VK_CONTROL, false),
                (VK_RETURN, false),
                (VK_RETURN, true),
                (VK_CONTROL, true),
            ],
            "submit",
        ),
    }
}

/// 在持有前台聚焦守护期间尝试触发发送按钮（优先走后台点击，失败回退至 `SendInput` 提交快捷键）
pub(crate) fn try_submit_in_foreground_inner(
    target_hwnd: HWND,
    send_button: Option<&UiNodeHandle>,
    shortcut: SendShortcut,
    identity: Option<&VerifiedWindowIdentity>,
) -> Result<()> {
    thread::sleep(Duration::from_millis(PRE_SUBMIT_FOREGROUND_DELAY_MS));
    ensure_foreground_target_intact(target_hwnd, identity)?;
    if let Some(button_handle) = send_button {
        let invoked = if let Some(verified) = identity {
            invoke_element_background_verified(verified, button_handle)
        } else {
            invoke_element_background(button_handle)
        };
        if invoked.is_ok() {
            return Ok(());
        }
    }
    ensure_foreground_target_intact(target_hwnd, identity)?;
    send_vk_submit(shortcut)
}

/// 校验聚合目标参数中显式携带的 `window_hwnd` 与不可变身份句柄一致（允许以 0 表示缺省复用身份句柄）
fn ensure_verified_target_window_matches(
    identity: &VerifiedWindowIdentity,
    target_window_hwnd: isize,
) -> Result<()> {
    if target_window_hwnd != 0 && target_window_hwnd != identity.hwnd() {
        return Err(AdapterError::WindowNotFound(format!(
            "目标参数窗口 HWND 0x{target_window_hwnd:x} 与已绑定目标窗口 HWND 0x{:x} 不一致",
            identity.hwnd()
        )));
    }
    Ok(())
}

/// 在已聚焦的活动编辑器内按行发送 UTF-16 Unicode 文本，并在行间注入 `Shift+Enter` 换行组合键
pub fn type_text_focus_restore_in_active_editor(
    window_hwnd: isize,
    text: &str,
    delay_range: &TypingDelayRange,
    identity: Option<&crate::win32::VerifiedWindowIdentity>,
) -> Result<()> {
    let target_hwnd = if let Some(verified) = identity {
        HWND(verified.hwnd() as *mut c_void)
    } else {
        HWND(window_hwnd as *mut c_void)
    };
    let normalized = normalize_line_endings(text);

    for (line_index, line) in normalized.split('\n').enumerate() {
        if line_index > 0 {
            ensure_foreground_target_intact(target_hwnd, identity)?;
            send_vk_events(
                &[
                    (VK_SHIFT, false),
                    (VK_RETURN, false),
                    (VK_RETURN, true),
                    (VK_SHIFT, true),
                ],
                "newline",
            )?;
            thread::sleep(Duration::from_millis(NEWLINE_CHORD_SETTLE_MS));
        }
        for utf16_unit in line.encode_utf16() {
            ensure_foreground_target_intact(target_hwnd, identity)?;
            send_unicode_unit(utf16_unit)?;
            sleep_random_delay(delay_range);
        }
    }

    Ok(())
}

/// 内部实现的前台安全聚焦文本输入流程（不触发发送）
fn type_text_focus_restore_inner(
    scope: TargetIdentityScope<'_>,
    editor: &crate::msaa::UiNodeHandle,
    text: &str,
    delay_range: &TypingDelayRange,
) -> Result<()> {
    let window_hwnd = scope.window_hwnd();
    let identity = scope.identity();
    let _foreground_guard = enter_window_editor_foreground_inner(window_hwnd, editor, identity)?;
    type_text_focus_restore_in_active_editor(window_hwnd, text, delay_range, identity)
}

/// 使用 `ForegroundGuard` 与 `SendInput(KEYEVENTF_UNICODE)` 向已绑定编辑器输入文本（支持多行 `Shift+Enter`），完成后自动交还前台焦点
pub fn type_text_focus_restore(
    window_hwnd: isize,
    editor: &crate::msaa::UiNodeHandle,
    text: &str,
    delay_range: &TypingDelayRange,
) -> Result<()> {
    type_text_focus_restore_inner(
        TargetIdentityScope::Raw(window_hwnd),
        editor,
        text,
        delay_range,
    )
}

/// 使用绑定后的实时身份守卫在前台安全输入多行文本（不触发发送）
pub fn type_text_focus_restore_verified(
    identity: &crate::win32::VerifiedWindowIdentity,
    editor: &crate::msaa::UiNodeHandle,
    text: &str,
    delay_range: &TypingDelayRange,
) -> Result<()> {
    type_text_focus_restore_inner(
        TargetIdentityScope::Verified(identity),
        editor,
        text,
        delay_range,
    )
}

/// 内部实现的前台安全聚焦输入+提交流程
fn type_and_submit_focus_restore_inner(
    scope: TargetIdentityScope<'_>,
    target: &ComposerSubmitTarget<'_>,
    text: &str,
    delay_range: &TypingDelayRange,
) -> Result<(bool, bool)> {
    let window_hwnd = scope.window_hwnd();
    let identity = scope.identity();
    let _foreground_guard =
        enter_window_editor_foreground_inner(window_hwnd, target.editor, identity)?;
    let target_hwnd = HWND(window_hwnd as *mut c_void);

    type_text_focus_restore_in_active_editor(window_hwnd, text, delay_range, identity)?;

    try_submit_in_foreground_inner(target_hwnd, target.send_button, target.shortcut, identity)?;
    Ok((true, false))
}

/// 兜底前台输入与发送接口：先避让物理鼠标，通过 RAII 守护短暂切换焦点并使用 `KEYEVENTF_UNICODE` 逐字键入后提交；返回 `(foreground_touched, mouse_touched)` 且 `mouse_touched` 恒为 `false`
pub fn type_and_submit_focus_restore(
    target: &ComposerSubmitTarget<'_>,
    text: &str,
    delay_range: &TypingDelayRange,
) -> Result<(bool, bool)> {
    type_and_submit_focus_restore_inner(
        TargetIdentityScope::Raw(target.window_hwnd),
        target,
        text,
        delay_range,
    )
}

/// [`type_and_submit_focus_restore`] 的强制身份保护版本，在每次按键前后实时校验进程/窗口一致性与鼠标安全状态
pub fn type_and_submit_focus_restore_verified(
    identity: &VerifiedWindowIdentity,
    target: &ComposerSubmitTarget<'_>,
    text: &str,
    delay_range: &TypingDelayRange,
) -> Result<(bool, bool)> {
    ensure_verified_target_window_matches(identity, target.window_hwnd)?;
    type_and_submit_focus_restore_inner(
        TargetIdentityScope::Verified(identity),
        target,
        text,
        delay_range,
    )
}

/// 内部实现的仅提交不重新输入逻辑
fn submit_only_focus_restore_inner(
    scope: TargetIdentityScope<'_>,
    target: &ComposerSubmitTarget<'_>,
) -> Result<(bool, bool)> {
    let window_hwnd = scope.window_hwnd();
    let identity = scope.identity();
    let _foreground_guard =
        enter_window_editor_foreground_inner(window_hwnd, target.editor, identity)?;
    let target_hwnd = HWND(window_hwnd as *mut c_void);
    try_submit_in_foreground_inner(target_hwnd, target.send_button, target.shortcut, identity)?;
    Ok((true, false))
}

/// 当文本已写入编辑器时，通过临时前台恢复机制触发发送按钮或发送提交快捷键（不再重复输入正文）
pub fn submit_only_focus_restore(target: &ComposerSubmitTarget<'_>) -> Result<(bool, bool)> {
    submit_only_focus_restore_inner(TargetIdentityScope::Raw(target.window_hwnd), target)
}

/// [`submit_only_focus_restore`] 的强制身份保护版本，严格校验绑定身份与每一步前台安全不变式
pub fn submit_only_focus_restore_verified(
    identity: &VerifiedWindowIdentity,
    target: &ComposerSubmitTarget<'_>,
) -> Result<(bool, bool)> {
    ensure_verified_target_window_matches(identity, target.window_hwnd)?;
    submit_only_focus_restore_inner(TargetIdentityScope::Verified(identity), target)
}

/// 内部实现的通过短暂前台聚焦发送 `Ctrl+A` 与 `BackSpace` 清空编辑器草稿
fn clear_editor_focus_restore_inner(
    window_hwnd: isize,
    editor: &UiNodeHandle,
    identity: Option<&VerifiedWindowIdentity>,
) -> Result<()> {
    let _foreground_guard = enter_window_editor_foreground_inner(window_hwnd, editor, identity)?;
    let target_hwnd = HWND(window_hwnd as *mut c_void);
    ensure_foreground_target_intact(target_hwnd, identity)?;
    send_vk_events(
        &[
            (VK_CONTROL, false),
            (VK_A, false),
            (VK_A, true),
            (VK_CONTROL, true),
        ],
        "select_all",
    )?;
    thread::sleep(Duration::from_millis(POST_SELECT_ALL_DELAY_MS));
    ensure_foreground_target_intact(target_hwnd, identity)?;
    send_vk_events(&[(VK_BACK, false), (VK_BACK, true)], "backspace")?;
    thread::sleep(Duration::from_millis(POST_BACKSPACE_DELAY_MS));
    Ok(())
}

/// 当富文本 `contenteditable` 编辑框拒绝后台值清空 API 时，短暂聚焦窗口发送 `Ctrl+A` 后退格清空残留文本
pub fn clear_editor_focus_restore(window_hwnd: isize, editor: &UiNodeHandle) -> Result<()> {
    clear_editor_focus_restore_inner(window_hwnd, editor, None)
}

/// [`clear_editor_focus_restore`] 的强制身份保护版本，在每一步虚拟键序列前执行快速目标验证
pub fn clear_editor_focus_restore_verified(
    identity: &VerifiedWindowIdentity,
    editor: &UiNodeHandle,
) -> Result<()> {
    clear_editor_focus_restore_inner(identity.hwnd(), editor, Some(identity))
}

/// 在目标窗口仍处于已绑定进程前台时发送单次 `Esc` 键以关闭弹出面板或右键菜单，绝不误发到无关前台进程
pub fn send_escape_key_verified(identity: &VerifiedWindowIdentity) -> Result<()> {
    identity.verify_fast()?;
    let fg = unsafe { GetForegroundWindow() };
    if fg.0.is_null() || crate::win32::get_window_pid(fg.0 as isize) != identity.pid() {
        return Ok(());
    }
    send_vk_events(&[(VK_ESCAPE, false), (VK_ESCAPE, true)], "escape")
}
