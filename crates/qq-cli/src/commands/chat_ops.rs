//! 群聊消息收发、撤回与目标群安全验证子命令模块：实现 `messages`、`send`、`recall` 与 `verify-group`

use anyhow::{bail, Result};
use chrono::Utc;
use common_types::{
    segments_to_plain_summary, AdapterConfig, AdapterError, AdapterStatus, BotDriver, ChatMessage,
    ChatTargetKind, DeleteMessageRequest, GroupTargetSpec, MessageHistoryFilter,
    OpenPrivateRequest, SendOutcome, SendRequest, StoredMessageRecord, WindowInfo,
};
use reqwest::Method;
use serde_json::{json, Value};

use crate::commands::{
    dispatch_remote_or_local, print_history_messages, print_messages, print_sandbox_guidance,
};
use crate::constants::{
    ROUTE_API_DELETE_MSG, ROUTE_API_ENSURE_QQ, ROUTE_API_MESSAGES, ROUTE_API_MESSAGES_HISTORY,
    ROUTE_API_OPEN_GROUP, ROUTE_API_OPEN_PRIVATE, ROUTE_API_SEND, ROUTE_API_SESSIONS_READ,
    ROUTE_API_WINDOWS, RUNTIME_DIR_NAME, RUNTIME_LATEST_REPORT_FILE,
};
use crate::http_client::RemoteClient;

pub use crate::commands::send_segments::{
    build_send_segments, MessagesCommandArgs, SendCommandArgs,
};

async fn write_report_json(value: &Value) -> Result<()> {
    tokio::fs::create_dir_all(RUNTIME_DIR_NAME).await?;
    let bytes = serde_json::to_vec_pretty(value)?;
    tokio::fs::write(RUNTIME_LATEST_REPORT_FILE, bytes).await?;
    Ok(())
}

fn print_verify_report(
    target_group: &str,
    below_members: u32,
    attached_status: &AdapterStatus,
    recent_messages: &[ChatMessage],
    send_outcome: Option<&SendOutcome>,
) {
    println!("================ QQNT V1 验证报告 ================");
    println!("当前桌面        : {}", attached_status.desktop_name);
    println!("目标群组        : {target_group} (安全阈值: < {below_members} 人)");
    println!("实际命中群组    : {:?}", attached_status.active_group);
    println!(
        "检测群成员数    : {:?}",
        attached_status.detected_member_count
    );
    println!(
        "解析 QQ 路径    : {}",
        attached_status
            .resolved_qq_path
            .as_deref()
            .unwrap_or("<none>")
    );
    println!(
        "安全熔断状态    : {}",
        attached_status.security_breaker_tripped
    );
    println!("捕获消息数量    : {}", recent_messages.len());
    if let Some(outcome) = send_outcome {
        println!(
            "发送验证结果    : ok={}, mode={}, mouse_touched={}, foreground_touched={}, verified_in_chat={}",
            outcome.ok,
            outcome.used_mode,
            outcome.mouse_touched,
            outcome.foreground_touched,
            outcome.verified_in_chat
        );
    }
    println!("详细报告已保存  : {RUNTIME_LATEST_REPORT_FILE}");
    println!("==================================================");
}

fn filter_local_history_records(
    target_config: &GroupTargetSpec,
    session_name: &str,
    records: &mut Vec<StoredMessageRecord>,
) -> Result<()> {
    if records.is_empty() {
        return Ok(());
    }
    records.retain(|rec| {
        rec.chat_type != ChatTargetKind::Private
            || target_config.is_private_friend_allowed(&rec.group_name)
    });
    if records.is_empty() {
        return Err(AdapterError::PrivateTargetNotAllowed {
            target: session_name.to_string(),
        }
        .into());
    }
    Ok(())
}

/// `messages` 离线历史深度分页、过滤条件与 JSON 输出控制参数
#[derive(Debug, Clone, Default)]
pub struct MessagesHistoryFilterArgs {
    pub before_id: Option<i64>,
    pub after_id: Option<i64>,
    pub sender: Option<String>,
    pub keyword: Option<String>,
    pub reverse_order: bool,
    pub json: bool,
}

fn non_empty_trimmed(raw: Option<&str>) -> Option<String> {
    raw.map(str::trim)
        .filter(|val| !val.is_empty())
        .map(ToOwned::to_owned)
}

