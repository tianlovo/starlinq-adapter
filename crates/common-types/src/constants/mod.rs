//! 共享常量聚合门面：集中管理运行时路径、进程类名、QQNT 中文界面标签、协议路由与数值阈值

pub mod limits;
pub mod paths;
pub mod processes;
pub mod routes_actions;
pub mod ui_labels;

pub use limits::*;
pub use paths::*;
pub use processes::*;
pub use routes_actions::*;
pub use ui_labels::*;
