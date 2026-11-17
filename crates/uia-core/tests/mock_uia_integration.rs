//! 模拟 QQ 窗口与 UIA/MSAA 后台采集、群切换、消息输入发送及风控弹窗检测端到端集成测试

use std::thread;
use std::time::Duration;

use common_types::constants::processes::{MOCK_QQ_MAIN_CLASS, QQ_WINDOW_TITLE};
use common_types::constants::ui_labels::{SECURITY_KEYWORDS, UI_LABEL_SEND_BUTTON};
use common_types::SendShortcut;
use mock_qq_win::MockQqServer;
use parking_lot::Mutex;
use uia_core::action::post_chars_background;
use uia_core::constants::{
    CONTROL_TYPE_BUTTON, CONTROL_TYPE_EDIT, CONTROL_TYPE_LIST, CONTROL_TYPE_LIST_ITEM,
    CONTROL_TYPE_TEXT,
};
use uia_core::{
    current_desktop_info, enumerate_candidate_windows, find_windows_with_keywords,
    invoke_element_background, is_user_mouse_busy, send_submit_background, type_text_background,
    TypingDelayRange, UiaSession,
};
use windows::Win32::Foundation::POINT;
use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, GetForegroundWindow};

/// 串行化依赖共享桌面前台与鼠标状态快照的集成测试全局互斥锁
static TEST_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn uia_captures_mock_tree_switches_group_and_sends_in_background() {
    let _guard = TEST_LOCK.lock();

    let (desktop_name, _isolated) = current_desktop_info();
    assert!(!desktop_name.is_empty());

    let mock_server = MockQqServer::spawn().expect("启动模拟 QQ 窗口");
    let window_hwnd = mock_server.hwnd();

    let windows = enumerate_candidate_windows(&[], &[MOCK_QQ_MAIN_CLASS.to_string()]);
    let found = windows
        .iter()
        .find(|window| window.hwnd == window_hwnd)
        .expect("枚举候选窗口应当包含模拟主窗口");
    assert_eq!(found.class_name, MOCK_QQ_MAIN_CLASS);
    assert_eq!(found.title, QQ_WINDOW_TITLE);
    assert!(!found.is_standalone_chat);
    assert!(found.is_visible);

    let session = UiaSession::new().expect("创建 UIA MTA 会话");
    let tree = session.dump_tree(window_hwnd, 4).expect("采集控件树快照");
    assert!(!tree.children.is_empty());

    let nodes = session
        .collect_flat(window_hwnd, 4)
        .expect("采集平铺控件列表");
    assert!(
        nodes
            .iter()
            .any(|node| node.snapshot.control_type == CONTROL_TYPE_LIST),
        "模拟控件树中应当包含 List 控件"
    );
    assert!(
        nodes
            .iter()
            .any(|node| node.snapshot.control_type == CONTROL_TYPE_EDIT),
        "模拟控件树中应当包含 Edit 控件"
    );
    assert!(
        nodes.iter().any(|node| {
            node.snapshot.control_type == CONTROL_TYPE_BUTTON
                && node.snapshot.name == UI_LABEL_SEND_BUTTON
        }),
        "模拟控件树中应当包含发送按钮"
    );

    // 通过后台 UIA 模式切换至 Demo测试群 (5)
    let target_group_item = nodes
        .iter()
        .find(|node| {
            node.snapshot.control_type == CONTROL_TYPE_LIST_ITEM
                && node.snapshot.name.contains("Demo测试群 (5)")
        })
        .expect("定位 Demo测试群 (5) 列表项");
    let method = invoke_element_background(&target_group_item.element)
        .expect("后台触发切换至 Demo测试群 (5)");
    assert!(!method.is_empty());

    thread::sleep(Duration::from_millis(80));
    assert_eq!(mock_server.active_group(), "Demo测试群 (5)");

    // 重新采集节点确认顶部静态标题文本同步更新
    let nodes_after_switch = session
        .collect_flat(window_hwnd, 4)
        .expect("群切换后重新采集平铺控件列表");
    assert!(
        nodes_after_switch.iter().any(|node| {
            node.snapshot.control_type == CONTROL_TYPE_TEXT
                && node.snapshot.name == "Demo测试群 (5)"
        }),
        "预期顶部 Static 标题更新为 Demo测试群 (5)"
    );

    let editor = nodes_after_switch
        .iter()
        .find(|node| node.snapshot.control_type == CONTROL_TYPE_EDIT)
        .expect("定位输入框 Edit 控件");
    let send_button = nodes_after_switch
        .iter()
        .find(|node| {
            node.snapshot.control_type == CONTROL_TYPE_BUTTON
                && node.snapshot.name == UI_LABEL_SEND_BUTTON
        })
        .expect("定位发送 Button 控件");

    let foreground_before = unsafe { GetForegroundWindow() }.0 as isize;
    let mut cursor_before = POINT::default();
    let got_cursor = unsafe { GetCursorPos(&mut cursor_before) }.is_ok();
    let typing_delay = TypingDelayRange::new(1, 2);

    let accepted = type_text_background(
        window_hwnd,
        &editor.element,
        "Hello UIA Background",
        &typing_delay,
    )
    .expect("执行后台文本输入");
    assert!(accepted, "模拟 EDIT 控件应当接受后台输入文本");

    send_submit_background(window_hwnd, Some(&send_button.element), SendShortcut::Enter)
        .expect("执行后台发送动作");

    thread::sleep(Duration::from_millis(80));

    let foreground_after = unsafe { GetForegroundWindow() }.0 as isize;
    if foreground_before != window_hwnd {
        assert_ne!(
            foreground_after, window_hwnd,
            "后台发送绝不能将模拟窗口窃取为系统前台"
        );
    }
    if got_cursor {
        let mut cursor_after = POINT::default();
        if unsafe { GetCursorPos(&mut cursor_after) }.is_ok() {
            assert_eq!(
                cursor_before.x, cursor_after.x,
                "物理鼠标 X 坐标不得发生位移"
            );
            assert_eq!(
                cursor_before.y, cursor_after.y,
                "物理鼠标 Y 坐标不得发生位移"
            );
        }
    }

    let messages = mock_server.messages_for("Demo测试群 (5)");
    assert!(
        messages
            .iter()
            .any(|message| message == "BotSelf [now]: Hello UIA Background"),
        "模拟历史记录中应包含已发送消息，实际内容: {messages:?}"
    );

    // 验证纯 WM_CHAR 后台路径同样能在无鼠标与焦点干预前提下正确送达 Unicode 中文消息
    let wm_char_accepted =
        post_chars_background(window_hwnd, &editor.element, "你好WM_CHAR", &typing_delay)
            .expect("执行 WM_CHAR 后台输入");
    assert!(wm_char_accepted, "WM_CHAR 路径应当正确填充 EDIT 控件");
    send_submit_background(window_hwnd, Some(&send_button.element), SendShortcut::Enter)
        .expect("WM_CHAR 输入后触发发送");
    thread::sleep(Duration::from_millis(60));
    let messages_after_wm_char = mock_server.messages_for("Demo测试群 (5)");
    assert!(
        messages_after_wm_char
            .iter()
            .any(|message| message == "BotSelf [now]: 你好WM_CHAR"),
        "模拟历史记录中应包含 WM_CHAR 中文消息，实际内容: {messages_after_wm_char:?}"
    );

    mock_server.shutdown();
}

#[test]
fn security_dialog_detection_and_mouse_idle_check() {
    let _guard = TEST_LOCK.lock();

    let mock_server = MockQqServer::spawn().expect("启动模拟 QQ 窗口");
    let pids = vec![std::process::id()];

    assert!(
        find_windows_with_keywords(&pids, SECURITY_KEYWORDS).is_empty(),
        "初始状态下不应存在安全验证弹窗"
    );

    mock_server.open_security_dialog("QQ安全验证");
    let hits = find_windows_with_keywords(&pids, SECURITY_KEYWORDS);
    assert!(
        hits.iter().any(|(_, title)| title.contains("安全验证")),
        "开启安全弹窗后应当准确检出，实际命中: {hits:?}"
    );

    mock_server.close_security_dialog();
    assert!(
        find_windows_with_keywords(&pids, SECURITY_KEYWORDS).is_empty(),
        "关闭安全弹窗后检测结果应当恢复为空"
    );

    let _busy = is_user_mouse_busy(5);
    mock_server.shutdown();
}
