//! 诊断与界面探查子命令模块：实现 `status`、`ensure-qq`、`diag`、`inspect` 与 `sessions`

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use common_types::{
    AdapterConfig, AdapterStatus, BotDriver, BurstModeState, OcrImageRequest, OcrImageResponse,
    SchedulerConfig, SchedulerTelemetrySnapshot, SessionSummary, UiNodeSnapshot,
    WindowDiagnosticReport, WindowInfo,
};
use reqwest::Method;
use serde_json::{json, Value};

use crate::cli_args::parse_hwnd;
use crate::commands::{
    dispatch_remote_or_local, print_sandbox_guidance, print_sessions, print_status, print_windows,
};
use crate::constants::{
    ROUTE_API_DIAG, ROUTE_API_ENSURE_QQ, ROUTE_API_OCR, ROUTE_API_SCHEDULER, ROUTE_API_SESSIONS,
    ROUTE_API_STATUS, ROUTE_API_TREE, ROUTE_API_WINDOWS, RUNTIME_DIR_NAME,
};
use crate::http_client::RemoteClient;

/// 远程 `POST /api/ocr` 自动 Base64 编码允许的本地图片最大原始字节数（留足 HTTP 8 MiB 请求体余量）
const MAX_REMOTE_OCR_RAW_BYTES: u64 = 5_900_000;

fn count_tree_nodes(node: &UiNodeSnapshot) -> (usize, usize, usize, usize) {
    let mut total = 1usize;
    let mut edits = 0usize;
    let mut buttons = 0usize;
    let mut lists = 0usize;

    let control_type = node.control_type.to_ascii_lowercase();
    let class_name = node.class_name.to_ascii_lowercase();
    if control_type.contains("edit") || class_name.contains("edit") {
        edits += 1;
    }
    if control_type.contains("button") || class_name.contains("button") {
        buttons += 1;
    }
    if control_type.contains("list") || class_name.contains("listbox") {
        lists += 1;
    }

    for child in &node.children {
        let (child_total, child_edits, child_buttons, child_lists) = count_tree_nodes(child);
        total += child_total;
        edits += child_edits;
        buttons += child_buttons;
        lists += child_lists;
    }
    (total, edits, buttons, lists)
}

async fn finish_inspect_tree(
    hwnd: isize,
    depth: usize,
    tree: &UiNodeSnapshot,
    out: Option<PathBuf>,
) -> Result<()> {
    let (total, edits, buttons, lists) = count_tree_nodes(tree);
    println!(
        "UIA Tree for hwnd=0x{hwnd:X} (depth={depth}): total_nodes={total}, lists={lists}, edits={edits}, buttons={buttons}"
    );

    if let Some(out_path) = out {
        if let Some(parent) = out_path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
            tokio::fs::create_dir_all(parent).await?;
        }
        let json_bytes = serde_json::to_vec_pretty(tree)?;
        tokio::fs::write(&out_path, json_bytes).await?;
        println!("已保存 UIA 树快照至: {}", out_path.display());
    }
    Ok(())
}

/// 查询并输出当前驱动运行状态
pub async fn cmd_status(config: AdapterConfig, remote: Option<RemoteClient>) -> Result<()> {
    dispatch_remote_or_local(
        config,
        remote,
        ".\\start-daemon.bat",
        |client| async move {
            let status: AdapterStatus = client.call(Method::GET, ROUTE_API_STATUS, None).await?;
            print_status(&status);
            Ok(())
        },
        |_driver, status| async move {
            print_status(&status);
            if status.sandbox_isolated && status.attached_window.is_none() {
                print_sandbox_guidance(&status.desktop_name, ".\\start-daemon.bat");
            }
            Ok(())
        },
    )
    .await
}

