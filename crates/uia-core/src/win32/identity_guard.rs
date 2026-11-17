//! 窗口与进程身份保护代理（Proxy）模块，通过不可变句柄绑定与 `GCW_ATOM` 类原子核验防止 OS PID 复用竞争与跨应用误发

use std::ffi::c_void;
use std::path::Path;

use common_types::constants::processes::{
    CHROME_WIDGET_WINDOW_CLASS, MOCK_QQ_MAIN_CLASS, TX_GUI_FOUNDATION_CLASS,
};
use common_types::{AdapterError, Result};
use windows::Win32::Foundation::{HWND, WAIT_TIMEOUT};
use windows::Win32::System::Threading::{
    OpenProcess, WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetClassLongPtrW, GetWindowThreadProcessId, IsWindow, GCW_ATOM,
};

use super::process_lookup::{
    is_exact_qq_process_name, query_image_path_from_handle, query_process_creation_time_100ns,
};
use super::window_enum::{get_class_name, get_window_pid, is_window_valid};
use super::HandleGuard;

/// 校验证明目标窗口类名与所属进程符合绑定目标（生产模式仅允许 QQNT，测试模式可显式放行模拟窗口）
fn is_verified_target_class_and_process(
    class_name: &str,
    process_name: &str,
    allow_mock: bool,
) -> bool {
    if allow_mock && class_name == MOCK_QQ_MAIN_CLASS {
        return true;
    }
    let is_qq_class = class_name.eq_ignore_ascii_case(CHROME_WIDGET_WINDOW_CLASS)
        || class_name.eq_ignore_ascii_case(TX_GUI_FOUNDATION_CLASS);
    is_qq_class && is_exact_qq_process_name(process_name)
}

/// 不可变进程与窗口绑定保护代理（Proxy），持有限定查询与同步进程句柄以消除 OS PID 复用与句柄漂移风险
#[derive(Debug)]
pub struct VerifiedWindowIdentity {
    window_hwnd: isize,
    process_id: u32,
    class_name: String,
    class_atom: usize,
    process_name: String,
    creation_time_100ns: u64,
    process_handle: HandleGuard,
}

