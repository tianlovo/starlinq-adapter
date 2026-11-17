//! Win32 底层窗口、进程、桌面与无障碍唤醒门面模块（Facade），统一封装原生句柄资源与跨线程边界

mod accessibility_wake;
mod desktop;
mod identity_guard;
mod launcher;
mod process_lookup;
mod topmost_guard;
mod window_enum;

#[cfg(test)]
mod tests;

use windows::Win32::Foundation::HANDLE;

pub use accessibility_wake::{wake_chromium_accessibility, wake_chromium_accessibility_ext};
pub use desktop::{are_mouse_buttons_pressed, current_desktop_info, is_user_mouse_busy};
pub use identity_guard::VerifiedWindowIdentity;
pub use launcher::{launch_or_wake_qq, resolve_qq_executable};
pub use process_lookup::find_pids_by_names;
pub use topmost_guard::TopmostUnoccludeGuard;
pub use window_enum::{
    enumerate_all_child_windows, enumerate_all_windows_for_pids, enumerate_candidate_windows,
    find_child_window_by_class, find_child_windows_by_class, find_windows_with_keywords,
    find_windows_with_keywords_ext, get_class_name, get_root_ancestor, get_window_pid,
    get_window_rect, get_window_title, is_window_minimized, is_window_valid, refresh_window_info,
    restore_no_activate,
};

pub(crate) use accessibility_wake::{
    diagnostic_probe_points, query_ia2_interfaces, variant_i4, SingleVariantGuard, VariantVecGuard,
};

/// 封装 Win32 内核进程与快照句柄的 RAII 守护对象，在离开作用域时自动释放句柄资源
#[derive(Debug)]
pub(crate) struct HandleGuard(pub(crate) HANDLE);

// Win32 进程句柄与快照句柄支持跨线程查询与等待，在 Drop 中关闭底层句柄是线程安全的
unsafe impl Send for HandleGuard {}
unsafe impl Sync for HandleGuard {}

impl Drop for HandleGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = windows::Win32::Foundation::CloseHandle(self.0);
            }
        }
    }
}

/// 将以空字符结尾的 UTF-16 切片解码为 Rust 字符串
pub(crate) fn utf16_slice_to_string(slice: &[u16]) -> String {
    let end = slice
        .iter()
        .position(|&code_unit| code_unit == 0)
        .unwrap_or(slice.len());
    String::from_utf16_lossy(&slice[..end])
}
