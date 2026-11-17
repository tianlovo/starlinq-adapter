//! CLI 子命令分发与输出格式化门面：统一远程/本地执行桥接及沙箱提示输出

pub mod chat_ops;
pub mod inspect_ops;
pub mod ops_ext;
pub mod send_segments;
pub mod storage_ops;

use std::future::Future;

use anyhow::Result;
use common_types::{
    AdapterConfig, AdapterError, AdapterStatus, BotDriver, ChatMessage, SessionSummary,
    StoredMessageRecord, WindowInfo,
};
use qqnt_driver::QqntDriver;

use crate::http_client::RemoteClient;

/// 根据是否探测到桌面守护进程自动选择远程 HTTP 桥接或本地 UIA 驱动执行，并统一处理沙箱桌面隔离提示
pub async fn dispatch_remote_or_local<F, FutR, G, FutL>(
    config: AdapterConfig,
    remote: Option<RemoteClient>,
    sandbox_bat_hint: &'static str,
    on_remote: F,
    on_local: G,
) -> Result<()>
where
    F: FnOnce(RemoteClient) -> FutR,
    FutR: Future<Output = Result<()>>,
    G: FnOnce(QqntDriver, AdapterStatus) -> FutL,
    FutL: Future<Output = Result<()>>,
{
    if let Some(client) = remote {
        return on_remote(client).await;
    }

    let driver = QqntDriver::new(config)?;
    let status = driver.status().await?;
    match on_local(driver, status.clone()).await {
        Ok(()) => Ok(()),
        Err(err) => {
            if status.sandbox_isolated
                && err
                    .downcast_ref::<AdapterError>()
                    .is_some_and(|e| matches!(e, AdapterError::WindowNotFound(_)))
            {
                print_sandbox_guidance(&status.desktop_name, sandbox_bat_hint);
                return Ok(());
            }
            Err(err)
        }
    }
}

/// 在沙箱桌面隔离环境中打印中文桌面守护进程桥接指引
pub fn print_sandbox_guidance(desktop_name: &str, primary_bat: &str) {
    println!("============================================================");
    println!("[沙箱桌面隔离提示] 当前进程运行在隔离桌面: '{desktop_name}'");
    println!("在此隔离桌面下无法直接枚举用户主桌面 (Default) 中运行的 QQNT 窗口。");
    println!(
        "这是 Windows 桌面隔离机制导致的预期现象（本适配器严格遵守隔离边界，不做跨桌面逃逸）。"
    );
    println!();
    println!("推荐方案（桌面守护进程桥接）：");
    println!("  1. 请在交互式 Windows 桌面终端中运行: .\\start-daemon.bat");
    println!("  2. 守护进程启动后，本终端的所有 CLI 命令将自动通过 HTTP (127.0.0.1:18770) 桥接至桌面守护进程。");
    println!();
    println!("也可在交互式 Windows 桌面终端中直接运行:");
    println!("  {primary_bat}");
    println!("  (常用桌面脚本: .\\start-daemon.bat | .\\verify-hoyo-group.bat | .\\inspect-qq.bat | .\\stop-daemon.bat)");
    println!("============================================================");
}

/// 格式化打印适配器状态快照
pub fn print_status(status: &AdapterStatus) {
    println!("Desktop         : {}", status.desktop_name);
    println!("Sandbox Isolated: {}", status.sandbox_isolated);
    println!("QQ PIDs         : {:?}", status.qq_pids);
    println!(
        "Resolved QQ Path: {}",
        status.resolved_qq_path.as_deref().unwrap_or("<none>")
    );
    println!(
        "Attached Window : {}",
        status
            .attached_window
            .as_ref()
            .map(|window| format!(
                "0x{:X} ({}, '{}')",
                window.hwnd, window.class_name, window.title
            ))
            .unwrap_or_else(|| "<none>".to_string())
    );
    println!(
        "Active Group    : {}",
        status.active_group.as_deref().unwrap_or("<none>")
    );
    let active_group_id_text = match (status.active_group_id, status.active_real_group_id) {
        (Some(gid), Some(real_gid)) => format!("{gid} (real_group_id={real_gid})"),
        (Some(gid), None) => format!("{gid} (synthetic)"),
        (None, Some(real_gid)) => format!("real_group_id={real_gid}"),
        (None, None) => "<none>".to_string(),
    };
    println!("Active Group ID : {active_group_id_text}");
    println!("Detected Members: {:?}", status.detected_member_count);
    println!("Security Breaker: {}", status.security_breaker_tripped);
    if let Some(ref reason) = status.security_breaker_reason {
        println!("Breaker Reason  : {reason}");
    }
    println!("Input Mode      : {}", status.input_mode);
    println!(
        "Counters        : observed={}, sent={}",
        status.messages_observed, status.messages_sent
    );
    println!(
        "Scheduler       : burst_mode_active={}, pending={}",
        status.burst_mode_active, status.scheduler_pending
    );
}

/// 格式化打印枚举到的候选 QQ 窗口列表
pub fn print_windows(windows: &[WindowInfo]) {
    println!("Candidate windows ({}):", windows.len());
    for window in windows {
        println!(
            "  hwnd=0x{:X} ({}) pid={} proc={} class='{}' title='{}' visible={} iconic={} standalone={} rect={:?}",
            window.hwnd,
            window.hwnd,
            window.pid,
            window.process_name,
            window.class_name,
            window.title,
            window.is_visible,
            window.is_iconic,
            window.is_standalone_chat,
            window.rect
        );
    }
}

/// 格式化打印左侧会话列表摘要
pub fn print_sessions(sessions: &[SessionSummary]) {
    println!("Sessions ({}):", sessions.len());
    for (idx, session) in sessions.iter().enumerate() {
        println!(
            "  [{idx}] title='{}' members={:?} unread={:?} preview={:?} rect={:?}",
            session.title,
            session.member_count_hint,
            session.unread_count,
            session.preview,
            session.rect
        );
    }
}

fn print_message_line(
    onebot_id: Option<i64>,
    is_self: bool,
    sender: &str,
    timestamp_hint: Option<&str>,
    content: &str,
) {
    let role = if is_self { "SELF" } else { "PEER" };
    let timestamp_label = timestamp_hint.unwrap_or("-");
    match onebot_id {
        Some(id) => println!("  [#{id} {role}] {sender} [{timestamp_label}]: {content}"),
        None => println!("  [{role}] {sender} [{timestamp_label}]: {content}"),
    }
}

/// 格式化打印聊天记录列表
pub fn print_messages(messages: &[ChatMessage]) {
    println!("Recent messages ({}):", messages.len());
    for message in messages {
        print_message_line(
            message.onebot_message_id,
            message.is_self,
            &message.sender,
            message.timestamp_hint.as_deref(),
            &message.content,
        );
    }
}

/// 格式化打印 SQLite 离线历史聊天记录列表
pub fn print_history_messages(records: &[StoredMessageRecord]) {
    println!("History messages ({}):", records.len());
    for record in records {
        print_message_line(
            Some(record.onebot_message_id),
            record.is_self,
            &record.sender_name,
            record.timestamp_hint.as_deref(),
            &record.raw_message,
        );
    }
}
