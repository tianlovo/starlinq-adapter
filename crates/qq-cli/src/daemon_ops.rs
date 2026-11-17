//! 后台守护进程管理与服务启停模块：负责配置加载、远程探活桥接、常驻托管与纯 Rust 热重启

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use common_types::{AdapterConfig, AdapterError, AdapterStatus, BotDriver, WindowInfo};
use qqnt_driver::QqntDriver;
use reqwest::Method;
use serde::Deserialize;
use serde_json::Value;

use crate::commands::{print_sandbox_guidance, print_status};
use crate::constants::{
    COPY_RETRY_ATTEMPTS, COPY_RETRY_DELAY_MS, DAEMON_ERR_LOG_NAME, DAEMON_EXE_NAME,
    DAEMON_OUT_LOG_NAME, DEFAULT_CONFIG_PATH, EXPLICIT_PROBE_TIMEOUT_MS, FAST_PROBE_TIMEOUT_MS,
    HEALTH_POLL_ATTEMPTS, HEALTH_POLL_INTERVAL_MS, MINGW_BIN_DIR, RELEASE_CLI_BIN_REL,
    RESTART_POLL_TIMEOUT_SECS, ROUTE_API_ENSURE_QQ, ROUTE_API_HEALTH, ROUTE_API_RESTART,
    ROUTE_API_SHUTDOWN, ROUTE_API_STATUS, RUNTIME_BIN_SUBDIR, RUNTIME_CONNECTION_FILE,
    RUNTIME_DIR_NAME,
};
use crate::http_client::{
    format_base_url, normalize_connect_host, probe_health, validate_http_base_url,
    HttpResponsePayload, RemoteClient,
};
use std::sync::Arc;

/// 序列化记录在 `.runtime/connection.json` 中的守护进程监听地址与 PID 元数据
#[derive(Debug, Deserialize)]
pub struct ConnectionFile {
    pub host: String,
    pub port: u16,
    #[serde(default)]
    pub pid: Option<u32>,
}

/// 沙箱隔离桌面下执行 `start-daemon` 时的分支动作策略
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxDaemonAction {
    ProceedLocalSpawn,
    DelegateHotRestart,
    RejectIsolatedNoRemote,
}

/// 依据当前桌面隔离态与本机已有守护进程探活结果决定 `start-daemon` 的安全动作
pub fn decide_start_daemon_action(
    sandbox_isolated: bool,
    remote_healthy: bool,
) -> SandboxDaemonAction {
    if !sandbox_isolated {
        return SandboxDaemonAction::ProceedLocalSpawn;
    }
    if remote_healthy {
        SandboxDaemonAction::DelegateHotRestart
    } else {
        SandboxDaemonAction::RejectIsolatedNoRemote
    }
}

/// 从磁盘加载 TOML 配置；当默认路径缺失时安全回退至内置默认值
pub fn load_config(path: &Path) -> Result<AdapterConfig> {
    let default_path = Path::new(DEFAULT_CONFIG_PATH);
    if !path.exists() {
        if path == default_path {
            tracing::warn!(
                path = %path.display(),
                "default config file not found; using built-in AdapterConfig defaults"
            );
            return Ok(AdapterConfig::default());
        }
        bail!("configuration file not found: {}", path.display());
    }
    let raw_toml = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read config {}", path.display()))?;
    toml::from_str(&raw_toml).with_context(|| format!("failed to parse TOML {}", path.display()))
}

