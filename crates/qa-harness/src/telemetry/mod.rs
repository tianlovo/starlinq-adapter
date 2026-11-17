//! Win32 进程内存、CPU 时间片与系统物理内存遥测采样门面（Facade）

pub mod sampler;
pub mod win32_ffi;

pub use sampler::{
    logical_cpu_cores, read_system_memory_mib, snapshot_current_process, snapshot_process,
    CpuUsageWindow,
};
