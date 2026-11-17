//! 窗口与诊断模型门面：对外导出窗口元数据、UIA 节点快照与唤醒诊断报告

pub mod diagnostics;
pub mod window_nodes;

pub use diagnostics::*;
pub use window_nodes::*;
