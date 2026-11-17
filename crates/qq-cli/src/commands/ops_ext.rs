//! Wave v6 运维、单条转发、转发明细回查、审计流水与熔断恢复子命令模块

use anyhow::{bail, Result};
use chrono::Utc;
use common_types::constants::{
    ROUTE_API_AUDIT_EVENTS, ROUTE_API_CACHE_CLEAN, ROUTE_API_FORWARD_MSG, ROUTE_API_SECURITY_RESET,
};
use common_types::{
    AdapterConfig, AdapterError, AdapterStatus, AuditEventRecord, BotDriver, ChatTargetKind,
    SendOutcome, SingleForwardRequest,
};
use reqwest::Method;
use serde_json::{json, Value};

use crate::commands::{dispatch_remote_or_local, print_status};
use crate::http_client::RemoteClient;

fn resolve_forward_target(
    config: &AdapterConfig,
    group: Option<String>,
    private: Option<String>,
) -> Result<(String, ChatTargetKind)> {
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
    Ok(match (clean_group, clean_private) {
        (None, Some(friend)) => (friend, ChatTargetKind::Private),
        (Some(grp), None) => (grp, ChatTargetKind::Group),
        (None, None) => (config.target.group_name.clone(), ChatTargetKind::Group),
        (Some(_), Some(_)) => unreachable!(),
    })
}

