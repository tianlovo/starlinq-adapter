//! 模拟 Win32 QQNT 窗口自检与多群未读 UIA 渲染单元测试模块

use std::ffi::c_void;
use std::thread;
use std::time::Duration;

use windows::Win32::Foundation::{HWND, RPC_E_CHANGED_MODE, S_FALSE, S_OK};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationSelectionItemPattern,
    TreeScope_Children, TreeScope_Descendants, UIA_ControlTypePropertyId, UIA_ListControlTypeId,
    UIA_ListItemControlTypeId, UIA_SelectionItemPatternId, UIA_TextControlTypeId,
};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowExW, SendMessageW, LB_GETCOUNT, LB_GETTEXT, LB_GETTEXTLEN,
};
use windows_core::PCWSTR;

use super::*;
use crate::mock_state::SharedMockState;
use crate::window_proc::to_wide;

fn run_with_uia<F>(action: F)
where
    F: FnOnce(&IUIAutomation),
{
    let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    let should_uninit = if hr == S_OK || hr == S_FALSE {
        true
    } else if hr == RPC_E_CHANGED_MODE {
        false
    } else {
        panic!("CoInitializeEx 失败: {hr:?}");
    };

    {
        let automation: IUIAutomation =
            unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }
                .expect("创建 CUIAutomation 实例");
        action(&automation);
    }

    if should_uninit {
        unsafe {
            CoUninitialize();
        }
    }
}

fn find_uia_elements_by_control_type(
    automation: &IUIAutomation,
    root: &IUIAutomationElement,
    scope: windows::Win32::UI::Accessibility::TreeScope,
    control_type_id: i32,
) -> Vec<IUIAutomationElement> {
    let condition = unsafe {
        automation
            .CreatePropertyCondition(UIA_ControlTypePropertyId, &VARIANT::from(control_type_id))
    }
    .expect("构造 UIA 控件类型条件");
    let array = unsafe { root.FindAll(scope, &condition) }.expect("执行 UIA FindAll");
    let len = unsafe { array.Length() }.expect("读取 UIA 元素数量");
    let mut elements = Vec::with_capacity(len.max(0) as usize);
    for idx in 0..len {
        if let Ok(elem) = unsafe { array.GetElement(idx) } {
            elements.push(elem);
        }
    }
    elements
}

fn uia_element_name(elem: &IUIAutomationElement) -> String {
    unsafe { elem.CurrentName() }
        .map(|bstr| bstr.to_string())
        .unwrap_or_default()
}

fn read_listbox_items_win32(hwnd: isize, control_id: i32) -> Vec<String> {
    let parent = HWND(hwnd as *mut c_void);
    let list_class = to_wide(constants::CLASS_LISTBOX);
    let mut child = unsafe { FindWindowExW(Some(parent), None, PCWSTR(list_class.as_ptr()), None) }
        .unwrap_or_default();
    while !child.0.is_null() {
        let id = unsafe { windows::Win32::UI::WindowsAndMessaging::GetDlgCtrlID(child) };
        if id == control_id {
            let count = unsafe { SendMessageW(child, LB_GETCOUNT, None, None) }.0;
            let mut items = Vec::with_capacity(count.max(0) as usize);
            for idx in 0..count {
                let len = unsafe {
                    SendMessageW(
                        child,
                        LB_GETTEXTLEN,
                        Some(windows::Win32::Foundation::WPARAM(idx as usize)),
                        None,
                    )
                }
                .0;
                if len >= 0 {
                    let mut buf = vec![0u16; (len as usize) + 2];
                    let copied = unsafe {
                        SendMessageW(
                            child,
                            LB_GETTEXT,
                            Some(windows::Win32::Foundation::WPARAM(idx as usize)),
                            Some(windows::Win32::Foundation::LPARAM(buf.as_mut_ptr() as isize)),
                        )
                    }
                    .0;
                    if copied >= 0 {
                        items.push(String::from_utf16_lossy(&buf[..copied as usize]));
                    }
                }
            }
            return items;
        }
        child =
            unsafe { FindWindowExW(Some(parent), Some(child), PCWSTR(list_class.as_ptr()), None) }
                .unwrap_or_default();
    }
    Vec::new()
}

