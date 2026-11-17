//! Win32 `SetWinEventHook` 实时事件观察者实现模块，使用 `parking_lot::RwLock` 维护目标进程过滤表并在独立消息泵线程防抖唤醒 `tokio::sync::Notify`

use std::cell::RefCell;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use common_types::constants::limits::WIN_EVENT_DEBOUNCE_MS;
use common_types::{AdapterError, Result};
use parking_lot::RwLock;
use tokio::sync::Notify;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, PeekMessageW, PostThreadMessageW, TranslateMessage,
    EVENT_OBJECT_CREATE, EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_VALUECHANGE, MSG, OBJID_CURSOR,
    PM_NOREMOVE, WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS, WM_QUIT,
};

use crate::constants::{EVENT_HOOK_DEBOUNCE_OFFSET_MS, EVENT_HOOK_THREAD_NAME};
use crate::win32;

thread_local! {
    // 每个 WinEventWatcher 运行在各自独立的 Win32 钩子消息泵线程中，借助线程局部槽保存状态可避免并发测试间冲突及热路径全局锁争用
    static HOOK_STATE: RefCell<Option<Arc<WatcherInner>>> = const { RefCell::new(None) };
}

/// 观察者共享状态容器，记录受跟踪进程集合、防抖时间戳与停机标志
struct WatcherInner {
    tracked_pids: RwLock<HashSet<u32>>,
    notify: Arc<Notify>,
    started_at: Instant,
    last_notify_ms: AtomicU64,
    shutdown: AtomicBool,
}

impl WatcherInner {
    /// 创建初始空跟踪列表的共享事件通知状态
    fn new() -> Self {
        Self {
            tracked_pids: RwLock::new(HashSet::new()),
            notify: Arc::new(Notify::new()),
            started_at: Instant::now(),
            last_notify_ms: AtomicU64::new(0),
            shutdown: AtomicBool::new(false),
        }
    }

    /// 检查触发事件窗口所属 PID 是否位于当前监控名单内
    fn is_tracked_pid(&self, pid: u32) -> bool {
        if pid == 0 {
            return false;
        }
        self.tracked_pids.read().contains(&pid)
    }

    /// 在 `WIN_EVENT_DEBOUNCE_MS` 防抖窗口内合并高频 UIA/WinEvent 事件并至多触发一次唤醒
    fn maybe_notify_debounced(&self) {
        // 叠加初始时间偏移量以避免首条事件毫秒时间戳与零值哨兵发生碰撞
        let now_ms = (self.started_at.elapsed().as_millis() as u64)
            .saturating_add(EVENT_HOOK_DEBOUNCE_OFFSET_MS);
        let previous = self.last_notify_ms.load(Ordering::Relaxed);
        if now_ms.saturating_sub(previous) < WIN_EVENT_DEBOUNCE_MS {
            return;
        }
        if self
            .last_notify_ms
            .compare_exchange(previous, now_ms, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            self.notify.notify_one();
        }
    }
}

/// Windows `WINEVENTPROC` 原生系统 ABI 回调函数（参数表由 user32 固定约束）
unsafe extern "system" fn win_event_proc(
    _hook: HWINEVENTHOOK,
    event: u32,
    window_handle: HWND,
    id_object: i32,
    _id_child: i32,
    _id_event_thread: u32,
    _dwms_event_time: u32,
) {
    if window_handle.0.is_null() {
        return;
    }
    if event == EVENT_OBJECT_LOCATIONCHANGE && id_object == OBJID_CURSOR.0 {
        return;
    }

    HOOK_STATE.with(|slot| {
        let borrow = slot.borrow();
        let Some(inner) = borrow.as_ref() else {
            return;
        };
        if inner.shutdown.load(Ordering::Acquire) {
            return;
        }

        let pid = win32::get_window_pid(window_handle.0 as isize);
        if !inner.is_tracked_pid(pid) {
            // 忽略来自非跟踪进程的无关窗口或桌面事件
            return;
        }
        inner.maybe_notify_debounced();
    });
}

/// 基于 Win32 `SetWinEventHook` 的实时观察者（Observer），每当受跟踪 QQ 进程触发创建或值变更事件时合并唤醒 Tokio 消费者
pub struct WinEventWatcher {
    inner: Arc<WatcherInner>,
    hook_thread_id: u32,
    join_handle: Option<JoinHandle<()>>,
}

