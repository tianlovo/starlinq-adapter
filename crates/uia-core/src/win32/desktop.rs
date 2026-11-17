//! 桌面环境隔离探测与物理键鼠活跃状态检测模块，防止在沙盒桌面误判或在用户操作时抢占焦点

use std::mem::size_of;
use std::time::Duration;

use windows::Win32::Foundation::{HWND, LPARAM, POINT};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON, VK_RBUTTON};
use windows::Win32::UI::WindowsAndMessaging::{EnumWindows, GetCursorPos};
use windows_core::BOOL;

use super::utf16_slice_to_string;
use crate::constants::{
    DESKTOP_NAME_BUFFER_LEN, DESKTOP_SANDBOX_PREFIX, DESKTOP_UNKNOWN_NAME,
    KEY_PRESSED_HIGH_BIT_MASK, MOUSE_MOVE_TOLERANCE_PX, USER_OBJECT_NAME_INDEX,
};

#[link(name = "user32")]
extern "system" {
    fn GetThreadDesktop(dwThreadId: u32) -> isize;
    fn GetUserObjectInformationW(
        hObj: isize,
        nIndex: i32,
        pvInfo: *mut u8,
        nLength: u32,
        lpnLengthNeeded: *mut u32,
    ) -> i32;
}

/// 查询当前线程所属 Win32 桌面名称及枚举顶层窗口数，用于识别 `exebox-*` 等沙盒隔离桌面
pub fn current_desktop_info() -> (String, bool) {
    let desktop_name = unsafe {
        let thread_id = GetCurrentThreadId();
        let desktop_handle = GetThreadDesktop(thread_id);
        if desktop_handle == 0 {
            DESKTOP_UNKNOWN_NAME.to_string()
        } else {
            let mut buffer = [0u16; DESKTOP_NAME_BUFFER_LEN];
            let mut needed_bytes: u32 = 0;
            let succeeded = GetUserObjectInformationW(
                desktop_handle,
                USER_OBJECT_NAME_INDEX,
                buffer.as_mut_ptr() as *mut u8,
                (buffer.len() * size_of::<u16>()) as u32,
                &mut needed_bytes,
            );
            if succeeded != 0 {
                let parsed = utf16_slice_to_string(&buffer);
                if parsed.is_empty() {
                    DESKTOP_UNKNOWN_NAME.to_string()
                } else {
                    parsed
                }
            } else {
                DESKTOP_UNKNOWN_NAME.to_string()
            }
        }
    };

    let mut enum_count: usize = 0;
    unsafe extern "system" fn count_windows_callback(_window: HWND, user_data: LPARAM) -> BOOL {
        let count_pointer = user_data.0 as *mut usize;
        if !count_pointer.is_null() {
            *count_pointer += 1;
        }
        BOOL(1)
    }

    unsafe {
        let _ = EnumWindows(
            Some(count_windows_callback),
            LPARAM(&mut enum_count as *mut usize as isize),
        );
    }

    let sandbox_isolated = desktop_name
        .to_ascii_lowercase()
        .starts_with(DESKTOP_SANDBOX_PREFIX)
        || enum_count == 0;
    (desktop_name, sandbox_isolated)
}

/// 检测物理鼠标左右键当前是否处于按下状态
fn any_mouse_button_down() -> bool {
    unsafe {
        let left_down =
            (GetAsyncKeyState(VK_LBUTTON.0 as i32) as u16 & KEY_PRESSED_HIGH_BIT_MASK) != 0;
        let right_down =
            (GetAsyncKeyState(VK_RBUTTON.0 as i32) as u16 & KEY_PRESSED_HIGH_BIT_MASK) != 0;
        left_down || right_down
    }
}

/// 返回物理鼠标左键或右键当前是否被按下
pub fn are_mouse_buttons_pressed() -> bool {
    any_mouse_button_down()
}

/// 在 `sample_ms` 采样窗口内检测用户是否正在按下鼠标按键或大幅移动光标
pub fn is_user_mouse_busy(sample_ms: u64) -> bool {
    if any_mouse_button_down() {
        return true;
    }

    let mut first_point = POINT::default();
    let got_first_point = unsafe { GetCursorPos(&mut first_point) }.is_ok();

    if sample_ms > 0 {
        std::thread::sleep(Duration::from_millis(sample_ms));
    }

    if any_mouse_button_down() {
        return true;
    }

    if got_first_point {
        let mut second_point = POINT::default();
        if unsafe { GetCursorPos(&mut second_point) }.is_ok() {
            let delta_x = (first_point.x - second_point.x).abs();
            let delta_y = (first_point.y - second_point.y).abs();
            if delta_x > MOUSE_MOVE_TOLERANCE_PX || delta_y > MOUSE_MOVE_TOLERANCE_PX {
                return true;
            }
        }
    }

    false
}
