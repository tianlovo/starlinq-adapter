//! `qq-uia-adapter` 命令行工具主入口：装配参数解析、子命令路由分发及核心辅助单元测试

mod cli_args;
mod commands;
mod constants;
mod daemon_ops;
mod http_client;
mod self_test;
mod ws_client;

use anyhow::Result;
use clap::Parser;
use tracing_subscriber::EnvFilter;

use crate::cli_args::{Cli, Commands};
use crate::commands::{chat_ops, inspect_ops, ops_ext, storage_ops};
use crate::daemon_ops::{
    cmd_restart, cmd_serve, cmd_start_daemon, load_config, try_connect_remote,
};

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,tower_http=info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();

    if matches!(cli.command, Commands::SelfTest) {
        return self_test::run_self_test().await;
    }

    let config = load_config(&cli.config)?;
    match cli.command {
        Commands::Serve {
            port,
            host,
            auto_ping,
        } => cmd_serve(config, port, host, auto_ping).await,
        Commands::Restart => cmd_restart(config, cli.remote.as_deref(), cli.local_only).await,
        Commands::StartDaemon => cmd_start_daemon(config, &cli.config).await,
        other => {
            let remote = try_connect_remote(cli.remote.as_deref(), cli.local_only, &config).await?;
            match other {
                Commands::Status => inspect_ops::cmd_status(config, remote).await,
                Commands::EnsureQq { force_wake } => {
                    inspect_ops::cmd_ensure_qq(config, remote, force_wake).await
                }
                Commands::Diag { hwnd } => inspect_ops::cmd_diag(config, remote, hwnd).await,
                Commands::Inspect { hwnd, depth, out } => {
                    inspect_ops::cmd_inspect(config, remote, hwnd, depth, out).await
                }
                Commands::Sessions => inspect_ops::cmd_sessions(config, remote).await,
                Commands::Messages {
                    group,
                    private,
                    history,
                    limit,
                    before_id,
                    after_id,
                    sender,
                    keyword,
                    reverse_order,
                    json,
                } => {
                    chat_ops::cmd_messages_ext(
                        config,
                        remote,
                        chat_ops::MessagesCommandArgs {
                            group,
                            private,
                            history,
                            limit,
                        },
                        chat_ops::MessagesHistoryFilterArgs {
                            before_id,
                            after_id,
                            sender,
                            keyword,
                            reverse_order,
                            json,
                        },
                    )
                    .await
                }
                Commands::Send {
                    group,
                    private,
                    text,
                    image,
                    file,
                    video,
                    face,
                    at,
                    reply,
                    mode,
                    response_window_ms,
                } => {
                    chat_ops::cmd_send(
                        config,
                        remote,
                        chat_ops::SendCommandArgs {
                            group,
                            private,
                            text,
                            image,
                            file,
                            video,
                            face,
                            at,
                            reply,
                            mode,
                            response_window_ms,
                        },
                    )
                    .await
                }
                Commands::Scheduler { json } => {
                    inspect_ops::cmd_scheduler(config, remote, json).await
                }
                Commands::MarkRead { group, private } => {
                    chat_ops::cmd_mark_read(config, remote, group, private).await
                }
                Commands::Recall { message_id } => {
                    chat_ops::cmd_recall(config, remote, message_id).await
                }
                Commands::VerifyGroup {
                    group,
                    below_members,
                    send_text,
                } => {
                    chat_ops::cmd_verify_group(config, remote, group, below_members, send_text)
                        .await
                }
                Commands::Storage => storage_ops::cmd_storage(config, remote).await,
                Commands::Friends { no_cache } => {
                    storage_ops::cmd_friends(config, remote, no_cache).await
                }
                Commands::Groups { no_cache } => {
                    storage_ops::cmd_groups(config, remote, no_cache).await
                }
                Commands::GroupInfo {
                    group,
                    group_id,
                    no_cache,
                } => storage_ops::cmd_group_info(config, remote, group, group_id, no_cache).await,
                Commands::GroupMembers {
                    group,
                    group_id,
                    no_cache,
                } => {
                    storage_ops::cmd_group_members(config, remote, group, group_id, no_cache).await
                }
                Commands::Ocr { image, lang, json } => {
                    inspect_ops::cmd_ocr(config, remote, image, lang, json).await
                }
                Commands::Forward {
                    message_id,
                    group,
                    private,
                    response_window_ms,
                } => {
                    ops_ext::cmd_forward(
                        config,
                        remote,
                        message_id,
                        group,
                        private,
                        response_window_ms,
                    )
                    .await
                }
                Commands::ForwardGet { id, json } => {
                    ops_ext::cmd_forward_get(config, remote, id, json).await
                }
                Commands::Audit { kind, limit, json } => {
                    ops_ext::cmd_audit(config, remote, kind, limit, json).await
                }
                Commands::CleanCache { json } => {
                    ops_ext::cmd_clean_cache(config, remote, json).await
                }
                Commands::ResetBreaker { json } => {
                    ops_ext::cmd_reset_breaker(config, remote, json).await
                }
                Commands::Serve { .. }
                | Commands::Restart
                | Commands::StartDaemon
                | Commands::SelfTest => unreachable!(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::process::Stdio;

    use clap::Parser;

    use crate::cli_args::{parse_hwnd, Cli};
    use crate::daemon_ops::{
        build_daemon_serve_command, decide_start_daemon_action, parse_health_pid,
        resolve_daemon_config_arg, resolve_repo_root, should_wait_for_restart,
        strip_windows_verbatim, SandboxDaemonAction,
    };
    use crate::http_client::{format_base_url, validate_http_base_url, HttpResponsePayload};

    /// 测试结束时自动销毁临时工作目录的 RAII 守卫
    struct TempDirGuard(PathBuf);

    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn parse_hwnd_hex_and_decimal() {
        assert_eq!(parse_hwnd("0x1A2B").unwrap(), 0x1A2B);
        assert_eq!(parse_hwnd("0Xff").unwrap(), 255);
        assert_eq!(parse_hwnd("12345").unwrap(), 12345);
        assert!(parse_hwnd("not_a_number").is_err());
    }

    #[test]
    fn cli_subcommands_and_global_flags_parse_cleanly() {
        assert!(Cli::try_parse_from(["qq-uia-adapter", "self-test"]).is_ok());
        assert!(Cli::try_parse_from(["qq-uia-adapter", "storage"]).is_ok());
        assert!(Cli::try_parse_from([
            "qq-uia-adapter",
            "--remote",
            "http://127.0.0.1:18770",
            "status"
        ])
        .is_ok());
        assert!(Cli::try_parse_from(["qq-uia-adapter", "--local-only", "ensure-qq"]).is_ok());
        assert!(Cli::try_parse_from(["qq-uia-adapter", "ensure-qq", "--force-wake"]).is_ok());
        assert!(Cli::try_parse_from(["qq-uia-adapter", "diag"]).is_ok());
        assert!(Cli::try_parse_from(["qq-uia-adapter", "diag", "--hwnd", "0x100"]).is_ok());
        assert!(Cli::try_parse_from(["qq-uia-adapter", "restart"]).is_ok());
        assert!(Cli::try_parse_from(["qq-uia-adapter", "start-daemon"]).is_ok());
        assert!(Cli::try_parse_from([
            "qq-uia-adapter",
            "--config",
            "config/default.toml",
            "start-daemon"
        ])
        .is_ok());
        assert!(Cli::try_parse_from([
            "qq-uia-adapter",
            "serve",
            "--config",
            "config/default.toml",
            "--port",
            "18770",
            "--auto-ping"
        ])
        .is_ok());
        assert!(Cli::try_parse_from([
            "qq-uia-adapter",
            "inspect",
            "--hwnd",
            "0x100",
            "--depth",
            "5",
            "--out",
            ".runtime/uia-tree.json"
        ])
        .is_ok());
        assert!(Cli::try_parse_from(["qq-uia-adapter", "sessions"]).is_ok());
        assert!(Cli::try_parse_from([
            "qq-uia-adapter",
            "messages",
            "--group",
            "Demo测试群",
            "--limit",
            "20"
        ])
        .is_ok());
        assert!(Cli::try_parse_from([
            "qq-uia-adapter",
            "messages",
            "--private",
            "--group",
            "测试好友",
            "--limit",
            "10"
        ])
        .is_ok());
        assert!(Cli::try_parse_from([
            "qq-uia-adapter",
            "messages",
            "--history",
            "--group",
            "Demo测试群",
            "--limit",
            "30"
        ])
        .is_ok());
        assert!(Cli::try_parse_from([
            "qq-uia-adapter",
            "send",
            "--group",
            "Demo测试群",
            "--text",
            "hello",
            "--mode",
            "background_uia"
        ])
        .is_ok());
        assert!(Cli::try_parse_from([
            "qq-uia-adapter",
            "send",
            "--group",
            "Demo测试群",
            "--reply",
            "10005",
            "--at",
            "20001",
            "--at",
            "all",
            "--face",
            "14",
            "--face",
            "微笑",
            "--text",
            "rich",
            "--image",
            "demo.png",
            "--file",
            "report.pdf"
        ])
        .is_ok());
        assert!(Cli::try_parse_from([
            "qq-uia-adapter",
            "send",
            "--group",
            "Demo测试群",
            "--video",
            "clip.mp4"
        ])
        .is_ok());
        assert!(Cli::try_parse_from([
            "qq-uia-adapter",
            "ocr",
            "--image",
            "shot.png",
            "--lang",
            "en-US",
            "--json"
        ])
        .is_ok());
        assert!(Cli::try_parse_from(["qq-uia-adapter", "recall", "--message-id", "10005"]).is_ok());
        assert!(Cli::try_parse_from(["qq-uia-adapter", "delete-msg", "--id", "10005"]).is_ok());
        assert!(Cli::try_parse_from(["qq-uia-adapter", "groups", "--no-cache"]).is_ok());
        assert!(Cli::try_parse_from([
            "qq-uia-adapter",
            "group-members",
            "--group-id",
            "100000001",
            "--no-cache"
        ])
        .is_ok());
        assert!(Cli::try_parse_from([
            "qq-uia-adapter",
            "verify-group",
            "--group",
            "Demo测试群",
            "--below-members",
            "10"
        ])
        .is_ok());
        assert!(Cli::try_parse_from([
            "qq-uia-adapter",
            "send",
            "--private",
            "--group",
            "测试好友",
            "--text",
            "hi",
            "--image",
            "demo.png"
        ])
        .is_ok());
        assert!(Cli::try_parse_from(["qq-uia-adapter", "friends", "--no-cache"]).is_ok());
        assert!(Cli::try_parse_from(["qq-uia-adapter", "group-info"]).is_ok());
        assert!(Cli::try_parse_from([
            "qq-uia-adapter",
            "group-info",
            "--group-id",
            "100000001",
            "--no-cache"
        ])
        .is_ok());
        let sched_plain = Cli::try_parse_from(["qq-uia-adapter", "scheduler"]).unwrap();
        assert!(matches!(
            sched_plain.command,
            crate::cli_args::Commands::Scheduler { json: false }
        ));
        let sched_json = Cli::try_parse_from(["qq-uia-adapter", "scheduler", "--json"]).unwrap();
        assert!(matches!(
            sched_json.command,
            crate::cli_args::Commands::Scheduler { json: true }
        ));
        assert!(Cli::try_parse_from(["qq-uia-adapter", "mark-read"]).is_ok());
        assert!(
            Cli::try_parse_from(["qq-uia-adapter", "mark-read", "--group", "Demo测试群"]).is_ok()
        );
        assert!(
            Cli::try_parse_from(["qq-uia-adapter", "mark-read", "--private", "测试好友"]).is_ok()
        );
        let send_win = Cli::try_parse_from([
            "qq-uia-adapter",
            "send",
            "--group",
            "Demo测试群",
            "--text",
            "hi",
            "--response-window-ms",
            "5000",
        ])
        .unwrap();
        assert!(matches!(
            send_win.command,
            crate::cli_args::Commands::Send {
                response_window_ms: Some(5000),
                ..
            }
        ));
        assert!(Cli::try_parse_from([
            "qq-uia-adapter",
            "messages",
            "--history",
            "--before-id",
            "10060",
            "--after-id",
            "10020",
            "--sender",
            "群友A",
            "--keyword",
            "ping",
            "--reverse-order",
            "--json"
        ])
        .is_ok());
        assert!(Cli::try_parse_from([
            "qq-uia-adapter",
            "forward",
            "--message-id",
            "10001",
            "--group",
            "Demo测试群",
            "--response-window-ms",
            "3000"
        ])
        .is_ok());
        assert!(Cli::try_parse_from([
            "qq-uia-adapter",
            "forward-get",
            "--id",
            "fwd-10001",
            "--json"
        ])
        .is_ok());
        assert!(Cli::try_parse_from([
            "qq-uia-adapter",
            "audit",
            "--kind",
            "security_breaker_reset",
            "--limit",
            "20",
            "--json"
        ])
        .is_ok());
        assert!(Cli::try_parse_from(["qq-uia-adapter", "clean-cache", "--json"]).is_ok());
        assert!(Cli::try_parse_from(["qq-uia-adapter", "reset-breaker", "--json"]).is_ok());
    }

    #[test]
    fn validate_http_base_url_handles_http_and_rejects_https() {
        assert_eq!(
            validate_http_base_url("http://127.0.0.1:18770/").unwrap(),
            "http://127.0.0.1:18770"
        );
        assert_eq!(
            validate_http_base_url("127.0.0.1:19999").unwrap(),
            "http://127.0.0.1:19999"
        );
        assert!(validate_http_base_url("https://127.0.0.1:18770").is_err());
    }

    #[test]
    fn should_wait_for_restart_enforces_pid_rotation_and_handles_missing_pid() {
        assert!(should_wait_for_restart(Some(1001), false, Some(1001)));
        assert!(should_wait_for_restart(Some(1001), true, Some(1001)));
        assert!(should_wait_for_restart(Some(1001), false, None));
        assert!(!should_wait_for_restart(Some(1001), true, None));
        assert!(!should_wait_for_restart(Some(1001), false, Some(2002)));
        assert!(!should_wait_for_restart(Some(1001), true, Some(2002)));

        assert!(should_wait_for_restart(None, false, None));
        assert!(!should_wait_for_restart(None, true, None));
        assert!(!should_wait_for_restart(None, false, Some(2002)));
        assert!(!should_wait_for_restart(None, true, Some(2002)));
    }

    #[test]
    fn parse_health_pid_extracts_u32_when_present() {
        let ok = HttpResponsePayload {
            status_code: 200,
            body: r#"{"ok":true,"pid":4242}"#.to_string(),
        };
        assert_eq!(parse_health_pid(&ok), Some(4242));

        let no_pid = HttpResponsePayload {
            status_code: 200,
            body: r#"{"ok":true}"#.to_string(),
        };
        assert_eq!(parse_health_pid(&no_pid), None);

        let bad_json = HttpResponsePayload {
            status_code: 200,
            body: "not json".to_string(),
        };
        assert_eq!(parse_health_pid(&bad_json), None);
    }

    #[test]
    fn start_daemon_helpers_resolve_repo_root_and_build_detached_serve_command() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp_root = std::env::temp_dir().join(format!(
            "qq-uia-start-daemon-test-{}-{}",
            std::process::id(),
            nanos
        ));
        let config_dir = temp_root.join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        let _cleanup = TempDirGuard(temp_root.clone());

        let config_file = config_dir.join("default.toml");
        std::fs::write(&config_file, "").unwrap();

        let resolved_root = resolve_repo_root(&config_file).unwrap();
        let expected_root = strip_windows_verbatim(&std::fs::canonicalize(&temp_root).unwrap());
        assert_eq!(resolved_root, expected_root);

        let arg = resolve_daemon_config_arg(&resolved_root, &config_file);
        assert_eq!(
            PathBuf::from(arg),
            PathBuf::from("config").join("default.toml")
        );

        let daemon_bin = resolved_root
            .join(".runtime")
            .join("bin")
            .join("qq-uia-adapter-daemon.exe");
        let command = build_daemon_serve_command(
            &daemon_bin,
            r"config\default.toml",
            &resolved_root,
            Stdio::null(),
            Stdio::null(),
        );
        assert_eq!(command.get_program(), daemon_bin.as_os_str());
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            vec!["serve", "--config", r"config\default.toml"]
        );
        assert_eq!(command.get_current_dir(), Some(resolved_root.as_path()));
        assert_eq!(
            adapter_server::rest::prepend_path_dir(
                Some(std::ffi::OsStr::new(r"C:\Windows")),
                r"C:\DevTools\mingw64\bin"
            ),
            r"C:\DevTools\mingw64\bin;C:\Windows"
        );
        assert_eq!(format_base_url("0.0.0.0", 18770), "http://0.0.0.0:18770");
        assert_eq!(format_base_url("::1", 18770), "http://[::1]:18770");
    }

    #[test]
    fn decide_start_daemon_action_guards_isolated_sandbox_desktops() {
        assert_eq!(
            decide_start_daemon_action(false, false),
            SandboxDaemonAction::ProceedLocalSpawn
        );
        assert_eq!(
            decide_start_daemon_action(false, true),
            SandboxDaemonAction::ProceedLocalSpawn
        );
        assert_eq!(
            decide_start_daemon_action(true, true),
            SandboxDaemonAction::DelegateHotRestart
        );
        assert_eq!(
            decide_start_daemon_action(true, false),
            SandboxDaemonAction::RejectIsolatedNoRemote
        );
    }

    #[tokio::test]
    async fn mark_read_rejects_conflicting_and_blank_targets() {
        let cfg = common_types::AdapterConfig::default();
        let conflict = crate::commands::chat_ops::cmd_mark_read(
            cfg.clone(),
            None,
            Some("Demo测试群".to_string()),
            Some("测试好友".to_string()),
        )
        .await;
        assert!(conflict.is_err());

        let blank_group = crate::commands::chat_ops::cmd_mark_read(
            cfg.clone(),
            None,
            Some("   ".to_string()),
            None,
        )
        .await;
        assert!(blank_group.is_err());

        let blank_private =
            crate::commands::chat_ops::cmd_mark_read(cfg, None, None, Some("  ".to_string())).await;
        assert!(blank_private.is_err());
    }
}
