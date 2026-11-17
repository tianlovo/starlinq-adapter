//! 基于 WinEventHook 实时唤醒与低频保底巡检的后台消息监听协程

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use common_types::{
    AdapterError, AdapterEvent, ChatMessage, ChatTargetKind, MessageSegment, SendRequest,
};
use serde_json::json;
use tokio::sync::oneshot;
use tracing::{debug, warn};
use uia_core::WinEventWatcher;

use super::commands::{DriverCommand, PriorityCommandQueue};
use super::runtime_state::SharedInner;
use crate::constants::{
    AUTO_PING_REPLY_TEXT, POLLER_FALLBACK_MIN_MS, POLLER_FAST_TEST_MIN_MS,
    POLLER_FAST_TEST_THRESHOLD_MS, POLLER_SYNC_MESSAGE_LIMIT, POLLER_WINEVENT_SETTLE_MS,
};

/// 后台消息轮询协程生命周期守卫，确保无论正常退出还是熔断中止均复位运行标识
struct PollerRunGuard(Arc<SharedInner>);

impl Drop for PollerRunGuard {
    fn drop(&mut self) {
        self.0.poller_running.store(false, Ordering::Release);
    }
}

/// 启动后台实时监听协程：优先响应 WinEventHook 界面事件，并由低频保底定时器协同同步会话与群消息
pub(crate) async fn run_background_poller(
    shared: Arc<SharedInner>,
    cmd_queue: Arc<PriorityCommandQueue>,
) {
    shared.poller_running.store(true, Ordering::Release);
    let _run_guard = PollerRunGuard(Arc::clone(&shared));

    let watcher = match WinEventWatcher::with_skip_own_process(!shared.allow_mock()) {
        Ok(hook) => Some(hook),
        Err(err) => {
            warn!(
                error = %err,
                "初始化 WinEventWatcher 失败，后台消息监听将回退为纯保底定时轮询模式"
            );
            None
        }
    };

    let fallback_ms = compute_poller_fallback_ms(shared.config.interaction.poll_interval_ms);
    let mut ticker = tokio::time::interval(Duration::from_millis(fallback_ms));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_tracked_pids: Vec<u32> = Vec::new();

    while !shared.shutdown.load(Ordering::Acquire) {
        sync_watcher_tracked_pids(watcher.as_ref(), &shared, &mut last_tracked_pids);

        wait_next_trigger(&shared, watcher.as_ref(), &mut ticker).await;
        if shared.shutdown.load(Ordering::Acquire) {
            break;
        }

        if shared.state.lock().security_breaker_tripped {
            break;
        }

        let (tx, rx) = oneshot::channel();
        if cmd_queue
            .push(DriverCommand::SyncAndPoll {
                poll_limit: POLLER_SYNC_MESSAGE_LIMIT,
                resp: tx,
            })
            .is_err()
        {
            break;
        }

        let poll_res = match rx.await {
            Ok(res) => res,
            Err(_) => break,
        };

        sync_watcher_tracked_pids(watcher.as_ref(), &shared, &mut last_tracked_pids);

        match poll_res {
            Ok((_sessions, new_incoming)) => {
                let active_group = shared.state.lock().active_group.clone();
                process_polled_incoming(&shared, &cmd_queue, active_group.as_deref(), new_incoming)
                    .await;
            }
            Err(AdapterError::SecurityPromptDetected { .. }) => {
                warn!("检测到安全验证提示弹窗，后台消息轮询协程立即停止");
                break;
            }
            Err(err) => {
                debug!(error = %err, "后台周期同步消息失败");
            }
        }
    }
}

/// 根据配置轮询周期计算保底定时器间隔：生产模式放缓至秒级以上，高速集成测试保留快速响应
pub(crate) fn compute_poller_fallback_ms(configured_ms: u64) -> u64 {
    if configured_ms < POLLER_FAST_TEST_THRESHOLD_MS {
        configured_ms.max(POLLER_FAST_TEST_MIN_MS)
    } else {
        configured_ms.max(POLLER_FALLBACK_MIN_MS)
    }
}

