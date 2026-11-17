//! Win32 模拟窗口过程与子控件管理（Command + Active Object 消息泵核心）

use std::ffi::c_void;
use std::sync::atomic::{AtomicIsize, Ordering};

use common_types::{AdapterError, Result};
use windows::Win32::Foundation::{
    ERROR_CLASS_ALREADY_EXISTS, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    GetStockObject, UpdateWindow, COLOR_WINDOW, DEFAULT_GUI_FONT, HBRUSH,
};
use windows::Win32::System::DataExchange::IsClipboardFormatAvailable;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CallWindowProcW, CreateWindowExW, DefWindowProcW, DestroyWindow, GetParent, GetPropW,
    GetWindowLongPtrW, GetWindowTextLengthW, GetWindowTextW, LoadCursorW, PostQuitMessage,
    RegisterClassExW, SendMessageW, SetPropW, SetWindowLongPtrW, SetWindowTextW, ShowWindow,
    BS_PUSHBUTTON, CREATESTRUCTW, CS_HREDRAW, CS_VREDRAW, ES_AUTOVSCROLL, ES_MULTILINE,
    ES_WANTRETURN, GWLP_USERDATA, GWLP_WNDPROC, HMENU, IDC_ARROW, LBN_SELCHANGE,
    LBS_NOINTEGRALHEIGHT, LBS_NOTIFY, LB_ADDSTRING, LB_GETCOUNT, LB_GETCURSEL, LB_GETTEXT,
    LB_GETTEXTLEN, LB_RESETCONTENT, LB_SETCURSEL, LB_SETITEMHEIGHT, SW_SHOWNOACTIVATE,
    WINDOW_EX_STYLE, WINDOW_STYLE, WM_CLOSE, WM_COMMAND, WM_CREATE, WM_DESTROY, WM_NCCREATE,
    WM_NCDESTROY, WM_PASTE, WM_SETFONT, WNDCLASSEXW, WNDPROC, WS_BORDER, WS_CAPTION, WS_CHILD,
    WS_POPUP, WS_SYSMENU, WS_TABSTOP, WS_VISIBLE, WS_VSCROLL,
};
use windows_core::PCWSTR;

use common_types::constants::UI_LABEL_SESSION_LIST;

use crate::constants::{
    BUTTON_CLICKED_NOTIFY_CODE, CLASS_BUTTON, CLASS_EDIT, CLASS_LISTBOX, CLASS_STATIC,
    CONTROL_ID_CHAT_HISTORY, CONTROL_ID_COMPOSER_EDIT, CONTROL_ID_HEADER_STATIC,
    CONTROL_ID_SEND_BUTTON, CONTROL_ID_SESSION_LIST, DEFAULT_BOT_SELF_NAME,
    IMAGE_PLACEHOLDER_TOKEN, MOCK_QQ_MAIN_CLASS, MOCK_QQ_SECURITY_CLASS,
    PROP_ORIGINAL_EDIT_WNDPROC, PROP_ORIGINAL_LISTBOX_WNDPROC, SECURITY_DIALOG_HEIGHT,
    SECURITY_DIALOG_WIDTH, SECURITY_DIALOG_X, SECURITY_DIALOG_Y, UI_LABEL_SEND_BUTTON,
    WM_APP_CLOSE_SECURITY, WM_APP_OPEN_SECURITY, WM_APP_REFRESH_CHAT, WM_APP_SHUTDOWN,
    WM_APP_SYNC_SELECTION,
};
use crate::mock_state::{MockAsyncRequest, WindowInstanceState};

static LAST_LISTBOX_ORIG_PROC: AtomicIsize = AtomicIsize::new(0);
static LAST_EDIT_ORIG_PROC: AtomicIsize = AtomicIsize::new(0);

/// 将 UTF-8 字符串编码为带结尾空字符的 UTF-16 宽字符缓冲区
pub(crate) fn to_wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn loword(value: usize) -> u16 {
    (value & 0xFFFF) as u16
}

fn hiword(value: usize) -> u16 {
    ((value >> 16) & 0xFFFF) as u16
}

unsafe fn set_window_text_str(window_handle: HWND, text: &str) {
    let wide_buffer = to_wide(text);
    let _ = SetWindowTextW(window_handle, PCWSTR(wide_buffer.as_ptr()));
}

unsafe fn get_window_text_str(window_handle: HWND) -> String {
    let length = GetWindowTextLengthW(window_handle);
    if length <= 0 {
        return String::new();
    }
    let mut wide_buffer = vec![0u16; (length as usize) + 2];
    let copied = GetWindowTextW(window_handle, &mut wide_buffer);
    if copied <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&wide_buffer[..copied as usize])
}

