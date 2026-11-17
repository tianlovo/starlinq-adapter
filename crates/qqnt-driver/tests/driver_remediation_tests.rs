//! 驱动层真实 QQ 缺陷回归集成测试：覆盖缓存清理、自账号元数据对齐、非阻塞状态快照与 MockQQ 回复发送验证

use std::time::Duration;

use common_types::constants::MOCK_QQ_MAIN_CLASS;
use common_types::{
    AdapterConfig, AdapterError, AdapterEvent, BotConfig, BotDriver, ChatTargetKind,
    GroupTargetSpec, InputMode, InteractionConfig, MessageSegment, QqClientConfig, SendRequest,
    StorageConfig,
};
use mock_qq_win::MockQqServer;
use qqnt_driver::profile::SECURITY_KEYWORDS;
use qqnt_driver::QqntDriver;
use storage::{
    RecordMessageInput, Storage, UpsertFriendInput, UpsertGroupInput, UpsertMemberInput,
};
use tokio::sync::Mutex as AsyncMutex;
use uia_core::{enumerate_candidate_windows, find_windows_with_keywords};

static REMEDIATION_MOCK_LOCK: AsyncMutex<()> = AsyncMutex::const_new(());

/// 等待其他并行测试进程创建的 MockQQ 或安全弹窗彻底退出，防止跨进程窗口句柄冲突
async fn wait_for_clear_mock_desktop() {
    for _ in 0..80 {
        let foreign_mock = enumerate_candidate_windows(&[], &[MOCK_QQ_MAIN_CLASS.to_string()])
            .into_iter()
            .any(|win| win.class_name == MOCK_QQ_MAIN_CLASS && win.pid != std::process::id());
        let foreign_security = !find_windows_with_keywords(&[], SECURITY_KEYWORDS).is_empty();
        if !foreign_mock && !foreign_security {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn temp_db_path(tag: &str) -> std::path::PathBuf {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    std::env::temp_dir().join(format!("qq_uia_remediation_{tag}_{unique}.db"))
}

fn mock_driver_config(db_path: String) -> AdapterConfig {
    AdapterConfig {
        qq_client: QqClientConfig {
            auto_launch_or_wake: false,
            startup_timeout_ms: 1000,
            ..Default::default()
        },
        target: GroupTargetSpec {
            group_name: "Demo测试群".to_string(),
            strict_below_members: Some(10),
            require_member_count_verified: false,
            allowed_private_friends: vec!["测试好友".to_string()],
            ..Default::default()
        },
        interaction: InteractionConfig {
            default_mode: InputMode::BackgroundUia,
            allow_focus_fallback: false,
            min_send_interval_ms: 50,
            poll_interval_ms: 150,
            extra_window_classes: vec![MOCK_QQ_MAIN_CLASS.to_string()],
            ..Default::default()
        },
        storage: StorageConfig { db_path },
        ..Default::default()
    }
}

#[tokio::test]
async fn startup_migrations_and_no_cache_queries_purge_session_pollution() {
    let db_path = temp_db_path("pollution");
    let db_str = db_path.to_string_lossy().to_string();

    {
        let storage = Storage::open(&db_str).expect("open pre-seeded db");
        storage
            .upsert_friend(&UpsertFriendInput {
                nickname: "消息".to_string(),
                remark: String::new(),
                real_user_id: None,
                last_preview: None,
            })
            .expect("seed polluted friend");
        storage
            .upsert_friend(&UpsertFriendInput {
                nickname: "测试好友".to_string(),
                remark: String::new(),
                real_user_id: None,
                last_preview: None,
            })
            .expect("seed valid friend");
        storage
            .upsert_group(&UpsertGroupInput {
                group_name: "12:30".to_string(),
                real_group_id: None,
                member_count: None,
                max_member_count: None,
                last_preview: None,
                unread_count: 0,
            })
            .expect("seed polluted group");
        storage
            .upsert_group(&UpsertGroupInput {
                group_name: "Demo测试群".to_string(),
                real_group_id: None,
                member_count: Some(5),
                max_member_count: Some(200),
                last_preview: None,
                unread_count: 0,
            })
            .expect("seed valid group");
    }

    let driver = QqntDriver::new(AdapterConfig {
        storage: StorageConfig {
            db_path: db_str.clone(),
        },
        ..Default::default()
    })
    .expect("create driver");

    // 启动迁移应已同步清理污染好友与污染群名并写入版本号标记
    let friends = driver.get_friend_list(false).await.expect("friends");
    assert_eq!(friends.len(), 1);
    assert_eq!(friends[0].nickname, "测试好友");

    let groups = driver.get_group_list(false).await.expect("groups");
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].group_name, "Demo测试群");

    // 直接通过底层存储再次注入非法行后，no_cache=true 必须再次清洗
    {
        let raw_storage = Storage::open(&db_str).expect("reopen db");
        assert_eq!(
            raw_storage
                .get_meta("migration.session_pollution_v2")
                .expect("meta v2")
                .as_deref(),
            Some("1")
        );
        assert_eq!(
            raw_storage
                .get_meta("migration.media_crop_dpi_v2")
                .expect("media meta v2")
                .as_deref(),
            Some("1")
        );
        raw_storage
            .upsert_friend(&UpsertFriendInput {
                nickname: "联系人".to_string(),
                remark: String::new(),
                real_user_id: None,
                last_preview: None,
            })
            .expect("inject invalid friend");
        raw_storage
            .upsert_group(&UpsertGroupInput {
                group_name: "昨天 12:30".to_string(),
                real_group_id: None,
                member_count: None,
                max_member_count: None,
                last_preview: None,
                unread_count: 0,
            })
            .expect("inject invalid group");
    }

    let purged_friends = driver
        .get_friend_list(true)
        .await
        .expect("no_cache friends");
    assert_eq!(purged_friends.len(), 1);
    assert_eq!(purged_friends[0].nickname, "测试好友");

    // 即使在无 QQ 窗口时调用 no_cache=true，也应先清洗被污染的本地群列表行
    let _ = driver.get_group_list(true).await;
    let purged_groups = driver
        .get_group_list(false)
        .await
        .expect("cached after purge");
    assert_eq!(purged_groups.len(), 1);
    assert_eq!(purged_groups[0].group_name, "Demo测试群");

    drop(driver);
    let _ = std::fs::remove_file(&db_path);
}

#[tokio::test]
async fn login_info_sets_real_qq_id_persists_self_uid_and_reconciles_members() {
    let db_path = temp_db_path("self_id");
    let db_str = db_path.to_string_lossy().to_string();

    {
        let storage = Storage::open(&db_str).expect("seed members");
        let gid = storage
            .upsert_group(&UpsertGroupInput {
                group_name: "Demo测试群".to_string(),
                real_group_id: Some(77889900),
                member_count: Some(3),
                max_member_count: Some(200),
                last_preview: None,
                unread_count: 0,
            })
            .expect("seed group")
            .effective_group_id();
        storage
            .upsert_group_members(
                "Demo测试群",
                gid,
                &[UpsertMemberInput {
                    nickname: "BotSelfNick".to_string(),
                    card: String::new(),
                    role: "member".to_string(),
                    real_user_id: None,
                }],
            )
            .expect("seed member");
    }

    let driver = QqntDriver::new(AdapterConfig {
        bot: BotConfig {
            self_id: 38123456,
            self_nicknames: vec!["BotSelfNick".to_string()],
            ..Default::default()
        },
        storage: StorageConfig {
            db_path: db_str.clone(),
        },
        ..Default::default()
    })
    .expect("create driver");

    let login = driver.get_login_info().await.expect("login info");
    assert_eq!(login.user_id, 38123456);
    assert_eq!(login.real_qq_id, Some(38123456));
    assert_eq!(login.nickname, "BotSelfNick");

    let cached = driver.cached_status().expect("cached_status implemented");
    assert_eq!(cached.input_mode, InputMode::Auto);

    let members = driver
        .get_group_member_list(None, Some("Demo测试群".to_string()), false)
        .await
        .expect("members");
    assert_eq!(members.len(), 1);
    assert_eq!(members[0].user_id, 38123456);

    let storage = Storage::open(&db_str).expect("verify meta");
    assert_eq!(
        storage
            .get_meta("self_user_id")
            .expect("self_user_id meta")
            .as_deref(),
        Some("38123456")
    );
    assert_eq!(
        storage
            .get_meta("self_nickname")
            .expect("self_nickname meta")
            .as_deref(),
        Some("BotSelfNick")
    );

    drop(driver);
    let _ = std::fs::remove_file(&db_path);
}

#[tokio::test]
async fn reply_send_against_mock_qq_succeeds_truthfully_without_trailing_newline() {
    let _guard = REMEDIATION_MOCK_LOCK.lock().await;
    wait_for_clear_mock_desktop().await;
    let mock = MockQqServer::spawn().expect("spawn mock qq");
    let db_path = temp_db_path("mock_reply");
    let db_str = db_path.to_string_lossy().to_string();

    let reply_target_id = {
        let storage = Storage::open(&db_str).expect("open temp db");
        let gid = storage
            .upsert_group(&UpsertGroupInput {
                group_name: "Demo测试群".to_string(),
                real_group_id: None,
                member_count: Some(5),
                max_member_count: Some(200),
                last_preview: None,
                unread_count: 0,
            })
            .expect("upsert group")
            .effective_group_id();
        storage
            .record_message(&RecordMessageInput {
                message_uid: "seed-reply-1".to_string(),
                group_name: "Demo测试群".to_string(),
                group_id: gid,
                user_id: 90001,
                sender_name: "群友A".to_string(),
                raw_message: "原始问题".to_string(),
                is_self: false,
                timestamp_hint: Some("09:00".to_string()),
                captured_at_ms: 1_700_000_000_000,
                chat_type: ChatTargetKind::Group,
                segments: vec![MessageSegment::Text {
                    text: "原始问题".to_string(),
                }],
            })
            .expect("record reply seed")
    };

    let driver = QqntDriver::new(mock_driver_config(db_str.clone())).expect("create driver");
    let open_status = driver
        .open_group(GroupTargetSpec {
            group_name: "Demo测试群".to_string(),
            strict_below_members: Some(10),
            require_member_count_verified: false,
            ..Default::default()
        })
        .await
        .expect("open mock group");
    assert_eq!(
        driver.cached_status().expect("cached").active_group,
        open_status.active_group
    );

    // 默认严格模式下找不到目标气泡时必须拒绝发送，严禁降级为「回复 ...」纯文字
    let strict_err = driver
        .send_group_message(SendRequest {
            group: "Demo测试群".to_string(),
            text: String::new(),
            mode: Some(InputMode::BackgroundUia),
            target_kind: ChatTargetKind::Group,
            segments: vec![MessageSegment::Reply {
                id: reply_target_id.to_string(),
            }],
            ..Default::default()
        })
        .await
        .expect_err("strict mode must reject non-visible native reply target");
    assert!(
        strict_err.to_string().contains("not visible"),
        "unexpected strict reply error: {strict_err}"
    );
    drop(driver);

    // 显式开启 allow_degraded_media_summary=true 时才允许降级回复且不带尾部换行符
    let mut degraded_cfg = mock_driver_config(db_str);
    degraded_cfg.onebot.allow_degraded_media_summary = true;
    let degraded_driver = QqntDriver::new(degraded_cfg).expect("create degraded driver");
    degraded_driver
        .open_group(GroupTargetSpec {
            group_name: "Demo测试群".to_string(),
            strict_below_members: Some(10),
            require_member_count_verified: false,
            ..Default::default()
        })
        .await
        .expect("open mock group for degraded reply");
    let reply_only = degraded_driver
        .send_group_message(SendRequest {
            group: "Demo测试群".to_string(),
            text: String::new(),
            mode: Some(InputMode::BackgroundUia),
            target_kind: ChatTargetKind::Group,
            segments: vec![MessageSegment::Reply {
                id: reply_target_id.to_string(),
            }],
            ..Default::default()
        })
        .await
        .expect("send degraded reply-only message");
    assert!(reply_only.ok);
    assert!(reply_only.verified_in_chat);
    assert_eq!(reply_only.degraded_segments, vec!["reply".to_string()]);
    assert!(reply_only.onebot_message_id.is_some());

    drop(degraded_driver);
    drop(mock);
    let _ = std::fs::remove_file(&db_path);
}

fn remediation_node(
    control_type: &str,
    name: &str,
    automation_id: &str,
    rect: [i32; 4],
) -> common_types::UiNodeSnapshot {
    common_types::UiNodeSnapshot {
        control_type: control_type.to_string(),
        name: name.to_string(),
        automation_id: automation_id.to_string(),
        rect,
        is_enabled: true,
        is_offscreen: false,
        ..Default::default()
    }
}

#[test]
fn header_matcher_ignores_document_msg_ml_nodes_and_clipped_top_bubbles() {
    let win_rect = [0, 0, 1280, 900];
    let ml_root = remediation_node("List", "消息列表", "ml-root", [310, 100, 1020, 720]);
    let real_header = remediation_node("Text", "Demo测试群 (9)", "", [340, 44, 520, 74]);
    let poison_doc = remediation_node("Document", "错误文档群 (88)", "", [340, 42, 620, 70]);
    let poison_msg = remediation_node(
        "Text",
        "Alice: 假标题 (120)",
        "MSG-9988",
        [350, 46, 560, 72],
    );
    let poison_ml = remediation_node(
        "Text",
        "列表前置污染 (77)",
        "ML-top-spacer",
        [350, 48, 560, 74],
    );
    let clipped_overlap = remediation_node(
        "Text",
        "顶部裁切气泡A (66)",
        "bubble-a",
        [360, 88, 620, 108],
    );
    let clipped_near_edge = remediation_node(
        "Text",
        "顶部裁切气泡B (55)",
        "bubble-b",
        [360, 95, 620, 101],
    );

    let snapshots = [
        &ml_root,
        &poison_doc,
        &poison_msg,
        &poison_ml,
        &clipped_overlap,
        &clipped_near_edge,
        &real_header,
    ];
    let (title, count) =
        qqnt_driver::matcher::read_chat_header_from_snapshots(&snapshots, win_rect, false, None);
    assert_eq!(title.as_deref(), Some("Demo测试群 (9)"));
    assert_eq!(count, Some(9));

    // 同时验证拆分渲染的群名与 `(9)` 在同批干扰节点下仍能提取出群名与成员数
    let split_title = remediation_node("Text", "Demo测试群", "", [340, 44, 460, 74]);
    let split_count = remediation_node("Text", "(9)", "", [464, 44, 498, 74]);
    let split_snapshots = [
        &ml_root,
        &poison_doc,
        &poison_msg,
        &poison_ml,
        &clipped_overlap,
        &clipped_near_edge,
        &split_title,
        &split_count,
    ];
    let (s_title, s_count) = qqnt_driver::matcher::read_chat_header_from_snapshots(
        &split_snapshots,
        win_rect,
        false,
        None,
    );
    assert_eq!(s_title.as_deref(), Some("Demo测试群"));
    assert_eq!(s_count, Some(9));
}

#[tokio::test]
async fn live_status_reconciles_external_switch_and_explicit_poll_self_heals() {
    let _guard = REMEDIATION_MOCK_LOCK.lock().await;
    wait_for_clear_mock_desktop().await;
    let mock = MockQqServer::spawn().expect("spawn mock qq");
    let db_a = temp_db_path("live_sync_a");
    let db_b = temp_db_path("live_sync_b");

    let mut cfg_a = mock_driver_config(db_a.to_string_lossy().to_string());
    cfg_a.interaction.poll_interval_ms = 600_000;
    let mut cfg_b = mock_driver_config(db_b.to_string_lossy().to_string());
    cfg_b.target.group_name = "测试大群".to_string();
    cfg_b.target.strict_below_members = None;
    cfg_b.interaction.poll_interval_ms = 600_000;

    let driver_a = QqntDriver::new(cfg_a).expect("create driver a");
    let driver_b = QqntDriver::new(cfg_b).expect("create driver b");

    let opened_a = driver_a
        .open_group(GroupTargetSpec {
            group_name: "Demo测试群".to_string(),
            strict_below_members: Some(10),
            require_member_count_verified: false,
            allowed_private_friends: vec!["测试好友".to_string()],
            ..Default::default()
        })
        .await
        .expect("driver a open target group");
    assert_eq!(opened_a.active_group.as_deref(), Some("Demo测试群 (5)"));
    assert_eq!(opened_a.detected_member_count, Some(5));
    let cached_open = driver_a.cached_status().expect("cached status after open");
    assert_eq!(cached_open.active_group.as_deref(), Some("Demo测试群 (5)"));
    assert_eq!(cached_open.active_chat_kind, Some(ChatTargetKind::Group));

    // 通过另一驱动句柄在模拟 QQ 内部将聊天切换至私聊好友，验证被动同步不抢占窗口且实时 status 同步最新会话
    driver_b
        .open_private("测试好友")
        .await
        .expect("driver b switch mock window to private");
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(mock.active_group(), "测试好友");

    let live_private = driver_a.status().await.expect("live status on private");
    assert_eq!(live_private.active_group.as_deref(), Some("测试好友"));
    assert_eq!(live_private.active_chat_kind, Some(ChatTargetKind::Private));
    assert_eq!(live_private.detected_member_count, None);
    let cached_private = driver_a
        .cached_status()
        .expect("cached status matches live");
    assert_eq!(cached_private.active_group.as_deref(), Some("测试好友"));
    assert_eq!(
        cached_private.active_chat_kind,
        Some(ChatTargetKind::Private)
    );

    // 切换至测试大群验证实时 status 核对群标题与成员数，再恢复小群后外部切走并测试显式 poll_messages 自愈重开
    driver_b
        .open_group(GroupTargetSpec {
            group_name: "测试大群".to_string(),
            strict_below_members: None,
            require_member_count_verified: false,
            allowed_private_friends: vec![],
            ..Default::default()
        })
        .await
        .expect("driver b switch mock window to big group");
    let live_big = driver_a.status().await.expect("live status on big group");
    assert_eq!(live_big.active_group.as_deref(), Some("测试大群 (128)"));
    assert_eq!(live_big.active_chat_kind, Some(ChatTargetKind::Group));
    assert_eq!(live_big.detected_member_count, Some(128));

    driver_a
        .open_group(GroupTargetSpec {
            group_name: "Demo测试群".to_string(),
            strict_below_members: Some(10),
            require_member_count_verified: false,
            allowed_private_friends: vec!["测试好友".to_string()],
            ..Default::default()
        })
        .await
        .expect("reattach Demo small group");
    mock.inject_incoming_message("Demo测试群 (5)", "群友B [14:05]: 自愈重开拉取成功");
    driver_b
        .open_private("测试好友")
        .await
        .expect("externally move window away before explicit poll");

    let messages = driver_a
        .poll_messages("Demo测试群", 20)
        .await
        .expect("explicit poll_messages self heals back to target group");
    assert!(
        messages
            .iter()
            .any(|msg| msg.content.contains("自愈重开拉取成功")),
        "expected self-healed poll to recover injected message: {messages:?}"
    );
    let post_heal = driver_a.cached_status().expect("cached status after heal");
    assert_eq!(post_heal.active_group.as_deref(), Some("Demo测试群 (5)"));
    assert_eq!(post_heal.active_chat_kind, Some(ChatTargetKind::Group));
    assert_eq!(post_heal.detected_member_count, Some(5));

    // 关闭模拟 QQ 窗口后，实时 status 应清空已失效的窗口与会话附着状态
    drop(driver_b);
    mock.shutdown();
    let after_shutdown = driver_a.status().await.expect("live status after shutdown");
    assert!(after_shutdown.attached_window.is_none());
    assert!(after_shutdown.active_group.is_none());
    assert_eq!(after_shutdown.detected_member_count, None);

    drop(driver_a);
    let _ = std::fs::remove_file(&db_a);
    let _ = std::fs::remove_file(&db_b);
}

#[tokio::test]
async fn biz_risk_001_guards_unauthorized_private_and_oversized_background_poll() {
    let _guard = REMEDIATION_MOCK_LOCK.lock().await;
    wait_for_clear_mock_desktop().await;
    let mock = MockQqServer::spawn().expect("spawn mock qq");
    let db_a = temp_db_path("biz001_a");
    let db_b = temp_db_path("biz001_b");

    // driver_a 仅监控 Demo测试群(<10人)，且私聊白名单为空（不允许“测试好友”）
    let mut cfg_a = mock_driver_config(db_a.to_string_lossy().to_string());
    cfg_a.target.allowed_private_friends.clear();
    cfg_a.interaction.poll_interval_ms = 120;

    // driver_b 作为外部切换控制端，允许打开“测试好友”与“测试大群 (128)”
    let mut cfg_b = mock_driver_config(db_b.to_string_lossy().to_string());
    cfg_b.target.group_name = "测试大群".to_string();
    cfg_b.target.strict_below_members = None;
    cfg_b.interaction.poll_interval_ms = 600_000;

    let driver_a = QqntDriver::new(cfg_a).expect("create driver a");
    let driver_b = QqntDriver::new(cfg_b).expect("create driver b");
    let mut rx_a = driver_a.subscribe_events();

    // 先打开授权小群触发基线绑定，然后验证 open_private 对非白名单好友立拒
    driver_a
        .open_group(GroupTargetSpec {
            group_name: "Demo测试群".to_string(),
            strict_below_members: Some(10),
            require_member_count_verified: false,
            allowed_private_friends: vec![],
            ..Default::default()
        })
        .await
        .expect("open monitored small group");
    let open_priv_err = driver_a
        .open_private("测试好友")
        .await
        .expect_err("non-whitelisted private must be rejected");
    assert!(matches!(
        open_priv_err,
        AdapterError::PrivateTargetNotAllowed { .. }
    ));

    // 外部切至非白名单私聊“测试好友”并注入消息，后台 SyncAndPoll 应如实同步状态但跳过抓取与落库
    driver_b
        .open_private("测试好友")
        .await
        .expect("driver b open private");
    mock.inject_incoming_message(
        "测试好友",
        "测试好友 [15:01]: secret_private_should_not_scrape",
    );
    tokio::time::sleep(Duration::from_millis(380)).await;

    let live_priv = driver_a.status().await.expect("live status on private");
    assert_eq!(live_priv.active_group.as_deref(), Some("测试好友"));
    assert_eq!(live_priv.active_chat_kind, Some(ChatTargetKind::Private));
    let stored_priv = driver_a
        .get_recent_messages_by_kind("测试好友", Some(ChatTargetKind::Private), 20)
        .await
        .expect("query private storage");
    assert!(
        !stored_priv
            .iter()
            .any(|m| m.raw_message.contains("secret_private_should_not_scrape")),
        "unauthorized private message must not be scraped or stored: {stored_priv:?}"
    );

    // 外部切至未授权大群“测试大群 (128)”并注入消息，后台 SyncAndPoll 同样应跳过抓取；显式 poll_messages 须触发人数超限拒绝
    driver_b
        .open_group(GroupTargetSpec {
            group_name: "测试大群".to_string(),
            strict_below_members: None,
            require_member_count_verified: false,
            ..Default::default()
        })
        .await
        .expect("driver b open large group");
    mock.inject_incoming_message(
        "测试大群 (128)",
        "群友X [15:02]: big_group_should_not_scrape",
    );
    tokio::time::sleep(Duration::from_millis(380)).await;

    let live_big = driver_a.status().await.expect("live status on big group");
    assert_eq!(live_big.active_group.as_deref(), Some("测试大群 (128)"));
    assert_eq!(live_big.detected_member_count, Some(128));
    let stored_big = driver_a
        .get_recent_messages_by_kind("测试大群", Some(ChatTargetKind::Group), 20)
        .await
        .expect("query big group storage");
    assert!(
        !stored_big
            .iter()
            .any(|m| m.raw_message.contains("big_group_should_not_scrape")),
        "oversized group must not be scraped by background poll: {stored_big:?}"
    );

    let poll_big_err = driver_a
        .poll_messages("测试大群", 20)
        .await
        .expect_err("explicit poll on oversized active group must return MemberCountExceeded");
    assert!(
        matches!(poll_big_err, AdapterError::MemberCountExceeded { .. }),
        "expected MemberCountExceeded, got {poll_big_err:?}"
    );

    let mut leaked_incoming = Vec::new();
    while let Ok(ev) = rx_a.try_recv() {
        if let AdapterEvent::PrivateMessage(m) | AdapterEvent::GroupMessage(m) = ev {
            if m.content.contains("should_not_scrape") {
                leaked_incoming.push(m);
            }
        }
    }
    assert!(
        leaked_incoming.is_empty(),
        "no unauthorized incoming event may be emitted: {leaked_incoming:?}"
    );

    drop(driver_b);
    drop(driver_a);
    mock.shutdown();
    wait_for_clear_mock_desktop().await;
    let _ = std::fs::remove_file(&db_a);
    let _ = std::fs::remove_file(&db_b);
}

#[tokio::test]
async fn biz_risk_007_and_008_mark_read_guard_and_confirmed_message_sent_and_poll_notices() {
    let _guard = REMEDIATION_MOCK_LOCK.lock().await;
    wait_for_clear_mock_desktop().await;
    let mock = MockQqServer::spawn().expect("spawn mock qq");
    let db_path = temp_db_path("biz007_008");

    let mut cfg = mock_driver_config(db_path.to_string_lossy().to_string());
    cfg.interaction.poll_interval_ms = 600_000;

    let driver = QqntDriver::new(cfg).expect("create driver");
    let mut rx = driver.subscribe_events();

    driver
        .open_group(GroupTargetSpec {
            group_name: "Demo测试群".to_string(),
            strict_below_members: Some(10),
            require_member_count_verified: false,
            allowed_private_friends: vec!["测试好友".to_string()],
            ..Default::default()
        })
        .await
        .expect("open group");

    // 非白名单私聊的 mark_session_read 拒绝放行，合法群聊 mark_session_read 正常通过
    let deny_read = driver
        .mark_session_read("非白名单陌生人", ChatTargetKind::Private)
        .await
        .expect_err("non-whitelisted mark_session_read must fail");
    assert!(matches!(
        deny_read,
        AdapterError::PrivateTargetNotAllowed { .. }
    ));

    driver
        .mark_session_read("Demo测试群", ChatTargetKind::Group)
        .await
        .expect("mark_session_read succeeds under send_gate without security dialog");

    // 失败发送不得派发 MessageSent 事件；真实成功回显确认后派发带真实 onebot_message_id 的 MessageSent
    let bad_send = driver
        .send_group_message(SendRequest {
            group: "Demo测试群".to_string(),
            text: "[CQ:reply,id=99999999] invalid reply".to_string(),
            target_kind: ChatTargetKind::Group,
            ..Default::default()
        })
        .await;
    assert!(bad_send.is_err());

    let mut bad_sent_events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if let AdapterEvent::MessageSent(out) = ev {
            bad_sent_events.push(out);
        }
    }
    assert!(
        bad_sent_events.is_empty(),
        "failed send must not emit MessageSent: {bad_sent_events:?}"
    );

    let ok_outcome = driver
        .send_group_message(SendRequest {
            group: "Demo测试群".to_string(),
            text: "confirmed_live_echo".to_string(),
            target_kind: ChatTargetKind::Group,
            ..Default::default()
        })
        .await
        .expect("valid group send succeeds");
    assert!(ok_outcome.ok);
    assert!(ok_outcome.onebot_message_id.is_some());

    let mut matched_sent = None;
    while let Ok(ev) = rx.try_recv() {
        if let AdapterEvent::MessageSent(out) = ev {
            if out.onebot_message_id == ok_outcome.onebot_message_id {
                matched_sent = Some(out);
            }
        }
    }
    assert!(
        matched_sent.is_some(),
        "confirmed send must emit AdapterEvent::MessageSent"
    );

    // 显式 poll_messages 拉取包含文件气泡的入站消息时，必须同步派发 group_upload 通知
    mock.inject_incoming_message(
        "Demo测试群 (5)",
        "群友A [15:10]: inbound_test_doc.pdf\n56.00 B 已发送",
    );
    let polled = driver
        .poll_messages("Demo测试群", 20)
        .await
        .expect("explicit poll returns messages");
    assert!(
        polled.iter().any(|m| m.segments.iter().any(|s| matches!(
            s,
            MessageSegment::File { name: Some(n), .. } if n == "inbound_test_doc.pdf"
        ))),
        "expected parsed file segment in polled messages: {polled:?}"
    );
    let mut upload_notice = None;
    while let Ok(ev) = rx.try_recv() {
        if let AdapterEvent::Notice(val) = ev {
            if val["notice_type"] == "group_upload" && val["file"]["name"] == "inbound_test_doc.pdf"
            {
                upload_notice = Some(val);
            }
        }
    }
    assert!(
        upload_notice.is_some(),
        "explicit poll_messages must emit inbound group_upload notice"
    );

    // 存在安全验证弹窗时 mark_session_read 经 CheckSecurity 检测并触发安全熔断拒绝放行
    mock.open_security_dialog("QQ安全验证");
    tokio::time::sleep(Duration::from_millis(180)).await;
    let sec_err = driver
        .mark_session_read("Demo测试群", ChatTargetKind::Group)
        .await
        .expect_err("mark_session_read must fail when security prompt is open");
    assert!(
        matches!(sec_err, AdapterError::SecurityPromptDetected { .. }),
        "expected SecurityPromptDetected, got {sec_err:?}"
    );
    mock.close_security_dialog();
    tokio::time::sleep(Duration::from_millis(180)).await;

    drop(driver);
    mock.shutdown();
    wait_for_clear_mock_desktop().await;
    let _ = std::fs::remove_file(&db_path);
}
