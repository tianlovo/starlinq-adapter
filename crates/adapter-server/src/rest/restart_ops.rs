//! 守护进程热重启命令构建模块：以纯 Rust 脱离控制台拉起 `start-daemon`，规避脚本宿主拦截

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::Json;
use common_types::AdapterError;
use serde_json::{json, Value};

use crate::constants::{
    MINGW_BIN_DIR, RELEASE_BINARY_PATH, RESTARTER_EXE_NAME, RESTART_LOG_PATH,
    RESTART_SPAWN_DELAY_MS, RUNTIME_BIN_DIR,
};
use crate::error::ApiResult;
use crate::AppState;

/// 创建新进程组标志，避免父进程接收 Ctrl 事件时连带终止子进程
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
/// 无控制台窗口创建标志，避免弹出黑框并允许独立句柄重定向
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// 脱离父进程控制台执行后台热重启使用的 Win32 进程创建标志组合
pub const RESTART_CREATION_FLAGS: u32 = CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP;

/// 将指定目录前置拼接到 Windows `PATH` 环境变量中（若已存在则按大小写无关去重）
pub fn prepend_path_dir(current_path: Option<&std::ffi::OsStr>, dir: &str) -> std::ffi::OsString {
    let normalized_target = dir.trim_end_matches(['\\', '/']);
    if let Some(existing) = current_path {
        let existing_str = existing.to_string_lossy();
        let already_present = existing_str.split(';').any(|entry| {
            entry
                .trim()
                .trim_end_matches(['\\', '/'])
                .eq_ignore_ascii_case(normalized_target)
        });
        if already_present {
            return existing.to_os_string();
        }
        if existing_str.is_empty() {
            return std::ffi::OsString::from(dir);
        }
        let mut combined = std::ffi::OsString::from(dir);
        combined.push(";");
        combined.push(existing);
        return combined;
    }
    std::ffi::OsString::from(dir)
}

/// 优先选取发行版构建产物作为热重启器源程序，缺失时回退至当前运行可执行文件
pub(crate) fn resolve_restarter_source_with_fallback(
    release_candidate: &Path,
    fallback_exe: Option<&Path>,
) -> std::result::Result<PathBuf, AdapterError> {
    if release_candidate.is_file() {
        return Ok(release_candidate.to_path_buf());
    }
    if let Some(fallback) = fallback_exe {
        if fallback.is_file() {
            return Ok(fallback.to_path_buf());
        }
    }
    Err(AdapterError::ConfigError(format!(
        "restarter executable not found at {} and fallback executable is unavailable",
        release_candidate.display()
    )))
}

/// 将重启器副本暂存至运行时目录，防止后续自编译替换时触发文件占用锁
pub(crate) fn prepare_restarter_binary(
    source_path: &Path,
    runtime_bin_dir: &Path,
) -> std::result::Result<PathBuf, AdapterError> {
    std::fs::create_dir_all(runtime_bin_dir).map_err(|err| {
        AdapterError::Io(std::io::Error::new(
            err.kind(),
            format!(
                "failed to create runtime bin directory {}: {err}",
                runtime_bin_dir.display()
            ),
        ))
    })?;
    let target_path = runtime_bin_dir.join(RESTARTER_EXE_NAME);
    let same_file = target_path.is_file()
        && std::fs::canonicalize(source_path).ok() == std::fs::canonicalize(&target_path).ok();
    if !same_file {
        std::fs::copy(source_path, &target_path).map_err(|err| {
            AdapterError::Io(std::io::Error::new(
                err.kind(),
                format!(
                    "failed to copy restarter binary {} -> {}: {err}",
                    source_path.display(),
                    target_path.display()
                ),
            ))
        })?;
    }
    Ok(target_path)
}

/// 构造脱离控制台窗口且附加 MinGW 依赖搜索路径的 `start-daemon` 子进程命令
pub(crate) fn build_restarter_command(restarter_bin: &Path, restart_log: Option<&Path>) -> Command {
    let mut command = Command::new(restarter_bin);
    command
        .arg("start-daemon")
        .env(
            "PATH",
            prepend_path_dir(std::env::var_os("PATH").as_deref(), MINGW_BIN_DIR),
        )
        .stdin(Stdio::null());

    let redirected = restart_log.and_then(|log_path| {
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let out_file = std::fs::File::create(log_path).ok()?;
        let err_file = out_file.try_clone().ok()?;
        Some((out_file, err_file))
    });
    match redirected {
        Some((out_file, err_file)) => {
            command
                .stdout(Stdio::from(out_file))
                .stderr(Stdio::from(err_file));
        }
        None => {
            command.stdout(Stdio::null()).stderr(Stdio::null());
        }
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(RESTART_CREATION_FLAGS);
    }

    command
}

/// 调度后台非阻塞热重启任务；若已有重启流程进行中则幂等返回 `Ok(false)`
pub(crate) fn schedule_daemon_restart(
    state: &AppState,
    delay_ms: u64,
) -> std::result::Result<bool, AdapterError> {
    if state
        .restart_in_progress
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Ok(false);
    }

    let current_exe = std::env::current_exe().ok();
    let restarter_source = match resolve_restarter_source_with_fallback(
        Path::new(RELEASE_BINARY_PATH),
        current_exe.as_deref(),
    ) {
        Ok(path) => path,
        Err(err) => {
            state.restart_in_progress.store(false, Ordering::Release);
            return Err(err);
        }
    };
    let restarter_bin =
        match prepare_restarter_binary(&restarter_source, Path::new(RUNTIME_BIN_DIR)) {
            Ok(path) => path,
            Err(err) => {
                state.restart_in_progress.store(false, Ordering::Release);
                return Err(err);
            }
        };

    let restart_flag: Arc<AtomicBool> = Arc::clone(&state.restart_in_progress);
    let restart_state = state.clone();
    tokio::spawn(async move {
        tokio::select! {
            _ = restart_state.wait_shutdown() => {
                restart_flag.store(false, Ordering::Release);
                return;
            }
            _ = tokio::time::sleep(Duration::from_millis(delay_ms)) => {}
        }
        if restart_state.is_shutting_down() {
            restart_flag.store(false, Ordering::Release);
            return;
        }
        let mut command =
            build_restarter_command(&restarter_bin, Some(Path::new(RESTART_LOG_PATH)));
        match command.spawn() {
            Ok(child) => {
                tracing::info!(
                    pid = child.id(),
                    restarter = %restarter_bin.display(),
                    "spawned detached pure-Rust daemon restarter"
                );
            }
            Err(err) => {
                restart_flag.store(false, Ordering::Release);
                tracing::error!(
                    ?err,
                    restarter = %restarter_bin.display(),
                    "failed to spawn pure-Rust daemon restarter"
                );
            }
        }
    });
    Ok(true)
}

/// `POST /api/restart` -> 暂存独立重启副本后异步拉起 `start-daemon` 完成服务热更新
pub async fn restart(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let scheduled = schedule_daemon_restart(&state, RESTART_SPAWN_DELAY_MS)?;
    Ok(Json(json!({
        "ok": true,
        "restarting": true,
        "already_scheduled": !scheduled,
    })))
}