fn print_forward_nodes(id_label: &str, nodes: &Value) {
    let items = nodes.as_array().map(Vec::as_slice).unwrap_or(&[]);
    println!("Forward nodes for '{id_label}' ({}):", items.len());
    for (idx, node) in items.iter().enumerate() {
        let data = node.get("data").unwrap_or(node);
        let sender = data
            .get("nickname")
            .or_else(|| data.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("成员");
        let content = data
            .get("content")
            .map(|v| match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .unwrap_or_else(|| {
                data.get("id")
                    .map(|id| format!("[引用消息 id={id}]"))
                    .unwrap_or_default()
            });
        println!("  [{idx}] {sender}: {content}");
    }
}

fn print_audit_events(events: &[AuditEventRecord]) {
    println!("Audit events ({}):", events.len());
    for ev in events {
        println!(
            "  [#{} ts={}] {}: {}",
            ev.id, ev.created_at_ms, ev.event_kind, ev.detail_json
        );
    }
}

fn print_clean_cache_summary(summary: &Value) {
    println!(
        "Cache cleaned: cleaned={}, removed_files={}, media_removed={}, staging_removed={}, freed_bytes={}",
        summary["cleaned"].as_bool().unwrap_or(false),
        summary["removed_files"].as_u64().unwrap_or(0),
        summary["media_removed"].as_u64().unwrap_or(0),
        summary["staging_removed"].as_u64().unwrap_or(0),
        summary["freed_bytes"].as_u64().unwrap_or(0),
    );
}

/// 将单条已缓存消息原生转发至指定群聊或白名单好友私聊
pub async fn cmd_forward(
    config: AdapterConfig,
    remote: Option<RemoteClient>,
    message_id: i64,
    group: Option<String>,
    private: Option<String>,
    response_window_ms: Option<u64>,
) -> Result<()> {
    if message_id <= 0 {
        bail!("--message-id 必须为正整数");
    }
    let (target_name, target_kind) = resolve_forward_target(&config, group, private)?;
    let target_cfg = config.target.clone();
    let remote_target = target_name.clone();

    dispatch_remote_or_local(
        config,
        remote,
        ".\\start-daemon.bat",
        move |client| async move {
            let mut body = json!({
                "message_id": message_id,
                "target": remote_target,
                "target_kind": target_kind,
            });
            if let Some(window_ms) = response_window_ms {
                body["response_window_ms"] = json!(window_ms);
            }
            let outcome: SendOutcome = client
                .call(Method::POST, ROUTE_API_FORWARD_MSG, Some(&body))
                .await?;
            println!("{}", serde_json::to_string_pretty(&outcome)?);
            Ok(())
        },
        move |driver, _status| async move {
            if target_kind == ChatTargetKind::Private
                && !target_cfg.is_private_friend_allowed(&target_name)
            {
                return Err(AdapterError::PrivateTargetNotAllowed {
                    target: target_name,
                }
                .into());
            }
            if let Some(src) = driver.get_message(message_id).await? {
                if src.chat_type == ChatTargetKind::Private
                    && !target_cfg.is_private_friend_allowed(&src.group_name)
                {
                    return Err(AdapterError::PrivateTargetNotAllowed {
                        target: src.group_name,
                    }
                    .into());
                }
            }
            let now_ms = Utc::now().timestamp_millis().max(0) as u64;
            let deadline_unix_ms = response_window_ms.map(|window| {
                if window == 0 {
                    now_ms.saturating_sub(1)
                } else {
                    now_ms.saturating_add(window)
                }
            });
            let outcome = driver
                .forward_single_message(SingleForwardRequest {
                    message_id,
                    target: target_name,
                    target_kind,
                    deadline_unix_ms,
                })
                .await?;
            println!("{}", serde_json::to_string_pretty(&outcome)?);
            Ok(())
        },
    )
    .await
}

/// 按 `fwd-*` 编号或消息数字 ID 查询持久化合并转发节点明细
pub async fn cmd_forward_get(
    config: AdapterConfig,
    remote: Option<RemoteClient>,
    id: String,
    json_output: bool,
) -> Result<()> {
    let clean_id = id.trim().to_string();
    if clean_id.is_empty() {
        bail!("--id 不能为空");
    }
    let target_cfg = config.target.clone();
    let remote_id = clean_id.clone();

    dispatch_remote_or_local(
        config,
        remote,
        ".\\start-daemon.bat",
        move |client| async move {
            let envelope: Value = client
                .call_query("/onebot/v11/get_forward_msg", &[("id", &remote_id)])
                .await?;
            let data = envelope.get("data").cloned().unwrap_or(envelope);
            if json_output {
                println!("{}", serde_json::to_string_pretty(&data)?);
            } else {
                print_forward_nodes(&remote_id, &data["messages"]);
            }
            Ok(())
        },
        move |driver, _status| async move {
            let mut raw_json_opt = driver.get_forward_nodes(&clean_id).await?;
            if raw_json_opt.is_none() {
                if let Ok(msg_id) = clean_id.parse::<i64>() {
                    if let Some(record) = driver.get_message(msg_id).await? {
                        if record.chat_type == ChatTargetKind::Private
                            && !target_cfg.is_private_friend_allowed(&record.group_name)
                        {
                            return Err(AdapterError::PrivateTargetNotAllowed {
                                target: record.group_name,
                            }
                            .into());
                        }
                        if let Some(fwd_id) =
                            adapter_server::onebot::extended_handlers::extract_forward_id_from_record(
                                &record,
                            )
                        {
                            raw_json_opt = driver.get_forward_nodes(&fwd_id).await?;
                        }
                    }
                }
            }
            let raw_json = raw_json_opt.ok_or_else(|| {
                AdapterError::InputRejected(format!(
                    "forward messages not found for id={clean_id}"
                ))
            })?;
            let nodes: Value = serde_json::from_str(&raw_json)?;
            let data = json!({ "messages": nodes });
            if json_output {
                println!("{}", serde_json::to_string_pretty(&data)?);
            } else {
                print_forward_nodes(&clean_id, &data["messages"]);
            }
            Ok(())
        },
    )
    .await
}

/// 检索脱敏后的持久化安全审计事件流水
pub async fn cmd_audit(
    config: AdapterConfig,
    remote: Option<RemoteClient>,
    kind: Option<String>,
    limit: usize,
    json_output: bool,
) -> Result<()> {
    let clamped_limit = limit.clamp(1, 200);
    let clean_kind = kind
        .as_deref()
        .map(str::trim)
        .filter(|val| !val.is_empty())
        .map(ToOwned::to_owned);
    let target_cfg = config.target.clone();
    let remote_kind = clean_kind.clone();

    dispatch_remote_or_local(
        config,
        remote,
        ".\\start-daemon.bat",
        move |client| async move {
            let limit_str = clamped_limit.to_string();
            let mut pairs = vec![("limit", limit_str.as_str())];
            if let Some(ref kind_val) = remote_kind {
                pairs.push(("event_kind", kind_val.as_str()));
            }
            let events: Vec<AuditEventRecord> =
                client.call_query(ROUTE_API_AUDIT_EVENTS, &pairs).await?;
            if json_output {
                println!("{}", serde_json::to_string_pretty(&events)?);
            } else {
                print_audit_events(&events);
            }
            Ok(())
        },
        move |driver, _status| async move {
            let mut events = driver.list_audit_events(clean_kind.as_deref(), 200).await?;
            events.retain(|rec| {
                adapter_server::onebot::wave6_handlers::is_audit_record_allowed(&target_cfg, rec)
            });
            events.truncate(clamped_limit);
            if json_output {
                println!("{}", serde_json::to_string_pretty(&events)?);
            } else {
                print_audit_events(&events);
            }
            Ok(())
        },
    )
    .await
}

/// 清理 `.runtime/media` 与 `.runtime/staging_uploads` 运行时缓存文件
pub async fn cmd_clean_cache(
    config: AdapterConfig,
    remote: Option<RemoteClient>,
    json_output: bool,
) -> Result<()> {
    dispatch_remote_or_local(
        config,
        remote,
        ".\\start-daemon.bat",
        move |client| async move {
            let summary: Value = client
                .call(Method::POST, ROUTE_API_CACHE_CLEAN, None)
                .await?;
            if json_output {
                println!("{}", serde_json::to_string_pretty(&summary)?);
            } else {
                print_clean_cache_summary(&summary);
            }
            Ok(())
        },
        move |_driver, _status| async move {
            let summary =
                adapter_server::onebot::extended_handlers::clean_runtime_cache_dirs().await;
            if json_output {
                println!("{}", serde_json::to_string_pretty(&summary)?);
            } else {
                print_clean_cache_summary(&summary);
            }
            Ok(())
        },
    )
    .await
}

/// 核验安全弹窗已关闭后手动重置桌面风控熔断器
pub async fn cmd_reset_breaker(
    config: AdapterConfig,
    remote: Option<RemoteClient>,
    json_output: bool,
) -> Result<()> {
    dispatch_remote_or_local(
        config,
        remote,
        ".\\start-daemon.bat",
        move |client| async move {
            let status: AdapterStatus = client
                .call(Method::POST, ROUTE_API_SECURITY_RESET, None)
                .await?;
            if json_output {
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                print_status(&status);
            }
            Ok(())
        },
        move |driver, _status| async move {
            let status = driver.reset_security_breaker().await?;
            if json_output {
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                print_status(&status);
            }
            Ok(())
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use common_types::GroupTargetSpec;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    #[tokio::test]
    async fn cmd_forward_and_audit_validate_args_and_filter_local_audit_events() {
        let cfg = AdapterConfig::default();
        assert!(cmd_forward(cfg.clone(), None, 0, None, None, None)
            .await
            .is_err());
        assert!(cmd_forward(
            cfg.clone(),
            None,
            10_001,
            Some("Demo测试群".to_string()),
            Some("测试好友".to_string()),
            None,
        )
        .await
        .is_err());

        // 本地直连非白名单私聊转发必须在进入 UIA 前即被 PrivateTargetNotAllowed 拒绝
        let denied = cmd_forward(
            cfg.clone(),
            None,
            10_001,
            None,
            Some("未授权陌生人".to_string()),
            None,
        )
        .await
        .expect_err("expected private whitelist rejection");
        assert!(matches!(
            denied.downcast_ref::<AdapterError>(),
            Some(AdapterError::PrivateTargetNotAllowed { .. })
        ));

        // 校验本地内存存储审计过滤与远程 GET /api/audit-events 参数拼接
        let mut local_cfg = cfg;
        local_cfg.storage.db_path = ":memory:".to_string();
        local_cfg.target.allowed_private_friends = vec!["测试好友".to_string()];
        assert!(cmd_audit(local_cfg.clone(), None, None, 10, true)
            .await
            .is_ok());

        let allowed_rec = AuditEventRecord {
            id: 1,
            event_kind: "security_breaker_reset".to_string(),
            detail_json: r#"{"reset":true}"#.to_string(),
            created_at_ms: 1_700_000_000_000,
        };
        let leaked_rec = AuditEventRecord {
            id: 2,
            event_kind: "private_whitelist_abort".to_string(),
            detail_json: r#"{"target":"秘密陌生人"}"#.to_string(),
            created_at_ms: 1_700_000_000_001,
        };
        assert!(
            adapter_server::onebot::wave6_handlers::is_audit_record_allowed(
                &local_cfg.target,
                &allowed_rec
            )
        );
        assert!(
            !adapter_server::onebot::wave6_handlers::is_audit_record_allowed(
                &local_cfg.target,
                &leaked_rec
            )
        );

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
        cmd_audit(
            AdapterConfig {
                target: GroupTargetSpec::default(),
                ..Default::default()
            },
            Some(client),
            Some("security_breaker_reset".to_string()),
            25,
            true,
        )
        .await
        .unwrap();
        let req_line = server_task.await.unwrap();
        assert!(req_line.contains("/api/audit-events?"));
        assert!(req_line.contains("limit=25"));
        assert!(req_line.contains("event_kind=security_breaker_reset"));
    }
}
