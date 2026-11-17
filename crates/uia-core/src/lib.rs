//! Windows UI Automation 与 Win32 底层适配器核心门面库（Facade），整合单线程 MTA COM 会话、树采集策略回退、不可变身份保护代理、零鼠标干预输入动作与实时事件监听能力

pub mod action;
pub mod clipboard_media;
pub mod constants;
mod dpi_capture;
pub mod event_hook;
pub mod msaa;
pub mod ocr_engine;
pub mod session;
pub mod tree;
pub mod wic_codec;
pub mod win32;

pub use action::{
    clear_editor_focus_restore, clear_editor_focus_restore_verified,
    click_point_cursor_restore_verified, current_foreground_hwnd, invoke_element_background,
    invoke_element_background_verified, post_chars_background,
    scroll_point_cursor_restore_verified, send_escape_key_verified, send_submit_background,
    send_submit_background_for_editor, send_submit_background_for_editor_verified,
    submit_only_focus_restore, submit_only_focus_restore_verified, type_and_submit_focus_restore,
    type_and_submit_focus_restore_verified, type_text_background, type_text_background_verified,
    type_text_focus_restore, type_text_focus_restore_in_active_editor,
    type_text_focus_restore_verified, verify_element_belongs_to_target, ComposerSubmitTarget,
    ForegroundRestoreGuard, SimulatedClickKind, TypingDelayRange,
};
pub use clipboard_media::{
    paste_clipboard_verified, read_clipboard_unicode_text, write_bgra_to_clipboard_dib,
    write_clipboard_unicode_text, write_files_to_clipboard_hdrop, ClipboardPasteSpec,
    ClipboardRestoreGuard,
};
pub use event_hook::WinEventWatcher;
pub use msaa::UiNodeHandle;
pub use ocr_engine::{recognize_bgra_image, recognize_image_bytes};
pub use session::UiaSession;
pub use tree::{
    clear_element_text, collect_flat, collect_flat_raw, dump_tree, flat_needs_chromium_wake,
    is_semantic_content_control, read_element_text, read_element_value_only,
    snapshot_needs_chromium_wake, CollectedNode,
};
pub use wic_codec::{
    capture_window_crop_png, capture_window_full_png, decode_image_bytes_to_bgra,
    encode_bgra_to_png_bytes, DecodedImageBgra,
};
pub use win32::{
    are_mouse_buttons_pressed, current_desktop_info, enumerate_all_child_windows,
    enumerate_all_windows_for_pids, enumerate_candidate_windows, find_child_window_by_class,
    find_pids_by_names, find_windows_with_keywords, find_windows_with_keywords_ext,
    get_root_ancestor, is_user_mouse_busy, launch_or_wake_qq, refresh_window_info,
    resolve_qq_executable, restore_no_activate, wake_chromium_accessibility,
    wake_chromium_accessibility_ext, TopmostUnoccludeGuard, VerifiedWindowIdentity,
};
