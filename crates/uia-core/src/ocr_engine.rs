//! 基于 Windows 原生 WinRT `Windows.Media.Ocr` 引擎的离线图片文字识别模块

use std::sync::OnceLock;

use common_types::{AdapterError, OcrImageResponse, OcrPoint, OcrTextDetection, Result};
use windows::Foundation::Rect;
use windows::Globalization::Language;
use windows::Graphics::Imaging::{BitmapPixelFormat, SoftwareBitmap};
use windows::Media::Ocr::{OcrEngine, OcrLine};
use windows::Storage::Streams::DataWriter;
use windows::Win32::Foundation::{RPC_E_CHANGED_MODE, S_FALSE, S_OK};
use windows::Win32::System::Com::{
    CoIncrementMTAUsage, CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED,
};
use windows_core::HSTRING;

use crate::wic_codec::decode_image_bytes_to_bgra;

/// 默认优先探测的简体中文 OCR 语言标签列表
const DEFAULT_OCR_LANGUAGES: [&str; 2] = ["zh-Hans-CN", "zh-CN"];

/// 钉住进程级 COM MTA 引用，防止 worker 线程 CoUninitialize 后卸载 WinRT DLL 导致 FactoryCache 悬空
fn ensure_process_mta_pinned() -> Result<()> {
    static MTA_PIN: OnceLock<std::result::Result<(), i32>> = OnceLock::new();
    let res = MTA_PIN.get_or_init(|| match unsafe { CoIncrementMTAUsage() } {
        Ok(_cookie) => Ok(()),
        Err(err) => Err(err.code().0),
    });
    match res {
        Ok(()) => Ok(()),
        Err(hresult) => Err(AdapterError::UiaFailure(format!(
            "CoIncrementMTAUsage 钉住 OCR WinRT MTA 失败: 0x{:08x}",
            *hresult as u32
        ))),
    }
}

/// 配对管理单次 WinRT OCR 调用过程中的 COM 套间初始化生命周期（RAII 守卫）
struct OcrComGuard {
    should_uninit: bool,
}

impl OcrComGuard {
    fn enter() -> Result<Self> {
        ensure_process_mta_pinned()?;
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
                "CoInitializeEx 初始化 OCR WinRT 套间失败: {hr:?}"
            )))
        }
    }
}

impl Drop for OcrComGuard {
    fn drop(&mut self) {
        if self.should_uninit {
            unsafe {
                CoUninitialize();
            }
        }
    }
}

fn winrt_err(action: &str, err: impl std::fmt::Display) -> AdapterError {
    AdapterError::UiaFailure(format!("WinRT OCR {action} 失败: {err}"))
}

fn is_cjk_char(ch: char) -> bool {
    matches!(
        ch,
        '\u{3000}'..='\u{9FFF}' | '\u{F900}'..='\u{FAFF}' | '\u{FF00}'..='\u{FFEF}'
    )
}

fn join_ocr_words<I, S>(words: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut joined = String::new();
    for raw_word in words {
        let token = raw_word.as_ref().trim();
        if token.is_empty() {
            continue;
        }
        if let (Some(prev_last), Some(next_first)) =
            (joined.chars().next_back(), token.chars().next())
        {
            if !is_cjk_char(prev_last) && !is_cjk_char(next_first) {
                joined.push(' ');
            }
        }
        joined.push_str(token);
    }
    joined
}

fn try_create_engine_for_tag(tag: &str) -> Option<OcrEngine> {
    let lang = Language::CreateLanguage(&HSTRING::from(tag)).ok()?;
    if !OcrEngine::IsLanguageSupported(&lang).ok()? {
        return None;
    }
    OcrEngine::TryCreateFromLanguage(&lang).ok()
}

fn create_ocr_engine(preferred_language: Option<&str>) -> Result<OcrEngine> {
    if let Some(tag) = preferred_language {
        let lang = Language::CreateLanguage(&HSTRING::from(tag)).map_err(|err| {
            AdapterError::InputRejected(format!("无效的 OCR 语言标签 '{tag}': {err}"))
        })?;
        let supported = OcrEngine::IsLanguageSupported(&lang).unwrap_or(false);
        if !supported {
            return Err(AdapterError::InputRejected(format!(
                "系统未安装或不支持 OCR 语言 '{tag}'"
            )));
        }
        return OcrEngine::TryCreateFromLanguage(&lang).map_err(|_| {
            AdapterError::InputRejected(format!("无法为语言 '{tag}' 创建 OCR 识别引擎"))
        });
    }

    for default_tag in DEFAULT_OCR_LANGUAGES {
        if let Some(engine) = try_create_engine_for_tag(default_tag) {
            return Ok(engine);
        }
    }

    if let Ok(engine) = OcrEngine::TryCreateFromUserProfileLanguages() {
        return Ok(engine);
    }

    let available = OcrEngine::AvailableRecognizerLanguages()
        .map_err(|err| winrt_err("AvailableRecognizerLanguages", err))?;
    let count = available.Size().unwrap_or(0);
    for idx in 0..count {
        if let Ok(lang) = available.GetAt(idx) {
            if let Ok(engine) = OcrEngine::TryCreateFromLanguage(&lang) {
                return Ok(engine);
            }
        }
    }

    Err(AdapterError::UiaFailure(
        "系统未安装任何可用的 Windows OCR 语言识别包".to_string(),
    ))
}

