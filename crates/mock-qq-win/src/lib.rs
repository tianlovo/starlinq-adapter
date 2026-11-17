//! 模拟 QQNT 桌面窗口门面（Facade）：对外暴露可测试的原生 Win32 窗口服务端与控制句柄

pub mod constants;
pub mod mock_server;
pub mod mock_state;
pub mod window_proc;

#[cfg(test)]
mod tests;

pub use mock_server::{MockQqHandle, MockQqServer};