unsafe fn listbox_add_str(listbox_handle: HWND, text: &str) {
    let wide_buffer = to_wide(text);
    SendMessageW(
        listbox_handle,
        LB_ADDSTRING,
        Some(WPARAM(0)),
        Some(LPARAM(wide_buffer.as_ptr() as isize)),
    );
}

unsafe fn listbox_get_cursel(listbox_handle: HWND) -> Option<usize> {
    let selected_index = SendMessageW(
        listbox_handle,
        LB_GETCURSEL,
        Some(WPARAM(0)),
        Some(LPARAM(0)),
    )
    .0;
    (selected_index >= 0).then_some(selected_index as usize)
}

unsafe fn listbox_get_item_text(listbox_handle: HWND, index: usize) -> Option<String> {
    let text_length = SendMessageW(
        listbox_handle,
        LB_GETTEXTLEN,
        Some(WPARAM(index)),
        Some(LPARAM(0)),
    )
    .0;
    if text_length < 0 {
        return None;
    }
    let mut wide_buffer = vec![0u16; (text_length as usize) + 2];
    let copied = SendMessageW(
        listbox_handle,
        LB_GETTEXT,
        Some(WPARAM(index)),
        Some(LPARAM(wide_buffer.as_mut_ptr() as isize)),
    )
    .0;
    if copied < 0 {
        return None;
    }
    Some(String::from_utf16_lossy(&wide_buffer[..copied as usize]))
}

unsafe fn listbox_items_match(listbox_handle: HWND, expected_items: &[String]) -> bool {
    let current_count = SendMessageW(
        listbox_handle,
        LB_GETCOUNT,
        Some(WPARAM(0)),
        Some(LPARAM(0)),
    )
    .0;
    if current_count < 0 || (current_count as usize) != expected_items.len() {
        return false;
    }
    expected_items.iter().enumerate().all(|(idx, expected)| {
        listbox_get_item_text(listbox_handle, idx).as_deref() == Some(expected.as_str())
    })
}

unsafe fn refresh_sessions_listbox(state: &WindowInstanceState) {
    let (session_rows, active_index) = {
        let guard = state.shared.lock().expect("mock state lock");
        let rows: Vec<String> = guard
            .groups
            .iter()
            .map(|group| guard.format_session_row(group))
            .collect();
        let active_idx = guard
            .groups
            .iter()
            .position(|group| group == &guard.active_group);
        (rows, active_idx)
    };

    let items_match = listbox_items_match(state.list_sessions, &session_rows);
    let current_sel = listbox_get_cursel(state.list_sessions);
    if items_match && current_sel == active_index {
        return;
    }

    // 重填会话列表并恢复选中态期间屏蔽重入通知，防止 LB_SETCURSEL 触发循环刷新
    let prev_suppress = state.suppress_selection_sync.get();
    state.suppress_selection_sync.set(true);
    if !items_match {
        SendMessageW(
            state.list_sessions,
            LB_RESETCONTENT,
            Some(WPARAM(0)),
            Some(LPARAM(0)),
        );
        for row_text in &session_rows {
            listbox_add_str(state.list_sessions, row_text);
        }
    }
    if let Some(idx) = active_index {
        SendMessageW(
            state.list_sessions,
            LB_SETCURSEL,
            Some(WPARAM(idx)),
            Some(LPARAM(0)),
        );
    }
    state.suppress_selection_sync.set(prev_suppress);
}

unsafe fn refresh_history_listbox(state: &WindowInstanceState) {
    let (active_group, group_messages) = {
        let guard = state.shared.lock().expect("mock state lock");
        let messages = guard
            .messages
            .get(&guard.active_group)
            .cloned()
            .unwrap_or_default();
        (guard.active_group.clone(), messages)
    };

    if get_window_text_str(state.static_header) != active_group {
        set_window_text_str(state.static_header, &active_group);
    }
    if listbox_items_match(state.list_history, &group_messages) {
        return;
    }
    SendMessageW(
        state.list_history,
        LB_RESETCONTENT,
        Some(WPARAM(0)),
        Some(LPARAM(0)),
    );
    for message_line in group_messages {
        listbox_add_str(state.list_history, &message_line);
    }
}

