//! Win32 进程快照枚举与进程镜像元信息查询模块，统一提供 QQ 客户端 PID 筛选能力

use std::collections::{HashMap, HashSet};
use std::mem::size_of;

use common_types::constants::processes::QQ_PROCESS_NAMES_LOWER;
use windows::Win32::Foundation::{FILETIME, HANDLE};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows_core::PWSTR;

use super::{utf16_slice_to_string, HandleGuard};
use crate::constants::IMAGE_PATH_BUFFER_LEN;

/// 通过 ToolHelp32 采集当前系统内 `pid -> 进程文件名` 的快照映射表
pub(crate) fn snapshot_process_map() -> HashMap<u32, String> {
    let mut process_map = HashMap::new();
    let snapshot = match unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) } {
        Ok(handle) if !handle.is_invalid() => HandleGuard(handle),
        _ => return process_map,
    };

    let mut entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };

    let first_ok = unsafe { Process32FirstW(snapshot.0, &mut entry) }.is_ok();
    if !first_ok {
        return process_map;
    }

    loop {
        let executable_name = utf16_slice_to_string(&entry.szExeFile);
        process_map.insert(entry.th32ProcessID, executable_name);
        if unsafe { Process32NextW(snapshot.0, &mut entry) }.is_err() {
            break;
        }
    }

    process_map
}

/// 按可执行文件名精确（忽略大小写）匹配 QQ 客户端或自定义附加列表中的所有进程 PID
pub fn find_pids_by_names(names: &[String]) -> Vec<u32> {
    let mut targets: HashSet<String> = QQ_PROCESS_NAMES_LOWER
        .iter()
        .map(|name| (*name).to_string())
        .collect();
    for candidate_name in names {
        let trimmed = candidate_name.trim();
        if !trimmed.is_empty() {
            targets.insert(trimmed.to_ascii_lowercase());
        }
    }

    let mut matched_pids = Vec::new();
    for (pid, executable_name) in snapshot_process_map() {
        if targets.contains(&executable_name.to_ascii_lowercase()) {
            matched_pids.push(pid);
        }
    }
    matched_pids.sort_unstable();
    matched_pids.dedup();
    matched_pids
}

/// 判断给定进程文件名是否精确匹配官方 QQNT 可执行文件（`QQ.exe` 或 `QQEX.exe`）
pub(crate) fn is_exact_qq_process_name(process_name: &str) -> bool {
    let normalized = process_name.trim().to_ascii_lowercase();
    QQ_PROCESS_NAMES_LOWER
        .iter()
        .any(|expected| *expected == normalized)
}

/// 通过已打开的 Win32 进程句柄读取完整的 Win32 可执行文件镜像路径
pub(crate) fn query_image_path_from_handle(process_handle: HANDLE) -> Option<String> {
    let mut buffer = vec![0u16; IMAGE_PATH_BUFFER_LEN];
    let mut length = buffer.len() as u32;
    let query_result = unsafe {
        QueryFullProcessImageNameW(
            process_handle,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &mut length,
        )
    };
    if query_result.is_ok() && length > 0 {
        Some(String::from_utf16_lossy(&buffer[..length as usize]))
    } else {
        None
    }
}

/// 按 PID 以受限查询权限打开进程并读取完整可执行文件路径
pub(crate) fn query_process_image_path(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    let guard = HandleGuard(handle);
    query_image_path_from_handle(guard.0)
}

/// 查询进程的创建时间（自 1601-01-01 UTC 起的 100 纳秒计数）
pub(crate) fn query_process_creation_time_100ns(process_handle: HANDLE) -> Option<u64> {
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    unsafe {
        GetProcessTimes(
            process_handle,
            &mut creation,
            &mut exit,
            &mut kernel,
            &mut user,
        )
    }
    .ok()?;
    Some(((creation.dwHighDateTime as u64) << 32) | (creation.dwLowDateTime as u64))
}
