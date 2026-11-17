//! 输入动作跨目标隔离保护、参数对象边界与前景安全守护不变式单元测试模块

use std::thread;
use std::time::Duration;

use common_types::constants::ui_labels::UI_LABEL_SEND_BUTTON;
use common_types::{AdapterError, SendShortcut};

use super::background_ops::should_skip_msaa_put_acc_value;
use super::*;
use crate::constants::{
    CONTROL_TYPE_BUTTON, CONTROL_TYPE_DOCUMENT, CONTROL_TYPE_EDIT, CONTROL_TYPE_GROUP,
};
use crate::msaa;
use crate::win32::VerifiedWindowIdentity;

#[test]
fn msaa_group_skips_direct_put_acc_value_while_edit_keeps_it() {
    assert!(should_skip_msaa_put_acc_value(CONTROL_TYPE_GROUP));
    assert!(!should_skip_msaa_put_acc_value(CONTROL_TYPE_EDIT));
    assert!(!should_skip_msaa_put_acc_value(CONTROL_TYPE_DOCUMENT));
}

#[test]
fn verify_element_belongs_to_target_rejects_mismatched_identity_and_stale_window() {
    let server_a = mock_qq_win::MockQqServer::spawn().expect("启动模拟 QQ 窗口 A");
    let server_b = mock_qq_win::MockQqServer::spawn().expect("启动模拟 QQ 窗口 B");

    let identity_a =
        VerifiedWindowIdentity::verify_and_bind(server_a.hwnd(), true).expect("绑定窗口 A");
    let _session = crate::UiaSession::new().expect("初始化 UIA MTA 会话");

    let nodes_a = msaa::collect_flat_msaa(server_a.hwnd(), 6).expect("采集窗口 A 节点");
    let nodes_b = msaa::collect_flat_msaa(server_b.hwnd(), 6).expect("采集窗口 B 节点");

    let editor_a = nodes_a
        .iter()
        .find(|node| node.snapshot.control_type == CONTROL_TYPE_EDIT)
        .expect("窗口 A 编辑框");
    let editor_b = nodes_b
        .iter()
        .find(|node| node.snapshot.control_type == CONTROL_TYPE_EDIT)
        .expect("窗口 B 编辑框");
    let send_button_b = nodes_b
        .iter()
        .find(|node| {
            node.snapshot.control_type == CONTROL_TYPE_BUTTON
                && node.snapshot.name == UI_LABEL_SEND_BUTTON
        })
        .expect("窗口 B 发送按钮");

    verify_element_belongs_to_target(&identity_a, &editor_a.element)
        .expect("窗口 A 自身节点必须通过身份校验");

    let instant_delay = TypingDelayRange::instant();
    assert!(matches!(
        verify_element_belongs_to_target(&identity_a, &editor_b.element),
        Err(AdapterError::WindowNotFound(_))
    ));
    assert!(matches!(
        type_text_background_verified(&identity_a, &editor_b.element, "blocked", &instant_delay),
        Err(AdapterError::WindowNotFound(_))
    ));
    assert!(matches!(
        invoke_element_background_verified(&identity_a, &send_button_b.element),
        Err(AdapterError::WindowNotFound(_))
    ));
    assert!(matches!(
        send_submit_background_for_editor_verified(
            &identity_a,
            Some(&editor_b.element),
            Some(&send_button_b.element),
            SendShortcut::Enter,
        ),
        Err(AdapterError::WindowNotFound(_))
    ));

    let mismatched_target = ComposerSubmitTarget {
        window_hwnd: server_a.hwnd(),
        editor: &editor_b.element,
        send_button: Some(&send_button_b.element),
        shortcut: SendShortcut::Enter,
    };
    assert!(matches!(
        type_and_submit_focus_restore_verified(
            &identity_a,
            &mismatched_target,
            "blocked",
            &instant_delay,
        ),
        Err(AdapterError::WindowNotFound(_))
    ));

    let wrong_hwnd_target = ComposerSubmitTarget {
        window_hwnd: server_b.hwnd(),
        editor: &editor_a.element,
        send_button: None,
        shortcut: SendShortcut::Enter,
    };
    assert!(matches!(
        type_and_submit_focus_restore_verified(
            &identity_a,
            &wrong_hwnd_target,
            "blocked",
            &instant_delay,
        ),
        Err(AdapterError::WindowNotFound(_))
    ));

    assert!(matches!(
        clear_editor_focus_restore_verified(&identity_a, &editor_b.element),
        Err(AdapterError::WindowNotFound(_))
    ));

    server_a.shutdown();
    assert!(matches!(
        verify_element_belongs_to_target(&identity_a, &editor_a.element),
        Err(AdapterError::WindowNotFound(_))
    ));

    server_b.shutdown();
}

