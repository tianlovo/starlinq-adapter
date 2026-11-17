//! Windows Imaging Component（WIC）原生硬件图像编解码与离线窗口区域裁剪（`PrintWindow`）引擎

use std::ffi::c_void;
use std::mem::size_of;

use common_types::{AdapterError, Result};
use windows::Win32::Foundation::{HGLOBAL, HWND, RECT, RPC_E_CHANGED_MODE, S_FALSE, S_OK};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GdiFlush, GetDC, ReleaseDC,
    SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ,
};
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_ContainerFormatPng, GUID_WICPixelFormat32bppBGRA,
    IWICImagingFactory, WICBitmapDitherTypeNone, WICBitmapEncoderNoCache,
    WICBitmapPaletteTypeMedianCut, WICDecodeMetadataCacheOnDemand,
};
use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS};
use windows::Win32::System::Com::StructuredStorage::{CreateStreamOnHGlobal, GetHGlobalFromStream};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};
use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;

use crate::constants::{
    MAX_DECODED_IMAGE_BYTES, MAX_DECODED_IMAGE_DIMENSION, TRANSIENT_BLANK_CAPTURE_RETRY_MS,
};
use crate::dpi_capture::{
    is_solid_interior_bgra, scale_crop_rect_for_dpi, ThreadDpiAwarenessGuard,
};

const PW_RENDERFULLCONTENT_FLAG: u32 = 2;

/// 统一解码为 32 位 BGRA 自顶向下行序的原始位图内存载体
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedImageBgra {
    pub width: u32,
    pub height: u32,
    pub bgra_top_down: Vec<u8>,
}

/// 配对管理单次图像编解码过程中的 COM 初始化生命周期（RAII 守卫）
struct ComInitGuard {
    should_uninit: bool,
}

impl ComInitGuard {
    fn enter() -> Result<Self> {
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr == S_OK || hr == S_FALSE {
            Ok(Self {
                should_uninit: true,
            })
        } else if hr == RPC_E_CHANGED_MODE {
            Ok(Self {
                should_uninit: false,
            })
        } else {
            Err(AdapterError::UiaFailure(format!(
                "CoInitializeEx 初始化 WIC COM 失败: {hr:?}"
            )))
        }
    }
}

impl Drop for ComInitGuard {
    fn drop(&mut self) {
        if self.should_uninit {
            unsafe {
                CoUninitialize();
            }
        }
    }
}

/// 统一回收离线截图过程中创建的屏幕 DC、内存 DC 与 DIB 位图句柄（RAII 守卫）
struct ScopedGdiCapture {
    screen_dc: Option<HDC>,
    mem_dc: Option<HDC>,
    dib: Option<HBITMAP>,
    old_obj: Option<HGDIOBJ>,
}

impl Drop for ScopedGdiCapture {
    fn drop(&mut self) {
        unsafe {
            if let (Some(mem), Some(old)) = (self.mem_dc, self.old_obj.take()) {
                let _ = SelectObject(mem, old);
            }
            if let Some(bitmap) = self.dib.take() {
                let _ = DeleteObject(HGDIOBJ(bitmap.0));
            }
            if let Some(mem) = self.mem_dc.take() {
                let _ = DeleteDC(mem);
            }
            if let Some(screen) = self.screen_dc.take() {
                let _ = ReleaseDC(None, screen);
            }
        }
    }
}

fn validate_bgra_dimensions(width: u32, height: u32) -> Result<(u32, usize)> {
    if width == 0 || height == 0 {
        return Err(AdapterError::InputRejected(
            "decoded image has zero dimensions".to_string(),
        ));
    }
    if width > MAX_DECODED_IMAGE_DIMENSION || height > MAX_DECODED_IMAGE_DIMENSION {
        return Err(AdapterError::InputRejected(format!(
            "image dimensions {width}x{height} exceed safe limit {MAX_DECODED_IMAGE_DIMENSION}"
        )));
    }
    let stride = width
        .checked_mul(4)
        .ok_or_else(|| AdapterError::InputRejected("image width overflow".to_string()))?;
    let byte_len = (stride as usize)
        .checked_mul(height as usize)
        .ok_or_else(|| AdapterError::InputRejected("image buffer size overflow".to_string()))?;
    if byte_len > MAX_DECODED_IMAGE_BYTES {
        return Err(AdapterError::InputRejected(format!(
            "image buffer {byte_len} bytes exceeds safe limit {MAX_DECODED_IMAGE_BYTES}"
        )));
    }
    Ok((stride, byte_len))
}

