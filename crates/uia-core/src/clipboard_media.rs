//! 剪贴板备份还原 RAII 守卫与认证保护下图片粘贴发送模块（Proxy + RAII Guard 模式）

use std::ffi::c_void;
use std::mem::size_of;
use std::thread;
use std::time::Duration;

use common_types::{AdapterError, InputMode, Result, SendShortcut};
use parking_lot::{ReentrantMutex, ReentrantMutexGuard};
use uiautomation::clipboards::{Clipboard, Snapshot};
use windows::Win32::Foundation::{GlobalFree, HANDLE, HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Gdi::{BITMAPINFOHEADER, BI_RGB};
use windows::Win32::System::DataExchange::{EmptyClipboard, SetClipboardData};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GHND};
use windows::Win32::UI::Input::KeyboardAndMouse::{VK_CONTROL, VK_V};
use windows::Win32::UI::WindowsAndMessaging::{SendMessageW, WM_PASTE};

use crate::action::{
    ensure_foreground_target_intact, enter_window_editor_foreground_inner,
    resolve_postmessage_target, send_submit_background_for_editor_verified, send_vk_events,
    try_submit_in_foreground_inner, verify_child_hwnd_belongs_to_target,
    verify_element_belongs_to_target, verify_postmessage_target_pid,
};
use crate::constants::{MAX_DECODED_IMAGE_BYTES, MAX_DECODED_IMAGE_DIMENSION};
use crate::msaa::UiNodeHandle;
use crate::tree;
use crate::wic_codec::DecodedImageBgra;
use crate::win32::VerifiedWindowIdentity;

const CLIPBOARD_FORMAT_DIB: u32 = 8;
const MIN_FOCUS_PASTE_SETTLE_MS: u64 = 120;
const MIN_BACKGROUND_PASTE_SETTLE_MS: u64 = 50;
const CLIPBOARD_OPEN_MAX_ATTEMPTS: usize = 8;
const CLIPBOARD_OPEN_RETRY_DELAY_MS: u64 = 15;

// 进程内可重入剪贴板互斥锁：跨线程严格串行化“备份 -> 写图/粘贴 -> 还原”，同时兼容单线程测试嵌套守卫
static CLIPBOARD_PROCESS_LOCK: ReentrantMutex<()> = ReentrantMutex::new(());

/// 带短暂退避重试地打开系统剪贴板，缓解 Explorer 或其他进程瞬时持锁导致的争用失败
fn open_clipboard_with_retry() -> Result<Clipboard> {
    let mut last_err = None;
    for attempt in 0..CLIPBOARD_OPEN_MAX_ATTEMPTS {
        match Clipboard::open() {
            Ok(clip) => return Ok(clip),
            Err(err) => {
                last_err = Some(err);
                if attempt + 1 < CLIPBOARD_OPEN_MAX_ATTEMPTS {
                    thread::sleep(Duration::from_millis(CLIPBOARD_OPEN_RETRY_DELAY_MS));
                }
            }
        }
    }
    Err(AdapterError::UiaFailure(format!(
        "打开系统剪贴板失败（已重试 {CLIPBOARD_OPEN_MAX_ATTEMPTS} 次）: {}",
        last_err
            .map(|err| err.to_string())
            .unwrap_or_else(|| "unknown".to_string())
    )))
}

/// 在粘贴图片前捕获宿主机剪贴板快照并于离开作用域时自动恢复的 RAII 守卫
pub struct ClipboardRestoreGuard {
    snapshot: Option<Snapshot>,
    _process_lock: ReentrantMutexGuard<'static, ()>,
}

impl ClipboardRestoreGuard {
    /// 打开系统剪贴板并拍摄既有常规数据快照，随后立刻释放 OS 剪贴板句柄并保持进程内互斥直到作用域结束
    pub fn capture() -> Result<Self> {
        let process_lock = CLIPBOARD_PROCESS_LOCK.lock();
        let clipboard = open_clipboard_with_retry()?;
        let snapshot = clipboard
            .snapshot(false)
            .map_err(|err| AdapterError::UiaFailure(format!("备份系统剪贴板内容失败: {err}")))?;
        Ok(Self {
            snapshot: Some(snapshot),
            _process_lock: process_lock,
        })
    }
}