impl VerifiedWindowIdentity {
    /// 校验目标窗口确实为存活的 QQ（或在允许模拟时为 `MockQQNTWndClass`）窗口并锁定其进程句柄与类原子标识
    pub fn verify_and_bind(window_hwnd: isize, allow_mock: bool) -> Result<Self> {
        if window_hwnd == 0 || !is_window_valid(window_hwnd) {
            return Err(AdapterError::WindowNotFound(format!(
                "目标窗口 HWND 0x{window_hwnd:x} 无效或已关闭"
            )));
        }
        let process_id = get_window_pid(window_hwnd);
        if process_id == 0 {
            return Err(AdapterError::WindowNotFound(format!(
                "目标窗口 HWND 0x{window_hwnd:x} 缺少所属进程 PID"
            )));
        }
        let class_name = get_class_name(window_hwnd);
        let class_atom = unsafe { GetClassLongPtrW(HWND(window_hwnd as *mut c_void), GCW_ATOM) };
        if class_name.is_empty() || class_atom == 0 {
            return Err(AdapterError::WindowNotFound(format!(
                "读取目标窗口 HWND 0x{window_hwnd:x} 的类名或类原子失败"
            )));
        }

        let raw_handle = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                false,
                process_id,
            )
        }
        .map_err(|error| {
            AdapterError::WindowNotFound(format!(
                "打开目标窗口 HWND 0x{window_hwnd:x} 所属进程 PID {process_id} 失败: {error}"
            ))
        })?;
        let process_handle = HandleGuard(raw_handle);

        let wait_status = unsafe { WaitForSingleObject(process_handle.0, 0) };
        if wait_status != WAIT_TIMEOUT {
            return Err(AdapterError::WindowNotFound(format!(
                "目标窗口 HWND 0x{window_hwnd:x} 所属进程 PID {process_id} 已退出 (wait=0x{:x})",
                wait_status.0
            )));
        }

        let image_path = query_image_path_from_handle(process_handle.0).ok_or_else(|| {
            AdapterError::WindowNotFound(format!(
                "查询 PID {process_id} (HWND 0x{window_hwnd:x}) 的镜像路径失败"
            ))
        })?;
        let process_name = Path::new(&image_path)
            .file_name()
            .and_then(|segment| segment.to_str())
            .unwrap_or(&image_path)
            .to_string();

        if !is_verified_target_class_and_process(&class_name, &process_name, allow_mock) {
            return Err(AdapterError::WindowNotFound(format!(
                "HWND 0x{window_hwnd:x} (class='{class_name}', process='{process_name}', allow_mock={allow_mock}) 不是受允许的 QQ 目标窗口"
            )));
        }

        let creation_time_100ns =
            query_process_creation_time_100ns(process_handle.0).ok_or_else(|| {
                AdapterError::WindowNotFound(format!(
                    "查询 PID {process_id} ({process_name}) 的进程创建时间失败"
                ))
            })?;

        // 消除初次读取窗口 PID 与 OpenProcess 之间的 TOCTOU 时间窗竞争
        if !is_window_valid(window_hwnd) || get_window_pid(window_hwnd) != process_id {
            return Err(AdapterError::WindowNotFound(format!(
                "绑定进程 PID {process_id} 期间目标窗口 HWND 0x{window_hwnd:x} 状态发生变化"
            )));
        }

        Ok(Self {
            window_hwnd,
            process_id,
            class_name,
            class_atom,
            process_name,
            creation_time_100ns,
            process_handle,
        })
    }

    /// 零堆分配超轻量校验：确认已绑定的窗口句柄、进程存活状态、PID 与 Win32 类原子仍然完全一致
    pub fn verify_fast(&self) -> Result<()> {
        let target = HWND(self.window_hwnd as *mut c_void);
        if self.window_hwnd == 0 || !unsafe { IsWindow(Some(target)).as_bool() } {
            return Err(AdapterError::WindowNotFound(format!(
                "已绑定窗口 HWND 0x{:x} 已失效",
                self.window_hwnd
            )));
        }
        let wait_status = unsafe { WaitForSingleObject(self.process_handle.0, 0) };
        if wait_status != WAIT_TIMEOUT {
            return Err(AdapterError::WindowNotFound(format!(
                "已绑定进程 '{}' (pid={}, hwnd=0x{:x}) 已退出",
                self.process_name, self.process_id, self.window_hwnd
            )));
        }
        let mut current_pid: u32 = 0;
        unsafe {
            GetWindowThreadProcessId(target, Some(&mut current_pid));
        }
        if current_pid == 0 || current_pid != self.process_id {
            return Err(AdapterError::WindowNotFound(format!(
                "已绑定窗口 HWND 0x{:x} 的所属 PID 从 {} 变更为 {current_pid}",
                self.window_hwnd, self.process_id
            )));
        }
        let atom = unsafe { GetClassLongPtrW(target, GCW_ATOM) };
        if atom != self.class_atom {
            return Err(AdapterError::WindowNotFound(format!(
                "已绑定窗口 HWND 0x{:x} 的窗口类不再匹配 '{}'",
                self.window_hwnd, self.class_name
            )));
        }
        Ok(())
    }

    /// 返回已绑定的 Win32 窗口句柄
    pub fn hwnd(&self) -> isize {
        self.window_hwnd
    }

    /// 返回已绑定的所属进程 PID
    pub fn pid(&self) -> u32 {
        self.process_id
    }

    /// 返回已绑定的 Win32 窗口类名
    pub fn class_name(&self) -> &str {
        &self.class_name
    }

    /// 返回已绑定的进程可执行文件名（如 `QQ.exe` 或模拟测试进程名）
    pub fn process_name(&self) -> &str {
        &self.process_name
    }

    /// 返回已绑定进程的 `FILETIME` 创建时间戳（100 纳秒单位）
    pub fn creation_time_100ns(&self) -> u64 {
        self.creation_time_100ns
    }
}
