//! QQNT 可执行文件路径探测与脱离控制台后台进程拉起模块

use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};

use common_types::constants::processes::QQ_PROCESS_NAMES;

use super::process_lookup::query_process_image_path;
use crate::constants::{DETACHED_PROCESS_CREATION_FLAG, QQ_EXECUTABLE_FALLBACK_PATHS};

/// 依次从显式配置路径、运行中进程路径以及默认安装候选路径中解析可用的 `QQ.exe` 位置
pub fn resolve_qq_executable(configured: Option<&str>, pids: &[u32]) -> Option<String> {
    if let Some(raw_path) = configured {
        let trimmed = raw_path.trim();
        if !trimmed.is_empty() && Path::new(trimmed).is_file() {
            return Some(trimmed.to_string());
        }
    }

    let primary_qq_name = QQ_PROCESS_NAMES[0];
    for &pid in pids {
        if let Some(image_path) = query_process_image_path(pid) {
            let path_object = Path::new(&image_path);
            let is_qq_executable = path_object
                .file_name()
                .and_then(|file_name| file_name.to_str())
                .map(|file_name| file_name.eq_ignore_ascii_case(primary_qq_name))
                .unwrap_or(false);
            if is_qq_executable && path_object.is_file() {
                return Some(image_path);
            }
        }
    }

    for &candidate in QQ_EXECUTABLE_FALLBACK_PATHS {
        if Path::new(candidate).is_file() {
            return Some(candidate.to_string());
        }
    }

    None
}

/// 使用 `DETACHED_PROCESS` 标志在独立后台进程组中启动或唤醒指定路径的 QQNT 客户端，返回子进程 PID
pub fn launch_or_wake_qq(executable_path: &str, args: &[String]) -> std::io::Result<u32> {
    let path = Path::new(executable_path);
    if !path.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("未在路径 '{executable_path}' 找到 QQ 可执行文件"),
        ));
    }

    let mut command = Command::new(path);
    command.args(args);
    if let Some(parent_directory) = path.parent() {
        if !parent_directory.as_os_str().is_empty() {
            command.current_dir(parent_directory);
        }
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(DETACHED_PROCESS_CREATION_FLAG);

    let child_process = command.spawn()?;
    Ok(child_process.id())
}