impl Drop for ClipboardRestoreGuard {
    fn drop(&mut self) {
        if let Some(snapshot) = self.snapshot.take() {
            if let Ok(clipboard) = open_clipboard_with_retry() {
                let _ = clipboard.restore(snapshot);
            }
        }
    }
}

/// 剪贴板粘贴发送参数对象（Parameter Object），确保接口参数个数严格受控
pub struct ClipboardPasteSpec<'a> {
    pub editor: &'a UiNodeHandle,
    pub send_button: Option<&'a UiNodeHandle>,
    pub mode: InputMode,
    pub shortcut: SendShortcut,
    pub press_enter: bool,
    pub settle_delay_ms: u64,
}

fn read_editor_text_snapshot(editor: &UiNodeHandle) -> String {
    tree::read_element_value_only(editor)
        .or_else(|| tree::read_element_text(editor))
        .unwrap_or_default()
}

/// 将 32 位顶向下 BGRA 位图转换为标准自底向上 `CF_DIB` 内存块写入系统剪贴板
pub fn write_bgra_to_clipboard_dib(img: &DecodedImageBgra) -> Result<()> {
    if img.width == 0 || img.height == 0 {
        return Err(AdapterError::InputRejected(
            "invalid BGRA bitmap for clipboard CF_DIB".to_string(),
        ));
    }
    if img.width > MAX_DECODED_IMAGE_DIMENSION || img.height > MAX_DECODED_IMAGE_DIMENSION {
        return Err(AdapterError::InputRejected(format!(
            "CF_DIB dimensions {}x{} exceed safe limit {MAX_DECODED_IMAGE_DIMENSION}",
            img.width, img.height
        )));
    }
    let row_bytes = (img.width as usize)
        .checked_mul(4)
        .ok_or_else(|| AdapterError::InputRejected("DIB row size overflow".to_string()))?;
    let pixel_bytes = row_bytes
        .checked_mul(img.height as usize)
        .ok_or_else(|| AdapterError::InputRejected("DIB image size overflow".to_string()))?;
    if pixel_bytes > MAX_DECODED_IMAGE_BYTES {
        return Err(AdapterError::InputRejected(format!(
            "CF_DIB image buffer {pixel_bytes} bytes exceeds safe limit {MAX_DECODED_IMAGE_BYTES}"
        )));
    }
    if img.bgra_top_down.len() < pixel_bytes {
        return Err(AdapterError::InputRejected(
            "invalid BGRA bitmap for clipboard CF_DIB".to_string(),
        ));
    }
    let bi_size_image = u32::try_from(pixel_bytes)
        .map_err(|_| AdapterError::InputRejected("DIB pixel buffer exceeds u32".to_string()))?;

    let header = BITMAPINFOHEADER {
        biSize: size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: img.width as i32,
        // 标准 CF_DIB 采用正高度自底向上行序，确保 Chromium/QQ 等全部贴图端稳定识别
        biHeight: img.height as i32,
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB.0,
        biSizeImage: bi_size_image,
        ..Default::default()
    };

    let header_size = size_of::<BITMAPINFOHEADER>();
    let total_size = header_size + pixel_bytes;

    let _process_lock = CLIPBOARD_PROCESS_LOCK.lock();
    let _clipboard = open_clipboard_with_retry()?;

    unsafe {
        let hglobal = GlobalAlloc(GHND, total_size)
            .map_err(|err| AdapterError::UiaFailure(format!("GlobalAlloc(CF_DIB) 失败: {err}")))?;
        let locked = GlobalLock(hglobal);
        if locked.is_null() {
            let _ = GlobalFree(Some(hglobal));
            return Err(AdapterError::UiaFailure(
                "GlobalLock(CF_DIB) 返回空指针".to_string(),
            ));
        }

        let dst = std::slice::from_raw_parts_mut(locked as *mut u8, total_size);
        let header_bytes = std::slice::from_raw_parts(
            &header as *const BITMAPINFOHEADER as *const u8,
            header_size,
        );
        dst[..header_size].copy_from_slice(header_bytes);

        let h = img.height as usize;
        for bottom_row in 0..h {
            let top_row = h - 1 - bottom_row;
            let src_off = top_row * row_bytes;
            let dst_off = header_size + bottom_row * row_bytes;
            dst[dst_off..dst_off + row_bytes]
                .copy_from_slice(&img.bgra_top_down[src_off..src_off + row_bytes]);
        }
        let _ = GlobalUnlock(hglobal);

        if let Err(err) = EmptyClipboard() {
            let _ = GlobalFree(Some(hglobal));
            return Err(AdapterError::UiaFailure(format!(
                "EmptyClipboard 失败: {err}"
            )));
        }
        if let Err(err) = SetClipboardData(CLIPBOARD_FORMAT_DIB, Some(HANDLE(hglobal.0))) {
            let _ = GlobalFree(Some(hglobal));
            return Err(AdapterError::UiaFailure(format!(
                "SetClipboardData(CF_DIB) 失败: {err}"
            )));
        }
        // SetClipboardData 成功后所有权已转移给系统剪贴板，切勿再释放 hglobal
    }

    Ok(())
}