/// 提取配置中非空的 HTTP 访问令牌
pub fn clean_access_token(config: &AdapterConfig) -> Option<String> {
    config
        .server
        .access_token
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn build_candidate_remotes(config: &AdapterConfig) -> Vec<String> {
    let mut candidates = Vec::new();
    if let Ok(raw) = std::fs::read_to_string(RUNTIME_CONNECTION_FILE) {
        if let Ok(connection) = serde_json::from_str::<ConnectionFile>(&raw) {
            let host = normalize_connect_host(&connection.host);
            candidates.push(format_base_url(host, connection.port));
        }
    }
    let config_host = normalize_connect_host(&config.server.host);
    let config_url = format_base_url(config_host, config.server.port);
    if !candidates.contains(&config_url) {
        candidates.push(config_url);
    }
    candidates
}

async fn probe_remote_client(
    raw_url: &str,
    token: Option<String>,
    fast_auto_probe: bool,
) -> Result<RemoteClient> {
    let base_url = validate_http_base_url(raw_url)?;
    let timeout = if fast_auto_probe {
        Duration::from_millis(FAST_PROBE_TIMEOUT_MS)
    } else {
        Duration::from_millis(EXPLICIT_PROBE_TIMEOUT_MS)
    };
    let response = probe_health(&base_url, token.as_deref(), timeout).await?;
    if response.status_code != 200 {
        bail!(
            "remote daemon health check at {base_url} returned HTTP {}: {}",
            response.status_code,
            response.body
        );
    }
    RemoteClient::build(base_url, token)
}

/// 按 `--local-only`、`--remote` 与 `.runtime/connection.json` 顺序探测可用远程守护进程
pub async fn try_connect_remote(
    explicit_remote: Option<&str>,
    local_only: bool,
    config: &AdapterConfig,
) -> Result<Option<RemoteClient>> {
    if local_only {
        return Ok(None);
    }
    let token = clean_access_token(config);

    if let Some(raw_url) = explicit_remote {
        let client = probe_remote_client(raw_url, token, false).await?;
        println!("[Remote Daemon {}]", client.base_url);
        return Ok(Some(client));
    }

    let (_, sandbox_isolated) = uia_core::current_desktop_info();
    for candidate in build_candidate_remotes(config) {
        match probe_remote_client(&candidate, token.clone(), true).await {
            Ok(client) => {
                println!("[Remote Daemon {}]", client.base_url);
                return Ok(Some(client));
            }
            Err(err) => {
                tracing::debug!(
                    candidate = %candidate,
                    sandbox_isolated,
                    error = %err,
                    "auto remote daemon probe skipped/unreachable"
                );
            }
        }
    }

    Ok(None)
}

pub fn read_connection_pid() -> Option<u32> {
    std::fs::read_to_string(RUNTIME_CONNECTION_FILE)
        .ok()
        .and_then(|raw| serde_json::from_str::<ConnectionFile>(&raw).ok())
        .and_then(|connection| connection.pid)
}

pub fn parse_health_pid(response: &HttpResponsePayload) -> Option<u32> {
    serde_json::from_str::<Value>(&response.body)
        .ok()
        .and_then(|val| val.get("pid").and_then(Value::as_u64))
        .and_then(|pid| u32::try_from(pid).ok())
}

/// 判断热重启轮询是否仍需继续等待旧进程彻底退出或新进程完成 PID 轮换
pub fn should_wait_for_restart(
    old_pid: Option<u32>,
    saw_down: bool,
    live_pid: Option<u32>,
) -> bool {
    match (old_pid, live_pid) {
        (Some(old), Some(live)) => live == old,
        (None, Some(_)) => false,
        (_, None) => !saw_down,
    }
}

/// 剥离 `std::fs::canonicalize` 在 Windows 上附加的 `\\?\` 逐字前缀以兼容常规命令传参
pub fn strip_windows_verbatim(path: &Path) -> PathBuf {
    let raw = path.to_string_lossy();
    if let Some(rest) = raw.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = raw.strip_prefix(r"\\?\") {
        if rest.as_bytes().get(1) == Some(&b':') {
            return PathBuf::from(rest);
        }
    }
    path.to_path_buf()
}

/// 根据配置文件所在位置反推仓库根目录
pub fn resolve_repo_root(cli_config: &Path) -> Result<PathBuf> {
    let cwd = std::env::current_dir().context("failed to determine current directory")?;
    let abs_config = if cli_config.is_absolute() {
        cli_config.to_path_buf()
    } else {
        cwd.join(cli_config)
    };
    if let Some(parent) = abs_config.parent() {
        if parent
            .file_name()
            .is_some_and(|name| name.eq_ignore_ascii_case("config"))
        {
            if let Ok(canon_cfg_dir) = std::fs::canonicalize(parent) {
                if let Some(root) = canon_cfg_dir.parent() {
                    return Ok(strip_windows_verbatim(root));
                }
            }
            if let Some(root) = parent.parent() {
                if !root.as_os_str().is_empty() {
                    return Ok(root.to_path_buf());
                }
            }
        }
    }
    Ok(cwd)
}

/// 将 CLI 传入的配置路径归一化为适合传递给脱离终端守护进程的相对或绝对参数
pub fn resolve_daemon_config_arg(repo_root: &Path, cli_config: &Path) -> String {
    let default_path = Path::new(DEFAULT_CONFIG_PATH);
    if !cli_config.exists() && cli_config == default_path {
        return DEFAULT_CONFIG_PATH.to_string();
    }
    let abs_config = if cli_config.is_absolute() {
        cli_config.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(cli_config))
            .unwrap_or_else(|_| cli_config.to_path_buf())
    };
    let clean_config = if abs_config.exists() {
        std::fs::canonicalize(&abs_config)
            .map(|path| strip_windows_verbatim(&path))
            .unwrap_or(abs_config)
    } else {
        abs_config
    };
    let clean_root = std::fs::canonicalize(repo_root)
        .map(|path| strip_windows_verbatim(&path))
        .unwrap_or_else(|_| repo_root.to_path_buf());
    if let Ok(rel) = clean_config.strip_prefix(&clean_root) {
        return rel.to_string_lossy().into_owned();
    }
    clean_config.to_string_lossy().into_owned()
}