fn build_line_coordinates(rects: &[Rect]) -> Vec<OcrPoint> {
    let mut min_x = f32::INFINITY;
    let mut min_y = f32::INFINITY;
    let mut max_x = f32::NEG_INFINITY;
    let mut max_y = f32::NEG_INFINITY;
    let mut any_valid = false;

    for rect in rects {
        let x0 = rect.X;
        let y0 = rect.Y;
        let x1 = rect.X + rect.Width;
        let y1 = rect.Y + rect.Height;
        if x0.is_finite() && y0.is_finite() && x1.is_finite() && y1.is_finite() {
            min_x = min_x.min(x0);
            min_y = min_y.min(y0);
            max_x = max_x.max(x1);
            max_y = max_y.max(y1);
            any_valid = true;
        }
    }

    if !any_valid {
        return Vec::new();
    }

    let left = min_x.round() as i32;
    let top = min_y.round() as i32;
    let right = (max_x.round() as i32).max(left);
    let bottom = (max_y.round() as i32).max(top);

    vec![
        OcrPoint { x: left, y: top },
        OcrPoint { x: right, y: top },
        OcrPoint {
            x: right,
            y: bottom,
        },
        OcrPoint { x: left, y: bottom },
    ]
}

fn extract_ocr_line(line: &OcrLine) -> Result<Option<OcrTextDetection>> {
    let words = line
        .Words()
        .map_err(|err| winrt_err("OcrLine::Words", err))?;
    let word_count = words
        .Size()
        .map_err(|err| winrt_err("IVectorView<OcrWord>::Size", err))?;

    let mut word_texts = Vec::with_capacity(word_count as usize);
    let mut word_rects = Vec::with_capacity(word_count as usize);
    for idx in 0..word_count {
        let word = words
            .GetAt(idx)
            .map_err(|err| winrt_err("IVectorView<OcrWord>::GetAt", err))?;
        let text = word
            .Text()
            .map_err(|err| winrt_err("OcrWord::Text", err))?
            .to_string_lossy();
        if !text.trim().is_empty() {
            word_texts.push(text);
        }
        if let Ok(rect) = word.BoundingRect() {
            word_rects.push(rect);
        }
    }

    let text = join_ocr_words(word_texts);
    if text.is_empty() {
        return Ok(None);
    }

    let coordinates = build_line_coordinates(&word_rects);
    Ok(Some(OcrTextDetection {
        text,
        confidence: 100,
        coordinates,
    }))
}

fn run_winrt_ocr_in_apartment(
    bgra: &[u8],
    width: u32,
    height: u32,
    preferred_language: Option<&str>,
) -> Result<OcrImageResponse> {
    let max_dim = OcrEngine::MaxImageDimension()
        .map_err(|err| winrt_err("OcrEngine::MaxImageDimension", err))?;
    if width > max_dim || height > max_dim {
        return Err(AdapterError::InputRejected(format!(
            "OCR 图像尺寸 {width}x{height} 超过系统引擎最大边长限制 {max_dim}"
        )));
    }
    let width_i32 = i32::try_from(width)
        .map_err(|_| AdapterError::InputRejected(format!("OCR 图像宽度 {width} 超出 i32 范围")))?;
    let height_i32 = i32::try_from(height)
        .map_err(|_| AdapterError::InputRejected(format!("OCR 图像高度 {height} 超出 i32 范围")))?;

    let engine = create_ocr_engine(preferred_language)?;
    let language = engine
        .RecognizerLanguage()
        .and_then(|lang| lang.LanguageTag())
        .map(|tag| tag.to_string_lossy())
        .unwrap_or_else(|_| preferred_language.unwrap_or("unknown").to_string());

    let writer = DataWriter::new().map_err(|err| winrt_err("DataWriter::new", err))?;
    writer
        .WriteBytes(bgra)
        .map_err(|err| winrt_err("DataWriter::WriteBytes", err))?;
    let buffer = writer
        .DetachBuffer()
        .map_err(|err| winrt_err("DataWriter::DetachBuffer", err))?;
    let soft_bmp = SoftwareBitmap::CreateCopyFromBuffer(
        &buffer,
        BitmapPixelFormat::Bgra8,
        width_i32,
        height_i32,
    )
    .map_err(|err| winrt_err("SoftwareBitmap::CreateCopyFromBuffer", err))?;

    let ocr_result = engine
        .RecognizeAsync(&soft_bmp)
        .map_err(|err| winrt_err("OcrEngine::RecognizeAsync", err))?
        .join()
        .map_err(|err| winrt_err("OcrResult::join", err))?;

    let lines = ocr_result
        .Lines()
        .map_err(|err| winrt_err("OcrResult::Lines", err))?;
    let line_count = lines
        .Size()
        .map_err(|err| winrt_err("IVectorView<OcrLine>::Size", err))?;

    let mut texts = Vec::with_capacity(line_count as usize);
    for idx in 0..line_count {
        let line = lines
            .GetAt(idx)
            .map_err(|err| winrt_err("IVectorView<OcrLine>::GetAt", err))?;
        if let Some(detection) = extract_ocr_line(&line)? {
            texts.push(detection);
        }
    }

    Ok(OcrImageResponse { texts, language })
}