/// 在绑定窗口身份校验通过的前提下执行剪贴板粘贴（优先后台 `WM_PASTE`，Chromium 不响应时退避至无鼠标 `Ctrl+V` 焦点恢复），返回 `(实际模式, 是否触碰前台)`
pub fn paste_clipboard_verified(
    identity: &VerifiedWindowIdentity,
    spec: &ClipboardPasteSpec<'_>,
) -> Result<(InputMode, bool)> {
    identity.verify_fast()?;
    verify_element_belongs_to_target(identity, spec.editor)?;

    if spec.mode != InputMode::FocusRestoreUnicode {
        let before_snapshot = read_editor_text_snapshot(spec.editor);
        let post_hwnd = resolve_postmessage_target(identity.hwnd(), Some(spec.editor));
        if post_hwnd != 0 {
            verify_child_hwnd_belongs_to_target(identity, post_hwnd, "WM_PASTE 粘贴目标")?;
            verify_postmessage_target_pid(identity, post_hwnd)?;
            unsafe {
                let _ = SendMessageW(
                    HWND(post_hwnd as *mut c_void),
                    WM_PASTE,
                    Some(WPARAM(0)),
                    Some(LPARAM(0)),
                );
            }
            thread::sleep(Duration::from_millis(
                spec.settle_delay_ms.max(MIN_BACKGROUND_PASTE_SETTLE_MS),
            ));
            let after_snapshot = read_editor_text_snapshot(spec.editor);
            if !after_snapshot.is_empty() && after_snapshot != before_snapshot {
                if spec.press_enter {
                    send_submit_background_for_editor_verified(
                        identity,
                        Some(spec.editor),
                        spec.send_button,
                        spec.shortcut,
                    )?;
                }
                return Ok((InputMode::BackgroundUia, false));
            }
        }
        if spec.mode == InputMode::BackgroundUia {
            return Err(AdapterError::UiaFailure(
                "后台 WM_PASTE 未产生可见粘贴回显，且当前严格限制为 BackgroundUia 模式".to_string(),
            ));
        }
    }

    let _fg_guard =
        enter_window_editor_foreground_inner(identity.hwnd(), spec.editor, Some(identity))?;
    let target_hwnd = HWND(identity.hwnd() as *mut c_void);
    ensure_foreground_target_intact(target_hwnd, Some(identity))?;

    send_vk_events(
        &[
            (VK_CONTROL, false),
            (VK_V, false),
            (VK_V, true),
            (VK_CONTROL, true),
        ],
        "paste",
    )?;
    thread::sleep(Duration::from_millis(
        spec.settle_delay_ms.max(MIN_FOCUS_PASTE_SETTLE_MS),
    ));
    ensure_foreground_target_intact(target_hwnd, Some(identity))?;

    if spec.press_enter {
        try_submit_in_foreground_inner(
            target_hwnd,
            spec.send_button,
            spec.shortcut,
            Some(identity),
        )?;
    }

    Ok((InputMode::FocusRestoreUnicode, true))
}

