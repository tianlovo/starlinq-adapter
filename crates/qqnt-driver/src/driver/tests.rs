//! 驱动模块与 V2 OneBot 持久化、防误触守卫、WinEvent 间隔及 Mock 窗体联调单元测试

use std::sync::Arc;

use common_types::constants::MOCK_QQ_MAIN_CLASS;
use common_types::{
    AdapterConfig, AdapterError, BotConfig, BotDriver, ChatMessage, ChatTargetKind,
    GroupTargetSpec, InputMode, InteractionConfig, MessageSegment, QqClientConfig, SendRequest,
    SessionSummary, StorageConfig, UiNodeSnapshot, WindowInfo,
};
use storage::constants::MEMORY_DB_PATH;
use storage::{Storage, UpsertFriendInput, UpsertGroupInput, UpsertMemberInput};
use tokio::sync::broadcast;
use uia_core::VerifiedWindowIdentity;

use super::poller::compute_poller_fallback_ms;
use super::runtime_state::SharedInner;
use super::segment_enricher::{enrich_parsed_messages, EnrichContext};
use super::send_handlers::{
    build_focus_restore_outcome, editor_value_matches_norm, finalize_send_result,
    verify_pre_send_chat_guard_snapshots, FocusRestoreOutcomeParams, PreSendGuardCheck,
};
use super::window_discovery::{
    all_non_standalone_windows_are_login_prompts, is_narrow_login_window,
    nodes_look_like_login_prompt, should_auto_launch_qq,
};
use super::QqntDriver;
use crate::matcher::{extract_sessions_from_snapshots, locate_composer_and_send_indices};
use crate::parser::{self, ParsedTreeMessage};
use crate::profile;

static UNIT_MOCK_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn memory_test_config() -> AdapterConfig {
    AdapterConfig {
        storage: StorageConfig {
            db_path: MEMORY_DB_PATH.to_string(),
        },
        ..Default::default()
    }
}

fn make_test_window(rect: [i32; 4], is_iconic: bool) -> WindowInfo {
    WindowInfo {
        hwnd: 1001,
        pid: 4242,
        process_name: "QQ.exe".to_string(),
        class_name: "Chrome_WidgetWin_1".to_string(),
        title: "QQ".to_string(),
        is_visible: !is_iconic,
        is_iconic,
        rect,
        is_standalone_chat: false,
    }
}

fn make_test_node(control_type: &str, name: &str, rect: [i32; 4]) -> UiNodeSnapshot {
    UiNodeSnapshot {
        control_type: control_type.to_string(),
        name: name.to_string(),
        rect,
        is_enabled: true,
        is_offscreen: false,
        ..Default::default()
    }
}

#[test]
fn should_auto_launch_qq_guards_sandbox_and_mock_classes() {
    let base_cfg = memory_test_config();
    assert!(should_auto_launch_qq(false, &base_cfg));
    assert!(!should_auto_launch_qq(true, &base_cfg));

    let disabled_cfg = AdapterConfig {
        qq_client: QqClientConfig {
            auto_launch_or_wake: false,
            ..Default::default()
        },
        ..memory_test_config()
    };
    assert!(!should_auto_launch_qq(false, &disabled_cfg));

    let mock_cfg = AdapterConfig {
        interaction: InteractionConfig {
            extra_window_classes: vec![MOCK_QQ_MAIN_CLASS.to_string()],
            ..Default::default()
        },
        ..memory_test_config()
    };
    assert!(!should_auto_launch_qq(false, &mock_cfg));
}

#[tokio::test]
async fn status_populates_resolved_qq_path_from_configured_file() {
    let temp_exe = std::env::temp_dir().join(format!(
        "qqnt_driver_resolved_stub_{}.exe",
        std::process::id()
    ));
    std::fs::write(&temp_exe, b"stub").expect("write temp stub exe");
    let temp_str = temp_exe.to_string_lossy().to_string();

    let cfg = AdapterConfig {
        qq_client: QqClientConfig {
            executable_path: Some(temp_str.clone()),
            auto_launch_or_wake: false,
            ..Default::default()
        },
        ..memory_test_config()
    };
    let driver = QqntDriver::new(cfg).expect("create driver");
    let status = driver.status().await.expect("query status");
    assert_eq!(status.resolved_qq_path.as_deref(), Some(temp_str.as_str()));

    drop(driver);
    let _ = std::fs::remove_file(&temp_exe);
}

#[test]
fn is_narrow_login_window_distinguishes_splash_main_and_iconic() {
    assert!(is_narrow_login_window(&make_test_window(
        [100, 100, 420, 548],
        false
    )));
    assert!(!is_narrow_login_window(&make_test_window(
        [100, 100, 600, 548],
        false
    )));
    assert!(!is_narrow_login_window(&make_test_window(
        [0, 0, 960, 720],
        false
    )));
    assert!(!is_narrow_login_window(&make_test_window(
        [-32000, -32000, -31840, -31972],
        true
    )));
    assert!(!is_narrow_login_window(&make_test_window(
        [0, 0, 0, 0],
        false
    )));
}

