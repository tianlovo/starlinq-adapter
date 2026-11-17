//! 自动化输入与发送动作门面模块（Facade + Strategy + Chain of Responsibility + RAII Guard），统一提供后台模式优先与前台聚焦安全恢复双通道动作原语

mod background_ops;
mod focus_guard;
mod focus_restore_ops;
mod models;
mod mouse_ops;

#[cfg(test)]
mod tests;

pub use background_ops::{
    invoke_element_background, invoke_element_background_verified, post_chars_background,
    send_submit_background, send_submit_background_for_editor,
    send_submit_background_for_editor_verified, type_text_background,
    type_text_background_verified, verify_element_belongs_to_target,
};
pub(crate) use background_ops::{
    resolve_postmessage_target, verify_child_hwnd_belongs_to_target, verify_postmessage_target_pid,
};
pub use focus_guard::{current_foreground_hwnd, ForegroundRestoreGuard};
pub(crate) use focus_guard::{
    ensure_foreground_target_intact, enter_window_editor_foreground_inner,
};
pub use focus_restore_ops::{
    clear_editor_focus_restore, clear_editor_focus_restore_verified, send_escape_key_verified,
    submit_only_focus_restore, submit_only_focus_restore_verified, type_and_submit_focus_restore,
    type_and_submit_focus_restore_verified, type_text_focus_restore,
    type_text_focus_restore_in_active_editor, type_text_focus_restore_verified,
};
pub(crate) use focus_restore_ops::{send_vk_events, try_submit_in_foreground_inner};
pub use models::{ComposerSubmitTarget, TypingDelayRange};
pub use mouse_ops::{
    click_point_cursor_restore_verified, scroll_point_cursor_restore_verified, SimulatedClickKind,
};
