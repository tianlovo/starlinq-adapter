//! 行业标准微基准与全栈基准测试门面（Facade + Macro Codegen）

pub mod stats_engine;
pub mod suite;

pub use suite::run_all_benchmarks;