/// 对自顶向下 32 位 BGRA 原始像素缓冲区执行 Windows WinRT 本地 OCR 识别
pub fn recognize_bgra_image(
    bgra: &[u8],
    width: u32,
    height: u32,
    preferred_language: Option<&str>,
) -> Result<OcrImageResponse> {
    if width == 0 || height == 0 {
        return Err(AdapterError::InputRejected(
            "OCR 图像宽高不能为 0".to_string(),
        ));
    }

    let expected_len = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| AdapterError::InputRejected("OCR 图像像素字节长度溢出".to_string()))?;
    if bgra.len() != expected_len {
        return Err(AdapterError::InputRejected(format!(
            "OCR BGRA 缓冲区长度 {} 与尺寸 {}x{} (期望 {} 字节) 不匹配",
            bgra.len(),
            width,
            height,
            expected_len
        )));
    }
    if u32::try_from(bgra.len()).is_err() {
        return Err(AdapterError::InputRejected(
            "OCR BGRA 缓冲区长度超出 WinRT DataWriter u32 上限".to_string(),
        ));
    }

    let normalized_lang = preferred_language.map(str::trim).filter(|s| !s.is_empty());
    let _com_guard = OcrComGuard::enter()?;
    run_winrt_ocr_in_apartment(bgra, width, height, normalized_lang)
}