#[test]
fn nodes_look_like_login_prompt_detects_login_button_without_sessions() {
    let narrow = make_test_window([100, 100, 420, 548], false);
    assert!(nodes_look_like_login_prompt(&narrow, &[]));

    let wide = make_test_window([0, 0, 800, 640], false);
    let center_login_btn = make_test_node("Button", "登录", [350, 400, 450, 440]);
    assert!(nodes_look_like_login_prompt(&wide, &[&center_login_btn]));

    let left_login_btn = make_test_node("Button", "扫码登录", [100, 320, 240, 360]);
    assert!(nodes_look_like_login_prompt(&wide, &[&left_login_btn]));

    let left_session = make_test_node("ListItem", "Demo测试群 (5)", [20, 120, 280, 170]);
    assert!(!nodes_look_like_login_prompt(
        &wide,
        &[&center_login_btn, &left_session]
    ));

    let non_login_btn = make_test_node("Button", "设置", [350, 400, 450, 440]);
    assert!(!nodes_look_like_login_prompt(&wide, &[&non_login_btn]));
    assert!(!nodes_look_like_login_prompt(&wide, &[]));
}

#[tokio::test]
async fn discover_windows_uses_explicit_extra_window_classes_config() {
    let _mock_lock = UNIT_MOCK_LOCK.lock().await;
    let mock = mock_qq_win::MockQqServer::spawn().expect("spawn mock qq");

    let empty_extra_cfg = AdapterConfig {
        qq_client: QqClientConfig {
            auto_launch_or_wake: false,
            ..Default::default()
        },
        ..memory_test_config()
    };
    let driver_without_mock = QqntDriver::new(empty_extra_cfg).expect("driver without extra");
    let windows_without = driver_without_mock
        .list_windows()
        .await
        .expect("list windows without extra");
    assert!(
        !windows_without.iter().any(|win| win.hwnd == mock.hwnd()),
        "MockQQNTWndClass should not be discovered when extra_window_classes is empty"
    );
    drop(driver_without_mock);

    let mock_extra_cfg = AdapterConfig {
        qq_client: QqClientConfig {
            auto_launch_or_wake: false,
            ..Default::default()
        },
        interaction: InteractionConfig {
            extra_window_classes: vec![MOCK_QQ_MAIN_CLASS.to_string()],
            ..Default::default()
        },
        ..memory_test_config()
    };
    let driver_with_mock = QqntDriver::new(mock_extra_cfg).expect("driver with extra");
    let windows_with = driver_with_mock
        .list_windows()
        .await
        .expect("list windows with extra");
    assert!(
        windows_with.iter().any(|win| win.hwnd == mock.hwnd()),
        "MockQQNTWndClass should be discovered when listed in extra_window_classes"
    );
    drop(driver_with_mock);
    drop(mock);
}

#[tokio::test]
async fn diagnose_window_and_ensure_ext_work_on_mock_window() {
    let _mock_lock = UNIT_MOCK_LOCK.lock().await;
    let mock = mock_qq_win::MockQqServer::spawn().expect("spawn mock qq");

    let cfg = AdapterConfig {
        qq_client: QqClientConfig {
            auto_launch_or_wake: false,
            ..Default::default()
        },
        interaction: InteractionConfig {
            extra_window_classes: vec![MOCK_QQ_MAIN_CLASS.to_string()],
            ..Default::default()
        },
        ..memory_test_config()
    };
    let driver = QqntDriver::new(cfg).expect("create driver");
    let windows = driver
        .ensure_qq_window_ext(true)
        .await
        .expect("ensure_qq_window_ext");
    assert!(windows.iter().any(|win| win.hwnd == mock.hwnd()));

    let report = driver
        .diagnose_window(Some(mock.hwnd()))
        .await
        .expect("diagnose_window");
    assert_eq!(report.target_hwnd, mock.hwnd());
    assert!(report.uia_control_count >= 5);
    assert!(report.uia_semantic_count > 0);
    assert!(report.uia_raw_count >= report.uia_control_count);
    assert!(!report.wake_report.target_hwnds_after.is_empty());

    drop(driver);
    drop(mock);
}

#[tokio::test]
async fn verified_window_identity_binds_mock_qq_only_when_allowed() {
    let _mock_lock = UNIT_MOCK_LOCK.lock().await;
    let mock = mock_qq_win::MockQqServer::spawn().expect("spawn mock qq");

    let rejected = VerifiedWindowIdentity::verify_and_bind(mock.hwnd(), false);
    assert!(
        rejected.is_err(),
        "production mode without allow_mock must reject MockQQNTWndClass"
    );

    let bound = VerifiedWindowIdentity::verify_and_bind(mock.hwnd(), true)
        .expect("allow_mock=true should bind mock window identity");
    assert_eq!(bound.hwnd(), mock.hwnd());
    bound
        .verify_fast()
        .expect("fast O(1) verification must pass for live mock HWND");

    drop(mock);
}