fn wic_err(action: &str, err: impl std::fmt::Display) -> AdapterError {
    AdapterError::UiaFailure(format!("WIC {action} 失败: {err}"))
}

fn create_wic_factory() -> Result<IWICImagingFactory> {
    unsafe { CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER) }
        .map_err(|err| wic_err("创建编码器工厂", err))
}

/// 使用系统原生 WIC 将 PNG/JPEG/GIF/BMP/WebP 字节流解码为 32 位 BGRA 位图
pub fn decode_image_bytes_to_bgra(encoded: &[u8]) -> Result<DecodedImageBgra> {
    if encoded.is_empty() {
        return Err(AdapterError::InputRejected(
            "image byte payload is empty".to_string(),
        ));
    }

    let _com = ComInitGuard::enter()?;
    let factory = create_wic_factory()?;
    unsafe {
        let stream = factory
            .CreateStream()
            .map_err(|err| wic_err("CreateStream", err))?;
        stream
            .InitializeFromMemory(encoded)
            .map_err(|err| wic_err("InitializeFromMemory", err))?;

        let decoder = factory
            .CreateDecoderFromStream(&stream, std::ptr::null(), WICDecodeMetadataCacheOnDemand)
            .map_err(|err| AdapterError::InputRejected(format!("不支持或损坏的图片数据: {err}")))?;
        let frame = decoder
            .GetFrame(0)
            .map_err(|err| wic_err("GetFrame", err))?;
        let converter = factory
            .CreateFormatConverter()
            .map_err(|err| wic_err("CreateFormatConverter", err))?;
        converter
            .Initialize(
                &frame,
                &GUID_WICPixelFormat32bppBGRA,
                WICBitmapDitherTypeNone,
                None,
                0.0,
                WICBitmapPaletteTypeMedianCut,
            )
            .map_err(|err| wic_err("FormatConverter::Initialize", err))?;

        let mut width = 0_u32;
        let mut height = 0_u32;
        converter
            .GetSize(&mut width, &mut height)
            .map_err(|err| wic_err("GetSize", err))?;
        let (stride, byte_len) = validate_bgra_dimensions(width, height)?;
        let mut pixels = vec![0_u8; byte_len];
        converter
            .CopyPixels(std::ptr::null(), stride, &mut pixels)
            .map_err(|err| wic_err("CopyPixels", err))?;

        Ok(DecodedImageBgra {
            width,
            height,
            bgra_top_down: pixels,
        })
    }
}

/// 使用系统原生 WIC 将 32 位 BGRA 自顶向下位图编码为标准 PNG 字节流
pub fn encode_bgra_to_png_bytes(img: &DecodedImageBgra) -> Result<Vec<u8>> {
    let (stride, expected_len) = validate_bgra_dimensions(img.width, img.height)?;
    if img.bgra_top_down.len() < expected_len {
        return Err(AdapterError::InputRejected(
            "invalid BGRA image buffer for PNG encoding".to_string(),
        ));
    }

    let _com = ComInitGuard::enter()?;
    let factory = create_wic_factory()?;
    unsafe {
        let stream = CreateStreamOnHGlobal(HGLOBAL::default(), true)
            .map_err(|err| wic_err("CreateStreamOnHGlobal", err))?;
        let encoder = factory
            .CreateEncoder(&GUID_ContainerFormatPng, std::ptr::null())
            .map_err(|err| wic_err("CreateEncoder(PNG)", err))?;
        encoder
            .Initialize(&stream, WICBitmapEncoderNoCache)
            .map_err(|err| wic_err("Encoder::Initialize", err))?;

        let mut frame_opt = None;
        let mut props_opt = None;
        encoder
            .CreateNewFrame(&mut frame_opt, &mut props_opt)
            .map_err(|err| wic_err("CreateNewFrame", err))?;
        let frame = frame_opt.ok_or_else(|| wic_err("CreateNewFrame", "empty frame"))?;

        frame
            .Initialize(props_opt.as_ref())
            .map_err(|err| wic_err("Frame::Initialize", err))?;
        frame
            .SetSize(img.width, img.height)
            .map_err(|err| wic_err("Frame::SetSize", err))?;
        let mut pixel_fmt = GUID_WICPixelFormat32bppBGRA;
        frame
            .SetPixelFormat(&mut pixel_fmt)
            .map_err(|err| wic_err("Frame::SetPixelFormat", err))?;

        frame
            .WritePixels(img.height, stride, &img.bgra_top_down[..expected_len])
            .map_err(|err| wic_err("Frame::WritePixels", err))?;
        frame
            .Commit()
            .map_err(|err| wic_err("Frame::Commit", err))?;
        encoder
            .Commit()
            .map_err(|err| wic_err("Encoder::Commit", err))?;

        let hglobal =
            GetHGlobalFromStream(&stream).map_err(|err| wic_err("GetHGlobalFromStream", err))?;
        let size = GlobalSize(hglobal);
        if size == 0 {
            return Ok(Vec::new());
        }
        let locked = GlobalLock(hglobal);
        if locked.is_null() {
            return Err(wic_err("GlobalLock", "returned null pointer"));
        }
        let png_bytes = std::slice::from_raw_parts(locked as *const u8, size).to_vec();
        let _ = GlobalUnlock(hglobal);
        Ok(png_bytes)
    }
}

