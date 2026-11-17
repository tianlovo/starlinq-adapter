//! Win32 实时无障碍事件监听门面模块（Observer 观察者模式），通过独立消息泵线程监听目标进程对象事件并按防抖窗口唤醒异步消费者

mod watcher;

#[cfg(test)]
mod tests;

pub use watcher::WinEventWatcher;