#[test]
fn login_prompt_detection_skips_transient_narrow_window_when_main_window_is_logged_in() {
    let mut narrow_transient = make_test_window([100, 100, 420, 548], false);
    narrow_transient.hwnd = 2001;
    let mut main_logged_in = make_test_window([0, 0, 1068, 779], false);
    main_logged_in.hwnd = 2002;
    let left_session = make_test_node("ListItem", "Demo测试群 (5)", [20, 120, 280, 170]);

    let main_hwnd = main_logged_in.hwnd;
    let inspect = |win: &WindowInfo| -> bool {
        if win.hwnd == main_hwnd {
            nodes_look_like_login_prompt(win, &[&left_session])
        } else {
            nodes_look_like_login_prompt(win, &[])
        }
    };

    assert!(!all_non_standalone_windows_are_login_prompts(
        &[narrow_transient.clone(), main_logged_in.clone()],
        inspect
    ));
    assert!(!all_non_standalone_windows_are_login_prompts(
        &[main_logged_in, narrow_transient],
        inspect
    ));
}

#[test]
fn login_prompt_detection_triggers_for_lone_narrow_or_explicit_qr_prompt() {
    let mut narrow_login = make_test_window([100, 100, 420, 548], false);
    narrow_login.hwnd = 3001;
    assert!(all_non_standalone_windows_are_login_prompts(
        &[narrow_login.clone()],
        |win| nodes_look_like_login_prompt(win, &[])
    ));

    let mut wide_qr_login = make_test_window([0, 0, 800, 640], false);
    wide_qr_login.hwnd = 3002;
    let qr_login_btn = make_test_node("Button", "扫码登录", [320, 420, 480, 460]);
    assert!(all_non_standalone_windows_are_login_prompts(
        &[wide_qr_login.clone()],
        |win| nodes_look_like_login_prompt(win, &[&qr_login_btn])
    ));
    assert!(all_non_standalone_windows_are_login_prompts(
        &[narrow_login, wide_qr_login],
        |win| nodes_look_like_login_prompt(win, &[&qr_login_btn])
    ));

    let mut standalone_chat = make_test_window([200, 200, 900, 760], false);
    standalone_chat.hwnd = 3003;
    standalone_chat.is_standalone_chat = true;
    assert!(!all_non_standalone_windows_are_login_prompts(&[], |win| {
        nodes_look_like_login_prompt(win, &[])
    }));
    assert!(!all_non_standalone_windows_are_login_prompts(
        &[standalone_chat],
        |win| nodes_look_like_login_prompt(win, &[])
    ));
}

#[test]
fn qqnt_150_dpi_composer_excludes_member_sidebar_decoys_and_selects_disabled_send() {
    let logical_win_rect = [450, 84, 1518, 863];
    let root = UiNodeSnapshot {
        control_type: "Pane".to_string(),
        name: "QQ".to_string(),
        class_name: "Chrome_WidgetWin_1".to_string(),
        rect: [675, 126, 2277, 1295],
        is_enabled: true,
        ..Default::default()
    };
    let member_sidebar = UiNodeSnapshot {
        control_type: "Window".to_string(),
        name: "群成员列表".to_string(),
        rect: [2018, 520, 2265, 1280],
        is_enabled: true,
        ..Default::default()
    };
    let decoy_sidebar_edit = UiNodeSnapshot {
        control_type: "Edit".to_string(),
        name: "".to_string(),
        rect: [2100, 1100, 2130, 1127],
        is_enabled: true,
        ..Default::default()
    };
    let composer = UiNodeSnapshot {
        control_type: "Group".to_string(),
        name: "Demo测试群".to_string(),
        value: Some("按住 Win + Alt，使用语音输入文字\n".to_string()),
        rect: [1134, 1038, 2001, 1205],
        is_enabled: true,
        ..Default::default()
    };
    let send_btn_disabled = UiNodeSnapshot {
        control_type: "Button".to_string(),
        name: "发送".to_string(),
        rect: [1854, 1226, 1896, 1268],
        is_enabled: false,
        ..Default::default()
    };

    let snaps = [
        &root,
        &member_sidebar,
        &decoy_sidebar_edit,
        &composer,
        &send_btn_disabled,
    ];
    let (editor_idx, send_idx) = locate_composer_and_send_indices(&snaps, logical_win_rect, false)
        .expect("should locate live QQNT 150%-DPI composer and send button");
    assert_eq!(editor_idx, 3);
    assert_eq!(send_idx, Some(4));
    assert!(profile::is_empty_composer_text(
        composer.value.as_deref().unwrap_or("")
    ));
    assert!(!profile::is_empty_composer_text("ping"));
}

#[test]
fn nodes_look_like_login_prompt_respects_150_dpi_session_list() {
    let win = make_test_window([450, 84, 1518, 863], false);
    let root = UiNodeSnapshot {
        control_type: "Pane".to_string(),
        name: "QQ".to_string(),
        class_name: "Chrome_WidgetWin_1".to_string(),
        rect: [675, 126, 2277, 1295],
        is_enabled: true,
        ..Default::default()
    };
    let login_btn = UiNodeSnapshot {
        control_type: "Button".to_string(),
        name: "登录".to_string(),
        rect: [1150, 210, 1220, 244],
        is_enabled: true,
        ..Default::default()
    };
    let session_list = UiNodeSnapshot {
        control_type: "Window".to_string(),
        name: "会话列表".to_string(),
        rect: [759, 264, 1134, 1289],
        is_enabled: true,
        ..Default::default()
    };
    let row = UiNodeSnapshot {
        control_type: "Group".to_string(),
        name: "Demo测试群 23:25 群友A: ping".to_string(),
        rect: [765, 280, 1120, 355],
        is_enabled: true,
        ..Default::default()
    };
    let snaps = [&root, &login_btn, &session_list, &row];
    assert!(!nodes_look_like_login_prompt(&win, &snaps));
}