/// 查询指定群、好友私聊或纯离线 SQLite 历史消息记录（默认参数入口，供单测复用）
#[cfg(test)]
pub async fn cmd_messages(
    config: AdapterConfig,
    remote: Option<RemoteClient>,
    args: MessagesCommandArgs,
) -> Result<()> {
    cmd_messages_ext(config, remote, args, MessagesHistoryFilterArgs::default()).await
}

/// 查询指定群、好友私聊或带深度分页过滤条件的离线 SQLite 历史消息记录
pub async fn cmd_messages_ext(
    config: AdapterConfig,
    remote: Option<RemoteClient>,
    args: MessagesCommandArgs,
    filter: MessagesHistoryFilterArgs,
) -> Result<()> {
    let explicit_group = args
        .group
        .as_deref()
        .map(str::trim)
        .filter(|val| !val.is_empty())
        .map(ToOwned::to_owned);
    if args.private && explicit_group.is_none() {
        bail!("私聊模式 (--private) 必须通过 --group 指定目标好友昵称");
    }
    let clean_sender = non_empty_trimmed(filter.sender.as_deref());
    let clean_keyword = non_empty_trimmed(filter.keyword.as_deref());
    let has_history_filters = filter.before_id.is_some()
        || filter.after_id.is_some()
        || clean_sender.is_some()
        || clean_keyword.is_some()
        || filter.reverse_order;
    let limit = args.limit;
    let is_history = args.history || has_history_filters;
    let is_private = args.private;
    let json_output = filter.json;
    let target_config = config.target.clone();
    let local_group = explicit_group
        .clone()
        .unwrap_or_else(|| config.target.group_name.clone());
    let remote_group = explicit_group;
    let remote_before = filter.before_id.filter(|&id| id > 0);
    let remote_after = filter.after_id.filter(|&id| id > 0);
    let remote_sender = clean_sender.clone();
    let remote_keyword = clean_keyword.clone();
    let remote_reverse = filter.reverse_order;

    dispatch_remote_or_local(
        config,
        remote,
        ".\\start-daemon.bat",
        move |client| async move {
            let limit_str = limit.to_string();
            if is_history {
                let mut query_pairs: Vec<(&str, String)> = Vec::new();
                if let Some(ref g) = remote_group {
                    query_pairs.push(("group", g.clone()));
                }
                if is_private {
                    query_pairs.push(("chat_type", "private".to_string()));
                }
                query_pairs.push(("limit", limit_str));
                if let Some(bid) = remote_before {
                    query_pairs.push(("before_id", bid.to_string()));
                }
                if let Some(aid) = remote_after {
                    query_pairs.push(("after_id", aid.to_string()));
                }
                if let Some(ref s) = remote_sender {
                    query_pairs.push(("sender", s.clone()));
                }
                if let Some(ref k) = remote_keyword {
                    query_pairs.push(("keyword", k.clone()));
                }
                if remote_reverse {
                    query_pairs.push(("reverse_order", "true".to_string()));
                }
                let refs: Vec<(&str, &str)> =
                    query_pairs.iter().map(|(k, v)| (*k, v.as_str())).collect();
                let records: Vec<StoredMessageRecord> =
                    client.call_query(ROUTE_API_MESSAGES_HISTORY, &refs).await?;
                if json_output {
                    println!("{}", serde_json::to_string_pretty(&records)?);
                } else {
                    print_history_messages(&records);
                }
                return Ok(());
            }
            if is_private {
                let target = remote_group.clone().unwrap_or_default();
                let open_payload = serde_json::to_value(&OpenPrivateRequest { target })?;
                let _: AdapterStatus = client
                    .call(Method::POST, ROUTE_API_OPEN_PRIVATE, Some(&open_payload))
                    .await?;
            }
            let messages: Vec<ChatMessage> = if let Some(ref g) = remote_group {
                client
                    .call_query(ROUTE_API_MESSAGES, &[("group", g), ("limit", &limit_str)])
                    .await?
            } else {
                client
                    .call_query(ROUTE_API_MESSAGES, &[("limit", &limit_str)])
                    .await?
            };
            if json_output {
                println!("{}", serde_json::to_string_pretty(&messages)?);
            } else {
                print_messages(&messages);
            }
            Ok(())
        },
        move |driver, _status| async move {
            if is_history {
                if is_private && !target_config.is_private_friend_allowed(&local_group) {
                    return Err(AdapterError::PrivateTargetNotAllowed {
                        target: local_group,
                    }
                    .into());
                }
                let chat_filter = is_private.then_some(ChatTargetKind::Private);
                let mut records = driver
                    .query_messages_history(MessageHistoryFilter {
                        session_name: local_group.clone(),
                        chat_type: chat_filter,
                        before_or_at_id: filter.before_id.filter(|&id| id > 0),
                        after_id: filter.after_id.filter(|&id| id > 0),
                        sender: clean_sender,
                        keyword: clean_keyword,
                        limit,
                        reverse_order: filter.reverse_order,
                    })
                    .await?;
                if !is_private {
                    filter_local_history_records(&target_config, &local_group, &mut records)?;
                }
                if json_output {
                    println!("{}", serde_json::to_string_pretty(&records)?);
                } else {
                    print_history_messages(&records);
                }
                return Ok(());
            }
            if is_private {
                driver.open_private(&local_group).await?;
            }
            let messages = driver.poll_messages(&local_group, limit).await?;
            if json_output {
                println!("{}", serde_json::to_string_pretty(&messages)?);
            } else {
                print_messages(&messages);
            }
            Ok(())
        },
    )
    .await
}