fn read_file_tail(path: &Path, max_lines: usize) -> String {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return "<unavailable>".to_string();
    };
    let lines: Vec<&str> = raw.lines().collect();
    let start = lines.len().saturating_sub(max_lines);
    let tail = lines[start..].join("\n");
    if tail.trim().is_empty() {
        "<empty>".to_string()
    } else {
        tail
    }
}

async fn copy_binary_with_retry(source_path: &Path, target_path: &Path) -> Result<()> {
    let same_file = target_path.is_file()
        && std::fs::canonicalize(source_path).ok() == std::fs::canonicalize(target_path).ok();
    if same_file {
        return Ok(());
    }
    let mut last_error = None;
    for attempt in 0..COPY_RETRY_ATTEMPTS {
        match std::fs::copy(source_path, target_path) {
            Ok(_) => return Ok(()),
            Err(err) => {
                last_error = Some(err);
                if attempt + 1 < COPY_RETRY_ATTEMPTS {
                    tokio::time::sleep(Duration::from_millis(COPY_RETRY_DELAY_MS)).await;
                }
            }
        }
    }
    Err(last_error.unwrap()).with_context(|| {
        format!(
            "failed to copy daemon binary {} -> {} after {COPY_RETRY_ATTEMPTS} attempts",
            source_path.display(),
            target_path.display()
        )
    })
}

/// 构建带无窗口标志和 MinGW 搜索路径的后台 `serve` 启动命令
pub fn build_daemon_serve_command(
    daemon_bin: &Path,
    config_arg: &str,
    repo_root: &Path,
    stdout_stdio: Stdio,
    stderr_stdio: Stdio,
) -> Command {
    let mut command = Command::new(daemon_bin);
    command
        .args(["serve", "--config", config_arg])
        .current_dir(repo_root)
        .env(
            "PATH",
            adapter_server::rest::prepend_path_dir(
                std::env::var_os("PATH").as_deref(),
                MINGW_BIN_DIR,
            ),
        )
        .stdin(Stdio::null())
        .stdout(stdout_stdio)
        .stderr(stderr_stdio);

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(adapter_server::rest::RESTART_CREATION_FLAGS);
    }

    command
}