#[test]
fn editor_value_matches_norm_avoids_placeholder_and_matches_normalized_target() {
    let target = "[qq-uia-adapter] V1 UIA 适配连接测试成功";
    let norm = parser::normalize_message_text(target);
    assert!(editor_value_matches_norm(target, &norm));
    assert!(editor_value_matches_norm(
        "[qq-uia-adapter] V1 UIA 适配连接测试成功\n\u{00a0}",
        &norm
    ));
    assert!(!editor_value_matches_norm(
        "按住 Win + Alt，使用语音输入文字\n",
        &norm
    ));
    assert!(!editor_value_matches_norm(
        "按住 Win + Alt，使用语音输入文字",
        "按住 Win + Alt，使用语音输入文字"
    ));
    assert!(!editor_value_matches_norm("partial draft", &norm));
}

#[test]
fn focus_restore_outcome_and_counter_enforce_truthfulness() {
    let cfg = memory_test_config();
    let storage = Arc::new(Storage::open(&cfg.storage.db_path).expect("open memory db"));
    let (event_tx, _) = broadcast::channel(8);
    let shared = Arc::new(SharedInner::new(cfg.clone(), storage, event_tx));
    let req = SendRequest {
        group: "Demo测试群".to_string(),
        text: "unverified msg".to_string(),
        mode: Some(InputMode::FocusRestoreUnicode),
        ..Default::default()
    };

    // 未证实的前台恢复发送必须返回 ok=false、不累计发信次数并回滚防重放缓存
    shared.tracker.lock().record_outbound(&req.text);
    let unverified = build_focus_restore_outcome(&FocusRestoreOutcomeParams {
        group: &req.group,
        foreground_touched: true,
        mouse_touched: false,
        verified: false,
        elapsed_ms: 420,
        verified_detail: "sent via focus-restore Unicode SendInput and verified in chat history",
        unverified_detail: "focus-restore Unicode SendInput could not be verified in chat history",
    });
    assert!(!unverified.ok);
    assert!(!unverified.verified_in_chat);
    assert!(unverified.detail.contains("could not be verified"));

    let out = finalize_send_result(&shared, &req, Some(5), Ok(unverified)).expect("ok result");
    assert!(!out.ok);
    assert_eq!(out.onebot_message_id, None);
    assert_eq!(shared.status_snapshot().messages_sent, 0);
    let probe = vec![ChatMessage {
        message_id: "m1".to_string(),
        group_name: req.group.clone(),
        sender: "Unknown".to_string(),
        content: req.text.clone(),
        timestamp_hint: None,
        is_self: false,
        captured_at_ms: 0,
        ..Default::default()
    }];
    let annotated = shared.tracker.lock().annotate_only(probe);
    assert!(
        !annotated[0].is_self,
        "rolled-back unverified outbound text must not suppress future detection"
    );

    // 经历史回显验证成功的发送必须累计发信计数并落盘返回 onebot_message_id 与 group_id
    shared.tracker.lock().record_outbound(&req.text);
    let verified = build_focus_restore_outcome(&FocusRestoreOutcomeParams {
        group: &req.group,
        foreground_touched: true,
        mouse_touched: false,
        verified: true,
        elapsed_ms: 150,
        verified_detail: "sent via focus-restore Unicode SendInput and verified in chat history",
        unverified_detail: "focus-restore Unicode SendInput could not be verified in chat history",
    });
    assert!(verified.ok && verified.verified_in_chat);
    let out_ok = finalize_send_result(&shared, &req, Some(5), Ok(verified)).expect("ok result");
    assert!(out_ok.ok);
    assert!(out_ok.onebot_message_id.is_some());
    assert!(out_ok.group_id.is_some());
    assert_eq!(shared.status_snapshot().messages_sent, 1);
}