/// 唤醒或拉起本地 QQ 窗口并打印检测到的窗口列表
pub async fn cmd_ensure_qq(
    config: AdapterConfig,
    remote: Option<RemoteClient>,
    force_wake: bool,
) -> Result<()> {
    dispatch_remote_or_local(
        config,
        remote,
        ".\\start-daemon.bat",
        move |client| async move {
            let path = if force_wake {
                format!("{ROUTE_API_ENSURE_QQ}?force_wake=true")
            } else {
                ROUTE_API_ENSURE_QQ.to_string()
            };
            let windows: Vec<WindowInfo> = client.call(Method::POST, &path, None).await?;
            print_windows(&windows);
            Ok(())
        },
        move |driver, status| async move {
            let windows = driver.ensure_qq_window_ext(force_wake).await?;
            print_windows(&windows);
            if status.sandbox_isolated && windows.is_empty() {
                print_sandbox_guidance(&status.desktop_name, ".\\start-daemon.bat");
            }
            Ok(())
        },
    )
    .await
}

/// 采集并输出目标窗口的 Win32/MSAA/IA2/UIA 诊断报告
pub async fn cmd_diag(
    config: AdapterConfig,
    remote: Option<RemoteClient>,
    hwnd_arg: Option<String>,
) -> Result<()> {
    let parsed_hwnd = hwnd_arg.as_deref().map(parse_hwnd).transpose()?;
    dispatch_remote_or_local(
        config,
        remote,
        ".\\start-daemon.bat",
        move |client| async move {
            let report: WindowDiagnosticReport = match parsed_hwnd {
                Some(hwnd) => {
                    let hwnd_str = hwnd.to_string();
                    client
                        .call_query(ROUTE_API_DIAG, &[("hwnd", &hwnd_str)])
                        .await?
                }
                None => client.call(Method::GET, ROUTE_API_DIAG, None).await?,
            };
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(())
        },
        move |driver, _status| async move {
            let report = driver.diagnose_window(parsed_hwnd).await?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(())
        },
    )
    .await
}

/// 枚举候选 QQ 窗口并导出指定窗口的 UIA 元素树
pub async fn cmd_inspect(
    config: AdapterConfig,
    remote: Option<RemoteClient>,
    hwnd_arg: Option<String>,
    depth: usize,
    out: Option<PathBuf>,
) -> Result<()> {
    let remote_hwnd_arg = hwnd_arg.clone();
    let remote_out = out.clone();
    dispatch_remote_or_local(
        config,
        remote,
        ".\\inspect-qq.bat",
        move |client| async move {
            let status: AdapterStatus = client.call(Method::GET, ROUTE_API_STATUS, None).await?;
            let mut windows: Vec<WindowInfo> =
                client.call(Method::GET, ROUTE_API_WINDOWS, None).await?;
            if windows.is_empty() && remote_hwnd_arg.is_none() {
                windows = client.call(Method::POST, ROUTE_API_ENSURE_QQ, None).await?;
            }
            println!(
                "Desktop: '{}' (sandbox_isolated={}), QQ PIDs: {:?}",
                status.desktop_name, status.sandbox_isolated, status.qq_pids
            );
            print_windows(&windows);

            let target_hwnd = match remote_hwnd_arg {
                Some(raw) => Some(parse_hwnd(&raw)?),
                None => windows.first().map(|window| window.hwnd),
            };
            let Some(hwnd) = target_hwnd else {
                bail!("远程桌面守护进程未发现 QQNT 候选窗口，请确认 QQ 已启动并登录。");
            };

            let hwnd_str = hwnd.to_string();
            let depth_str = depth.to_string();
            let tree: UiNodeSnapshot = client
                .call_query(
                    ROUTE_API_TREE,
                    &[("hwnd", &hwnd_str), ("depth", &depth_str)],
                )
                .await?;
            finish_inspect_tree(hwnd, depth, &tree, remote_out).await
        },
        move |driver, status| async move {
            let windows = driver.list_windows().await?;
            println!(
                "Desktop: '{}' (sandbox_isolated={}), QQ PIDs: {:?}",
                status.desktop_name, status.sandbox_isolated, status.qq_pids
            );
            print_windows(&windows);

            let target_hwnd = match hwnd_arg {
                Some(raw) => Some(parse_hwnd(&raw)?),
                None => windows.first().map(|window| window.hwnd),
            };
            let Some(hwnd) = target_hwnd else {
                if status.sandbox_isolated {
                    print_sandbox_guidance(&status.desktop_name, ".\\inspect-qq.bat");
                    return Ok(());
                }
                bail!("未发现 QQNT 候选窗口，请确认 QQ 已启动并登录。");
            };

            let tree = driver.inspect_tree(Some(hwnd), depth).await?;
            finish_inspect_tree(hwnd, depth, &tree, out).await
        },
    )
    .await
}

