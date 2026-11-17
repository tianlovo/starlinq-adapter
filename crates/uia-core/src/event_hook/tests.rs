//! Win32 实时事件钩子观察者的线程安全约束与模拟窗口即时唤醒单元测试模块

use std::time::Duration;

use super::*;

fn assert_send_sync<T: Send + Sync>() {}

#[test]
fn watcher_type_is_send_and_sync() {
    assert_send_sync::<WinEventWatcher>();
}

#[tokio::test]
async fn win_event_watcher_wakes_on_mock_peer_message_and_drops_cleanly() {
    let mock_server = mock_qq_win::MockQqServer::spawn().expect("启动模拟 QQ 窗口");
    tokio::time::sleep(Duration::from_millis(60)).await;

    let watcher =
        WinEventWatcher::with_skip_own_process(false).expect("启动同进程内测试用事件观察者");
    watcher.set_tracked_pids(&[std::process::id()]);

    // 先等待启动期间焦点与窗口创建事件跨过防抖窗口，并排空已存储的初始信号许可
    tokio::time::sleep(Duration::from_millis(50)).await;
    if tokio::time::timeout(Duration::from_millis(10), watcher.notified())
        .await
        .is_ok()
    {
        tokio::time::sleep(Duration::from_millis(45)).await;
    }

    mock_server.inject_incoming_message("测试大群 (128)", "群友C [14:05]: realtime hook msg");

    tokio::time::timeout(Duration::from_secs(3), watcher.notified())
        .await
        .expect("注入模拟群友消息后 WinEventWatcher 应在 3 秒内触发唤醒");

    drop(watcher);
    mock_server.shutdown();
}