#[test]
fn pre_send_chat_guard_writes_audit_and_aborts_on_mismatch_or_member_overflow() {
    let storage = Storage::open(MEMORY_DB_PATH).expect("open memory sqlite");
    let win_rect = [0, 0, 1000, 800];
    let spec = GroupTargetSpec {
        group_name: "Demo测试群".to_string(),
        strict_below_members: Some(10),
        require_member_count_verified: false,
        ..Default::default()
    };

    let matched_header = make_test_node("Text", "Demo测试群 (5)", [360, 20, 620, 56]);
    let matched_check = PreSendGuardCheck {
        expected_group: "Demo测试群",
        win_rect,
        is_standalone: false,
        target_spec: &spec,
    };
    verify_pre_send_chat_guard_snapshots(&storage, &[&matched_header], &matched_check)
        .expect("matching header below 10 members should pass guard");
    assert_eq!(storage.stats().expect("stats").audit_count, 0);

    // 若当前聊天标题意外切换为无关群聊，守卫必须写入审计记录并立即拦截发送
    let foreign_header = make_test_node("Text", "无关工作群 (5)", [360, 20, 620, 56]);
    let mismatch_err =
        verify_pre_send_chat_guard_snapshots(&storage, &[&foreign_header], &matched_check)
            .expect_err("header mismatch must abort send");
    assert!(matches!(mismatch_err, AdapterError::InputRejected(_)));
    assert_eq!(storage.stats().expect("stats").audit_count, 1);

    // 若同一群人数意外膨胀到 10 人或以上，守卫必须触发 MemberCountExceeded 并写入审计
    let large_group_header = make_test_node("Text", "Demo测试群 (12)", [360, 20, 620, 56]);
    let count_err =
        verify_pre_send_chat_guard_snapshots(&storage, &[&large_group_header], &matched_check)
            .expect_err("member count >= 10 must abort send");
    assert!(matches!(
        count_err,
        AdapterError::MemberCountExceeded {
            detected: Some(12),
            limit: 10,
            ..
        }
    ));
    assert_eq!(storage.stats().expect("stats").audit_count, 2);
}

#[tokio::test]
async fn v2_storage_backed_onebot_queries_and_self_profile_work_end_to_end() {
    let cfg = AdapterConfig {
        qq_client: QqClientConfig {
            auto_launch_or_wake: false,
            ..Default::default()
        },
        bot: BotConfig {
            self_nicknames: vec!["默认昵称".to_string()],
            ..Default::default()
        },
        ..memory_test_config()
    };
    let driver = QqntDriver::new(cfg).expect("create v2 driver with memory sqlite");

    // 模拟从左侧栏头像节点被动感知到当前登录账号昵称
    let mut tree = UiNodeSnapshot {
        rect: [0, 0, 1000, 800],
        ..Default::default()
    };
    tree.children.push(UiNodeSnapshot {
        control_type: "Button".to_string(),
        name: "测试机器人的头像".to_string(),
        rect: [12, 24, 52, 64],
        is_enabled: true,
        ..Default::default()
    });
    driver.shared.register_tree_self_nicknames(&tree, tree.rect);

    let login = driver.get_login_info().await.expect("get_login_info");
    assert_eq!(login.nickname, "测试机器人");
    assert!(login.user_id > 0);

    // 同步会话摘要与群成员数据至 SQLite，验证 V2 群列表、群名称反查与成员查询接口
    let changed = driver.shared.upsert_session_summaries(&[SessionSummary {
        title: "Demo测试群 (5)".to_string(),
        raw_name: "Demo测试群 1 条未读".to_string(),
        preview: Some("群友A: ping".to_string()),
        unread_count: Some(1),
        member_count_hint: Some(5),
        rect: [60, 120, 300, 180],
        ..Default::default()
    }]);
    assert_eq!(changed, vec!["Demo测试群".to_string()]);

    let stored = driver
        .shared
        .storage
        .upsert_group(&UpsertGroupInput {
            group_name: "Demo测试群".to_string(),
            real_group_id: Some(987_654_321),
            member_count: Some(5),
            max_member_count: Some(200),
            last_preview: Some("群友A: ping".to_string()),
            unread_count: 1,
        })
        .expect("upsert group metadata");

    let groups = driver.get_group_list(false).await.expect("get_group_list");
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].group_name, "Demo测试群");
    assert_eq!(groups[0].group_id, 987_654_321);
    assert_eq!(groups[0].max_member_count, 200);

    let resolved_name = driver
        .resolve_group_name(stored.effective_group_id())
        .await
        .expect("resolve_group_name");
    assert_eq!(resolved_name.as_deref(), Some("Demo测试群"));

    driver
        .shared
        .storage
        .upsert_group_members(
            "Demo测试群",
            stored.effective_group_id(),
            &[
                UpsertMemberInput {
                    nickname: "测试机器人".to_string(),
                    role: "owner".to_string(),
                    ..Default::default()
                },
                UpsertMemberInput {
                    nickname: "群友A".to_string(),
                    role: "member".to_string(),
                    ..Default::default()
                },
            ],
        )
        .expect("upsert group members");

    let members = driver
        .get_group_member_list(Some(987_654_321), None, false)
        .await
        .expect("get_group_member_list");
    assert_eq!(members.len(), 2);

    // 验证一条确认发出的消息能立即通过 get_message 按数字 ID 溯源
    let (msg_id_opt, gid) = driver.shared.persist_confirmed_sent_message(
        "Demo测试群",
        ChatTargetKind::Group,
        "V2 SQLite 联调测试消息",
        &[],
        Some(5),
    );
    let msg_id = msg_id_opt.expect("persisted sent onebot_message_id");
    assert_eq!(gid, 987_654_321);

    let fetched = driver
        .get_message(msg_id)
        .await
        .expect("get_message")
        .expect("stored message exists");
    assert_eq!(fetched.onebot_message_id, msg_id);
    assert_eq!(fetched.group_name, "Demo测试群");
    assert_eq!(fetched.group_id, 987_654_321);
    assert_eq!(fetched.raw_message, "V2 SQLite 联调测试消息");
    assert!(fetched.is_self);

    let stats = driver.storage_stats().await.expect("storage_stats");
    assert_eq!(stats.group_count, 1);
    assert_eq!(stats.member_count, 2);
    assert_eq!(stats.message_count, 1);

    // 验证当附着群聊时，status_snapshot 与 get_group_info(None, None, false) 都能直接返回生效与真实群号及默认群容量
    {
        let mut guard = driver.shared.state.lock();
        guard.active_group = Some("Demo测试群 (5)".to_string());
        guard.detected_member_count = Some(5);
    }
    let enriched = driver.shared.status_snapshot();
    assert_eq!(enriched.active_group_id, Some(987_654_321));
    assert_eq!(enriched.active_real_group_id, Some(987_654_321));

    let info_by_default = driver
        .get_group_info(None, None, false)
        .await
        .expect("get_group_info fallback");
    assert_eq!(info_by_default.group_id, 987_654_321);
    assert_eq!(info_by_default.real_group_id, Some(987_654_321));
    assert_eq!(info_by_default.max_member_count, 200);
}

