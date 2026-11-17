//! MSAA 角色映射、可编辑输入框空文本防护与后台无障碍读写单元测试模块

use common_types::constants::ui_labels::UI_LABEL_SEND_BUTTON;
use common_types::AdapterError;
use windows::Win32::UI::Accessibility::ROLE_SYSTEM_TEXT;

use super::node_handle::{is_editable_role, MsaaRole};
use super::operations::{is_known_empty_placeholder, resolve_msaa_text};
use super::*;
use crate::action::TypingDelayRange;
use crate::constants::{
    ACTION_METHOD_MSAA_DEFAULT, CONTROL_TYPE_BUTTON, CONTROL_TYPE_EDIT, CONTROL_TYPE_GROUP,
    CONTROL_TYPE_LINK, CONTROL_TYPE_LIST_ITEM, CONTROL_TYPE_PANE, CONTROL_TYPE_TAB,
    CONTROL_TYPE_TAB_ITEM, CONTROL_TYPE_TEXT,
};
use crate::tree;

#[test]
fn role_system_text_distinguishes_editable_vs_readonly() {
    assert_eq!(
        map_system_role(ROLE_SYSTEM_TEXT as i32, 0),
        CONTROL_TYPE_EDIT
    );
    assert_eq!(
        map_system_role(ROLE_SYSTEM_TEXT as i32, STATE_SYSTEM_READONLY),
        CONTROL_TYPE_TEXT
    );
    assert_eq!(map_system_role(41, 0), CONTROL_TYPE_TEXT);
    assert_eq!(map_system_role(43, 0), CONTROL_TYPE_BUTTON);
    assert_eq!(map_system_role(34, 0), CONTROL_TYPE_LIST_ITEM);
    assert_eq!(map_system_role(30, 0), CONTROL_TYPE_LINK);
}

#[test]
fn custom_bstr_aria_roles_map_cleanly() {
    assert_eq!(map_custom_role("textbox"), CONTROL_TYPE_EDIT);
    assert_eq!(map_custom_role("SearchBox"), CONTROL_TYPE_EDIT);
    assert_eq!(map_custom_role("button"), CONTROL_TYPE_BUTTON);
    assert_eq!(map_custom_role("listitem"), CONTROL_TYPE_LIST_ITEM);
    assert_eq!(map_custom_role("option"), CONTROL_TYPE_LIST_ITEM);
    assert_eq!(map_custom_role("tab"), CONTROL_TYPE_TAB_ITEM);
    assert_eq!(map_custom_role("tablist"), CONTROL_TYPE_TAB);
    assert_eq!(map_custom_role("unknown_widget"), CONTROL_TYPE_PANE);

    assert!(is_editable_role(&MsaaRole::System(42), 0));
    assert!(!is_editable_role(
        &MsaaRole::System(42),
        STATE_SYSTEM_READONLY
    ));
    assert!(is_editable_role(
        &MsaaRole::Custom("textbox".to_string()),
        0
    ));
    assert!(is_editable_role(
        &MsaaRole::Custom("searchbox".to_string()),
        0
    ));
}

#[test]
fn read_msaa_text_never_falls_back_to_placeholder_name_for_editable_inputs() {
    // get_accValue 返回空字符串时应当直接得出 None，绝不回退到 accName 占位文案
    assert_eq!(resolve_msaa_text(true, Some(""), Some("输入消息")), None);
    // 非空 accValue 原样返回
    assert_eq!(
        resolve_msaa_text(true, Some("hello"), Some("输入消息")),
        Some("hello".to_string())
    );
    // 可编辑控件读取 accValue 失败或缺失时也返回 None，防止将搜索框占位文字误认为用户输入
    assert_eq!(resolve_msaa_text(true, None, Some("搜索")), None);

    // 不可编辑控件暴露了空 accValue 时同样返回 None
    assert_eq!(
        resolve_msaa_text(false, Some(""), Some(UI_LABEL_SEND_BUTTON)),
        None
    );
    // 仅当不可编辑控件缺少 accValue 时才回退到 accName
    assert_eq!(
        resolve_msaa_text(false, None, Some("Demo测试群")),
        Some("Demo测试群".to_string())
    );
    assert_eq!(
        resolve_msaa_text(false, Some("42"), Some("counter")),
        Some("42".to_string())
    );
}

