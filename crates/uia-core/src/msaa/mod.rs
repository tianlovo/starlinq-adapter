//! MSAA (`IAccessible`) 无障碍适配器门面模块（Adapter + Facade），统一将旧版接口的节点遍历、角色映射与后台读写操作适配为抽象节点句柄接口

mod node_handle;
mod operations;
mod tree_collector;

#[cfg(test)]
mod tests;

pub use crate::constants::{
    CHILDID_SELF, MAX_CHILDREN_PER_CONTAINER, MAX_MSAA_TREE_NODES, SELFLAG_TAKEFOCUS,
    SELFLAG_TAKESELECTION, STATE_SYSTEM_INVISIBLE, STATE_SYSTEM_OFFSCREEN, STATE_SYSTEM_READONLY,
    STATE_SYSTEM_UNAVAILABLE,
};

pub use node_handle::{map_custom_role, map_system_role, UiNodeHandle};
pub use operations::{
    clear_msaa_value, focus_msaa, invoke_msaa, read_msaa_text, read_msaa_value_only,
    window_from_accessible, write_msaa_value,
};
pub use tree_collector::{collect_flat_msaa, dump_tree_msaa};

pub(crate) use operations::{is_known_empty_placeholder, msaa_control_type};