/// 列出 QQ 左侧会话列表条目
pub async fn cmd_sessions(config: AdapterConfig, remote: Option<RemoteClient>) -> Result<()> {
    dispatch_remote_or_local(
        config,
        remote,
        ".\\inspect-qq.bat",
        |client| async move {
            let sessions: Vec<SessionSummary> =
                client.call(Method::GET, ROUTE_API_SESSIONS, None).await?;
            print_sessions(&sessions);
            Ok(())
        },
        |driver, status| async move {
            let windows = driver.list_windows().await?;
            if status.sandbox_isolated && windows.is_empty() {
                print_sandbox_guidance(&status.desktop_name, ".\\inspect-qq.bat");
                return Ok(());
            }
            let sessions = driver.list_sessions().await?;
            print_sessions(&sessions);
            Ok(())
        },
    )
    .await
}

fn format_burst_state(state: BurstModeState) -> &'static str {
    match state {
        BurstModeState::Normal => "normal",
        BurstModeState::BurstActive => "burst_active",
    }
}

fn print_scheduler_summary(cfg: &SchedulerConfig, tel: &SchedulerTelemetrySnapshot) {
    println!("================ Flood Scheduler 状态 ================");
    println!(
        "Burst State             : {}",
        format_burst_state(tel.burst_state)
    );
    println!("Burst Enter Count       : {}", tel.burst_enter_count);
    println!("Recent Window Arrivals  : {}", tel.recent_window_arrivals);
    println!(
        "Pending Events          : total={} (p0={}, p1={}, p2={})",
        tel.pending_total, tel.pending_p0, tel.pending_p1, tel.pending_p2
    );
    println!(
        "Dispatched Totals       : p0={}, p1={}, p2={} (received={})",
        tel.dispatched_p0_total,
        tel.dispatched_p1_total,
        tel.dispatched_p2_total,
        tel.received_total
    );
    println!("Selective Shed P2 Total : {}", tel.selective_shed_p2_total);
    println!("Expired Window Total    : {}", tel.expired_window_total);
    println!(
        "Outbound Deadline Abort : {}",
        tel.outbound_deadline_aborted_total
    );
    println!(
        "Multi-Group Switches    : {}",
        tel.multi_group_switches_total
    );
    println!("Active Monitored Groups : {}", tel.active_monitored_groups);
    println!(
        "Last Dispatched Group   : {}",
        tel.last_dispatched_group.as_deref().unwrap_or("<none>")
    );
    println!(
        "Response Windows (ms)   : critical={}, contextual={}, ordinary={}",
        cfg.critical_response_window_ms,
        cfg.contextual_response_window_ms,
        cfg.ordinary_response_window_ms
    );
    println!(
        "Burst Thresholds        : enabled={}, window_ms={}, enter={}, exit={}, single_poll={}",
        cfg.enabled,
        cfg.burst_window_ms,
        cfg.burst_enter_threshold,
        cfg.burst_exit_threshold,
        cfg.single_poll_burst_threshold
    );
    println!(
        "Queue Caps              : hard_shed_depth={}, max_pending={}, drop_expired={}",
        cfg.hard_shed_queue_depth, cfg.max_pending_events, cfg.drop_expired_events
    );
    println!("======================================================");
}

