//! 非激活态临时置顶解遮挡模块，使用 RAII 守护对象模式在不抢占焦点或鼠标的前提下强制重绘 Chromium 渲染窗口

use std::collections::HashSet;
use std::ffi::c_void;

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Gdi::{InvalidateRect, UpdateWindow};
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindow, GetWindowLongW, IsIconic, IsWindowVisible, SetWindowPos, ShowWindow, GWL_EXSTYLE,
    GW_HWNDPREV, HWND_NOTOPMOST, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    SWP_SHOWWINDOW, SW_SHOWNOACTIVATE, WS_EX_TOPMOST,
};

use super::window_enum::is_window_valid;
use crate::constants::MAX_Z_ORDER_WALK_STEPS;

/// 基于 RAII 守护对象模式临时将目标窗口设为 `HWND_TOPMOST` 并触发重绘，析构时精确还原原 Z 序而不窃取前台焦点或移动鼠标
#[derive(Debug)]
pub struct TopmostUnoccludeGuard {
    window_hwnd: isize,
    was_topmost: bool,
    previous_above_hwnd: Option<isize>,
}

impl TopmostUnoccludeGuard {
    /// 以 `SWP_NOACTIVATE` 将 `window_hwnd` 临时置顶并使客户区无效化重绘；若句柄无效则返回 `None`
    pub fn acquire(window_hwnd: isize) -> Option<Self> {
        if !is_window_valid(window_hwnd) {
            return None;
        }
        let target_handle = HWND(window_hwnd as *mut c_void);
        let was_topmost =
            ((unsafe { GetWindowLongW(target_handle, GWL_EXSTYLE) } as u32) & WS_EX_TOPMOST.0) != 0;

        let mut seen = HashSet::new();
        seen.insert(window_hwnd);
        let mut previous_above_hwnd: Option<isize> = None;
        let mut current = target_handle;
        for _ in 0..MAX_Z_ORDER_WALK_STEPS {
            let Ok(previous_handle) = (unsafe { GetWindow(current, GW_HWNDPREV) }) else {
                break;
            };
            let previous_isize = previous_handle.0 as isize;
            if previous_isize == 0 || !seen.insert(previous_isize) {
                break;
            }
            if is_window_valid(previous_isize)
                && unsafe { IsWindowVisible(previous_handle).as_bool() }
            {
                previous_above_hwnd = Some(previous_isize);
                break;
            }
            current = previous_handle;
        }

        if unsafe { IsIconic(target_handle).as_bool() } {
            unsafe {
                let _ = ShowWindow(target_handle, SW_SHOWNOACTIVATE);
            }
        }

        unsafe {
            let _ = SetWindowPos(
                target_handle,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
            let _ = InvalidateRect(Some(target_handle), None, false);
            let _ = UpdateWindow(target_handle);
        }

        Some(Self {
            window_hwnd,
            was_topmost,
            previous_above_hwnd,
        })
    }
}

impl Drop for TopmostUnoccludeGuard {
    fn drop(&mut self) {
        if !self.was_topmost && is_window_valid(self.window_hwnd) {
            let target_handle = HWND(self.window_hwnd as *mut c_void);
            unsafe {
                let _ = SetWindowPos(
                    target_handle,
                    Some(HWND_NOTOPMOST),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
                if let Some(previous) = self.previous_above_hwnd {
                    let previous_handle = HWND(previous as *mut c_void);
                    if is_window_valid(previous) && IsWindowVisible(previous_handle).as_bool() {
                        let _ = SetWindowPos(
                            target_handle,
                            Some(previous_handle),
                            0,
                            0,
                            0,
                            0,
                            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                        );
                    }
                }
            }
        }
    }
}
