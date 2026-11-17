//! Win32 进程性能采样器：通过 RAII 句柄守卫安全采集内存峰值与归一化 CPU 占用率

use std::mem::size_of;
use std::time::Instant;

use anyhow::{bail, Result};
use windows::Win32::Foundation::{CloseHandle, FILETIME, HANDLE};
use windows::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
};

use super::win32_ffi::{
    GetProcessTimes, GlobalMemoryStatusEx, K32GetProcessMemoryInfo, MemoryStatusEx,
    ProcessMemoryCountersEx,
};
use crate::models::ProcessTelemetrySnapshot;

/// 托管通过 `OpenProcess` 打开的 Win32 进程句柄（RAII 守卫）
struct ProcessHandleGuard(HANDLE);

impl Drop for ProcessHandleGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

/// 在指定时间窗口前后对本进程或目标 PID 采样并计算归一化 CPU 使用率（Proxy + RAII 采样窗口）
pub struct CpuUsageWindow {
    start_wall: Instant,
    start_kernel_ms: f64,
    start_user_ms: f64,
    pid: Option<u32>,
}

impl CpuUsageWindow {
    /// 开始采集当前进程 CPU 时间窗口
    pub fn begin_current() -> Result<Self> {
        let snap = snapshot_current_process("window-start")?;
        Ok(Self {
            start_wall: Instant::now(),
            start_kernel_ms: snap.kernel_time_ms,
            start_user_ms: snap.user_time_ms,
            pid: None,
        })
    }

    /// 开始采集指定 PID 进程 CPU 时间窗口
    pub fn begin_pid(pid: u32) -> Result<Self> {
        let snap = snapshot_process(pid, "window-start")?;
        Ok(Self {
            start_wall: Instant::now(),
            start_kernel_ms: snap.kernel_time_ms,
            start_user_ms: snap.user_time_ms,
            pid: Some(pid),
        })
    }

    /// 结束采样窗口并计算窗口期间平均归一化 CPU 百分比与最新内存占用
    pub fn finish_and_snapshot(self, label: &str) -> Result<ProcessTelemetrySnapshot> {
        let wall_ms = self.start_wall.elapsed().as_secs_f64() * 1000.0;
        let mut end = match self.pid {
            Some(pid) => snapshot_process(pid, label)?,
            None => snapshot_current_process(label)?,
        };
        let delta_kernel = (end.kernel_time_ms - self.start_kernel_ms).max(0.0);
        let delta_user = (end.user_time_ms - self.start_user_ms).max(0.0);
        let cores = logical_cpu_cores().max(1) as f64;
        // 归一化 CPU 占用率按逻辑核数与墙钟时长折算，确保多核空闲阈值可跨机器稳定复现
        let cpu_pct = if wall_ms > 0.0 {
            100.0 * (delta_kernel + delta_user) / (wall_ms * cores)
        } else {
            0.0
        };
        end.cpu_percent_window = Some(cpu_pct);
        Ok(end)
    }
}

/// 返回当前系统逻辑 CPU 核心数量
pub fn logical_cpu_cores() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

fn filetime_to_ms(ft: FILETIME) -> f64 {
    let ticks = (u64::from(ft.dwHighDateTime) << 32) | u64::from(ft.dwLowDateTime);
    (ticks as f64) / 10_000.0
}

fn bytes_to_mib(bytes: usize) -> f64 {
    (bytes as f64) / (1024.0 * 1024.0)
}

fn snapshot_handle(handle: HANDLE, pid: u32, label: &str) -> Result<ProcessTelemetrySnapshot> {
    let mut mem = ProcessMemoryCountersEx {
        cb: size_of::<ProcessMemoryCountersEx>() as u32,
        ..Default::default()
    };
    let mem_ok = unsafe { K32GetProcessMemoryInfo(handle, &mut mem, mem.cb) };
    if !mem_ok.as_bool() {
        bail!("K32GetProcessMemoryInfo 调用失败（pid={pid}）");
    }

    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    let time_ok =
        unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) };
    if !time_ok.as_bool() {
        bail!("GetProcessTimes 调用失败（pid={pid}）");
    }

    Ok(ProcessTelemetrySnapshot {
        pid,
        label: label.to_string(),
        working_set_mib: bytes_to_mib(mem.working_set_size),
        peak_working_set_mib: bytes_to_mib(mem.peak_working_set_size),
        private_bytes_mib: bytes_to_mib(mem.private_usage),
        kernel_time_ms: filetime_to_ms(kernel),
        user_time_ms: filetime_to_ms(user),
        cpu_percent_window: None,
    })
}

/// 采样当前测试进程的内存及 CPU 累计指标
pub fn snapshot_current_process(label: &str) -> Result<ProcessTelemetrySnapshot> {
    let handle = unsafe { GetCurrentProcess() };
    snapshot_handle(handle, std::process::id(), label)
}

/// 打开并采样目标 PID 进程的内存及 CPU 累计指标
pub fn snapshot_process(pid: u32, label: &str) -> Result<ProcessTelemetrySnapshot> {
    let raw_handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)? };
    let guard = ProcessHandleGuard(raw_handle);
    snapshot_handle(guard.0, pid, label)
}

/// 读取宿主机总物理内存与可用物理内存（MiB）
pub fn read_system_memory_mib() -> Result<(f64, f64)> {
    let mut status = MemoryStatusEx {
        dw_length: size_of::<MemoryStatusEx>() as u32,
        ..Default::default()
    };
    let ok = unsafe { GlobalMemoryStatusEx(&mut status) };
    if !ok.as_bool() {
        bail!("GlobalMemoryStatusEx 调用失败");
    }
    let mib_div = 1024.0 * 1024.0;
    Ok((
        (status.ull_total_phys as f64) / mib_div,
        (status.ull_avail_phys as f64) / mib_div,
    ))
}