/// 等待下一次触发：支持停机信号零延迟唤醒，否则优先响应 WinEvent 钩子或保底定时器
async fn wait_next_trigger(
    shared: &SharedInner,
    watcher: Option<&WinEventWatcher>,
    ticker: &mut tokio::time::Interval,
) {
    if let Some(hook) = watcher {
        tokio::select! {
            _ = shared.wait_shutdown() => {}
            _ = ticker.tick() => {}
            _ = hook.notified() => {
                tokio::select! {
                    _ = shared.wait_shutdown() => {}
                    _ = tokio::time::sleep(Duration::from_millis(POLLER_WINEVENT_SETTLE_MS)) => {}
                }
            }
        }
    } else {
        tokio::select! {
            _ = shared.wait_shutdown() => {}
            _ = ticker.tick() => {}
        }
    }
}

/// 将共享状态中最新扫描到的 QQ 进程 ID 集合同步给事件钩子过滤器
fn sync_watcher_tracked_pids(
    watcher: Option<&WinEventWatcher>,
    shared: &SharedInner,
    last_tracked_pids: &mut Vec<u32>,
) {
    let Some(hook) = watcher else {
        return;
    };
    let current_pids = if shared.allow_mock() {
        vec![std::process::id()]
    } else {
        shared.state.lock().qq_pids.clone()
    };
    if current_pids != *last_tracked_pids {
        hook.set_tracked_pids(&current_pids);
        *last_tracked_pids = current_pids;
    }
}

pub(super) fn emit_inbound_file_upload_notices(
    shared: &Arc<SharedInner>,
    msg: &ChatMessage,
    self_id: i64,
) {
    if msg.is_self {
        return;
    }
    let time_secs = i64::try_from(msg.captured_at_ms / 1000).unwrap_or(0);
    let notice_time = if time_secs > 0 {
        time_secs
    } else {
        chrono::Utc::now().timestamp()
    };
    let user_id = msg.user_id.unwrap_or(0);
    for seg in &msg.segments {
        let MessageSegment::File { file, name, size } = seg else {
            continue;
        };
        let file_name = name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| file.trim());
        let file_size = size.unwrap_or(0);
        let event_json = match msg.target_kind {
            ChatTargetKind::Group => {
                let file_id = if !file.trim().is_empty() {
                    file.as_str()
                } else {
                    msg.message_id.as_str()
                };
                json!({
                    "time": notice_time,
                    "self_id": self_id,
                    "post_type": "notice",
                    "notice_type": "group_upload",
                    "group_id": msg.group_id.unwrap_or(0),
                    "user_id": user_id,
                    "file": {
                        "id": file_id,
                        "name": file_name,
                        "size": file_size,
                        "busid": 0,
                    },
                })
            }
            ChatTargetKind::Private => json!({
                "time": notice_time,
                "self_id": self_id,
                "post_type": "notice",
                "notice_type": "offline_file",
                "user_id": user_id,
                "file": {
                    "name": file_name,
                    "size": file_size,
                    "url": "",
                },
            }),
        };
        shared.emit_event(AdapterEvent::Notice(event_json));
    }
}