#[test]
fn poller_fallback_interval_distinguishes_fast_test_and_production() {
    assert_eq!(compute_poller_fallback_ms(10), 50);
    assert_eq!(compute_poller_fallback_ms(80), 80);
    assert_eq!(compute_poller_fallback_ms(499), 499);
    assert_eq!(compute_poller_fallback_ms(500), 2000);
    assert_eq!(compute_poller_fallback_ms(800), 2000);
    assert_eq!(compute_poller_fallback_ms(5000), 5000);
}

#[tokio::test]
async fn v3_enricher_resolves_at_reply_and_session_classification() {
    let driver = QqntDriver::new(memory_test_config()).expect("create driver");

    // 左侧会话摘要同步应自动区分群聊与私聊好友并分别写入 groups / friends 表
    let _ = driver.shared.upsert_session_summaries(&[
        SessionSummary {
            title: "Demo测试群 (5)".to_string(),
            raw_name: "Demo测试群".to_string(),
            preview: Some("群友A: 你好".to_string()),
            unread_count: Some(0),
            member_count_hint: Some(5),
            rect: [60, 120, 300, 180],
            is_group_hint: Some(true),
            ..Default::default()
        },
        SessionSummary {
            title: "测试好友".to_string(),
            raw_name: "测试好友".to_string(),
            preview: Some("私聊预览".to_string()),
            unread_count: Some(1),
            member_count_hint: None,
            rect: [60, 185, 300, 245],
            is_group_hint: None,
            ..Default::default()
        },
    ]);

    let groups = driver.get_group_list(false).await.expect("groups");
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].group_name, "Demo测试群");

    let friends = driver.get_friend_list(false).await.expect("friends");
    assert_eq!(friends.len(), 1);
    assert_eq!(friends[0].nickname, "测试好友");

    let gid = groups[0].group_id;
    driver
        .shared
        .storage
        .upsert_group_members(
            "Demo测试群",
            gid,
            &[UpsertMemberInput {
                nickname: "测试机器人".to_string(),
                real_user_id: Some(123_456_789),
                card: String::new(),
                role: "member".to_string(),
            }],
        )
        .expect("upsert member");

    let (quoted_id_opt, _) = driver.shared.persist_confirmed_sent_message(
        "Demo测试群",
        ChatTargetKind::Group,
        "上一条待引用消息",
        &[],
        Some(5),
    );
    let quoted_id = quoted_id_opt.expect("quoted message id");
    let bot_login = driver.get_login_info().await.expect("login");
    let members = driver
        .get_group_member_list(Some(gid), None, false)
        .await
        .expect("members");

    let mut parsed = vec![ParsedTreeMessage {
        message: ChatMessage {
            message_id: "v3_msg_1".to_string(),
            group_name: "Demo测试群".to_string(),
            sender: "群友A".to_string(),
            content: "「回复 @BotSelf: 上一条待引用消息」 @测试机器人 请看".to_string(),
            ..Default::default()
        },
        image_rects: Vec::new(),
        ..Default::default()
    }];
    let enrich_ctx = EnrichContext {
        storage: &driver.shared.storage,
        session_key: "Demo测试群",
        chat_kind: ChatTargetKind::Group,
        hwnd: 0,
        members: &members,
        bot_login: &bot_login,
    };
    enrich_parsed_messages(&enrich_ctx, &mut parsed);

    let segments = &parsed[0].message.segments;
    assert!(
        segments.contains(&MessageSegment::Reply {
            id: quoted_id.to_string(),
        }),
        "expected Reply segment referencing {quoted_id}, got: {segments:?}"
    );
    assert!(
        segments.contains(&MessageSegment::At {
            qq: "123456789".to_string(),
            name: Some("测试机器人".to_string()),
        }),
        "expected resolved At segment, got: {segments:?}"
    );
}

