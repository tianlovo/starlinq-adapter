//! 带光标位置瞬时复原与 `WindowFromPoint` 进程归属校验的安全模拟鼠标点击动作模块

use std::ffi::c_void;
use std::mem::size_of;
use std::thread;
use std::time::Duration;

use common_types::{AdapterError, Result};
use windows::Win32::Foundation::{HWND, POINT};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL, MOUSEINPUT,
    MOUSE_EVENT_FLAGS,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetForegroundWindow, IsWindow, SetCursorPos, WindowFromPoint,
};

use super::focus_guard::{send_alt_keyup_pulse, try_bring_to_foreground};
use crate::constants::{
    FOREGROUND_ALT_PULSE_SETTLE_MS, FOREGROUND_SWITCH_SETTLE_MS, MAX_FOREGROUND_MOUSE_WAIT_ROUNDS,
    MOUSE_IDLE_SAMPLE_MS,
};
use crate::win32::{self, VerifiedWindowIdentity};

/// 支持的安全模拟鼠标点击及滚轮动作类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimulatedClickKind {
    Left,
    Right,
    DoubleLeft,
    WheelDown,
    WheelUp,
}

/// RAII 光标坐标守卫：离开作用域时无条件把物理光标还原到操作前屏幕坐标
struct CursorPositionGuard {
    saved_point: Option<POINT>,
}

impl CursorPositionGuard {
    fn capture() -> Self {
        let mut pt = POINT::default();
        let ok = unsafe { GetCursorPos(&mut pt) }.is_ok();
        Self {
            saved_point: ok.then_some(pt),
        }
    }
}

impl Drop for CursorPositionGuard {
    fn drop(&mut self) {
        if let Some(pt) = self.saved_point {
            unsafe {
                let _ = SetCursorPos(pt.x, pt.y);
            }
        }
    }
}

fn send_mouse_flags(flags_list: &[MOUSE_EVENT_FLAGS], label: &str) -> Result<()> {
    let inputs: Vec<INPUT> = flags_list
        .iter()
        .map(|&flags| INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: 0,
                    dy: 0,
                    mouseData: 0,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        })
        .collect();
    let sent = unsafe { SendInput(&inputs, size_of::<INPUT>() as i32) };
    if sent != inputs.len() as u32 {
        return Err(AdapterError::InputRejected(format!(
            "SendInput({label}) 仅注入 {sent}/{} 个鼠标事件",
            inputs.len()
        )));
    }
    Ok(())
}

/// 在光标守卫与目标归属校验完成后向目标坐标点注入垂直滚轮滚动事件
fn send_mouse_wheel(delta: i32) -> Result<()> {
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: delta as u32,
                dwFlags: MOUSEEVENTF_WHEEL,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    let sent = unsafe { SendInput(&[input], size_of::<INPUT>() as i32) };
    if sent != 1 {
        return Err(AdapterError::InputRejected(format!(
            "SendInput(wheel={delta}) 仅注入 {sent}/1 个鼠标事件"
        )));
    }
    Ok(())
}

fn verify_hit_point_ownership(
    identity: &VerifiedWindowIdentity,
    screen_x: i32,
    screen_y: i32,
    allow_popup_same_pid: bool,
) -> Result<()> {
    let hit_hwnd = unsafe {
        WindowFromPoint(POINT {
            x: screen_x,
            y: screen_y,
        })
    };
    if hit_hwnd.0.is_null() || !unsafe { IsWindow(Some(hit_hwnd)).as_bool() } {
        return Err(AdapterError::WindowNotFound(format!(
            "屏幕坐标 ({screen_x}, {screen_y}) 未命中任何有效窗口"
        )));
    }
    let hit_raw = hit_hwnd.0 as isize;
    let hit_pid = win32::get_window_pid(hit_raw);
    if hit_pid != identity.pid() {
        return Err(AdapterError::WindowNotFound(format!(
            "坐标 ({screen_x}, {screen_y}) 命中非目标进程 HWND 0x{hit_raw:x} (pid={hit_pid}, expected={})",
            identity.pid()
        )));
    }
    if !allow_popup_same_pid {
        let root = win32::get_root_ancestor(hit_raw);
        if root != 0 && root != identity.hwnd() {
            return Err(AdapterError::WindowNotFound(format!(
                "坐标 ({screen_x}, {screen_y}) 命中根窗口 0x{root:x} 与已绑定目标 HWND 0x{:x} 不一致",
                identity.hwnd()
            )));
        }
    }
    Ok(())
}