impl WinEventWatcher {
    /// 启动默认跳过当前宿主进程事件（`WINEVENT_SKIPOWNPROCESS`）的后台 WinEvent 监听线程
    pub fn new() -> Result<Self> {
        Self::with_skip_own_process(true)
    }

    /// 启动可选跳过自身进程标志的后台事件监听线程，传 `false` 可在单进程内确定性监听 `MockQqServer` 窗口事件
    pub fn with_skip_own_process(skip_own_process: bool) -> Result<Self> {
        let inner = Arc::new(WatcherInner::new());
        let inner_thread = Arc::clone(&inner);
        let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<u32>>(1);

        let flags = if skip_own_process {
            WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS
        } else {
            WINEVENT_OUTOFCONTEXT
        };

        let join_handle = thread::Builder::new()
            .name(EVENT_HOOK_THREAD_NAME.to_string())
            .spawn(move || {
                let thread_id = unsafe { GetCurrentThreadId() };
                let mut message = MSG::default();
                // 强制先行创建线程消息队列后再回报 thread_id，避免 Drop 中的 PostThreadMessageW(WM_QUIT) 与队列初始化发生竞争
                unsafe {
                    let _ = PeekMessageW(&mut message, None, 0, 0, PM_NOREMOVE);
                }

                HOOK_STATE.with(|slot| {
                    *slot.borrow_mut() = Some(Arc::clone(&inner_thread));
                });

                let hook_handle = unsafe {
                    SetWinEventHook(
                        EVENT_OBJECT_CREATE,
                        EVENT_OBJECT_VALUECHANGE,
                        None,
                        Some(win_event_proc),
                        0,
                        0,
                        flags,
                    )
                };
                if hook_handle.is_invalid() {
                    HOOK_STATE.with(|slot| {
                        *slot.borrow_mut() = None;
                    });
                    let _ = ready_tx.send(Err(AdapterError::UiaFailure(
                        "SetWinEventHook 安装 WinEvent 监听器失败".to_string(),
                    )));
                    return;
                }

                let _ = ready_tx.send(Ok(thread_id));

                while !inner_thread.shutdown.load(Ordering::Acquire) {
                    let get_status = unsafe { GetMessageW(&mut message, None, 0, 0) };
                    if get_status.0 <= 0 {
                        break;
                    }
                    unsafe {
                        let _ = TranslateMessage(&message);
                        DispatchMessageW(&message);
                    }
                }

                unsafe {
                    let _ = UnhookWinEvent(hook_handle);
                }
                HOOK_STATE.with(|slot| {
                    *slot.borrow_mut() = None;
                });
            })
            .map_err(|error| {
                AdapterError::UiaFailure(format!("创建 WinEventWatcher 监听线程失败: {error}"))
            })?;

        let hook_thread_id = ready_rx.recv().map_err(|_| {
            AdapterError::UiaFailure("WinEventWatcher 监听线程在就绪前意外退出".to_string())
        })??;

        Ok(Self {
            inner,
            hook_thread_id,
            join_handle: Some(join_handle),
        })
    }

    /// 原子替换事件回调关注的进程 PID 过滤集合
    pub fn set_tracked_pids(&self, pids: &[u32]) {
        let mut guard = self.inner.tracked_pids.write();
        guard.clear();
        guard.extend(pids.iter().copied().filter(|&pid| pid != 0));
    }

    /// 返回可跨任务共享的合并防抖 `tokio::sync::Notify` 句柄
    pub fn notify_handle(&self) -> Arc<Notify> {
        Arc::clone(&self.inner.notify)
    }

    /// 异步挂起直到任一受跟踪 PID 发射新的合并无障碍对象事件
    pub async fn notified(&self) {
        self.inner.notify.notified().await;
    }
}

impl Drop for WinEventWatcher {
    fn drop(&mut self) {
        self.inner.shutdown.store(true, Ordering::Release);
        if self.hook_thread_id != 0 {
            unsafe {
                let _ = PostThreadMessageW(self.hook_thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
            }
        }
        if let Some(handle) = self.join_handle.take() {
            let _ = handle.join();
        }
    }
}
