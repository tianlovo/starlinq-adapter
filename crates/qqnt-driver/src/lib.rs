//! QQNT 9.9.31 Windows UI Automation 异步驱动门面：提供拟人化输入、SQLite 元数据融合及 OneBot 数据面支撑

pub(crate) mod composer;
mod constants;
pub mod driver;
pub mod humanizer;
pub mod matcher;
pub mod parser;
pub mod profile;
pub mod scheduler;

pub use driver::QqntDriver;