#[tokio::test]
async fn duplicate_outbound_messages_receive_distinct_onebot_ids() {
    let driver = QqntDriver::new(memory_test_config()).expect("create driver");
    let (id1, _) = driver.shared.persist_confirmed_sent_message(
        "Demo测试群",
        ChatTargetKind::Group,
        "收到",
        &[],
        Some(9),
    );
    let (id2, _) = driver.shared.persist_confirmed_sent_message(
        "Demo测试群",
        ChatTargetKind::Group,
        "收到",
        &[],
        Some(9),
    );

    let first_id = id1.expect("first outbound message id should be assigned");
    let second_id = id2.expect("second outbound message id should be assigned");
    assert!(
        second_id > first_id,
        "expected strictly increasing outbound message_id, got {first_id} then {second_id}"
    );
    let stats = driver.storage_stats().await.expect("storage stats");
    assert_eq!(stats.message_count, 2);
}

#[tokio::test]
async fn live_qqnt_session_strings_evict_groups_from_friends_and_keep_friend() {
    let mut cfg = memory_test_config();
    cfg.target.allowed_private_friends = vec!["测试好友A".to_string()];
    let driver = QqntDriver::new(cfg).expect("create driver");
    driver
        .shared
        .storage
        .upsert_friend(&UpsertFriendInput {
            nickname: "示例讨论群".to_string(),
            remark: String::new(),
            real_user_id: None,
            last_preview: None,
        })
        .expect("seed misclassified friend");
    driver
        .shared
        .storage
        .upsert_group(&UpsertGroupInput {
            group_name: "测试好友A".to_string(),
            real_group_id: None,
            member_count: None,
            max_member_count: None,
            last_preview: None,
            unread_count: 0,
        })
        .expect("seed misclassified friend inside groups");

    let win_rect = [275, 97, 1378, 876];
    let session_list = UiNodeSnapshot {
        control_type: "Window".to_string(),
        name: "会话列表".to_string(),
        rect: [330, 188, 580, 872],
        is_enabled: true,
        ..Default::default()
    };
    let row_group_1 = UiNodeSnapshot {
        control_type: "Group".to_string(),
        name: "示例讨论群\n00:42\n昔涟：\n[动画表情]\n26".to_string(),
        rect: [336, 200, 574, 260],
        is_enabled: true,
        ..Default::default()
    };
    let row_group_2 = UiNodeSnapshot {
        control_type: "Group".to_string(),
        name: "示例资料群\n10/18\n示例用户B：\n[动画表情]".to_string(),
        rect: [336, 265, 574, 325],
        is_enabled: true,
        ..Default::default()
    };
    let row_group_3 = UiNodeSnapshot {
        control_type: "Group".to_string(),
        name: "Demo测试群\n昨天22:19\n测试好友B\n回应了你的\n消息:".to_string(),
        rect: [336, 330, 574, 390],
        is_enabled: true,
        ..Default::default()
    };
    let row_friend = UiNodeSnapshot {
        control_type: "Group".to_string(),
        name: "测试好友A\n03/16".to_string(),
        rect: [336, 395, 574, 455],
        is_enabled: true,
        ..Default::default()
    };

    let snapshots = [
        &session_list,
        &row_group_1,
        &row_group_2,
        &row_group_3,
        &row_friend,
    ];
    let sessions = extract_sessions_from_snapshots(&snapshots, win_rect);
    assert_eq!(sessions.len(), 4);
    driver.shared.upsert_session_summaries(&sessions);

    let friends = driver.get_friend_list(false).await.expect("friends");
    assert_eq!(
        friends.len(),
        1,
        "expected only 测试好友A in friends: {friends:?}"
    );
    assert_eq!(friends[0].nickname, "测试好友A");

    let groups = driver.shared.storage.list_groups().expect("groups");
    assert!(
        !groups.iter().any(|group| group.group_name == "测试好友A"),
        "expected whitelisted friend '测试好友A' evicted from groups table: {groups:?}"
    );
    for expected in ["示例讨论群", "示例资料群", "Demo测试群"] {
        assert!(
            groups.iter().any(|group| group.group_name == expected),
            "expected '{expected}' in groups table: {groups:?}"
        );
    }
}

fn make_header_tree_snapshot(header_text: Option<&str>, with_ml: bool) -> UiNodeSnapshot {
    let mut children = Vec::new();
    if let Some(title) = header_text {
        children.push(make_test_node("Text", title, [340, 44, 580, 74]));
    }
    if with_ml {
        children.push(UiNodeSnapshot {
            control_type: "List".to_string(),
            name: "消息列表".to_string(),
            automation_id: "ml-root".to_string(),
            rect: [310, 100, 1020, 720],
            is_enabled: true,
            ..Default::default()
        });
    }
    UiNodeSnapshot {
        control_type: "Window".to_string(),
        name: "QQ".to_string(),
        rect: [0, 0, 1280, 900],
        is_enabled: true,
        children,
        ..Default::default()
    }
}