#[test]
fn focus_restore_aborts_on_invalid_hwnd_and_enforces_foreground_safety_invariant() {
    let server = mock_qq_win::MockQqServer::spawn().expect("启动模拟 QQ 窗口");
    let window_hwnd = server.hwnd();
    let _session = crate::UiaSession::new().expect("初始化 UIA MTA 会话");
    let nodes = msaa::collect_flat_msaa(window_hwnd, 6).expect("采集模拟窗口 MSAA 节点");
    let editor = nodes
        .iter()
        .find(|node| node.snapshot.control_type == CONTROL_TYPE_EDIT)
        .expect("定位编辑框控件");
    let send_button = nodes
        .iter()
        .find(|node| {
            node.snapshot.control_type == CONTROL_TYPE_BUTTON
                && node.snapshot.name == UI_LABEL_SEND_BUTTON
        })
        .expect("定位发送按钮控件");

    let instant_delay = TypingDelayRange::instant();
    let invalid_target = ComposerSubmitTarget {
        window_hwnd: 0,
        editor: &editor.element,
        send_button: Some(&send_button.element),
        shortcut: SendShortcut::Enter,
    };
    let invalid = type_and_submit_focus_restore(&invalid_target, "should fail", &instant_delay);
    assert!(matches!(invalid, Err(AdapterError::WindowNotFound(_))));

    let valid_target = ComposerSubmitTarget {
        window_hwnd,
        editor: &editor.element,
        send_button: Some(&send_button.element),
        shortcut: SendShortcut::Enter,
    };
    match type_and_submit_focus_restore(&valid_target, "fg-restore-ok", &instant_delay) {
        Ok((foreground_touched, mouse_touched)) => {
            assert!(foreground_touched);
            assert!(!mouse_touched);
            thread::sleep(Duration::from_millis(80));
            let messages = server.messages_for(&server.active_group());
            assert!(
                messages
                    .iter()
                    .any(|message| message.contains("fg-restore-ok")),
                "活动群消息中应出现 'fg-restore-ok'，实际内容: {messages:?}"
            );
        }
        Err(AdapterError::UiaFailure(message)) => {
            assert!(
                message.contains("SendInput"),
                "未预期的 UIA 失败消息: {message}"
            );
        }
        Err(other) => panic!("未预期的错误变体: {other:?}"),
    }

    server.shutdown();
}

#[test]
fn foreground_restore_guard_predicates_and_capture_safety_work() {
    assert!(!super::focus_guard::should_capture_foreground_pid(
        0,
        &[100]
    ));
    assert!(!super::focus_guard::should_capture_foreground_pid(
        100,
        &[100, 200]
    ));
    assert!(super::focus_guard::should_capture_foreground_pid(
        300,
        &[100, 200]
    ));

    assert!(super::focus_guard::should_restore_for_current_pid(
        true,
        0,
        &[100]
    ));
    assert!(super::focus_guard::should_restore_for_current_pid(
        false,
        100,
        &[100, 200]
    ));
    assert!(!super::focus_guard::should_restore_for_current_pid(
        false,
        300,
        &[100, 200]
    ));
    assert!(!super::focus_guard::should_restore_for_current_pid(
        false,
        0,
        &[100]
    ));

    let excluded_self = super::ForegroundRestoreGuard::capture(&[std::process::id()]);
    let _ = excluded_self.captured_hwnd();
    drop(excluded_self);

    let capture_any = super::ForegroundRestoreGuard::capture(&[]);
    let _ = capture_any.captured_hwnd();
    drop(capture_any);
}

#[test]
fn type_text_focus_restore_in_active_editor_rejects_null_hwnd_before_sending_keys() {
    let instant = TypingDelayRange::instant();
    assert!(super::type_text_focus_restore_in_active_editor(0, "", &instant, None).is_ok());

    let single_err = super::type_text_focus_restore_in_active_editor(0, "abc", &instant, None);
    assert!(single_err.is_err());

    let multiline_err =
        super::type_text_focus_restore_in_active_editor(0, "第一行\r\n第二行", &instant, None);
    assert!(multiline_err.is_err());
}

#[test]
fn normalize_line_endings_and_native_edit_classification_work() {
    assert_eq!(
        super::background_ops::normalize_line_endings("a\r\nb\rc\nd"),
        "a\nb\nc\nd"
    );
    assert!(super::background_ops::is_native_edit_descriptor(
        crate::constants::CONTROL_TYPE_EDIT,
        Some("Edit")
    ));
    assert!(!super::background_ops::is_native_edit_descriptor(
        "Group",
        Some("Group")
    ));
    assert!(!super::background_ops::is_native_edit_descriptor(
        crate::constants::CONTROL_TYPE_EDIT,
        Some("Chrome_RenderWidgetHostHWND")
    ));
    assert!(!super::background_ops::is_native_edit_descriptor(
        crate::constants::CONTROL_TYPE_EDIT,
        None
    ));
    assert!(!super::background_ops::is_native_edit_descriptor(
        "Document", None
    ));

    // 防止 Chromium 异步刷新导致的双倍重复回显通过子串校验误判成功
    assert!(super::background_ops::text_matches_expected(
        "[LIVE-06-FWD-2]",
        "[LIVE-06-FWD-2]"
    ));
    assert!(super::background_ops::text_matches_expected(
        "[LIVE-06-FWD-2]\n",
        "[LIVE-06-FWD-2]"
    ));
    assert!(!super::background_ops::text_matches_expected(
        "[LIVE-06-FWD-2][LIVE-06-FWD-2]",
        "[LIVE-06-FWD-2]"
    ));
    assert!(!super::background_ops::text_matches_expected(
        "输入消息",
        "[LIVE-06-FWD-2]"
    ));
}