/// 查询并输出洪峰调度器遥测快照、响应窗口策略及突发限流状态
pub async fn cmd_scheduler(
    config: AdapterConfig,
    remote: Option<RemoteClient>,
    json_output: bool,
) -> Result<()> {
    let target_cfg = config.target.clone();
    dispatch_remote_or_local(
        config,
        remote,
        ".\\start-daemon.bat",
        move |client| async move {
            let payload: Value = client.call(Method::GET, ROUTE_API_SCHEDULER, None).await?;
            if json_output {
                println!("{}", serde_json::to_string_pretty(&payload)?);
                return Ok(());
            }
            let cfg: SchedulerConfig = serde_json::from_value(payload["config"].clone())
                .context("failed to decode /api/scheduler config")?;
            let tel: SchedulerTelemetrySnapshot =
                serde_json::from_value(payload["telemetry"].clone())
                    .context("failed to decode /api/scheduler telemetry")?;
            print_scheduler_summary(&cfg, &tel);
            Ok(())
        },
        move |driver, _status| async move {
            let cfg = driver.scheduler_config().await?;
            let tel = driver.scheduler_telemetry().await?;
            if json_output {
                let payload = json!({
                    "ok": true,
                    "config": cfg,
                    "telemetry": tel,
                    "monitored_groups": target_cfg.effective_monitored_groups(),
                    "allow_large_monitored_groups": target_cfg.allow_large_monitored_groups,
                    "strict_below_members": target_cfg.strict_below_members,
                });
                println!("{}", serde_json::to_string_pretty(&payload)?);
                return Ok(());
            }
            print_scheduler_summary(&cfg, &tel);
            Ok(())
        },
    )
    .await
}

fn strip_local_file_scheme(raw: &str) -> &str {
    let mut trimmed = raw.trim();
    if let Some(prefix) = trimmed.get(..7) {
        if prefix.eq_ignore_ascii_case("file://") {
            trimmed = &trimmed[7..];
            if cfg!(windows) && trimmed.starts_with('/') && trimmed.as_bytes().get(2) == Some(&b':')
            {
                trimmed = &trimmed[1..];
            }
        }
    }
    trimmed
}

fn is_path_inside_dir(candidate: &Path, root_dir: &Path) -> bool {
    let Ok(canon_file) = std::fs::canonicalize(candidate) else {
        return false;
    };
    let Ok(canon_root) = std::fs::canonicalize(root_dir) else {
        return false;
    };
    canon_file.starts_with(canon_root)
}

/// 为远程守护进程准备 `POST /api/ocr` 图片源：位于 `.runtime` 外的现有本地文件自动转为 `base64://`
pub fn resolve_remote_ocr_image_source(raw_image: &str, runtime_dir: &Path) -> Result<String> {
    let trimmed = raw_image.trim();
    if trimmed.is_empty() {
        bail!("--image 不能为空");
    }
    let has_passthrough_scheme = trimmed
        .get(..9)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("base64://"))
        || trimmed
            .get(..7)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("http://"))
        || trimmed
            .get(..8)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("https://"));
    if has_passthrough_scheme {
        return Ok(trimmed.to_string());
    }

    let local_path = Path::new(strip_local_file_scheme(trimmed));
    if !local_path.is_file() {
        return Ok(trimmed.to_string());
    }
    if runtime_dir.exists() && is_path_inside_dir(local_path, runtime_dir) {
        return Ok(trimmed.to_string());
    }

    let meta = std::fs::metadata(local_path)
        .with_context(|| format!("failed to inspect image file '{}'", local_path.display()))?;
    if meta.len() > MAX_REMOTE_OCR_RAW_BYTES {
        bail!(
            "image file '{}' ({} bytes) exceeds remote OCR base64 upload limit ({} bytes)",
            local_path.display(),
            meta.len(),
            MAX_REMOTE_OCR_RAW_BYTES
        );
    }
    let bytes = std::fs::read(local_path)
        .with_context(|| format!("failed to read image file '{}'", local_path.display()))?;
    Ok(format!("base64://{}", BASE64_STANDARD.encode(bytes)))
}