unsafe fn sync_selection_from_listbox(state: &WindowInstanceState) {
    let Some(selected_index) = listbox_get_cursel(state.list_sessions) else {
        return;
    };

    // 通过索引回查规范群名而非读取列表显示文本，避免未读计数与预览修饰干扰状态映射
    let (selection_changed, had_unread) = {
        let mut guard = state.shared.lock().expect("mock state lock");
        let Some(selected_group) = guard.groups.get(selected_index).cloned() else {
            return;
        };
        let selection_changed = guard.active_group != selected_group;
        guard.active_group = selected_group.clone();
        let had_unread = guard.clear_unread(&selected_group);
        (selection_changed, had_unread)
    };

    if selection_changed || had_unread {
        refresh_sessions_listbox(state);
    }
    if selection_changed {
        refresh_history_listbox(state);
    }
}

unsafe extern "system" fn session_listbox_subclass_proc(
    window_handle: HWND,
    message_id: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let original_raw = GetPropW(
        window_handle,
        PCWSTR(to_wide(PROP_ORIGINAL_LISTBOX_WNDPROC).as_ptr()),
    )
    .0 as isize;
    let fallback_proc = LAST_LISTBOX_ORIG_PROC.load(Ordering::Relaxed);
    let proc_address = if original_raw != 0 {
        original_raw
    } else {
        fallback_proc
    };

    let call_result = if proc_address != 0 {
        let original_fn: WNDPROC = std::mem::transmute::<isize, WNDPROC>(proc_address);
        CallWindowProcW(original_fn, window_handle, message_id, wparam, lparam)
    } else {
        DefWindowProcW(window_handle, message_id, wparam, lparam)
    };

    // UIA 的 SelectionItemPattern::Select 在 Win32 LISTBOX 上发送 LB_SETCURSEL 时
    // 未必同步触发 LBN_SELCHANGE，因此捕获该消息并立即通知父窗口刷新历史区
    if message_id == LB_SETCURSEL {
        if let Ok(parent_window) = GetParent(window_handle) {
            if !parent_window.0.is_null() {
                SendMessageW(
                    parent_window,
                    WM_APP_SYNC_SELECTION,
                    Some(WPARAM(0)),
                    Some(LPARAM(0)),
                );
            }
        }
    }

    call_result
}

unsafe extern "system" fn composer_edit_subclass_proc(
    window_handle: HWND,
    message_id: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // 拦截编辑框 WM_PASTE：若系统剪贴板含 CF_DIB 位图数据，则向输入区追加 `[图片]` 占位符模拟富媒体粘贴
    if message_id == WM_PASTE && IsClipboardFormatAvailable(8).is_ok() {
        let mut current = get_window_text_str(window_handle);
        if !current.is_empty() && !current.ends_with(' ') && !current.ends_with('\n') {
            current.push(' ');
        }
        current.push_str(IMAGE_PLACEHOLDER_TOKEN);
        set_window_text_str(window_handle, &current);
        return LRESULT(0);
    }

    let original_raw = GetPropW(
        window_handle,
        PCWSTR(to_wide(PROP_ORIGINAL_EDIT_WNDPROC).as_ptr()),
    )
    .0 as isize;
    let fallback_proc = LAST_EDIT_ORIG_PROC.load(Ordering::Relaxed);
    let proc_address = if original_raw != 0 {
        original_raw
    } else {
        fallback_proc
    };

    if proc_address != 0 {
        let original_fn: WNDPROC = std::mem::transmute::<isize, WNDPROC>(proc_address);
        CallWindowProcW(original_fn, window_handle, message_id, wparam, lparam)
    } else {
        DefWindowProcW(window_handle, message_id, wparam, lparam)
    }
}

unsafe extern "system" fn security_wnd_proc(
    window_handle: HWND,
    message_id: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    DefWindowProcW(window_handle, message_id, wparam, lparam)
}

