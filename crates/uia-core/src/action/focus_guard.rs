//! Win32 输入队列附着与前台窗口聚焦 RAII 守护模块，在回退路径期间负责鼠标避让检查、安全激活焦点与析构时自动复原原前台窗口

use std::ffi::c_void;
use std::mem::size_of;
use std::thread;
use std::time::Duration;

use common_types::{AdapterError, Result};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, SetActiveWindow, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
    KEYEVENTF_KEYUP, VK_MENU,
};
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, GetForegroundWindow, GetWindowThreadProcessId, IsWindow, SetForegroundWindow,
};

use super::background_ops::verify_element_belongs_to_target;
use crate::constants::{
    FOREGROUND_ALT_PULSE_SETTLE_MS, FOREGROUND_SWITCH_SETTLE_MS, MAX_FOREGROUND_MOUSE_WAIT_ROUNDS,
    MOUSE_IDLE_SAMPLE_MS, POST_FOCUS_EDITOR_DELAY_MS, PRE_FOCUS_EDITOR_DELAY_MS,
};
use crate::msaa::{self, UiNodeHandle};
use crate::win32::{self, VerifiedWindowIdentity};

/// 临时将当前工作线程附着至当前前台窗口及目标窗口消息队列的 RAII 守护对象，用于绕过 Windows 前台锁并在离开作用域后解绑
pub(crate) struct ThreadInputAttachGuard {
    current_thread_id: u32,
    foreground_thread_id: u32,
    target_thread_id: u32,
}

impl ThreadInputAttachGuard {
    /// 附着当前线程、原前台窗口线程与 `target_hwnd` 所属线程的输入状态
    pub(crate) fn attach_for(target_hwnd: HWND) -> Self {
        let current_thread_id = unsafe { GetCurrentThreadId() };
        let foreground_hwnd = unsafe { GetForegroundWindow() };
        let foreground_thread_id = if !foreground_hwnd.0.is_null() {
            unsafe { GetWindowThreadProcessId(foreground_hwnd, None) }
        } else {
            0
        };
        let target_thread_id = if !target_hwnd.0.is_null() {
            unsafe { GetWindowThreadProcessId(target_hwnd, None) }
        } else {
            0
        };

        let attached_foreground = foreground_thread_id != 0
            && foreground_thread_id != current_thread_id
            && unsafe {
                AttachThreadInput(current_thread_id, foreground_thread_id, true).as_bool()
            };
        let attached_target = target_thread_id != 0
            && target_thread_id != current_thread_id
            && target_thread_id != foreground_thread_id
            && unsafe { AttachThreadInput(current_thread_id, target_thread_id, true).as_bool() };

        Self {
            current_thread_id,
            foreground_thread_id: if attached_foreground {
                foreground_thread_id
            } else {
                0
            },
            target_thread_id: if attached_target { target_thread_id } else { 0 },
        }
    }
}

impl Drop for ThreadInputAttachGuard {
    fn drop(&mut self) {
        if self.target_thread_id != 0 {
            unsafe {
                let _ = AttachThreadInput(self.current_thread_id, self.target_thread_id, false);
            }
        }
        if self.foreground_thread_id != 0 {
            unsafe {
                let _ = AttachThreadInput(self.current_thread_id, self.foreground_thread_id, false);
            }
        }
    }
}

/// 在附着输入队列前提下尝试将 `target_hwnd` 提至前台并设为活动窗口
pub(super) fn try_bring_to_foreground(target_hwnd: HWND) -> bool {
    let _attach_guard = ThreadInputAttachGuard::attach_for(target_hwnd);
    unsafe {
        let _ = BringWindowToTop(target_hwnd);
        let _ = SetForegroundWindow(target_hwnd);
        let _ = SetActiveWindow(target_hwnd);
        GetForegroundWindow() == target_hwnd
    }
}

/// 向 Windows 输入系统发送一次仅含 Alt 抬起标志的脉冲按键以合法刷新前台锁定权
pub(super) fn send_alt_keyup_pulse() {
    let inputs = [INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VK_MENU,
                wScan: 0,
                dwFlags: KEYEVENTF_EXTENDEDKEY | KEYEVENTF_KEYUP,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }];
    unsafe {
        let _ = SendInput(&inputs, size_of::<INPUT>() as i32);
    }
}