/// 执行单次双 DPI 上下文离线截图（先按调用线程上下文匹配 UIA 逻辑矩形，再切至 `PerMonitorAwareV2` 执行 `PrintWindow` 与物理裁剪）
fn capture_window_crop_bgra_once(
    hwnd: isize,
    element_screen_rect: [i32; 4],
) -> Result<DecodedImageBgra> {
    if hwnd == 0 {
        return Err(AdapterError::WindowNotFound(
            "cannot capture crop from null HWND".to_string(),
        ));
    }

    let target_hwnd = HWND(hwnd as *mut c_void);
    let mut logical_rect = RECT::default();
    unsafe {
        GetWindowRect(target_hwnd, &mut logical_rect).map_err(|err| {
            AdapterError::UiaFailure(format!("GetWindowRect(0x{hwnd:x}) 失败: {err}"))
        })?;
    }

    // 采用 i64 宽整数做相对坐标裁剪，避免极限坐标矩形（如 i32::MIN/MAX）触发有符号减法溢出
    let logical_w_i64 = i64::from(logical_rect.right) - i64::from(logical_rect.left);
    let logical_h_i64 = i64::from(logical_rect.bottom) - i64::from(logical_rect.top);
    if logical_w_i64 <= 0 || logical_h_i64 <= 0 {
        return Err(AdapterError::UiaFailure(
            "目标窗口尺寸为 0，无法离线截图裁剪".to_string(),
        ));
    }
    if logical_w_i64 > i64::from(MAX_DECODED_IMAGE_DIMENSION)
        || logical_h_i64 > i64::from(MAX_DECODED_IMAGE_DIMENSION)
    {
        return Err(AdapterError::InputRejected(format!(
            "目标窗口逻辑尺寸 {logical_w_i64}x{logical_h_i64} 超过离线截图安全上限 {MAX_DECODED_IMAGE_DIMENSION}"
        )));
    }

    let rel_left = (i64::from(element_screen_rect[0]) - i64::from(logical_rect.left))
        .clamp(0, logical_w_i64) as i32;
    let rel_top = (i64::from(element_screen_rect[1]) - i64::from(logical_rect.top))
        .clamp(0, logical_h_i64) as i32;
    let rel_right = (i64::from(element_screen_rect[2]) - i64::from(logical_rect.left))
        .clamp(0, logical_w_i64) as i32;
    let rel_bottom = (i64::from(element_screen_rect[3]) - i64::from(logical_rect.top))
        .clamp(0, logical_h_i64) as i32;
    let logical_crop_w = rel_right.saturating_sub(rel_left);
    let logical_crop_h = rel_bottom.saturating_sub(rel_top);
    if logical_crop_w <= 8 || logical_crop_h <= 8 {
        return Err(AdapterError::UiaFailure(format!(
            "待裁剪图片区域超出可见窗体或过小: {logical_crop_w}x{logical_crop_h}"
        )));
    }

    let _dpi_guard = ThreadDpiAwarenessGuard::enter_per_monitor_v2();
    let mut phys_rect = RECT::default();
    unsafe {
        GetWindowRect(target_hwnd, &mut phys_rect).map_err(|err| {
            AdapterError::UiaFailure(format!(
                "GetWindowRect(PerMonitorV2, 0x{hwnd:x}) 失败: {err}"
            ))
        })?;
    }
    let phys_w_i64 = i64::from(phys_rect.right) - i64::from(phys_rect.left);
    let phys_h_i64 = i64::from(phys_rect.bottom) - i64::from(phys_rect.top);
    if phys_w_i64 <= 0 || phys_h_i64 <= 0 {
        return Err(AdapterError::UiaFailure(
            "目标窗口物理尺寸为 0，无法离线截图裁剪".to_string(),
        ));
    }
    if phys_w_i64 > i64::from(MAX_DECODED_IMAGE_DIMENSION)
        || phys_h_i64 > i64::from(MAX_DECODED_IMAGE_DIMENSION)
    {
        return Err(AdapterError::InputRejected(format!(
            "目标窗口物理尺寸 {phys_w_i64}x{phys_h_i64} 超过离线截图安全上限 {MAX_DECODED_IMAGE_DIMENSION}"
        )));
    }
    let phys_w = phys_w_i64 as i32;
    let phys_h = phys_h_i64 as i32;
    let [crop_left, crop_top, crop_right, crop_bottom] = scale_crop_rect_for_dpi(
        logical_w_i64 as i32,
        logical_h_i64 as i32,
        phys_w,
        phys_h,
        [rel_left, rel_top, rel_right, rel_bottom],
    )?;
    let crop_w = crop_right.saturating_sub(crop_left);
    let crop_h = crop_bottom.saturating_sub(crop_top);
    if crop_w <= 8 || crop_h <= 8 {
        return Err(AdapterError::UiaFailure(format!(
            "缩放后物理裁剪图片区域过小: {crop_w}x{crop_h}"
        )));
    }
    let _ = validate_bgra_dimensions(crop_w as u32, crop_h as u32)?;

    unsafe {
        let screen_dc = GetDC(None);
        if screen_dc.is_invalid() {
            return Err(AdapterError::UiaFailure("GetDC(NULL) 失败".to_string()));
        }
        let mut guard = ScopedGdiCapture {
            screen_dc: Some(screen_dc),
            mem_dc: None,
            dib: None,
            old_obj: None,
        };

        let mem_dc = CreateCompatibleDC(Some(screen_dc));
        if mem_dc.is_invalid() {
            return Err(AdapterError::UiaFailure(
                "CreateCompatibleDC 失败".to_string(),
            ));
        }
        guard.mem_dc = Some(mem_dc);

        // 负高度创建顶向下 DIBSection，使内存行号与物理屏幕纵坐标一致便于精确裁剪
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: phys_w,
                biHeight: -phys_h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits_ptr: *mut c_void = std::ptr::null_mut();
        let dib = CreateDIBSection(Some(mem_dc), &bmi, DIB_RGB_COLORS, &mut bits_ptr, None, 0)
            .map_err(|err| AdapterError::UiaFailure(format!("CreateDIBSection 失败: {err}")))?;
        guard.dib = Some(dib);
        if bits_ptr.is_null() {
            return Err(AdapterError::UiaFailure(
                "CreateDIBSection 返回空像素缓冲区".to_string(),
            ));
        }

        let old_obj = SelectObject(mem_dc, HGDIOBJ(dib.0));
        guard.old_obj = Some(old_obj);

        let printed = PrintWindow(
            target_hwnd,
            mem_dc,
            PRINT_WINDOW_FLAGS(PW_RENDERFULLCONTENT_FLAG),
        )
        .as_bool()
            || PrintWindow(target_hwnd, mem_dc, PRINT_WINDOW_FLAGS(0)).as_bool();
        if !printed {
            return Err(AdapterError::UiaFailure(format!(
                "PrintWindow(0x{hwnd:x}) 返回失败"
            )));
        }
        let _ = GdiFlush();

        let full_stride = (phys_w as usize) * 4;
        let crop_stride = (crop_w as usize) * 4;
        let full_bytes =
            std::slice::from_raw_parts(bits_ptr as *const u8, full_stride * (phys_h as usize));
        let mut cropped = vec![0_u8; crop_stride * (crop_h as usize)];

        for row in 0..(crop_h as usize) {
            let src_y = (crop_top as usize) + row;
            let src_off = src_y * full_stride + (crop_left as usize) * 4;
            let dst_off = row * crop_stride;
            cropped[dst_off..dst_off + crop_stride]
                .copy_from_slice(&full_bytes[src_off..src_off + crop_stride]);
            // GDI 渲染区域透明度常置 0，强制填满不透明 Alpha 以确保所有看图器正确显示
            for px in cropped[dst_off..dst_off + crop_stride]
                .as_chunks_mut::<4>()
                .0
            {
                px[3] = 0xFF;
            }
        }

        Ok(DecodedImageBgra {
            width: crop_w as u32,
            height: crop_h as u32,
            bgra_top_down: cropped,
        })
    }
}