/// 附着目标群聊或白名单好友私聊并发送结构化复合消息
pub async fn cmd_send(
    config: AdapterConfig,
    remote: Option<RemoteClient>,
    args: SendCommandArgs,
) -> Result<()> {
    let segments = build_send_segments(&args)?;
    let summary_text = segments_to_plain_summary(&segments);
    let target_kind = if args.private {
        ChatTargetKind::Private
    } else {
        ChatTargetKind::Group
    };
    let target_name = if args.private {
        let explicit = args
            .group
            .as_deref()
            .map(str::trim)
            .filter(|val| !val.is_empty())
            .map(ToOwned::to_owned);
        match explicit {
            Some(name) => name,
            None => bail!("私聊模式 (--private) 必须通过 --group 指定目标好友昵称"),
        }
    } else {
        args.group
            .unwrap_or_else(|| config.target.group_name.clone())
    };

    let spec = GroupTargetSpec {
        group_name: target_name.clone(),
        ..config.target.clone()
    };
    let now_ms = Utc::now().timestamp_millis().max(0) as u64;
    let deadline_unix_ms = args.response_window_ms.map(|window| {
        if window == 0 {
            now_ms.saturating_sub(1)
        } else {
            now_ms.saturating_add(window)
        }
    });
    let send_request = SendRequest {
        group: target_name.clone(),
        text: summary_text,
        mode: args.mode,
        target_kind,
        segments,
        deadline_unix_ms,
    };
    let remote_spec = spec.clone();
    let remote_target_name = target_name.clone();
    let remote_send_request = send_request.clone();
    let remote_response_window_ms = args.response_window_ms;

    dispatch_remote_or_local(
        config,
        remote,
        ".\\start-daemon.bat",
        move |client| async move {
            if args.private {
                let open_val = serde_json::to_value(&OpenPrivateRequest {
                    target: remote_target_name,
                })?;
                let _: AdapterStatus = client
                    .call(Method::POST, ROUTE_API_OPEN_PRIVATE, Some(&open_val))
                    .await?;
            } else {
                let spec_val = serde_json::to_value(&remote_spec)?;
                let _: AdapterStatus = client
                    .call(Method::POST, ROUTE_API_OPEN_GROUP, Some(&spec_val))
                    .await?;
            }
            let mut req_val = serde_json::to_value(&remote_send_request)?;
            if let Some(window_ms) = remote_response_window_ms {
                if let Some(obj) = req_val.as_object_mut() {
                    obj.insert("response_window_ms".to_string(), json!(window_ms));
                }
            }
            let outcome: SendOutcome = client
                .call(Method::POST, ROUTE_API_SEND, Some(&req_val))
                .await?;
            println!("{}", serde_json::to_string_pretty(&outcome)?);
            Ok(())
        },
        move |driver, _status| async move {
            let outcome = if args.private {
                driver.open_private(&target_name).await?;
                driver.send_private_message(send_request).await?
            } else {
                driver.open_group(spec).await?;
                driver.send_group_message(send_request).await?
            };
            println!("{}", serde_json::to_string_pretty(&outcome)?);
            Ok(())
        },
    )
    .await
}

