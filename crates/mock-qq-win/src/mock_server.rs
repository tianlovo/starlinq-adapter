//! 模拟 QQNT 窗口服务端与线程安全控制句柄实现

use std::cell::Cell;
use std::ffi::c_void;
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};

use common_types::{AdapterError, Result};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Gdi::UpdateWindow;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DispatchMessageW, GetMessageW, PostMessageW, ShowWindow, TranslateMessage,
    MSG, SW_SHOWNOACTIVATE, WINDOW_EX_STYLE, WS_OVERLAPPEDWINDOW,
};
use windows_core::PCWSTR;

use crate::constants::{
    MAIN_WINDOW_HEIGHT, MAIN_WINDOW_WIDTH, MAIN_WINDOW_X, MAIN_WINDOW_Y, MOCK_QQ_MAIN_CLASS,
    MOCK_THREAD_NAME, QQ_WINDOW_TITLE, WM_APP_CLOSE_SECURITY, WM_APP_OPEN_SECURITY,
    WM_APP_REFRESH_CHAT, WM_APP_SHUTDOWN,
};
use crate::mock_state::{MockAsyncRequest, SharedMockState, WindowInstanceState};
use crate::window_proc::{register_window_classes, to_wide};

/// 在独立消息泵线程中拉起原生 Win32 模拟 QQNT 窗口的入口类型
pub struct MockQqServer;

impl MockQqServer {
    /// 启动模拟 Win32 窗口线程并等待主窗口就绪返回控制句柄
    pub fn spawn() -> Result<MockQqHandle> {
        Self::spawn_with_state(SharedMockState::new())
    }

    /// 使用指定会话列表与初始消息记录启动模拟 Win32 窗口线程（支持多超大群并发仿真）
    pub fn spawn_with_groups(initial_groups: Vec<(String, Vec<String>)>) -> Result<MockQqHandle> {
        Self::spawn_with_state(SharedMockState::from_groups(initial_groups))
    }

    fn spawn_with_state(initial_state: SharedMockState) -> Result<MockQqHandle> {
        let shared_state = Arc::new(Mutex::new(initial_state));
        let shared_clone = Arc::clone(&shared_state);
        let (ready_sender, ready_receiver) = mpsc::sync_channel::<Result<isize>>(1);

        let join_handle = thread::Builder::new()
            .name(MOCK_THREAD_NAME.to_string())
            .spawn(move || run_mock_window_thread(shared_clone, ready_sender))
            .map_err(AdapterError::Io)?;

        let window_handle = ready_receiver.recv().map_err(|err| {
            AdapterError::UiaFailure(format!("mock-qq-win thread failed before ready: {err}"))
        })??;

        Ok(MockQqHandle {
            hwnd: window_handle,
            shared: shared_state,
            join_handle: Some(join_handle),
        })
    }
}

fn run_mock_window_thread(
    shared_state: Arc<Mutex<SharedMockState>>,
    ready_sender: mpsc::SyncSender<Result<isize>>,
) {
    let module_instance = match unsafe { GetModuleHandleW(None) } {
        Ok(module) => HINSTANCE(module.0),
        Err(err) => {
            let _ = ready_sender.send(Err(AdapterError::UiaFailure(format!(
                "GetModuleHandleW failed: {err}"
            ))));
            return;
        }
    };

    if let Err(err) = register_window_classes(module_instance) {
        let _ = ready_sender.send(Err(err));
        return;
    }

    let instance_state = Box::new(WindowInstanceState {
        shared: shared_state,
        list_sessions: HWND::default(),
        static_header: HWND::default(),
        list_history: HWND::default(),
        edit_composer: HWND::default(),
        btn_send: HWND::default(),
        security_hwnd: HWND::default(),
        suppress_selection_sync: Cell::new(false),
    });
    let raw_state_ptr = Box::into_raw(instance_state);

    let create_result = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(to_wide(MOCK_QQ_MAIN_CLASS).as_ptr()),
            PCWSTR(to_wide(QQ_WINDOW_TITLE).as_ptr()),
            WS_OVERLAPPEDWINDOW,
            MAIN_WINDOW_X,
            MAIN_WINDOW_Y,
            MAIN_WINDOW_WIDTH,
            MAIN_WINDOW_HEIGHT,
            None,
            None,
            Some(module_instance),
            Some(raw_state_ptr as *const c_void),
        )
    };

    let main_hwnd = match create_result {
        Ok(hwnd) if !hwnd.0.is_null() => hwnd,
        Ok(_) => {
            unsafe {
                drop(Box::from_raw(raw_state_ptr));
            }
            let _ = ready_sender.send(Err(AdapterError::UiaFailure(
                "CreateWindowExW returned null HWND".to_string(),
            )));
            return;
        }
        Err(err) => {
            unsafe {
                drop(Box::from_raw(raw_state_ptr));
            }
            let _ = ready_sender.send(Err(AdapterError::UiaFailure(format!(
                "CreateWindowExW failed: {err}"
            ))));
            return;
        }
    };

    unsafe {
        let _ = ShowWindow(main_hwnd, SW_SHOWNOACTIVATE);
        let _ = UpdateWindow(main_hwnd);
    }

    let _ = ready_sender.send(Ok(main_hwnd.0 as isize));

    let mut window_msg = MSG::default();
    unsafe {
        while GetMessageW(&mut window_msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&window_msg);
            DispatchMessageW(&window_msg);
        }
    }
}