#[test]
fn reconcile_status_from_tree_tracks_container_kind_and_member_counts() {
    let cfg = AdapterConfig {
        target: GroupTargetSpec {
            allowed_private_friends: vec!["白名单好友".to_string()],
            ..Default::default()
        },
        ..memory_test_config()
    };
    let driver = QqntDriver::new(cfg).expect("create driver");
    let shared = &driver.shared;
    shared
        .storage
        .upsert_group(&UpsertGroupInput {
            group_name: "本地已知群".to_string(),
            real_group_id: None,
            member_count: Some(18),
            max_member_count: None,
            last_preview: None,
            unread_count: 0,
        })
        .expect("seed group");
    shared
        .storage
        .upsert_friend(&UpsertFriendInput {
            nickname: "本地好友".to_string(),
            remark: String::new(),
            real_user_id: None,
            last_preview: None,
        })
        .expect("seed friend");

    let win = make_test_window([0, 0, 1280, 900], false);
    // 1) 无消息列表容器时清空激活会话与人数但保留附着窗口
    {
        let mut guard = shared.state.lock();
        guard.active_group = Some("Demo测试群 (9)".to_string());
        guard.active_chat_kind = ChatTargetKind::Group;
        guard.detected_member_count = Some(9);
    }
    super::window_discovery::reconcile_status_from_tree(
        shared,
        win.clone(),
        &make_header_tree_snapshot(Some("空白页"), false),
    );
    let after_blank = shared.status_snapshot();
    assert!(after_blank.attached_window.is_some());
    assert!(after_blank.active_group.is_none());
    assert_eq!(after_blank.detected_member_count, None);

    // 2) 白名单私聊优先级与清空群人数
    super::window_discovery::reconcile_status_from_tree(
        shared,
        win.clone(),
        &make_header_tree_snapshot(Some("白名单好友"), true),
    );
    let after_private = shared.status_snapshot();
    assert_eq!(after_private.active_group.as_deref(), Some("白名单好友"));
    assert_eq!(
        after_private.active_chat_kind,
        Some(ChatTargetKind::Private)
    );
    assert_eq!(after_private.detected_member_count, None);

    // 3) 通过数据库命中群名并延续同标题人数、切换新无人数群时清零成员数
    super::window_discovery::reconcile_status_from_tree(
        shared,
        win.clone(),
        &make_header_tree_snapshot(Some("本地已知群 (18)"), true),
    );
    assert_eq!(shared.status_snapshot().detected_member_count, Some(18));
    super::window_discovery::reconcile_status_from_tree(
        shared,
        win.clone(),
        &make_header_tree_snapshot(Some("本地已知群"), true),
    );
    let kept_same = shared.status_snapshot();
    assert_eq!(kept_same.active_chat_kind, Some(ChatTargetKind::Group));
    assert_eq!(kept_same.detected_member_count, Some(18));

    // 4) 数据库命中私聊好友时切换为私聊类型；未知标题保留既有会话类型并清零非匹配群成员数
    super::window_discovery::reconcile_status_from_tree(
        shared,
        win.clone(),
        &make_header_tree_snapshot(Some("本地好友"), true),
    );
    assert_eq!(
        shared.status_snapshot().active_chat_kind,
        Some(ChatTargetKind::Private)
    );
    super::window_discovery::reconcile_status_from_tree(
        shared,
        win.clone(),
        &make_header_tree_snapshot(Some("未收录陌生好友"), true),
    );
    let kept_private = shared.status_snapshot();
    assert_eq!(kept_private.active_chat_kind, Some(ChatTargetKind::Private));
    assert_eq!(kept_private.detected_member_count, None);

    {
        let mut guard = shared.state.lock();
        guard.active_chat_kind = ChatTargetKind::Group;
        guard.active_group = Some("旧群 (11)".to_string());
        guard.detected_member_count = Some(11);
    }
    super::window_discovery::reconcile_status_from_tree(
        shared,
        win.clone(),
        &make_header_tree_snapshot(Some("未收录新群"), true),
    );
    let kept_group_cleared_count = shared.status_snapshot();
    assert_eq!(
        kept_group_cleared_count.active_chat_kind,
        Some(ChatTargetKind::Group)
    );
    assert_eq!(
        kept_group_cleared_count.active_group.as_deref(),
        Some("未收录新群")
    );
    assert_eq!(kept_group_cleared_count.detected_member_count, None);

    // 验证轮询辅助校验与被动失配状态校正
    let tree_other_group = make_header_tree_snapshot(Some("其他群 (33)"), true);
    let (missing, live_hdr, live_cnt, mismatch) =
        super::group_handlers::inspect_poll_tree_target(&tree_other_group, &win, "本地已知群");
    assert!(!missing);
    assert_eq!(live_hdr.as_deref(), Some("其他群 (33)"));
    assert_eq!(live_cnt, Some(33));
    assert!(mismatch);
    super::group_handlers::reconcile_poll_tree_mismatch(shared, &win, &tree_other_group, false);
    let after_mismatch = shared.status_snapshot();
    assert_eq!(after_mismatch.active_group.as_deref(), Some("其他群 (33)"));
    assert_eq!(after_mismatch.active_chat_kind, Some(ChatTargetKind::Group));
    assert_eq!(after_mismatch.detected_member_count, Some(33));
}