unsafe fn create_child_controls(parent_hwnd: HWND, state: &mut WindowInstanceState) {
    let module_instance = GetModuleHandleW(None)
        .map(|module| HINSTANCE(module.0))
        .unwrap_or_default();
    let gui_font = GetStockObject(DEFAULT_GUI_FONT);

    let listbox_class = to_wide(CLASS_LISTBOX);
    let static_class = to_wide(CLASS_STATIC);
    let edit_class = to_wide(CLASS_EDIT);
    let button_class = to_wide(CLASS_BUTTON);
    let listbox_style = WS_CHILD
        | WS_VISIBLE
        | WS_BORDER
        | WS_VSCROLL
        | WINDOW_STYLE((LBS_NOTIFY | LBS_NOINTEGRALHEIGHT) as u32);

    let initial_header = {
        let guard = state.shared.lock().expect("mock state lock");
        guard.active_group.clone()
    };

    let session_list_name = to_wide(UI_LABEL_SESSION_LIST);
    let _static_sessions_label = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        PCWSTR(static_class.as_ptr()),
        PCWSTR(session_list_name.as_ptr()),
        WS_CHILD | WS_VISIBLE,
        10,
        12,
        225,
        34,
        Some(parent_hwnd),
        None,
        Some(module_instance),
        None,
    )
    .unwrap_or_default();

    let list_sessions = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        PCWSTR(listbox_class.as_ptr()),
        PCWSTR(session_list_name.as_ptr()),
        listbox_style,
        10,
        50,
        225,
        540,
        Some(parent_hwnd),
        Some(HMENU(CONTROL_ID_SESSION_LIST as isize as *mut c_void)),
        Some(module_instance),
        None,
    )
    .unwrap_or_default();

    let static_header = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        PCWSTR(static_class.as_ptr()),
        PCWSTR(to_wide(&initial_header).as_ptr()),
        WS_CHILD | WS_VISIBLE,
        250,
        12,
        525,
        34,
        Some(parent_hwnd),
        Some(HMENU(CONTROL_ID_HEADER_STATIC as isize as *mut c_void)),
        Some(module_instance),
        None,
    )
    .unwrap_or_default();

    let list_history = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        PCWSTR(listbox_class.as_ptr()),
        PCWSTR::null(),
        listbox_style,
        250,
        52,
        525,
        378,
        Some(parent_hwnd),
        Some(HMENU(CONTROL_ID_CHAT_HISTORY as isize as *mut c_void)),
        Some(module_instance),
        None,
    )
    .unwrap_or_default();

    let edit_composer = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        PCWSTR(edit_class.as_ptr()),
        PCWSTR::null(),
        WS_CHILD
            | WS_VISIBLE
            | WS_BORDER
            | WS_TABSTOP
            | WINDOW_STYLE((ES_MULTILINE | ES_WANTRETURN | ES_AUTOVSCROLL) as u32),
        250,
        440,
        525,
        95,
        Some(parent_hwnd),
        Some(HMENU(CONTROL_ID_COMPOSER_EDIT as isize as *mut c_void)),
        Some(module_instance),
        None,
    )
    .unwrap_or_default();

    let btn_send = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        PCWSTR(button_class.as_ptr()),
        PCWSTR(to_wide(UI_LABEL_SEND_BUTTON).as_ptr()),
        WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(BS_PUSHBUTTON as u32),
        680,
        545,
        95,
        40,
        Some(parent_hwnd),
        Some(HMENU(CONTROL_ID_SEND_BUTTON as isize as *mut c_void)),
        Some(module_instance),
        None,
    )
    .unwrap_or_default();

    for control_hwnd in [
        list_sessions,
        static_header,
        list_history,
        edit_composer,
        btn_send,
    ] {
        if !control_hwnd.0.is_null() {
            SendMessageW(
                control_hwnd,
                WM_SETFONT,
                Some(WPARAM(gui_font.0 as usize)),
                Some(LPARAM(1)),
            );
        }
    }
    if !list_sessions.0.is_null() {
        SendMessageW(
            list_sessions,
            LB_SETITEMHEIGHT,
            Some(WPARAM(0)),
            Some(LPARAM(36)),
        );
    }

    state.list_sessions = list_sessions;
    state.static_header = static_header;
    state.list_history = list_history;
    state.edit_composer = edit_composer;
    state.btn_send = btn_send;

    refresh_sessions_listbox(state);

    // 子类化会话列表控件，以接管 UIA 选择触发时的即时历史刷新
    let orig_proc = SetWindowLongPtrW(
        list_sessions,
        GWLP_WNDPROC,
        session_listbox_subclass_proc as *const () as usize as isize,
    );
    LAST_LISTBOX_ORIG_PROC.store(orig_proc, Ordering::Relaxed);
    let _ = SetPropW(
        list_sessions,
        PCWSTR(to_wide(PROP_ORIGINAL_LISTBOX_WNDPROC).as_ptr()),
        Some(HANDLE(orig_proc as *mut c_void)),
    );

    // 子类化输入框控件，以便拦截后台 WM_PASTE 并识别剪贴板 CF_DIB 图片粘贴
    let orig_edit_proc = SetWindowLongPtrW(
        edit_composer,
        GWLP_WNDPROC,
        composer_edit_subclass_proc as *const () as usize as isize,
    );
    LAST_EDIT_ORIG_PROC.store(orig_edit_proc, Ordering::Relaxed);
    let _ = SetPropW(
        edit_composer,
        PCWSTR(to_wide(PROP_ORIGINAL_EDIT_WNDPROC).as_ptr()),
        Some(HANDLE(orig_edit_proc as *mut c_void)),
    );

    refresh_history_listbox(state);
}