#[test]
fn group_node_with_empty_acc_value_returns_none_instead_of_acc_name() {
    assert_eq!(map_system_role(20, 0), CONTROL_TYPE_GROUP);
    // 实盘 QQNT 9.9.31 的 Group 输入区在空内容时返回 get_accValue=Ok("") 且 name 为当前群名，必须映射为 None 以防误触发清空
    assert_eq!(resolve_msaa_text(false, Some(""), Some("Demo测试群")), None);
    assert_eq!(
        resolve_msaa_text(false, Some("草稿内容"), Some("Demo测试群")),
        Some("草稿内容".to_string())
    );
    assert!(is_known_empty_placeholder(""));
    assert!(is_known_empty_placeholder(
        "按住 Win + Alt，使用语音输入文字\n"
    ));
    assert!(!is_known_empty_placeholder("待发送内容"));
}

#[test]
fn msaa_walks_mock_qq_server_reads_writes_and_invokes_in_background() {
    let server = mock_qq_win::MockQqServer::spawn().expect("启动模拟 QQ 窗口");
    let window_hwnd = server.hwnd();
    let _session = crate::UiaSession::new().expect("初始化 UIA MTA COM 会话");

    let tree_snapshot = dump_tree_msaa(window_hwnd, 6).expect("采集 MSAA 分层树");
    assert!(tree::count_snapshot_nodes(&tree_snapshot) >= 5);

    let nodes = collect_flat_msaa(window_hwnd, 6).expect("采集 MSAA 平铺节点");
    assert!(nodes
        .iter()
        .any(|node| node.snapshot.control_type == CONTROL_TYPE_LIST_ITEM
            && node.snapshot.name.contains("Demo")));
    let editor = nodes
        .iter()
        .find(|node| node.snapshot.control_type == CONTROL_TYPE_EDIT)
        .expect("定位 MSAA 编辑框节点");
    let send_button = nodes
        .iter()
        .find(|node| {
            node.snapshot.control_type == CONTROL_TYPE_BUTTON
                && node.snapshot.name == UI_LABEL_SEND_BUTTON
        })
        .expect("定位 MSAA 发送按钮节点");

    assert!(tree::read_element_text(&editor.element).is_none());
    assert_eq!(
        tree::read_element_value_only(&editor.element).as_deref(),
        Some("")
    );

    let delay_range = TypingDelayRange {
        min_ms: 0,
        max_ms: 0,
    };
    let typed = crate::action::type_text_background(
        window_hwnd,
        &editor.element,
        "msaa hello",
        &delay_range,
    )
    .expect("执行 MSAA 与 PostMessage 后台输入");
    assert!(typed);
    assert_eq!(
        tree::read_element_value_only(&editor.element).as_deref(),
        Some("msaa hello")
    );
    assert_eq!(
        tree::read_element_text(&editor.element).as_deref(),
        Some("msaa hello")
    );

    let method =
        crate::action::invoke_element_background(&send_button.element).expect("后台触发发送按钮");
    assert_eq!(method, ACTION_METHOD_MSAA_DEFAULT);

    std::thread::sleep(std::time::Duration::from_millis(80));
    let messages = server.messages_for(&server.active_group());
    assert!(
        messages
            .iter()
            .any(|message| message.contains("msaa hello")),
        "活动群消息中应出现 'msaa hello'，实际消息列表: {messages:?}"
    );

    // 验证当编辑框不支持 put_accValue 清空时返回清晰错误而不发生 panic
    let clear_result = tree::clear_element_text(&editor.element);
    assert!(clear_result.is_ok() || matches!(clear_result, Err(AdapterError::UiaFailure(_))));

    server.shutdown();
}