/// 先使用原生 WIC 将常见编码图片字节（PNG/JPEG/BMP/GIF/WebP）解码为 BGRA，再执行 WinRT OCR 识别
pub fn recognize_image_bytes(
    bytes: &[u8],
    preferred_language: Option<&str>,
) -> Result<OcrImageResponse> {
    let decoded = decode_image_bytes_to_bgra(bytes)?;
    recognize_bgra_image(
        &decoded.bgra_top_down,
        decoded.width,
        decoded.height,
        preferred_language,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wic_codec::{encode_bgra_to_png_bytes, DecodedImageBgra};
    use windows::Win32::Foundation::COLORREF;
    use windows::Win32::Graphics::Gdi::{
        CreateCompatibleDC, CreateDIBSection, CreateFontW, DeleteDC, DeleteObject, GdiFlush,
        SelectObject, SetBkMode, SetTextColor, TextOutW, ANTIALIASED_QUALITY, BITMAPINFO,
        BITMAPINFOHEADER, BI_RGB, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DIB_RGB_COLORS, HGDIOBJ,
        OUT_DEFAULT_PRECIS, TRANSPARENT,
    };

    fn render_synthetic_banner_bgra(text: &str) -> DecodedImageBgra {
        let width = 480_u32;
        let height = 140_u32;
        let byte_len = (width as usize) * (height as usize) * 4;
        let mut bgra = vec![255_u8; byte_len];

        unsafe {
            let hdc = CreateCompatibleDC(None);
            assert!(!hdc.is_invalid(), "CreateCompatibleDC failed");

            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width as i32,
                    biHeight: -(height as i32),
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };

            let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
            let hbmp = CreateDIBSection(Some(hdc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0)
                .expect("CreateDIBSection failed");
            assert!(!bits.is_null(), "DIB pixel pointer must not be null");

            std::ptr::write_bytes(bits.cast::<u8>(), 255, byte_len);
            let old_bmp = SelectObject(hdc, HGDIOBJ(hbmp.0));
            let hfont = CreateFontW(
                -36,
                0,
                0,
                0,
                700,
                0,
                0,
                0,
                DEFAULT_CHARSET,
                OUT_DEFAULT_PRECIS,
                CLIP_DEFAULT_PRECIS,
                ANTIALIASED_QUALITY,
                0,
                windows_core::w!("Tahoma"),
            );
            assert!(!hfont.is_invalid(), "CreateFontW failed");
            let old_font = SelectObject(hdc, HGDIOBJ(hfont.0));

            let _ = SetBkMode(hdc, TRANSPARENT);
            let _ = SetTextColor(hdc, COLORREF(0x0000_0000));
            let wide: Vec<u16> = text.encode_utf16().collect();
            let drawn = TextOutW(hdc, 48, 36, &wide);
            assert!(drawn.as_bool(), "TextOutW failed");
            let _ = GdiFlush();

            let slice = std::slice::from_raw_parts(bits.cast::<u8>(), byte_len);
            bgra.copy_from_slice(slice);
            for pixel in bgra.as_chunks_mut::<4>().0 {
                pixel[3] = 255;
            }

            let _ = SelectObject(hdc, old_font);
            let _ = SelectObject(hdc, old_bmp);
            let _ = DeleteObject(HGDIOBJ(hfont.0));
            let _ = DeleteObject(HGDIOBJ(hbmp.0));
            let _ = DeleteDC(hdc);
        }

        DecodedImageBgra {
            width,
            height,
            bgra_top_down: bgra,
        }
    }

    #[test]
    fn join_ocr_words_applies_cjk_aware_spacing() {
        assert_eq!(join_ocr_words(["你", "好", "世", "界"]), "你好世界");
        assert_eq!(join_ocr_words(["HOYO", "2026"]), "HOYO 2026");
        assert_eq!(join_ocr_words(["Demo", "测", "试", "群"]), "Demo测试群");
        assert_eq!(join_ocr_words(["第", "1", "名"]), "第1名");
    }

    #[test]
    fn ocr_rejects_invalid_dimensions_buffers_and_unsupported_language() {
        let zero_err = recognize_bgra_image(&[], 0, 10, None).unwrap_err();
        assert!(matches!(zero_err, AdapterError::InputRejected(_)));

        let len_err = recognize_bgra_image(&[255; 12], 2, 2, None).unwrap_err();
        assert!(matches!(len_err, AdapterError::InputRejected(_)));

        let com_guard = OcrComGuard::enter().expect("enter COM");
        let max_dim = OcrEngine::MaxImageDimension().expect("MaxImageDimension");
        drop(com_guard);
        assert!(max_dim >= 1024);

        let oversized_err =
            recognize_bgra_image(&[255_u8; 4 * 4], max_dim + 1, 1, None).unwrap_err();
        assert!(matches!(oversized_err, AdapterError::InputRejected(_)));

        let bad_lang_err = recognize_bgra_image(&[255; 16], 2, 2, Some("xx-INVALID-999"))
            .expect_err("unsupported language must return InputRejected");
        assert!(matches!(bad_lang_err, AdapterError::InputRejected(_)));
    }

    #[test]
    fn real_winrt_ocr_recognizes_synthetic_bgra_and_png_on_worker_thread() {
        let img = render_synthetic_banner_bgra("HOYO 2026");
        let png_bytes = encode_bgra_to_png_bytes(&img).expect("encode synthetic PNG");

        // 直接测试 BGRA 识别并检查行级文本与顺时针 4 角点
        let bgra_res = recognize_bgra_image(&img.bgra_top_down, img.width, img.height, None)
            .expect("recognize_bgra_image should succeed");
        assert!(!bgra_res.language.is_empty());
        let combined_bgra: String = bgra_res
            .texts
            .iter()
            .map(|d| d.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            combined_bgra.contains("HOYO") && combined_bgra.contains("2026"),
            "unexpected BGRA OCR text: '{combined_bgra}' ({bgra_res:?})"
        );
        for item in &bgra_res.texts {
            assert_eq!(item.confidence, 100);
            assert_eq!(item.coordinates.len(), 4);
            assert!(item.coordinates[0].x <= item.coordinates[1].x);
            assert!(item.coordinates[1].y <= item.coordinates[2].y);
        }

        // 在全新工作线程上测试 PNG 字节解码 + WinRT OCR，验证套间初始化与生命周期安全
        let worker = std::thread::spawn(move || recognize_image_bytes(&png_bytes, None));
        let png_res = worker
            .join()
            .expect("worker thread should not panic")
            .expect("recognize_image_bytes should succeed");
        let combined_png: String = png_res
            .texts
            .iter()
            .map(|d| d.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            combined_png.contains("HOYO") && combined_png.contains("2026"),
            "unexpected PNG OCR text: '{combined_png}' ({png_res:?})"
        );
        for item in &png_res.texts {
            assert_eq!(item.coordinates.len(), 4);
        }
    }
}