/// 撤回或删除指定 OneBot 消息 ID 对应的已发送消息记录
pub async fn cmd_recall(
    config: AdapterConfig,
    remote: Option<RemoteClient>,
    message_id: i64,
) -> Result<()> {
    if message_id <= 0 {
        bail!("--message-id 必须为正整数");
    }
    dispatch_remote_or_local(
        config,
        remote,
        ".\\start-daemon.bat",
        move |client| async move {
            let req_val = serde_json::to_value(&DeleteMessageRequest { message_id })?;
            let resp: Value = client
                .call(Method::POST, ROUTE_API_DELETE_MSG, Some(&req_val))
                .await?;
            println!("{}", serde_json::to_string_pretty(&resp)?);
            Ok(())
        },
        move |driver, _status| async move {
            driver.delete_message(message_id).await?;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "ok": true,
                    "message_id": message_id,
                }))?
            );
            Ok(())
        },
    )
    .await
}

/// 执行真实桌面目标群的安全人数校验、拉取历史及可选发信并保存验证报告
pub async fn cmd_verify_group(
    config: AdapterConfig,
    remote: Option<RemoteClient>,
    group: Option<String>,
    below_members: u32,
    send_text: Option<String>,
) -> Result<()> {
    let target_group = group.unwrap_or_else(|| config.target.group_name.clone());
    let spec = GroupTargetSpec {
        group_name: target_group.clone(),
        strict_below_members: Some(below_members),
        require_member_count_verified: false,
        ..Default::default()
    };
    let remote_group = target_group.clone();
    let remote_spec = spec.clone();
    let remote_send_text = send_text.clone();

    dispatch_remote_or_local(
        config,
        remote,
        ".\\verify-hoyo-group.bat",
        move |client| async move {
            let mut windows: Vec<WindowInfo> =
                client.call(Method::GET, ROUTE_API_WINDOWS, None).await?;
            if windows.is_empty() {
                windows = client.call(Method::POST, ROUTE_API_ENSURE_QQ, None).await?;
            }
            let spec_val = serde_json::to_value(&remote_spec)?;
            let attached_status: AdapterStatus = client
                .call(Method::POST, ROUTE_API_OPEN_GROUP, Some(&spec_val))
                .await?;
            let recent_messages: Vec<ChatMessage> = client
                .call_query(
                    ROUTE_API_MESSAGES,
                    &[("group", &remote_group), ("limit", "20")],
                )
                .await?;

            let send_outcome = match remote_send_text {
                Some(text) if !text.trim().is_empty() => {
                    let req_val = serde_json::to_value(SendRequest {
                        group: remote_group.clone(),
                        text,
                        mode: None,
                        ..Default::default()
                    })?;
                    Some(
                        client
                            .call::<SendOutcome>(Method::POST, ROUTE_API_SEND, Some(&req_val))
                            .await?,
                    )
                }
                _ => None,
            };

            let report = json!({
                "checked_at": Utc::now().to_rfc3339(),
                "status": "verified",
                "via_remote_daemon": true,
                "remote_url": client.base_url,
                "desktop_name": attached_status.desktop_name,
                "sandbox_isolated": attached_status.sandbox_isolated,
                "target_group": remote_group,
                "strict_below_members": below_members,
                "windows": windows,
                "adapter_status": attached_status,
                "recent_messages": recent_messages,
                "send_outcome": send_outcome,
                "passed": true,
            });
            write_report_json(&report).await?;
            print_verify_report(
                &remote_group,
                below_members,
                &attached_status,
                &recent_messages,
                send_outcome.as_ref(),
            );
            Ok(())
        },
        move |driver, status| async move {
            let windows = driver.list_windows().await?;
            if status.sandbox_isolated && windows.is_empty() {
                let report = json!({
                    "checked_at": Utc::now().to_rfc3339(),
                    "status": "sandbox_isolated",
                    "desktop_name": status.desktop_name,
                    "sandbox_isolated": true,
                    "target_group": target_group,
                    "strict_below_members": below_members,
                    "windows": [],
                    "passed": false,
                    "guidance": "当前运行在 exebox 隔离桌面，请在交互式桌面终端运行 .\\start-daemon.bat 或 .\\verify-hoyo-group.bat",
                });
                write_report_json(&report).await?;
                print_sandbox_guidance(&status.desktop_name, ".\\verify-hoyo-group.bat");
                println!("已写入状态报告: {RUNTIME_LATEST_REPORT_FILE}");
                return Ok(());
            }

            let attached_status = driver.open_group(spec).await?;
            let recent_messages = driver.poll_messages(&target_group, 20).await?;
            let send_outcome = match send_text {
                Some(text) if !text.trim().is_empty() => Some(
                    driver
                        .send_group_message(SendRequest {
                            group: target_group.clone(),
                            text,
                            mode: None,
                            ..Default::default()
                        })
                        .await?,
                ),
                _ => None,
            };

            let report = json!({
                "checked_at": Utc::now().to_rfc3339(),
                "status": "verified",
                "via_remote_daemon": false,
                "desktop_name": attached_status.desktop_name,
                "sandbox_isolated": attached_status.sandbox_isolated,
                "target_group": target_group,
                "strict_below_members": below_members,
                "windows": windows,
                "adapter_status": attached_status,
                "recent_messages": recent_messages,
                "send_outcome": send_outcome,
                "passed": true,
            });
            write_report_json(&report).await?;
            print_verify_report(
                &target_group,
                below_members,
                &attached_status,
                &recent_messages,
                send_outcome.as_ref(),
            );
            Ok(())
        },
    )
    .await
}

