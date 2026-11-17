//! QA 缺陷审计目录与可视化报告生成器门面（Facade）

pub mod defect_catalog;
pub mod html_dashboard;

pub use defect_catalog::known_defect_records;
pub use html_dashboard::write_qa_reports;