async fn stop_existing_daemon(client: &RemoteClient) -> Result<()> {
    let Ok(health_response) = client
        .request_with_timeout(
            Method::GET,
            ROUTE_API_HEALTH,
            None,
            Duration::from_millis(1500),
        )
        .await
    else {
        return Ok(());
    };
    if health_response.status_code != 200 {
        return Ok(());
    }

    println!(
        "[qq-uia-adapter] Existing daemon detected at {}; sending shutdown...",
        client.base_url
    );
    let _ = client
        .request_with_timeout(
            Method::POST,
            ROUTE_API_SHUTDOWN,
            None,
            Duration::from_secs(3),
        )
        .await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    for _ in 0..20 {
        match client
            .request_with_timeout(
                Method::GET,
                ROUTE_API_HEALTH,
                None,
                Duration::from_millis(800),
            )
            .await
        {
            Ok(resp) if resp.status_code == 200 => {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            _ => return Ok(()),
        }
    }
    bail!(
        "existing daemon at {} remained reachable after shutdown request",
        client.base_url
    )
}

async fn wait_for_daemon_healthy(
    child: &mut std::process::Child,
    client: &RemoteClient,
    err_log: &Path,
) -> Result<()> {
    let child_pid = child.id();
    for _ in 0..HEALTH_POLL_ATTEMPTS {
        if let Some(exit_status) = child
            .try_wait()
            .context("failed to check spawned daemon process status")?
        {
            let err_tail = read_file_tail(err_log, 25);
            bail!(
                "daemon process pid={child_pid} exited early ({exit_status}); daemon.err.log tail:\n{err_tail}"
            );
        }

        if let Ok(resp) = client
            .request_with_timeout(
                Method::GET,
                ROUTE_API_HEALTH,
                None,
                Duration::from_millis(800),
            )
            .await
        {
            if resp.status_code == 200
                && resp.json().ok().and_then(|val| val["ok"].as_bool()) == Some(true)
            {
                println!(
                    "[qq-uia-adapter] Daemon healthy at {} (pid={child_pid}).",
                    client.base_url
                );
                return Ok(());
            }
        }
        tokio::time::sleep(Duration::from_millis(HEALTH_POLL_INTERVAL_MS)).await;
    }

    let err_tail = read_file_tail(err_log, 25);
    bail!(
        "timed out waiting for daemon pid={child_pid} at {} to become healthy; daemon.err.log tail:\n{err_tail}",
        client.base_url
    );
}

async fn report_post_start_status(client: &RemoteClient) -> Result<()> {
    match client
        .request_with_timeout(
            Method::POST,
            ROUTE_API_ENSURE_QQ,
            None,
            Duration::from_secs(20),
        )
        .await
    {
        Ok(resp) if resp.status_code == 200 => {
            let count = serde_json::from_str::<Vec<WindowInfo>>(&resp.body)
                .map(|windows| windows.len())
                .unwrap_or(0);
            println!("[qq-uia-adapter] ensure-qq windows count: {count}");
        }
        Ok(resp) => {
            println!(
                "[qq-uia-adapter] ensure-qq returned HTTP {} (non-fatal): {}",
                resp.status_code, resp.body
            );
        }
        Err(err) => {
            println!("[qq-uia-adapter] ensure-qq failed (non-fatal): {err}");
        }
    }

    let status_resp = client
        .request_with_timeout(Method::GET, ROUTE_API_STATUS, None, Duration::from_secs(10))
        .await
        .with_context(|| format!("failed to query {}/api/status", client.base_url))?;
    if status_resp.status_code >= 400 {
        bail!(
            "GET /api/status returned HTTP {}: {}",
            status_resp.status_code,
            status_resp.body
        );
    }
    let status: AdapterStatus = serde_json::from_str(&status_resp.body)
        .with_context(|| format!("failed to decode /api/status JSON: {}", status_resp.body))?;
    println!(
        "[qq-uia-adapter] Status: desktop={} sandbox_isolated={} qq_pids={:?} resolved_qq_path={} active_group={} input_mode={}",
        status.desktop_name,
        status.sandbox_isolated,
        status.qq_pids,
        status.resolved_qq_path.as_deref().unwrap_or("<none>"),
        status.active_group.as_deref().unwrap_or("<none>"),
        status.input_mode
    );
    Ok(())
}

/// 启动常驻 HTTP / WebSocket / OneBot v11 适配器服务
pub async fn cmd_serve(
    mut config: AdapterConfig,
    port: Option<u16>,
    host: Option<String>,
    auto_ping: bool,
) -> Result<()> {
    if let Some(listen_port) = port {
        config.server.port = listen_port;
    }
    if let Some(listen_host) = host {
        config.server.host = listen_host;
    }
    if auto_ping {
        config.bot.auto_ping_reply = true;
    }

    let driver = Arc::new(QqntDriver::new(config.clone())?);
    let initial_status = driver.status().await?;
    if initial_status.sandbox_isolated {
        tracing::warn!(
            desktop = %initial_status.desktop_name,
            "running inside isolated desktop; live QQ windows may not be visible here"
        );
    }

    match driver.open_group(config.target.clone()).await {
        Ok(attached_status) => {
            println!(
                "已附加目标群: {:?} (成员数检测={:?}, 模式={})",
                attached_status.active_group,
                attached_status.detected_member_count,
                attached_status.input_mode
            );
        }
        Err(err) => {
            tracing::warn!(
                ?err,
                "startup open_group did not attach yet; HTTP/WS server will still start"
            );
            if initial_status.sandbox_isolated && matches!(err, AdapterError::WindowNotFound(_)) {
                print_sandbox_guidance(&initial_status.desktop_name, ".\\start-daemon.bat");
            }
        }
    }

    let base_url = format!("http://{}:{}", config.server.host, config.server.port);
    let ws_base = format!("ws://{}:{}", config.server.host, config.server.port);
    println!("QQNT UIA Adapter listening on {base_url}");
    println!(
        "  Native REST : {base_url}/api/health, /api/status, /api/windows, /api/ensure-qq, /api/diag, /api/groups, /api/group-members, /api/storage, /api/shutdown, /api/restart, /api/tree, /api/sessions, /api/messages, /api/open-group, /api/send"
    );
    println!("  Native WS   : {ws_base}/ws/events");
    println!(
        "  OneBot v11  : {base_url}/onebot/v11/get_status, /get_login_info, /get_version_info, /send_group_msg, /send_msg, /get_msg, /get_group_list, /get_group_info, /get_group_member_list, /get_group_member_info"
    );
    println!("  OneBot WS   : {ws_base}/onebot/v11/ws");
    if !config.onebot.reverse_ws_urls.is_empty() {
        println!("  Reverse WS  : {:?}", config.onebot.reverse_ws_urls);
    }

    adapter_server::bind_and_serve(driver, config).await
}

/// 向已运行的守护进程发起热重启请求并等待新 PID 上线
pub async fn cmd_restart(
    config: AdapterConfig,
    explicit_remote: Option<&str>,
    local_only: bool,
) -> Result<()> {
    let Some(client) = try_connect_remote(explicit_remote, local_only, &config).await? else {
        bail!(
            "no running adapter daemon found via .runtime/connection.json or {}:{} (start one first via start-daemon.bat or serve)",
            normalize_connect_host(&config.server.host),
            config.server.port
        );
    };

    let probe_url = match explicit_remote {
        Some(raw) => validate_http_base_url(raw)?,
        None => client.base_url.clone(),
    };
    let old_pid = probe_health(
        &probe_url,
        client.token.as_deref(),
        Duration::from_millis(1200),
    )
    .await
    .ok()
    .filter(|resp| resp.status_code == 200)
    .and_then(|resp| parse_health_pid(&resp))
    .or_else(|| {
        if explicit_remote.is_none() {
            read_connection_pid()
        } else {
            None
        }
    });

    let response: Value = client.call(Method::POST, ROUTE_API_RESTART, None).await?;
    println!("Restart requested: {response}");
    tokio::time::sleep(Duration::from_millis(800)).await;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(RESTART_POLL_TIMEOUT_SECS);
    let mut saw_down = false;
    let mut last_live_pid: Option<u32> = None;

    loop {
        if tokio::time::Instant::now() >= deadline {
            bail!(
                "timed out waiting up to {RESTART_POLL_TIMEOUT_SECS}s for daemon at {} to restart (old_pid={old_pid:?}, live_pid={last_live_pid:?}, connection_pid={:?}, saw_down={saw_down})",
                client.base_url,
                read_connection_pid(),
            );
        }

        let probe_result = probe_health(
            &probe_url,
            client.token.as_deref(),
            Duration::from_millis(1200),
        )
        .await;

        match probe_result {
            Ok(health) if health.status_code == 200 => {
                let live_pid = parse_health_pid(&health).or_else(|| {
                    if explicit_remote.is_none() {
                        read_connection_pid()
                    } else {
                        None
                    }
                });
                last_live_pid = live_pid;
                if should_wait_for_restart(old_pid, saw_down, live_pid) {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    continue;
                }
                if let Ok(status) = client
                    .call::<AdapterStatus>(Method::GET, ROUTE_API_STATUS, None)
                    .await
                {
                    println!("Daemon restarted and online at {}:", client.base_url);
                    print_status(&status);
                    return Ok(());
                }
            }
            _ => {
                saw_down = true;
            }
        }

        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

/// 停掉旧守护进程、暂存运行副本并在后台拉起最新常驻适配器进程
pub async fn cmd_start_daemon(config: AdapterConfig, cli_config: &Path) -> Result<()> {
    let repo_root = resolve_repo_root(cli_config)?;
    let connect_host = normalize_connect_host(&config.server.host);
    let base_url = format_base_url(connect_host, config.server.port);
    let token = clean_access_token(&config);
    let client = RemoteClient::build(base_url, token)?;

    let (desktop_name, sandbox_isolated) = uia_core::current_desktop_info();
    let remote_healthy = if sandbox_isolated {
        matches!(
            client
                .request_with_timeout(
                    Method::GET,
                    ROUTE_API_HEALTH,
                    None,
                    Duration::from_millis(1500),
                )
                .await,
            Ok(resp) if resp.status_code == 200
        )
    } else {
        false
    };
    match decide_start_daemon_action(sandbox_isolated, remote_healthy) {
        SandboxDaemonAction::DelegateHotRestart => {
            println!(
                "[qq-uia-adapter] Caller desktop '{desktop_name}' is sandbox-isolated; delegating to POST /api/restart so the active Default-desktop daemon hot-restarts without shutting down."
            );
            return cmd_restart(config, None, false).await;
        }
        SandboxDaemonAction::RejectIsolatedNoRemote => {
            print_sandbox_guidance(&desktop_name, ".\\start-daemon.bat");
            bail!(
                "refusing to spawn a blind daemon inside sandbox-isolated desktop '{desktop_name}' with no reachable Default-desktop daemon; run .\\start-daemon.bat on the Default desktop"
            );
        }
        SandboxDaemonAction::ProceedLocalSpawn => {}
    }

    stop_existing_daemon(&client).await?;

    let release_binary = repo_root.join(RELEASE_CLI_BIN_REL);
    let source_binary = if release_binary.is_file() {
        release_binary
    } else {
        std::env::current_exe().context("failed to resolve current executable path")?
    };

    let runtime_dir = repo_root.join(RUNTIME_DIR_NAME);
    let runtime_bin_dir = runtime_dir.join(RUNTIME_BIN_SUBDIR);
    std::fs::create_dir_all(&runtime_bin_dir).with_context(|| {
        format!(
            "failed to create runtime bin directory {}",
            runtime_bin_dir.display()
        )
    })?;
    let daemon_binary = runtime_bin_dir.join(DAEMON_EXE_NAME);
    copy_binary_with_retry(&source_binary, &daemon_binary).await?;

    let config_path_arg = resolve_daemon_config_arg(&repo_root, cli_config);
    let out_log = runtime_dir.join(DAEMON_OUT_LOG_NAME);
    let err_log = runtime_dir.join(DAEMON_ERR_LOG_NAME);
    let out_file = std::fs::File::create(&out_log)
        .with_context(|| format!("failed to create {}", out_log.display()))?;
    let err_file = std::fs::File::create(&err_log)
        .with_context(|| format!("failed to create {}", err_log.display()))?;

    let mut command = build_daemon_serve_command(
        &daemon_binary,
        &config_path_arg,
        &repo_root,
        Stdio::from(out_file),
        Stdio::from(err_file),
    );
    let mut child = command
        .spawn()
        .with_context(|| format!("failed to spawn daemon binary {}", daemon_binary.display()))?;

    wait_for_daemon_healthy(&mut child, &client, &err_log).await?;
    report_post_start_status(&client).await
}