std::thread_local! {
    /// 记录当前线程已持有的外层发送期前台恢复守卫深度，用于抑制嵌套动作提前交还焦点
    static OUTER_RESTORE_DEPTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// 查询当前系统前台窗口句柄（原始 `isize` 值，空句柄返回 `0`）
pub fn current_foreground_hwnd() -> isize {
    unsafe { GetForegroundWindow().0 as isize }
}

/// 前台焦点安全恢复 RAII 守护对象，离开作用域时仅在无外层总守卫嵌套的情况下交还前台焦点
pub(crate) struct ForegroundGuard {
    previous_foreground: HWND,
    target_hwnd: HWND,
}

impl Drop for ForegroundGuard {
    fn drop(&mut self) {
        if OUTER_RESTORE_DEPTH.with(|depth| depth.get() > 0) {
            return;
        }
        let current_foreground = unsafe { GetForegroundWindow() };
        if !self.previous_foreground.0.is_null()
            && self.previous_foreground != self.target_hwnd
            && (current_foreground == self.target_hwnd || current_foreground.0.is_null())
            && unsafe { IsWindow(Some(self.previous_foreground)).as_bool() }
        {
            let _ = try_bring_to_foreground(self.previous_foreground);
        }
    }
}

/// 读取指定窗口所属进程 PID，无效句柄或未获取到时返回 `0`
fn window_pid(hwnd: HWND) -> u32 {
    if hwnd.0.is_null() {
        return 0;
    }
    let mut pid: u32 = 0;
    unsafe {
        let _ = GetWindowThreadProcessId(hwnd, Some(&mut pid));
    }
    pid
}

/// 判断抓取到的前台进程 PID 是否属于应记录并等待恢复的非排除窗口
pub(crate) fn should_capture_foreground_pid(pid: u32, excluded_pids: &[u32]) -> bool {
    pid != 0 && !excluded_pids.contains(&pid)
}

/// 判断在 `Drop` 时当前前台状态是否满足“为空或被排除进程占用”的恢复条件
pub(crate) fn should_restore_for_current_pid(
    current_is_null: bool,
    current_pid: u32,
    excluded_pids: &[u32],
) -> bool {
    current_is_null || (current_pid != 0 && excluded_pids.contains(&current_pid))
}

/// 跨操作前台焦点兜底恢复 RAII 守护，构造时记录非排除进程前台窗口并在离开作用域且未按鼠标键时归还焦点
pub struct ForegroundRestoreGuard {
    previous_foreground: isize,
    excluded_pids: Vec<u32>,
    restored: bool,
}

impl ForegroundRestoreGuard {
    /// 捕获当前前台窗口（仅当句柄非空且所属 PID 不在 `excluded_pids` 中时记录）并递增外层保护深度
    pub fn capture(excluded_pids: &[u32]) -> Self {
        let foreground = unsafe { GetForegroundWindow() };
        let pid = window_pid(foreground);
        let previous_foreground =
            if !foreground.0.is_null() && should_capture_foreground_pid(pid, excluded_pids) {
                foreground.0 as isize
            } else {
                0
            };
        OUTER_RESTORE_DEPTH.with(|depth| depth.set(depth.get().saturating_add(1)));
        Self {
            previous_foreground,
            excluded_pids: excluded_pids.to_vec(),
            restored: false,
        }
    }

    /// 返回构造时捕获到的原前台窗口句柄，未捕获时返回 `None`
    pub fn captured_hwnd(&self) -> Option<isize> {
        (self.previous_foreground != 0).then_some(self.previous_foreground)
    }

    /// 返回构造时捕获到的原前台窗口句柄（未捕获返回 `0`）
    pub fn previous_hwnd(&self) -> isize {
        self.previous_foreground
    }

    /// 显式执行焦点交还（含至多 3 次重试与 Alt 脉冲解锁），幂等防止重复归还
    pub fn restore_now(&mut self) {
        if self.restored {
            return;
        }
        self.restored = true;
        if self.previous_foreground == 0 || crate::win32::are_mouse_buttons_pressed() {
            return;
        }
        let current = unsafe { GetForegroundWindow() };
        let current_pid = window_pid(current);
        if !should_restore_for_current_pid(current.0.is_null(), current_pid, &self.excluded_pids) {
            return;
        }
        let previous = HWND(self.previous_foreground as *mut std::ffi::c_void);
        if !unsafe { IsWindow(Some(previous)).as_bool() }
            || crate::win32::is_window_minimized(self.previous_foreground)
        {
            return;
        }
        for attempt in 0..3 {
            if try_bring_to_foreground(previous) {
                return;
            }
            if attempt == 0 {
                send_alt_keyup_pulse();
            }
            thread::sleep(Duration::from_millis(15));
        }
    }
}

impl Drop for ForegroundRestoreGuard {
    fn drop(&mut self) {
        OUTER_RESTORE_DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
        self.restore_now();
    }
}

/// 在每次执行全局 `SendInput` 前严格校验窗口存活、身份未飘移、未抢占鼠标且前台焦点未离开目标窗口
#[inline]
pub(crate) fn ensure_foreground_target_intact(
    target_hwnd: HWND,
    identity: Option<&VerifiedWindowIdentity>,
) -> Result<()> {
    if let Some(verified) = identity {
        verified.verify_fast()?;
    }
    if !unsafe { IsWindow(Some(target_hwnd)).as_bool() } {
        return Err(AdapterError::WindowNotFound(format!(
            "目标窗口 HWND 0x{:x} 在操作执行中途失效",
            target_hwnd.0 as isize
        )));
    }
    if win32::are_mouse_buttons_pressed() {
        return Err(AdapterError::UiaFailure(
            "检测到物理鼠标按键按下，已安全中止 SendInput 前台输入".to_string(),
        ));
    }
    if unsafe { GetForegroundWindow() } != target_hwnd {
        return Err(AdapterError::UiaFailure(format!(
            "前台焦点在动作执行中途离开目标窗口 HWND 0x{:x}，已安全中止 SendInput",
            target_hwnd.0 as isize
        )));
    }
    Ok(())
}

/// 在后台或前台聚焦前向编辑器控件请求输入焦点
pub(crate) fn focus_editor_handle(editor: &UiNodeHandle) {
    match editor {
        UiNodeHandle::Uia(uia_element) => {
            let _ = uia_element.set_focus();
        }
        UiNodeHandle::Msaa { acc, child_id } => {
            let _ = msaa::focus_msaa(acc, *child_id);
        }
    }
}

/// 完成鼠标避让等待、窗口非最小化恢复、前台切换以及编辑框聚焦校验，并返回析构自动还原焦点的守护对象
pub(crate) fn enter_window_editor_foreground_inner(
    window_hwnd: isize,
    editor: &UiNodeHandle,
    identity: Option<&VerifiedWindowIdentity>,
) -> Result<ForegroundGuard> {
    if let Some(verified) = identity {
        verify_element_belongs_to_target(verified, editor)?;
    }
    if !win32::is_window_valid(window_hwnd) {
        return Err(AdapterError::WindowNotFound(format!(
            "无效窗口句柄 HWND 0x{window_hwnd:x}"
        )));
    }

    // 有界轮询退让，确保不打断用户当前拖拽或点击操作
    for _ in 0..MAX_FOREGROUND_MOUSE_WAIT_ROUNDS {
        if !win32::is_user_mouse_busy(MOUSE_IDLE_SAMPLE_MS) {
            break;
        }
    }

    win32::restore_no_activate(window_hwnd);

    let target_hwnd = HWND(window_hwnd as *mut c_void);
    let previous_foreground = unsafe { GetForegroundWindow() };
    let guard = ForegroundGuard {
        previous_foreground,
        target_hwnd,
    };

    if unsafe { GetForegroundWindow() } != target_hwnd && !try_bring_to_foreground(target_hwnd) {
        thread::sleep(Duration::from_millis(FOREGROUND_ALT_PULSE_SETTLE_MS));
        send_alt_keyup_pulse();
        let _ = try_bring_to_foreground(target_hwnd);
    }

    thread::sleep(Duration::from_millis(FOREGROUND_SWITCH_SETTLE_MS));
    ensure_foreground_target_intact(target_hwnd, identity)?;

    thread::sleep(Duration::from_millis(PRE_FOCUS_EDITOR_DELAY_MS));
    focus_editor_handle(editor);
    thread::sleep(Duration::from_millis(POST_FOCUS_EDITOR_DELAY_MS));
    ensure_foreground_target_intact(target_hwnd, identity)?;
    Ok(guard)
}