unsafe fn handle_wm_command(
    parent_hwnd: HWND,
    message_id: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let state_ptr = GetWindowLongPtrW(parent_hwnd, GWLP_USERDATA) as *mut WindowInstanceState;
    if state_ptr.is_null() {
        return DefWindowProcW(parent_hwnd, message_id, wparam, lparam);
    }
    let state = &*state_ptr;
    let control_id = loword(wparam.0) as i32;
    let notify_code = hiword(wparam.0);

    if control_id == CONTROL_ID_SESSION_LIST && notify_code == LBN_SELCHANGE as u16 {
        if !state.suppress_selection_sync.get() {
            sync_selection_from_listbox(state);
        }
        return LRESULT(0);
    }

    if control_id == CONTROL_ID_SEND_BUTTON && notify_code == BUTTON_CLICKED_NOTIFY_CODE {
        let raw_text = get_window_text_str(state.edit_composer);
        let trimmed = raw_text.trim_end_matches(&['\r', '\n'][..]);
        if !trimmed.trim().is_empty() {
            let formatted_line = format!("{DEFAULT_BOT_SELF_NAME} [now]: {trimmed}");
            {
                let mut guard = state.shared.lock().expect("mock state lock");
                let active_group = guard.active_group.clone();
                guard
                    .messages
                    .entry(active_group)
                    .or_default()
                    .push(formatted_line);
            }
            set_window_text_str(state.edit_composer, "");
            refresh_history_listbox(state);
        }
        return LRESULT(0);
    }

    DefWindowProcW(parent_hwnd, message_id, wparam, lparam)
}

unsafe fn handle_open_security(parent_hwnd: HWND, lparam: LPARAM) {
    let state_ptr = GetWindowLongPtrW(parent_hwnd, GWLP_USERDATA) as *mut WindowInstanceState;
    let request = Box::from_raw(lparam.0 as *mut MockAsyncRequest);
    if !state_ptr.is_null() {
        let state = &mut *state_ptr;
        if !state.security_hwnd.0.is_null() {
            let _ = DestroyWindow(state.security_hwnd);
            state.security_hwnd = HWND::default();
        }
        let module_instance = GetModuleHandleW(None)
            .map(|module| HINSTANCE(module.0))
            .unwrap_or_default();
        let popup_title = request.security_title.as_deref().unwrap_or("");
        let security_hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(to_wide(MOCK_QQ_SECURITY_CLASS).as_ptr()),
            PCWSTR(to_wide(popup_title).as_ptr()),
            WS_POPUP | WS_CAPTION | WS_SYSMENU,
            SECURITY_DIALOG_X,
            SECURITY_DIALOG_Y,
            SECURITY_DIALOG_WIDTH,
            SECURITY_DIALOG_HEIGHT,
            Some(parent_hwnd),
            None,
            Some(module_instance),
            None,
        )
        .unwrap_or_default();

        if !security_hwnd.0.is_null() {
            let _ = ShowWindow(security_hwnd, SW_SHOWNOACTIVATE);
            let _ = UpdateWindow(security_hwnd);
            state.security_hwnd = security_hwnd;
        }
    }
    let _ = request.ack_sender.send(());
}