#[test]
fn mock_qq_spawns_switches_and_injects_messages() {
    let mock_handle = MockQqServer::spawn().expect("spawn mock qq");
    assert_ne!(mock_handle.hwnd(), 0);
    assert_eq!(mock_handle.active_group(), "测试大群 (128)");

    let hoyo_small_messages = mock_handle.messages_for("Demo测试群 (5)");
    assert_eq!(hoyo_small_messages.len(), 1);
    assert!(hoyo_small_messages[0].contains("欢迎来到Demo测试群"));

    let hoyo_large_messages = mock_handle.messages_for("Demo测试群 (26)");
    assert_eq!(hoyo_large_messages.len(), 1);
    assert!(hoyo_large_messages[0].contains("26人大群测试"));

    mock_handle.inject_incoming_message("Demo测试群 (5)", "群友B [14:01]: 测试注入");
    let after_inject = mock_handle.messages_for("Demo测试群 (5)");
    assert_eq!(after_inject.len(), 2);
    assert!(after_inject[1].contains("测试注入"));
    assert_eq!(mock_handle.unread_for("Demo测试群 (5)"), 1);

    mock_handle.open_security_dialog("QQ安全验证");
    mock_handle.close_security_dialog();
    mock_handle.shutdown();
}

#[test]
fn shared_mock_state_tracks_large_groups_unread_and_mentions() {
    let mut state = SharedMockState::from_groups(vec![
        (
            "大群Alpha (520)".to_string(),
            vec!["群友1 [11:50]: 欢迎".to_string()],
        ),
        (
            "大群Beta (680)".to_string(),
            vec!["群友2 [11:55]: 收到".to_string()],
        ),
        (
            "大群Gamma (920)".to_string(),
            vec!["群友3 [11:58]: 大家好".to_string()],
        ),
    ]);
    assert_eq!(state.active_group, "大群Alpha (520)");
    assert_eq!(state.format_session_row("大群Beta (680)"), "大群Beta (680)");

    // 激活会话收到消息不计入未读，也不生成未读提醒预览
    state.record_incoming_batch(
        "大群Alpha (520)",
        &["群友1 [12:00]: @qq-bot 激活群内艾特".to_string()],
    );
    assert_eq!(state.unread_counts.get("大群Alpha (520)").copied(), None);
    assert_eq!(
        state.format_session_row("大群Alpha (520)"),
        "大群Alpha (520)"
    );

    // 非激活大群收到含 @qq-bot 与多行换行的批量消息时应生成单行 [有人@我] 预览与未读计数
    state.record_incoming_batch(
        "大群Beta (680)",
        &[
            "群友A [12:01]: @qq-bot 请查看".to_string(),
            "群友B [12:02]: 第一行\r\n第二行".to_string(),
        ],
    );
    assert_eq!(state.unread_counts.get("大群Beta (680)").copied(), Some(2));
    let beta_row = state.format_session_row("大群Beta (680)");
    assert!(!beta_row.contains('\n') && !beta_row.contains('\r'));
    assert_eq!(
        beta_row,
        "大群Beta (680) 12:00 [有人@我] 群友B [12:02]: 第一行 第二行 (2条未读)"
    );

    // 未读状态下继续涌入普通群聊水群消息时保留最高优先级的 [有人@我] 提示
    state.record_incoming_batch(
        "大群Beta (680)",
        &["群友C [12:03]: 水群刷屏消息".to_string()],
    );
    assert_eq!(state.unread_counts.get("大群Beta (680)").copied(), Some(3));
    assert_eq!(
        state.format_session_row("大群Beta (680)"),
        "大群Beta (680) 12:00 [有人@我] 群友C [12:03]: 水群刷屏消息 (3条未读)"
    );

    // 仅含 @全体成员 时标记 [有全体消息]，随后出现 @测试机器人 时升级为 [有人@我]
    state.record_incoming_batch(
        "大群Gamma (920)",
        &["管理员 [12:04]: @全体成员 今晚维护".to_string()],
    );
    assert_eq!(
        state.format_session_row("大群Gamma (920)"),
        "大群Gamma (920) 12:00 [有全体消息] 管理员 [12:04]: @全体成员 今晚维护 (1条未读)"
    );
    state.record_incoming_batch(
        "大群Gamma (920)",
        &["群友D [12:05]: @测试机器人 帮忙确认".to_string()],
    );
    assert_eq!(
        state.format_session_row("大群Gamma (920)"),
        "大群Gamma (920) 12:00 [有人@我] 群友D [12:05]: @测试机器人 帮忙确认 (2条未读)"
    );

    assert!(state.clear_unread("大群Beta (680)"));
    assert_eq!(state.unread_counts.get("大群Beta (680)").copied(), Some(0));
    assert_eq!(state.format_session_row("大群Beta (680)"), "大群Beta (680)");
}

