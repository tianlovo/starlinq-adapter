//! 端到端自测套件门面：暴露临时反向 WebSocket 模拟服务器与 14 步全链路校验入口

pub mod mock_reverse_server;
mod ocr_probe;
pub mod runner;

pub use runner::run_self_test;