/// 线程安全的模拟 QQNT 窗口实例控制句柄，在 Drop 时自动关闭后台线程（RAII 守卫）
pub struct MockQqHandle {
    hwnd: isize,
    shared: Arc<Mutex<SharedMockState>>,
    join_handle: Option<JoinHandle<()>>,
}

// `hwnd` 仅用于跨线程通过 `PostMessageW` 安全投递消息，其余状态均由互斥锁与线程句柄封装
unsafe impl Send for MockQqHandle {}
unsafe impl Sync for MockQqHandle {}

impl MockQqHandle {
    /// 返回模拟主窗口句柄数值
    pub fn hwnd(&self) -> isize {
        self.hwnd
    }

    /// 获取模拟窗口当前处于激活状态的群聊标题
    pub fn active_group(&self) -> String {
        self.shared
            .lock()
            .expect("mock state lock")
            .active_group
            .clone()
    }

    /// 读取指定群聊已累计的全部聊天消息文本列表
    pub fn messages_for(&self, group: &str) -> Vec<String> {
        self.shared
            .lock()
            .expect("mock state lock")
            .messages
            .get(group)
            .cloned()
            .unwrap_or_default()
    }

    /// 读取指定群聊当前的未读消息计数
    pub fn unread_for(&self, group: &str) -> u32 {
        self.shared
            .lock()
            .expect("mock state lock")
            .unread_counts
            .get(group)
            .copied()
            .unwrap_or(0)
    }

    /// 清零指定群聊的未读计数与提醒预览，并同步刷新左侧会话列表控件
    pub fn mark_read(&self, group: &str) {
        {
            let mut guard = self.shared.lock().expect("mock state lock");
            guard.clear_unread(group);
        }
        self.post_ack_request(WM_APP_REFRESH_CHAT, None);
    }

    /// 向目标群聊追加一条格式化消息，并通过 `PostMessageW` 同步刷新图形列表控件
    pub fn inject_incoming_message(&self, group_name: &str, formatted_msg: &str) {
        self.inject_incoming_batch(group_name, &[formatted_msg.to_string()]);
    }

    /// 向目标群聊批量注入入站消息，非激活会话自动累加未读数与 `@` 提醒预览并触发单次界面刷新
    pub fn inject_incoming_batch(&self, group_name: &str, formatted_msgs: &[String]) {
        {
            let mut guard = self.shared.lock().expect("mock state lock");
            guard.record_incoming_batch(group_name, formatted_msgs);
        }
        self.post_ack_request(WM_APP_REFRESH_CHAT, None);
    }

    /// 弹出带指定标题的模拟安全验证窗口（例如“QQ安全验证”）
    pub fn open_security_dialog(&self, title: &str) {
        self.post_ack_request(WM_APP_OPEN_SECURITY, Some(title.to_string()));
    }

    /// 关闭当前处于打开状态的模拟安全验证弹窗
    pub fn close_security_dialog(&self) {
        self.post_ack_request(WM_APP_CLOSE_SECURITY, None);
    }

    fn post_ack_request(&self, window_message: u32, security_title: Option<String>) {
        let (ack_sender, ack_receiver) = mpsc::sync_channel(1);
        let request = Box::new(MockAsyncRequest {
            security_title,
            ack_sender,
        });
        let raw_ptr = Box::into_raw(request);
        let target_hwnd = HWND(self.hwnd as *mut c_void);
        let post_result = unsafe {
            PostMessageW(
                Some(target_hwnd),
                window_message,
                WPARAM(0),
                LPARAM(raw_ptr as isize),
            )
        };
        if post_result.is_err() {
            unsafe {
                drop(Box::from_raw(raw_ptr));
            }
        } else {
            let _ = ack_receiver.recv();
        }
    }

    fn shutdown_inner(&mut self) {
        if let Some(handle) = self.join_handle.take() {
            let target_hwnd = HWND(self.hwnd as *mut c_void);
            unsafe {
                let _ = PostMessageW(Some(target_hwnd), WM_APP_SHUTDOWN, WPARAM(0), LPARAM(0));
            }
            let _ = handle.join();
        }
    }

    /// 显式向图形线程发送停机信号并等待其安全退出
    pub fn shutdown(mut self) {
        self.shutdown_inner();
    }
}

impl Drop for MockQqHandle {
    fn drop(&mut self) {
        self.shutdown_inner();
    }
}