/// 模拟 QQNT 主窗口 Win32 消息处理入口
pub(crate) unsafe extern "system" fn main_wnd_proc(
    window_handle: HWND,
    message_id: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message_id {
        WM_NCCREATE => {
            let create_struct = &*(lparam.0 as *const CREATESTRUCTW);
            SetWindowLongPtrW(
                window_handle,
                GWLP_USERDATA,
                create_struct.lpCreateParams as isize,
            );
            DefWindowProcW(window_handle, message_id, wparam, lparam)
        }
        WM_CREATE => {
            let state_ptr =
                GetWindowLongPtrW(window_handle, GWLP_USERDATA) as *mut WindowInstanceState;
            if state_ptr.is_null() {
                return LRESULT(-1);
            }
            create_child_controls(window_handle, &mut *state_ptr);
            LRESULT(0)
        }
        WM_COMMAND => handle_wm_command(window_handle, message_id, wparam, lparam),
        WM_APP_SYNC_SELECTION => {
            let state_ptr =
                GetWindowLongPtrW(window_handle, GWLP_USERDATA) as *mut WindowInstanceState;
            if !state_ptr.is_null() {
                let state = &*state_ptr;
                if !state.suppress_selection_sync.get() {
                    sync_selection_from_listbox(state);
                }
            }
            LRESULT(0)
        }
        WM_APP_REFRESH_CHAT => {
            let state_ptr =
                GetWindowLongPtrW(window_handle, GWLP_USERDATA) as *mut WindowInstanceState;
            if !state_ptr.is_null() {
                let state = &*state_ptr;
                refresh_sessions_listbox(state);
                refresh_history_listbox(state);
            }
            if lparam.0 != 0 {
                let request = Box::from_raw(lparam.0 as *mut MockAsyncRequest);
                let _ = request.ack_sender.send(());
            }
            LRESULT(0)
        }
        WM_APP_OPEN_SECURITY => {
            handle_open_security(window_handle, lparam);
            LRESULT(0)
        }
        WM_APP_CLOSE_SECURITY => {
            let state_ptr =
                GetWindowLongPtrW(window_handle, GWLP_USERDATA) as *mut WindowInstanceState;
            if !state_ptr.is_null() {
                let state = &mut *state_ptr;
                if !state.security_hwnd.0.is_null() {
                    let _ = DestroyWindow(state.security_hwnd);
                    state.security_hwnd = HWND::default();
                }
            }
            if lparam.0 != 0 {
                let request = Box::from_raw(lparam.0 as *mut MockAsyncRequest);
                let _ = request.ack_sender.send(());
            }
            LRESULT(0)
        }
        WM_APP_SHUTDOWN | WM_CLOSE => {
            let state_ptr =
                GetWindowLongPtrW(window_handle, GWLP_USERDATA) as *mut WindowInstanceState;
            if !state_ptr.is_null() {
                let state = &mut *state_ptr;
                if !state.security_hwnd.0.is_null() {
                    let _ = DestroyWindow(state.security_hwnd);
                    state.security_hwnd = HWND::default();
                }
            }
            let _ = DestroyWindow(window_handle);
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        WM_NCDESTROY => {
            let state_ptr =
                GetWindowLongPtrW(window_handle, GWLP_USERDATA) as *mut WindowInstanceState;
            if !state_ptr.is_null() {
                SetWindowLongPtrW(window_handle, GWLP_USERDATA, 0);
                drop(Box::from_raw(state_ptr));
            }
            DefWindowProcW(window_handle, message_id, wparam, lparam)
        }
        _ => DefWindowProcW(window_handle, message_id, wparam, lparam),
    }
}

/// 注册模拟主窗口与安全弹窗的 Win32 窗口类
pub(crate) fn register_window_classes(module_instance: HINSTANCE) -> Result<()> {
    let arrow_cursor = unsafe { LoadCursorW(None, IDC_ARROW) }.unwrap_or_default();
    let background_brush = HBRUSH((COLOR_WINDOW.0 + 1) as isize as *mut c_void);

    let main_class_wide = to_wide(MOCK_QQ_MAIN_CLASS);
    let main_window_class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(main_wnd_proc),
        hInstance: module_instance,
        hCursor: arrow_cursor,
        hbrBackground: background_brush,
        lpszClassName: PCWSTR(main_class_wide.as_ptr()),
        ..Default::default()
    };
    register_single_class(&main_window_class, MOCK_QQ_MAIN_CLASS)?;

    let security_class_wide = to_wide(MOCK_QQ_SECURITY_CLASS);
    let security_window_class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(security_wnd_proc),
        hInstance: module_instance,
        hCursor: arrow_cursor,
        hbrBackground: background_brush,
        lpszClassName: PCWSTR(security_class_wide.as_ptr()),
        ..Default::default()
    };
    register_single_class(&security_window_class, MOCK_QQ_SECURITY_CLASS)?;

    Ok(())
}

fn register_single_class(window_class: &WNDCLASSEXW, class_label: &str) -> Result<()> {
    let atom = unsafe { RegisterClassExW(window_class) };
    if atom == 0 {
        let win_err = windows_core::Error::from_thread();
        if win_err.code() != ERROR_CLASS_ALREADY_EXISTS.to_hresult() {
            return Err(AdapterError::UiaFailure(format!(
                "RegisterClassExW({class_label}) failed: {win_err}"
            )));
        }
    }
    Ok(())
}