fn print_ocr_response(resp: &OcrImageResponse) {
    println!("language={} detections={}", resp.language, resp.texts.len());
    if resp.texts.is_empty() {
        println!("(no text detected)");
        return;
    }
    for (idx, item) in resp.texts.iter().enumerate() {
        let coords = item
            .coordinates
            .iter()
            .map(|pt| format!("({},{})", pt.x, pt.y))
            .collect::<Vec<_>>()
            .join(" -> ");
        println!(
            "[{}] (conf={}) {} | box=[{}]",
            idx + 1,
            item.confidence,
            item.text,
            coords
        );
    }
}

/// `ocr`：通过远程 `POST /api/ocr` 或本地驱动识别图片文本并格式化返回坐标
pub async fn cmd_ocr(
    config: AdapterConfig,
    remote: Option<RemoteClient>,
    image: String,
    lang: Option<String>,
    json_output: bool,
) -> Result<()> {
    let trimmed_image = image.trim().to_string();
    if trimmed_image.is_empty() {
        bail!("--image 不能为空");
    }
    let normalized_lang = lang
        .as_deref()
        .map(str::trim)
        .filter(|val| !val.is_empty())
        .map(ToOwned::to_owned);

    let remote_image = trimmed_image.clone();
    let remote_lang = normalized_lang.clone();
    dispatch_remote_or_local(
        config,
        remote,
        "run-with-win-desktop.bat",
        move |client| async move {
            let encoded_image =
                resolve_remote_ocr_image_source(&remote_image, Path::new(RUNTIME_DIR_NAME))?;
            let body = serde_json::to_value(OcrImageRequest {
                image: encoded_image,
                language: remote_lang,
            })?;
            let resp: OcrImageResponse = client
                .call(Method::POST, ROUTE_API_OCR, Some(&body))
                .await?;
            if json_output {
                println!("{}", serde_json::to_string_pretty(&resp)?);
            } else {
                print_ocr_response(&resp);
            }
            Ok(())
        },
        move |driver, _status| async move {
            let resp = driver
                .ocr_image(&trimmed_image, normalized_lang.as_deref())
                .await?;
            if json_output {
                println!("{}", serde_json::to_string_pretty(&resp)?);
            } else {
                print_ocr_response(&resp);
            }
            Ok(())
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 临时目录清理守卫，避免单元测试在磁盘留下临时文件
    struct TempDirCleanup {
        dir: PathBuf,
    }

    impl Drop for TempDirCleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn resolve_remote_ocr_image_source_encodes_external_files_and_keeps_runtime_or_base64() {
        let base_dir = std::env::temp_dir().join(format!(
            "qq_cli_ocr_test_{}_{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        let runtime_dir = base_dir.join(".runtime");
        let external_file = base_dir.join("outside.png");
        let internal_file = runtime_dir.join("media").join("cached.png");
        std::fs::create_dir_all(internal_file.parent().unwrap()).unwrap();
        std::fs::write(&external_file, b"EXT_PNG").unwrap();
        std::fs::write(&internal_file, b"INT_PNG").unwrap();
        let _cleanup = TempDirCleanup { dir: base_dir };

        let encoded =
            resolve_remote_ocr_image_source(external_file.to_str().unwrap(), &runtime_dir).unwrap();
        assert_eq!(
            encoded,
            format!("base64://{}", BASE64_STANDARD.encode(b"EXT_PNG"))
        );

        let kept_internal =
            resolve_remote_ocr_image_source(internal_file.to_str().unwrap(), &runtime_dir).unwrap();
        assert_eq!(kept_internal, internal_file.to_str().unwrap());

        assert_eq!(
            resolve_remote_ocr_image_source("base64://AAAA", &runtime_dir).unwrap(),
            "base64://AAAA"
        );
        assert!(resolve_remote_ocr_image_source("   ", &runtime_dir).is_err());
    }
}