/// 先经洪峰响应窗口选择器分级过滤再广播入站消息，并在开启测试自动回环开关时按节流冷却自动回复 `pong`
async fn process_polled_incoming(
    shared: &Arc<SharedInner>,
    cmd_queue: &Arc<PriorityCommandQueue>,
    active_group: Option<&str>,
    new_incoming: Vec<ChatMessage>,
) {
    let selected = shared.select_incoming_events(new_incoming);
    let self_id = shared.resolve_login_info().user_id;
    for msg in selected {
        let is_ping = msg.content.trim() == "ping";
        let target_kind = msg.target_kind;
        match target_kind {
            ChatTargetKind::Private => shared.emit_event(AdapterEvent::PrivateMessage(msg.clone())),
            ChatTargetKind::Group => shared.emit_event(AdapterEvent::GroupMessage(msg.clone())),
        }
        emit_inbound_file_upload_notices(shared, &msg, self_id);

        if !(shared.config.bot.auto_ping_reply && is_ping) {
            continue;
        }

        let target_group = if !msg.group_name.trim().is_empty() {
            msg.group_name.clone()
        } else if let Some(group) = active_group {
            group.to_string()
        } else {
            continue;
        };

        // 抢锁前预检私聊白名单与受监控小群人数护栏，防止未授权会话触发自动回环发送
        let (is_active_match, detected_count) = {
            let guard = shared.state.lock();
            let matches = guard
                .active_group
                .as_deref()
                .is_some_and(|act| crate::matcher::group_title_matches(act, &target_group));
            (matches, guard.detected_member_count)
        };
        let known_count = if is_active_match {
            detected_count
        } else {
            None
        }
        .or_else(|| crate::matcher::parse_member_count(&target_group));
        let mut guard_relaxed = shared.config.target.member_guard_for_group(&target_group);
        guard_relaxed.require_member_count_verified = false;
        let allowed_target = match target_kind {
            ChatTargetKind::Private => shared
                .config
                .target
                .is_private_friend_allowed(&target_group),
            ChatTargetKind::Group => {
                shared.config.target.is_monitored_group(&target_group)
                    && crate::matcher::passes_member_guard(&guard_relaxed, known_count).is_ok()
            }
        };
        if !allowed_target {
            continue;
        }

        let _send_guard = shared.send_gate.lock().await;
        if shared
            .check_outbound_deadline(&target_group, msg.response_deadline_ms)
            .is_err()
        {
            continue;
        }
        let wait_ms = shared.humanizer.lock().remaining_cooldown_ms();
        if wait_ms > 0 {
            tokio::select! {
                _ = shared.wait_shutdown() => break,
                _ = tokio::time::sleep(Duration::from_millis(wait_ms)) => {}
            }
        }
        if shared.shutdown.load(Ordering::Acquire) {
            break;
        }

        let pause = {
            let mut humanizer = shared.humanizer.lock();
            if humanizer.check_cooldown().is_err() {
                continue;
            }
            humanizer.mark_sent();
            humanizer.sample_pre_typing_pause()
        };
        tokio::select! {
            _ = shared.wait_shutdown() => break,
            _ = tokio::time::sleep(pause) => {}
        }

        let (send_tx, send_rx) = oneshot::channel();
        let reply_req = SendRequest {
            group: target_group,
            text: AUTO_PING_REPLY_TEXT.to_string(),
            mode: None,
            target_kind,
            segments: Vec::new(),
            deadline_unix_ms: msg.response_deadline_ms,
        };
        let cmd = match target_kind {
            ChatTargetKind::Private => DriverCommand::SendPrivateMessage {
                req: reply_req,
                resp: send_tx,
            },
            ChatTargetKind::Group => DriverCommand::SendGroupMessage {
                req: reply_req,
                resp: send_tx,
            },
        };
        if cmd_queue.push(cmd).is_ok() {
            if let Ok(Ok(outcome)) = send_rx.await {
                if outcome.ok && outcome.onebot_message_id.is_some() {
                    shared.emit_event(AdapterEvent::MessageSent(outcome));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use common_types::{AdapterConfig, StorageConfig};
    use storage::Storage;
    use tokio::sync::broadcast;

    #[test]
    fn process_polled_incoming_emits_group_upload_and_offline_file_for_inbound_only() {
        let cfg = AdapterConfig {
            storage: StorageConfig {
                db_path: ":memory:".to_string(),
            },
            ..Default::default()
        };
        let storage = Arc::new(Storage::open(&cfg.storage.db_path).expect("memory db"));
        let (tx, _) = broadcast::channel(16);
        let shared = Arc::new(SharedInner::new(cfg, storage, tx));
        let mut rx = shared.event_tx.subscribe();

        let now_ms = super::super::runtime_state::now_utc_ms();
        let cmd_queue = Arc::new(PriorityCommandQueue::new());
        let incoming = vec![
            ChatMessage {
                message_id: "uid-group-file".to_string(),
                group_name: "Demo测试群".to_string(),
                sender: "群友A".to_string(),
                content: "sample_live_verify.txt\n56.00 B 已发送".to_string(),
                is_self: false,
                captured_at_ms: now_ms,
                group_id: Some(987654321),
                user_id: Some(11223344),
                target_kind: ChatTargetKind::Group,
                segments: vec![MessageSegment::File {
                    file: "sample_live_verify.txt".to_string(),
                    name: Some("sample_live_verify.txt".to_string()),
                    size: Some(56),
                }],
                ..Default::default()
            },
            ChatMessage {
                message_id: "uid-priv-file".to_string(),
                group_name: "测试好友".to_string(),
                sender: "测试好友".to_string(),
                content: "priv_doc.bin\n1.00 KB 未下载".to_string(),
                is_self: false,
                captured_at_ms: now_ms,
                user_id: Some(55667788),
                target_kind: ChatTargetKind::Private,
                segments: vec![MessageSegment::File {
                    file: "priv_doc.bin".to_string(),
                    name: Some("priv_doc.bin".to_string()),
                    size: Some(1024),
                }],
                ..Default::default()
            },
            ChatMessage {
                message_id: "uid-self-file".to_string(),
                group_name: "Demo测试群".to_string(),
                sender: "BotSelf".to_string(),
                content: "self_out.txt\n10.00 B 已发送".to_string(),
                is_self: true,
                captured_at_ms: now_ms,
                group_id: Some(987654321),
                user_id: Some(10001),
                target_kind: ChatTargetKind::Group,
                segments: vec![MessageSegment::File {
                    file: "self_out.txt".to_string(),
                    name: Some("self_out.txt".to_string()),
                    size: Some(10),
                }],
                ..Default::default()
            },
        ];

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio rt");
        rt.block_on(process_polled_incoming(&shared, &cmd_queue, None, incoming));

        let mut events = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }

        let group_upload = events.iter().find_map(|ev| match ev {
            AdapterEvent::Notice(val) if val["notice_type"] == "group_upload" => Some(val),
            _ => None,
        });
        let upload_val = group_upload.expect("group_upload notice");
        assert_eq!(upload_val["group_id"], 987654321);
        assert_eq!(upload_val["user_id"], 11223344);
        assert_eq!(upload_val["file"]["id"], "sample_live_verify.txt");
        assert_eq!(upload_val["file"]["name"], "sample_live_verify.txt");
        assert_eq!(upload_val["file"]["size"], 56);
        assert_eq!(upload_val["file"]["busid"], 0);

        let offline_file = events.iter().find_map(|ev| match ev {
            AdapterEvent::Notice(val) if val["notice_type"] == "offline_file" => Some(val),
            _ => None,
        });
        let off_val = offline_file.expect("offline_file notice");
        assert_eq!(off_val["user_id"], 55667788);
        assert_eq!(off_val["file"]["name"], "priv_doc.bin");
        assert_eq!(off_val["file"]["size"], 1024);
        assert_eq!(off_val["file"]["url"], "");

        let notice_count = events
            .iter()
            .filter(|ev| matches!(ev, AdapterEvent::Notice(_)))
            .count();
        assert_eq!(
            notice_count, 2,
            "self file message must not emit upload notice"
        );
    }

    #[test]
    fn auto_ping_reply_skips_unwhitelisted_private_and_unauthorized_or_oversized_group() {
        let mut cfg = AdapterConfig {
            storage: StorageConfig {
                db_path: ":memory:".to_string(),
            },
            ..Default::default()
        };
        cfg.bot.auto_ping_reply = true;
        cfg.target.group_name = "受监控小群".to_string();
        cfg.target.strict_below_members = Some(10);
        cfg.target.allowed_private_friends = vec!["白名单好友".to_string()];

        let storage = Arc::new(Storage::open(&cfg.storage.db_path).expect("memory db"));
        let (tx, _) = broadcast::channel(16);
        let shared = Arc::new(SharedInner::new(cfg, storage, tx));
        let cmd_queue = Arc::new(PriorityCommandQueue::new());
        let now_ms = super::super::runtime_state::now_utc_ms();

        // 分别注入未在白名单私聊、未授权他群、人数超限大群的三条 ping 消息
        let incoming = vec![
            ChatMessage {
                message_id: "uid-ping-priv-deny".to_string(),
                group_name: "陌生好友".to_string(),
                sender: "陌生好友".to_string(),
                content: "ping".to_string(),
                is_self: false,
                captured_at_ms: now_ms,
                target_kind: ChatTargetKind::Private,
                ..Default::default()
            },
            ChatMessage {
                message_id: "uid-ping-group-unmon".to_string(),
                group_name: "未监控群 (5)".to_string(),
                sender: "群友A".to_string(),
                content: "ping".to_string(),
                is_self: false,
                captured_at_ms: now_ms,
                target_kind: ChatTargetKind::Group,
                ..Default::default()
            },
            ChatMessage {
                message_id: "uid-ping-group-big".to_string(),
                group_name: "受监控小群 (128)".to_string(),
                sender: "群友B".to_string(),
                content: "ping".to_string(),
                is_self: false,
                captured_at_ms: now_ms,
                target_kind: ChatTargetKind::Group,
                ..Default::default()
            },
        ];

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio rt");
        rt.block_on(process_polled_incoming(&shared, &cmd_queue, None, incoming));
        assert_eq!(
            cmd_queue.urgent_len(),
            0,
            "unauthorized or oversized ping must not enqueue auto reply"
        );
    }
}