/// 在不抢占前台与鼠标的前提下，使用 `PrintWindow(PW_RENDERFULLCONTENT)` 离线截取窗口内指定屏幕坐标矩形并编码为 PNG（遇到瞬态纯色占位帧时等待 80ms 重试一次）
pub fn capture_window_crop_png(hwnd: isize, element_screen_rect: [i32; 4]) -> Result<Vec<u8>> {
    let first = capture_window_crop_bgra_once(hwnd, element_screen_rect)?;
    if is_solid_interior_bgra(&first.bgra_top_down, first.width, first.height) {
        std::thread::sleep(std::time::Duration::from_millis(
            TRANSIENT_BLANK_CAPTURE_RETRY_MS,
        ));
        let second = capture_window_crop_bgra_once(hwnd, element_screen_rect)?;
        return encode_bgra_to_png_bytes(&second);
    }
    encode_bgra_to_png_bytes(&first)
}

/// 使用 `PrintWindow` 采集指定窗体的完整像素视图并编码为无损 PNG 字节流
pub fn capture_window_full_png(hwnd: isize) -> Result<Vec<u8>> {
    let rect = crate::win32::get_window_rect(hwnd).ok_or_else(|| {
        AdapterError::WindowNotFound(format!("读取窗口 HWND 0x{hwnd:x} 屏幕矩形失败"))
    })?;
    capture_window_crop_png(hwnd, rect)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wic_png_encode_decode_roundtrip_preserves_dimensions_and_pixels() {
        // 构造 2x2 顶向下 BGRA 像素块（蓝、绿、红、白）验证 WIC 编解码往返一致
        let original = DecodedImageBgra {
            width: 2,
            height: 2,
            bgra_top_down: vec![
                255, 0, 0, 255, // (0, 0) 纯蓝
                0, 255, 0, 255, // (1, 0) 纯绿
                0, 0, 255, 255, // (0, 1) 纯红
                255, 255, 255, 255, // (1, 1) 纯白
            ],
        };
        let png = encode_bgra_to_png_bytes(&original).expect("encode PNG");
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));

        let decoded = decode_image_bytes_to_bgra(&png).expect("decode PNG");
        assert_eq!(decoded, original);
    }

    #[test]
    fn wic_rejects_oversized_dimensions_and_saturates_crop_rect() {
        // 验证 QA-BUG-004：超大尺寸位图在分配像素缓冲区前即被安全拦截
        let oversized = DecodedImageBgra {
            width: MAX_DECODED_IMAGE_DIMENSION + 1,
            height: 1,
            bgra_top_down: Vec::new(),
        };
        assert!(encode_bgra_to_png_bytes(&oversized).is_err());
        assert!(crate::clipboard_media::write_bgra_to_clipboard_dib(&oversized).is_err());

        // 传入 i32 极值矩形坐标应安全返回业务错误而非整数减法溢出崩溃
        let desktop_hwnd = unsafe { windows::Win32::UI::WindowsAndMessaging::GetDesktopWindow() };
        let res = capture_window_crop_png(
            desktop_hwnd.0 as isize,
            [i32::MIN, i32::MIN, i32::MAX, i32::MAX],
        );
        assert!(res.is_err());
    }
}