fn prepare_verified_cursor_point(
    identity: &VerifiedWindowIdentity,
    screen_x: i32,
    screen_y: i32,
    allow_popup_same_pid: bool,
) -> Result<CursorPositionGuard> {
    let mut mouse_idle = false;
    for _ in 0..MAX_FOREGROUND_MOUSE_WAIT_ROUNDS {
        if !win32::is_user_mouse_busy(MOUSE_IDLE_SAMPLE_MS) && !win32::are_mouse_buttons_pressed() {
            mouse_idle = true;
            break;
        }
    }
    if !mouse_idle || win32::are_mouse_buttons_pressed() {
        return Err(AdapterError::InputRejected(
            "检测到用户正在移动或按下物理鼠标，已安全中止模拟点击".to_string(),
        ));
    }

    identity.verify_fast()?;
    win32::restore_no_activate(identity.hwnd());
    let target_hwnd = HWND(identity.hwnd() as *mut c_void);
    if unsafe { GetForegroundWindow() } != target_hwnd && !try_bring_to_foreground(target_hwnd) {
        thread::sleep(Duration::from_millis(FOREGROUND_ALT_PULSE_SETTLE_MS));
        send_alt_keyup_pulse();
        let _ = try_bring_to_foreground(target_hwnd);
    }
    thread::sleep(Duration::from_millis(FOREGROUND_SWITCH_SETTLE_MS));
    identity.verify_fast()?;

    verify_hit_point_ownership(identity, screen_x, screen_y, allow_popup_same_pid)?;

    let cursor_guard = CursorPositionGuard::capture();
    if unsafe { SetCursorPos(screen_x, screen_y) }.is_err() {
        return Err(AdapterError::UiaFailure(format!(
            "SetCursorPos({screen_x}, {screen_y}) 调用失败"
        )));
    }
    thread::sleep(Duration::from_millis(12));
    Ok(cursor_guard)
}

/// 等待物理鼠标空闲、校验证书目标归属与 `WindowFromPoint` 命中权后，短暂移动并点击指定坐标且立即还原物理光标
pub fn click_point_cursor_restore_verified(
    identity: &VerifiedWindowIdentity,
    screen_x: i32,
    screen_y: i32,
    kind: SimulatedClickKind,
    allow_popup_same_pid: bool,
) -> Result<()> {
    let _cursor_guard =
        prepare_verified_cursor_point(identity, screen_x, screen_y, allow_popup_same_pid)?;

    match kind {
        SimulatedClickKind::Left => {
            send_mouse_flags(&[MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP], "left_click")?;
        }
        SimulatedClickKind::Right => {
            send_mouse_flags(&[MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP], "right_click")?;
        }
        SimulatedClickKind::DoubleLeft => {
            send_mouse_flags(&[MOUSEEVENTF_MOVE], "dbl_hover")?;
            thread::sleep(Duration::from_millis(40));
            send_mouse_flags(&[MOUSEEVENTF_LEFTDOWN], "dbl_down_1")?;
            thread::sleep(Duration::from_millis(20));
            send_mouse_flags(&[MOUSEEVENTF_LEFTUP], "dbl_up_1")?;
            thread::sleep(Duration::from_millis(65));
            send_mouse_flags(&[MOUSEEVENTF_LEFTDOWN], "dbl_down_2")?;
            thread::sleep(Duration::from_millis(20));
            send_mouse_flags(&[MOUSEEVENTF_LEFTUP], "dbl_up_2")?;
            thread::sleep(Duration::from_millis(120));
        }
        SimulatedClickKind::WheelDown => {
            send_mouse_flags(&[MOUSEEVENTF_MOVE], "wheel_hover")?;
            thread::sleep(Duration::from_millis(40));
            send_mouse_wheel(-240)?;
        }
        SimulatedClickKind::WheelUp => {
            send_mouse_flags(&[MOUSEEVENTF_MOVE], "wheel_hover")?;
            thread::sleep(Duration::from_millis(40));
            send_mouse_wheel(240)?;
        }
    }
    thread::sleep(Duration::from_millis(35));
    Ok(())
}

/// 在单次空闲保护与光标守卫内向目标坐标连续注入多段垂直滚轮事件并自动还原物理光标
pub fn scroll_point_cursor_restore_verified(
    identity: &VerifiedWindowIdentity,
    screen_x: i32,
    screen_y: i32,
    delta_per_burst: i32,
    bursts: usize,
) -> Result<()> {
    if bursts == 0 || delta_per_burst == 0 {
        return Ok(());
    }
    let _cursor_guard = prepare_verified_cursor_point(identity, screen_x, screen_y, false)?;
    send_mouse_flags(&[MOUSEEVENTF_MOVE], "wheel_hover")?;
    thread::sleep(Duration::from_millis(35));
    for idx in 0..bursts {
        send_mouse_wheel(delta_per_burst)?;
        if idx + 1 < bursts {
            thread::sleep(Duration::from_millis(28));
        }
    }
    thread::sleep(Duration::from_millis(80));
    Ok(())
}