/// 将指定群聊或白名单私聊会话的未读角标标记为已读
pub async fn cmd_mark_read(
    config: AdapterConfig,
    remote: Option<RemoteClient>,
    group: Option<String>,
    private: Option<String>,
) -> Result<()> {
    if group.is_some() && private.is_some() {
        bail!("--group 与 --private 不能同时指定，请仅选择单一目标会话");
    }
    let clean_group = match group {
        Some(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                bail!("--group 目标群名不能为空");
            }
            Some(trimmed.to_string())
        }
        None => None,
    };
    let clean_private = match private {
        Some(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                bail!("--private 好友昵称不能为空");
            }
            Some(trimmed.to_string())
        }
        None => None,
    };

    let (target_name, target_kind) = match (clean_group, clean_private) {
        (None, Some(friend)) => (friend, ChatTargetKind::Private),
        (Some(grp), None) => (grp, ChatTargetKind::Group),
        (None, None) => (config.target.group_name.clone(), ChatTargetKind::Group),
        (Some(_), Some(_)) => unreachable!(),
    };
    let remote_target = target_name.clone();
    dispatch_remote_or_local(
        config,
        remote,
        ".\\start-daemon.bat",
        move |client| async move {
            let body = match target_kind {
                ChatTargetKind::Group => json!({
                    "group_name": remote_target,
                    "target_kind": "group",
                }),
                ChatTargetKind::Private => json!({
                    "nickname": remote_target,
                    "target_kind": "private",
                }),
            };
            let resp: Value = client
                .call(Method::POST, ROUTE_API_SESSIONS_READ, Some(&body))
                .await?;
            println!("{}", serde_json::to_string_pretty(&resp)?);
            Ok(())
        },
        move |driver, _status| async move {
            driver.mark_session_read(&target_name, target_kind).await?;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "ok": true,
                    "target": target_name,
                    "target_kind": target_kind,
                }))?
            );
            Ok(())
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use common_types::{MessageSegment, StoredMessageRecord};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    fn sample_stored_record(
        id: i64,
        group_name: &str,
        chat_type: ChatTargetKind,
    ) -> StoredMessageRecord {
        StoredMessageRecord {
            onebot_message_id: id,
            message_uid: format!("uid-{id}"),
            group_name: group_name.to_string(),
            group_id: if chat_type == ChatTargetKind::Group {
                555_005
            } else {
                0
            },
            user_id: 20_001,
            sender_name: "测试发送者".to_string(),
            raw_message: format!("msg-{id}"),
            chat_type,
            segments: vec![MessageSegment::Text {
                text: format!("msg-{id}"),
            }],
            is_self: false,
            timestamp_hint: None,
            captured_at_ms: 1_700_000_000_000 + id as u64,
        }
    }

    #[tokio::test]
    async fn cmd_messages_remote_private_history_sends_chat_type_and_propagates_403() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server_task = tokio::spawn(async move {
            let mut request_lines = Vec::new();
            for (status_line, body) in [
                ("HTTP/1.1 200 OK", "[]"),
                (
                    "HTTP/1.1 403 Forbidden",
                    r#"{"ok":false,"code":"PRIVATE_TARGET_NOT_ALLOWED","error":"private target denied"}"#,
                ),
            ] {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 4096];
                let n = stream.read(&mut buf).await.unwrap();
                let req = String::from_utf8_lossy(&buf[..n]);
                let first_line = req.lines().next().unwrap_or("").to_string();
                request_lines.push(first_line);
                let resp = format!(
                    "{status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(resp.as_bytes()).await.unwrap();
                stream.flush().await.unwrap();
            }
            request_lines
        });

        let client = RemoteClient::build(format!("http://{addr}"), None).unwrap();
        let ok_res = cmd_messages(
            AdapterConfig::default(),
            Some(client.clone()),
            MessagesCommandArgs {
                group: Some("测试好友".to_string()),
                private: true,
                history: true,
                limit: 15,
            },
        )
        .await;
        assert!(ok_res.is_ok(), "unexpected error: {ok_res:?}");

        let denied_res = cmd_messages(
            AdapterConfig::default(),
            Some(client),
            MessagesCommandArgs {
                group: Some("陌生人".to_string()),
                private: true,
                history: true,
                limit: 10,
            },
        )
        .await;
        assert!(denied_res.is_err(), "expected 403 error propagation");

        let lines = server_task.await.unwrap();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("/api/messages/history?"));
        assert!(lines[0].contains("chat_type=private"));
        assert!(lines[0].contains("limit=15"));
        assert!(lines[1].contains("chat_type=private"));
        assert!(lines[1].contains("limit=10"));
    }

    #[test]
    fn filter_local_history_records_enforces_private_whitelist() {
        let target_cfg = GroupTargetSpec {
            allowed_private_friends: vec!["白名单好友".to_string()],
            ..Default::default()
        };

        // 同名群与未白名单私聊混存时，仅保留群记录
        let mut mixed = vec![
            sample_stored_record(1, "同名会话", ChatTargetKind::Group),
            sample_stored_record(2, "同名会话", ChatTargetKind::Private),
        ];
        filter_local_history_records(&target_cfg, "同名会话", &mut mixed).unwrap();
        assert_eq!(mixed.len(), 1);
        assert_eq!(mixed[0].onebot_message_id, 1);

        // 仅含未白名单私聊记录时，过滤后应拒绝访问
        let mut only_denied_priv = vec![sample_stored_record(3, "陌生人", ChatTargetKind::Private)];
        let err = filter_local_history_records(&target_cfg, "陌生人", &mut only_denied_priv)
            .expect_err("expected private whitelist rejection");
        assert!(
            matches!(
                err.downcast_ref::<AdapterError>(),
                Some(AdapterError::PrivateTargetNotAllowed { .. })
            ),
            "unexpected error: {err:?}"
        );

        // 白名单私聊记录应正常放行
        let mut allowed_priv = vec![sample_stored_record(
            4,
            "白名单好友",
            ChatTargetKind::Private,
        )];
        filter_local_history_records(&target_cfg, "白名单好友", &mut allowed_priv).unwrap();
        assert_eq!(allowed_priv.len(), 1);
    }

    #[tokio::test]
    async fn cmd_messages_ext_forwards_deep_filters_and_blocks_local_private() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let n = stream.read(&mut buf).await.unwrap();
            let req = String::from_utf8_lossy(&buf[..n]);
            let first_line = req.lines().next().unwrap_or("").to_string();
            let body = "[]";
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(resp.as_bytes()).await.unwrap();
            stream.flush().await.unwrap();
            first_line
        });

        let client = RemoteClient::build(format!("http://{addr}"), None).unwrap();
        cmd_messages_ext(
            AdapterConfig::default(),
            Some(client),
            MessagesCommandArgs {
                group: Some("Demo测试群".to_string()),
                private: false,
                history: true,
                limit: 30,
            },
            MessagesHistoryFilterArgs {
                before_id: Some(10_060),
                after_id: Some(10_020),
                sender: Some("Alice".to_string()),
                keyword: Some("ping".to_string()),
                reverse_order: true,
                json: true,
            },
        )
        .await
        .unwrap();

        let line = server_task.await.unwrap();
        assert!(line.contains("before_id=10060"));
        assert!(line.contains("after_id=10020"));
        assert!(line.contains("sender=Alice"));
        assert!(line.contains("keyword=ping"));
        assert!(line.contains("reverse_order=true"));

        let mut local_cfg = AdapterConfig::default();
        local_cfg.storage.db_path = ":memory:".to_string();
        let err = cmd_messages_ext(
            local_cfg,
            None,
            MessagesCommandArgs {
                group: Some("未授权私聊".to_string()),
                private: true,
                history: true,
                limit: 10,
            },
            MessagesHistoryFilterArgs::default(),
        )
        .await
        .expect_err("expected local private whitelist error");
        assert!(matches!(
            err.downcast_ref::<AdapterError>(),
            Some(AdapterError::PrivateTargetNotAllowed { .. })
        ));
    }
}
