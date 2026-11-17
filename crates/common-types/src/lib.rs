//! 门面模块（Facade）：对外统一聚合错误、配置、窗口节点、消息负载、OneBot 实体、共享常量与驱动契约

pub mod config;
pub mod constants;
pub mod driver_trait;
pub mod error;
pub mod messaging;
pub mod onebot;
pub mod window;

#[cfg(test)]
mod tests;

pub use config::*;
pub use driver_trait::*;
pub use error::*;
pub use messaging::*;
pub use onebot::*;
pub use window::*;
