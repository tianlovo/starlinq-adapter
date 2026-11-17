//! 端到端自测第 12 步：利用 Win32 GDI 生成抗锯齿高对比度图片并验证 `can_send_video` 与 `POST /api/ocr`

use anyhow::{bail, ensure, Context, Result};
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use common_types::OcrImageResponse;
use reqwest::Method;
use serde_json::json;
use uia_core::{encode_bgra_to_png_bytes, DecodedImageBgra};
use windows::core::PCWSTR;
use windows::Win32::Foundation::COLORREF;
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, CreateFontW, DeleteDC, DeleteObject, GdiFlush,
    SelectObject, SetBkMode, SetTextColor, TextOutW, ANTIALIASED_QUALITY, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DIB_RGB_COLORS, HGDIOBJ,
    OUT_DEFAULT_PRECIS, TRANSPARENT,
};

use crate::constants::{ROUTE_API_OCR, ROUTE_ONEBOT_CAN_SEND_VIDEO};
use crate::http_client::http_json_request;

/// 使用 Win32 GDI 绘制大号抗锯齿黑字白底 `"HOYO 2026"` PNG 图像供真实 Windows OCR 引擎识别
pub(super) fn build_synthetic_ocr_png_bytes() -> Result<Vec<u8>> {
    let width = 480_u32;
    let height = 140_u32;
    let byte_len = (width as usize) * (height as usize) * 4;
    let mut bgra = vec![255_u8; byte_len];

    unsafe {
        let hdc = CreateCompatibleDC(None);
        ensure!(!hdc.is_invalid(), "CreateCompatibleDC failed");

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
        let Ok(hbmp) = CreateDIBSection(Some(hdc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0) else {
            let _ = DeleteDC(hdc);
            bail!("CreateDIBSection failed");
        };
        if bits.is_null() {
            let _ = DeleteObject(HGDIOBJ(hbmp.0));
            let _ = DeleteDC(hdc);
            bail!("CreateDIBSection returned null pixel pointer");
        }

        std::ptr::write_bytes(bits.cast::<u8>(), 255, byte_len);
        let old_bmp = SelectObject(hdc, HGDIOBJ(hbmp.0));
        let face_wide: Vec<u16> = "Tahoma\0".encode_utf16().collect();
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
            PCWSTR(face_wide.as_ptr()),
        );
        if hfont.is_invalid() {
            let _ = SelectObject(hdc, old_bmp);
            let _ = DeleteObject(HGDIOBJ(hbmp.0));
            let _ = DeleteDC(hdc);
            bail!("CreateFontW failed");
        }
        let old_font = SelectObject(hdc, HGDIOBJ(hfont.0));

        let _ = SetBkMode(hdc, TRANSPARENT);
        let _ = SetTextColor(hdc, COLORREF(0x0000_0000));
        let wide: Vec<u16> = "HOYO 2026".encode_utf16().collect();
        let drawn = TextOutW(hdc, 48, 36, &wide).as_bool();
        let _ = GdiFlush();

        if drawn {
            let slice = std::slice::from_raw_parts(bits.cast::<u8>(), byte_len);
            bgra.copy_from_slice(slice);
            for pixel in bgra.as_chunks_mut::<4>().0 {
                pixel[3] = 255;
            }
        }

        let _ = SelectObject(hdc, old_font);
        let _ = SelectObject(hdc, old_bmp);
        let _ = DeleteObject(HGDIOBJ(hfont.0));
        let _ = DeleteObject(HGDIOBJ(hbmp.0));
        let _ = DeleteDC(hdc);
        ensure!(drawn, "TextOutW failed");
    }

    encode_bgra_to_png_bytes(&DecodedImageBgra {
        width,
        height,
        bgra_top_down: bgra,
    })
    .map_err(Into::into)
}

/// 校验 OneBot `can_send_video` 与原生 REST `POST /api/ocr` 端到端识别返回
pub(super) async fn verify_video_cap_and_ocr_api(base_url: &str) -> Result<()> {
    let vid_cap_resp =
        http_json_request(base_url, Method::GET, ROUTE_ONEBOT_CAN_SEND_VIDEO, None).await?;
    if vid_cap_resp.status_code != 200 || vid_cap_resp.json()?["data"]["yes"] != true {
        bail!(
            "expected can_send_video yes==true, got: {}",
            vid_cap_resp.body
        );
    }

    let png_bytes = build_synthetic_ocr_png_bytes()?;
    let ocr_payload = json!({
        "image": format!("base64://{}", BASE64_STANDARD.encode(&png_bytes)),
    });
    let ocr_resp =
        http_json_request(base_url, Method::POST, ROUTE_API_OCR, Some(&ocr_payload)).await?;
    if ocr_resp.status_code != 200 {
        bail!(
            "expected POST /api/ocr 200, got {}: {}",
            ocr_resp.status_code,
            ocr_resp.body
        );
    }
    let decoded: OcrImageResponse = serde_json::from_str(&ocr_resp.body)
        .with_context(|| format!("failed to decode POST /api/ocr response: {}", ocr_resp.body))?;
    if decoded.language.trim().is_empty() || decoded.texts.is_empty() {
        bail!(
            "expected non-empty OCR language and detections, got: {}",
            ocr_resp.body
        );
    }
    let combined = decoded
        .texts
        .iter()
        .map(|item| item.text.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    ensure!(
        combined.contains("HOYO") && combined.contains("2026"),
        "expected OCR text containing HOYO and 2026, got '{combined}': {}",
        ocr_resp.body
    );
    if decoded
        .texts
        .iter()
        .any(|item| item.text.trim().is_empty() || item.coordinates.len() != 4)
    {
        bail!(
            "unexpected OCR detection shape in self-test: {}",
            ocr_resp.body
        );
    }
    Ok(())
}