#[test]
fn mock_qq_spawn_with_groups_exposes_uia_sidebar_and_clears_unread_on_select() {
    let mock = MockQqServer::spawn_with_groups(vec![
        (
            "大群Alpha (520)".to_string(),
            vec!["群友1 [11:50]: Alpha 欢迎".to_string()],
        ),
        (
            "大群Beta (680)".to_string(),
            vec!["群友2 [11:55]: Beta 初始".to_string()],
        ),
        (
            "大群Gamma (840)".to_string(),
            vec!["群友3 [11:58]: Gamma 初始".to_string()],
        ),
    ])
    .expect("spawn_with_groups");

    assert_eq!(mock.active_group(), "大群Alpha (520)");
    assert_eq!(mock.unread_for("大群Alpha (520)"), 0);
    assert_eq!(mock.unread_for("大群Beta (680)"), 0);

    mock.inject_incoming_batch(
        "大群Beta (680)",
        &[
            "群友A [12:01]: @qq-bot 高优通知".to_string(),
            "群友B [12:02]: 多行文本\n第二行内容".to_string(),
        ],
    );
    mock.inject_incoming_batch(
        "大群Gamma (840)",
        &[
            "管理员 [12:03]: @全体成员 今晚九点发布".to_string(),
            "群友C [12:04]: 收到".to_string(),
            "群友D [12:05]: +1".to_string(),
        ],
    );

    assert_eq!(mock.unread_for("大群Alpha (520)"), 0);
    assert_eq!(mock.unread_for("大群Beta (680)"), 2);
    assert_eq!(mock.unread_for("大群Gamma (840)"), 3);

    run_with_uia(|automation| {
        let root = unsafe { automation.ElementFromHandle(HWND(mock.hwnd() as *mut c_void)) }
            .expect("获取模拟主窗口 UIA 根元素");

        let lists = find_uia_elements_by_control_type(
            automation,
            &root,
            TreeScope_Descendants,
            UIA_ListControlTypeId.0,
        );
        let session_list = lists
            .iter()
            .find(|elem| uia_element_name(elem) == common_types::constants::UI_LABEL_SESSION_LIST)
            .expect("UIA 树中应包含会话列表控件");

        let items = find_uia_elements_by_control_type(
            automation,
            session_list,
            TreeScope_Children,
            UIA_ListItemControlTypeId.0,
        );
        assert_eq!(items.len(), 3);

        let item_names: Vec<String> = items.iter().map(uia_element_name).collect();
        assert_eq!(item_names[0], "大群Alpha (520)");
        assert!(
            item_names[1].starts_with("大群Beta (680) 12:00 [有人@我] ")
                && item_names[1].contains("第二行内容")
                && item_names[1].ends_with("(2条未读)")
                && !item_names[1].contains('\n'),
            "Beta UIA 行文本不符合预期: {:?}",
            item_names[1]
        );
        assert!(
            item_names[2].starts_with("大群Gamma (840) 12:00 [有全体消息] ")
                && item_names[2].ends_with("(3条未读)"),
            "Gamma UIA 行文本不符合预期: {:?}",
            item_names[2]
        );

        // 通过真实 UIA SelectionItemPattern::Select 切换至大群Beta (680)
        let select_pattern: IUIAutomationSelectionItemPattern =
            unsafe { items[1].GetCurrentPatternAs(UIA_SelectionItemPatternId) }
                .expect("获取 ListItem SelectionItemPattern");
        unsafe { select_pattern.Select() }.expect("通过 UIA Select 选中大群Beta");
        thread::sleep(Duration::from_millis(50));

        assert_eq!(mock.active_group(), "大群Beta (680)");
        assert_eq!(mock.unread_for("大群Beta (680)"), 0);
        assert_eq!(mock.unread_for("大群Gamma (840)"), 3);

        // 验证切换后侧栏大群Beta 行清除未读与提醒修饰，顶部静态标题同步为大群Beta (680)
        let items_after = find_uia_elements_by_control_type(
            automation,
            session_list,
            TreeScope_Children,
            UIA_ListItemControlTypeId.0,
        );
        let names_after: Vec<String> = items_after.iter().map(uia_element_name).collect();
        assert_eq!(names_after[0], "大群Alpha (520)");
        assert_eq!(names_after[1], "大群Beta (680)");
        assert!(names_after[2].contains("(3条未读)"));

        let texts = find_uia_elements_by_control_type(
            automation,
            &root,
            TreeScope_Descendants,
            UIA_TextControlTypeId.0,
        );
        assert!(
            texts
                .iter()
                .any(|elem| uia_element_name(elem) == "大群Beta (680)"),
            "切换后顶部标题控件应同步为大群Beta (680)"
        );

        let beta_history =
            read_listbox_items_win32(mock.hwnd(), constants::CONTROL_ID_CHAT_HISTORY);
        assert_eq!(beta_history.len(), 3);
        assert!(beta_history[1].contains("@qq-bot 高优通知"));

        // 通过控制句柄显式标记大群Gamma 已读而不切换当前激活会话
        mock.mark_read("大群Gamma (840)");
        assert_eq!(mock.unread_for("大群Gamma (840)"), 0);
        assert_eq!(mock.active_group(), "大群Beta (680)");

        let items_after_mark = find_uia_elements_by_control_type(
            automation,
            session_list,
            TreeScope_Children,
            UIA_ListItemControlTypeId.0,
        );
        assert_eq!(uia_element_name(&items_after_mark[2]), "大群Gamma (840)");

        // 验证 mark_read 刷新列表后当前选中项索引仍保持在大群Beta (680)
        let beta_pattern_after_mark: IUIAutomationSelectionItemPattern =
            unsafe { items_after_mark[1].GetCurrentPatternAs(UIA_SelectionItemPatternId) }
                .expect("获取刷新后 Beta SelectionItemPattern");
        let beta_selected = unsafe { beta_pattern_after_mark.CurrentIsSelected() }
            .expect("读取 Beta CurrentIsSelected");
        assert!(
            beta_selected.as_bool(),
            "mark_read 刷新会话列表后应保持当前激活会话选中态"
        );

        // 切回大群Alpha (520) 验证历史记录区同步回切
        let alpha_select: IUIAutomationSelectionItemPattern =
            unsafe { items_after_mark[0].GetCurrentPatternAs(UIA_SelectionItemPatternId) }
                .expect("获取 Alpha SelectionItemPattern");
        unsafe { alpha_select.Select() }.expect("通过 UIA Select 切回大群Alpha");
        thread::sleep(Duration::from_millis(50));
        assert_eq!(mock.active_group(), "大群Alpha (520)");
        let alpha_history =
            read_listbox_items_win32(mock.hwnd(), constants::CONTROL_ID_CHAT_HISTORY);
        assert_eq!(alpha_history, vec!["群友1 [11:50]: Alpha 欢迎".to_string()]);
    });

    mock.shutdown();
}
