//! Windows Kernel32 原生 FFI 绑定模块：提供零外部依赖的进程工作集、私有字节、CPU 时间及系统内存读取

use windows::core::BOOL;
use windows::Win32::Foundation::{FILETIME, HANDLE};

/// 对应 Win32 `PROCESS_MEMORY_COUNTERS_EX` 内存布局结构体，用于采集精确工作集、峰值工作集与私有提交字节
#[repr(C)]
#[derive(Debug, Clone, Default)]
pub struct ProcessMemoryCountersEx {
    pub cb: u32,
    pub page_fault_count: u32,
    pub peak_working_set_size: usize,
    pub working_set_size: usize,
    pub quota_peak_paged_pool_usage: usize,
    pub quota_paged_pool_usage: usize,
    pub quota_peak_non_paged_pool_usage: usize,
    pub quota_non_paged_pool_usage: usize,
    pub pagefile_usage: usize,
    pub peak_pagefile_usage: usize,
    pub private_usage: usize,
}

/// 对应 Win32 `MEMORYSTATUSEX` 内存布局结构体，用于采集宿主机物理内存总量与可用量
#[repr(C)]
#[derive(Debug, Clone, Default)]
pub struct MemoryStatusEx {
    pub dw_length: u32,
    pub dw_memory_load: u32,
    pub ull_total_phys: u64,
    pub ull_avail_phys: u64,
    pub ull_total_page_file: u64,
    pub ull_avail_page_file: u64,
    pub ull_total_virtual: u64,
    pub ull_avail_virtual: u64,
    pub ull_avail_extended_virtual: u64,
}

#[link(name = "kernel32")]
extern "system" {
    /// 调用 Kernel32 `K32GetProcessMemoryInfo` 采集进程内存计数器
    pub fn K32GetProcessMemoryInfo(
        process: HANDLE,
        ppsmem_counters: *mut ProcessMemoryCountersEx,
        cb: u32,
    ) -> BOOL;

    /// 调用 Kernel32 `GetProcessTimes` 获取进程内核态与用户态累计 CPU 时间
    pub fn GetProcessTimes(
        h_process: HANDLE,
        creation_time: *mut FILETIME,
        exit_time: *mut FILETIME,
        kernel_time: *mut FILETIME,
        user_time: *mut FILETIME,
    ) -> BOOL;

    /// 调用 Kernel32 `GlobalMemoryStatusEx` 获取全局物理内存容量信息
    pub fn GlobalMemoryStatusEx(lp_buffer: *mut MemoryStatusEx) -> BOOL;
}