/// 将本地文件路径集合以标准 Win32 `CF_HDROP` (`DROPFILES`) 结构写入剪贴板，供文件粘贴发送使用
pub fn write_files_to_clipboard_hdrop(paths: &[std::path::PathBuf]) -> Result<()> {
    use std::path::{Path, PathBuf};

    if paths.is_empty() {
        return Err(AdapterError::InputRejected(
            "CF_HDROP 剪贴板写入要求至少提供一个文件路径".to_string(),
        ));
    }

    let mut payload_u16: Vec<u16> = Vec::with_capacity(10 + 128 * paths.len());
    // 按 Win32 DROPFILES 20 字节固定头布局写入：pFiles=20, pt=(0,0), fNC=0, fWide=1
    payload_u16.extend_from_slice(&[20, 0, 0, 0, 0, 0, 0, 0, 1, 0]);
    for path in paths {
        let canon: PathBuf = std::fs::canonicalize(path).unwrap_or_else(|_| path.clone());
        let raw_str = canon.to_string_lossy();
        let cleaned = raw_str.strip_prefix(r"\\?\").unwrap_or(&raw_str);
        if !Path::new(cleaned).is_file() {
            return Err(AdapterError::InputRejected(format!(
                "待写入剪贴板的本地文件不存在: {cleaned}"
            )));
        }
        payload_u16.extend(cleaned.encode_utf16());
        payload_u16.push(0);
    }
    payload_u16.push(0);

    let total_bytes = payload_u16.len() * std::mem::size_of::<u16>();
    let _serial = CLIPBOARD_PROCESS_LOCK.lock();
    let _open = open_clipboard_with_retry()?;

    unsafe {
        let _ = windows::Win32::System::DataExchange::EmptyClipboard();
        let mem = windows::Win32::System::Memory::GlobalAlloc(
            windows::Win32::System::Memory::GMEM_MOVEABLE,
            total_bytes,
        )
        .map_err(|err| {
            AdapterError::UiaFailure(format!("GlobalAlloc 分配 CF_HDROP 缓冲失败: {err}"))
        })?;
        let locked = windows::Win32::System::Memory::GlobalLock(mem);
        if locked.is_null() {
            let _ = windows::Win32::Foundation::GlobalFree(Some(mem));
            return Err(AdapterError::UiaFailure(
                "GlobalLock 锁定 CF_HDROP 内存失败".to_string(),
            ));
        }
        std::ptr::copy_nonoverlapping(
            payload_u16.as_ptr() as *const u8,
            locked as *mut u8,
            total_bytes,
        );
        let _ = windows::Win32::System::Memory::GlobalUnlock(mem);

        const CF_HDROP_FORMAT: u32 = 15;
        let handle = windows::Win32::Foundation::HANDLE(mem.0);
        if windows::Win32::System::DataExchange::SetClipboardData(CF_HDROP_FORMAT, Some(handle))
            .is_err()
        {
            let _ = windows::Win32::Foundation::GlobalFree(Some(mem));
            return Err(AdapterError::UiaFailure(
                "SetClipboardData(CF_HDROP) 写入失败".to_string(),
            ));
        }
    }

    Ok(())
}

/// 在进程级剪贴板互斥锁保护下读取当前 Unicode 文本（不可读或非文本格式时返回 `None`）
pub fn read_clipboard_unicode_text() -> Option<String> {
    let _serial = CLIPBOARD_PROCESS_LOCK.lock();
    Clipboard::open().ok()?.get_text().ok()
}

/// 在进程级剪贴板互斥锁保护下写入 Unicode 文本（供前后台还原自检及诊断使用）
pub fn write_clipboard_unicode_text(text: &str) -> Result<()> {
    let _serial = CLIPBOARD_PROCESS_LOCK.lock();
    let clip = Clipboard::open()
        .map_err(|err| AdapterError::UiaFailure(format!("打开剪贴板写入文本失败: {err}")))?;
    clip.set_text(text)
        .map_err(|err| AdapterError::UiaFailure(format!("写入剪贴板文本失败: {err}")))
}

#[cfg(test)]
mod tests {
    use parking_lot::Mutex;
    use uiautomation::clipboards::ClipboardFormat;

    use super::*;

    static CLIPBOARD_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn clipboard_restore_guard_preserves_prior_text_across_dib_write() {
        let _serial = CLIPBOARD_TEST_LOCK.lock();

        // 若当前测试桌面完全允许剪贴板访问，写入哨兵文本后写图再随 Drop 自动还原
        let Ok(prior_guard) = ClipboardRestoreGuard::capture() else {
            return;
        };

        let sentinel = "qq-uia-clipboard-sentinel-2026";
        if let Ok(clip) = Clipboard::open() {
            if clip.set_text(sentinel).is_err() {
                drop(prior_guard);
                return;
            }
        }

        {
            let _restore = ClipboardRestoreGuard::capture().expect("capture sentinel");
            let img = DecodedImageBgra {
                width: 2,
                height: 2,
                bgra_top_down: vec![255; 16],
            };
            write_bgra_to_clipboard_dib(&img).expect("write CF_DIB");
            let clip = Clipboard::open().expect("open after dib");
            assert!(clip
                .is_format_available(ClipboardFormat::DIB)
                .unwrap_or(false));
        }

        let restored = Clipboard::open()
            .and_then(|clip| clip.get_text())
            .unwrap_or_default();
        assert_eq!(restored, sentinel);
        drop(prior_guard);
    }

    #[test]
    fn clipboard_process_lock_serializes_concurrent_threads() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let active = Arc::new(AtomicUsize::new(0));
        let max_concurrent = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();

        for _ in 0..4 {
            let active_ref = Arc::clone(&active);
            let max_ref = Arc::clone(&max_concurrent);
            handles.push(thread::spawn(move || {
                for _ in 0..5 {
                    let _lock = CLIPBOARD_PROCESS_LOCK.lock();
                    let cur = active_ref.fetch_add(1, Ordering::SeqCst) + 1;
                    max_ref.fetch_max(cur, Ordering::SeqCst);
                    thread::sleep(Duration::from_millis(2));
                    active_ref.fetch_sub(1, Ordering::SeqCst);
                }
            }));
        }

        for handle in handles {
            handle.join().expect("worker thread finished");
        }
        assert_eq!(max_concurrent.load(Ordering::SeqCst), 1);

        // 若当前测试桌面支持剪贴板访问，并发捕获守卫也应安全串行通过
        if ClipboardRestoreGuard::capture().is_ok() {
            let mut clip_handles = Vec::new();
            for _ in 0..4 {
                clip_handles.push(thread::spawn(|| {
                    let guard = ClipboardRestoreGuard::capture().expect("concurrent capture");
                    thread::sleep(Duration::from_millis(2));
                    drop(guard);
                }));
            }
            for handle in clip_handles {
                handle.join().expect("concurrent clipboard thread finished");
            }
        }
    }

    #[test]
    fn clipboard_restore_guard_preserves_prior_text_across_hdrop_write() {
        let _serial = CLIPBOARD_TEST_LOCK.lock();

        let Ok(prior_guard) = ClipboardRestoreGuard::capture() else {
            return;
        };

        let sentinel = "qq-uia-hdrop-sentinel-2026";
        if let Ok(clip) = Clipboard::open() {
            if clip.set_text(sentinel).is_err() {
                drop(prior_guard);
                return;
            }
        }

        let temp_file = std::env::temp_dir().join("qq_uia_hdrop_test.txt");
        std::fs::write(&temp_file, b"hdrop-test").expect("write temp hdrop file");
        {
            let _restore = ClipboardRestoreGuard::capture().expect("capture sentinel");
            write_files_to_clipboard_hdrop(std::slice::from_ref(&temp_file))
                .expect("write CF_HDROP");
        }
        let _ = std::fs::remove_file(&temp_file);

        let restored = Clipboard::open()
            .and_then(|clip| clip.get_text())
            .unwrap_or_default();
        assert_eq!(restored, sentinel);
        drop(prior_guard);
    }
}
