//! 高 DPI 离线截图线程感知上下文守卫、逻辑到物理坐标缩放与纯色空白帧检测辅助模块

use common_types::{AdapterError, Result};
use windows::core::{s, w};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};

use crate::constants::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2;

type SetThreadDpiAwarenessContextFn = unsafe extern "system" fn(isize) -> isize;

/// 线程级 `PerMonitorAwareV2` DPI 感知上下文 RAII 守卫，通过动态绑定 `user32.dll` 在离开作用域时自动恢复原上下文
pub(crate) struct ThreadDpiAwarenessGuard {
    previous_context: isize,
    setter: Option<SetThreadDpiAwarenessContextFn>,
}

impl ThreadDpiAwarenessGuard {
    /// 动态加载 `SetThreadDpiAwarenessContext` 并切换当前线程到 `PerMonitorAwareV2`
    pub(crate) fn enter_per_monitor_v2() -> Self {
        let Some(setter) = load_set_thread_dpi_awareness_context() else {
            return Self {
                previous_context: 0,
                setter: None,
            };
        };
        let previous_context = unsafe { setter(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        Self {
            previous_context,
            setter: Some(setter),
        }
    }
}

impl Drop for ThreadDpiAwarenessGuard {
    fn drop(&mut self) {
        if self.previous_context != 0 {
            if let Some(setter) = self.setter {
                unsafe {
                    let _ = setter(self.previous_context);
                }
            }
        }
    }
}

/// 动态解析 `user32.dll!SetThreadDpiAwarenessContext`，避免在根清单之外引入额外特性依赖
fn load_set_thread_dpi_awareness_context() -> Option<SetThreadDpiAwarenessContextFn> {
    unsafe {
        let module = GetModuleHandleW(w!("user32.dll")).ok()?;
        let proc = GetProcAddress(module, s!("SetThreadDpiAwarenessContext"))?;
        Some(std::mem::transmute::<
            unsafe extern "system" fn() -> isize,
            SetThreadDpiAwarenessContextFn,
        >(proc))
    }
}

/// 将单轴逻辑坐标按四舍五入比例缩放并裁剪到 `[0, phys]` 物理边界内
fn scale_axis_coord(value: i32, logical: i32, phys: i32) -> i32 {
    let clamped = i64::from(value).clamp(0, i64::from(logical));
    let scaled = (clamped * i64::from(phys) + i64::from(logical) / 2) / i64::from(logical);
    scaled.clamp(0, i64::from(phys)) as i32
}

/// 按逻辑窗口尺寸与物理窗口尺寸将逻辑相对裁剪矩形 `[l, t, r, b]` 等比例映射为物理裁剪矩形
pub(crate) fn scale_crop_rect_for_dpi(
    logical_w: i32,
    logical_h: i32,
    phys_w: i32,
    phys_h: i32,
    rel_rect: [i32; 4],
) -> Result<[i32; 4]> {
    if logical_w <= 0 || logical_h <= 0 || phys_w <= 0 || phys_h <= 0 {
        return Err(AdapterError::UiaFailure(format!(
            "DPI 缩放裁剪尺寸非法: logical={logical_w}x{logical_h}, phys={phys_w}x{phys_h}"
        )));
    }
    Ok([
        scale_axis_coord(rel_rect[0], logical_w, phys_w),
        scale_axis_coord(rel_rect[1], logical_h, phys_h),
        scale_axis_coord(rel_rect[2], logical_w, phys_w),
        scale_axis_coord(rel_rect[3], logical_h, phys_h),
    ])
}

/// 计算单像素 6 位量化 RGB 桶值（每通道右移 2 位），忽略微弱抗锯齿噪声与 Alpha 通道
fn quantized_rgb_bucket(bgra_pixel: &[u8]) -> u32 {
    let b = u32::from(bgra_pixel[0] >> 2);
    let g = u32::from(bgra_pixel[1] >> 2);
    let r = u32::from(bgra_pixel[2] >> 2);
    (r << 12) | (g << 6) | b
}

/// 检查裁剪出的 BGRA 图像内部像素（跳过边缘圆角描边）是否 100% 落在同一量化 RGB 桶内
pub(crate) fn is_solid_interior_bgra(bgra: &[u8], width: u32, height: u32) -> bool {
    let Some(pixel_count) = (width as usize).checked_mul(height as usize) else {
        return false;
    };
    let Some(expected_len) = pixel_count.checked_mul(4) else {
        return false;
    };
    if width == 0 || height == 0 || bgra.len() < expected_len {
        return false;
    }

    let w = width as usize;
    let h = height as usize;
    let margin_x = if w > 8 { 2 } else { 0 };
    let margin_y = if h > 8 { 2 } else { 0 };

    let mut baseline: Option<u32> = None;
    for y in margin_y..(h - margin_y) {
        for x in margin_x..(w - margin_x) {
            let offset = (y * w + x) * 4;
            let bucket = quantized_rgb_bucket(&bgra[offset..offset + 4]);
            match baseline {
                None => baseline = Some(bucket),
                Some(first) if first != bucket => return false,
                Some(_) => {}
            }
        }
    }

    baseline.is_some()
}

#[cfg(test)]
mod tests {
    use super::{is_solid_interior_bgra, scale_crop_rect_for_dpi, ThreadDpiAwarenessGuard};

    fn make_solid_bgra(width: u32, height: u32, b: u8, g: u8, r: u8) -> Vec<u8> {
        let mut buf = Vec::with_capacity((width * height * 4) as usize);
        for _ in 0..(width * height) {
            buf.extend_from_slice(&[b, g, r, 255]);
        }
        buf
    }

    #[test]
    fn scale_crop_rect_handles_150_percent_dpi_rounding_and_clamping() {
        let scaled = scale_crop_rect_for_dpi(1103, 779, 1655, 1169, [100, 200, 300, 400])
            .expect("150% DPI 缩放应成功");
        assert_eq!(scaled, [150, 300, 450, 600]);

        let full = scale_crop_rect_for_dpi(1103, 779, 1655, 1169, [-12, 0, 1200, 900])
            .expect("越界矩形应自动裁剪到物理尺寸");
        assert_eq!(full, [0, 0, 1655, 1169]);

        let identity = scale_crop_rect_for_dpi(1103, 779, 1103, 779, [275, 97, 811, 512])
            .expect("100% DPI 应保持恒等映射");
        assert_eq!(identity, [275, 97, 811, 512]);
        assert!(scale_crop_rect_for_dpi(0, 779, 1655, 1169, [0, 0, 10, 10]).is_err());
    }

    #[test]
    fn solid_interior_bgra_detects_uniform_vs_patterned_and_ignores_minor_noise_or_border() {
        let mut solid = make_solid_bgra(24, 24, 240, 241, 242);
        // 在最外圈第 0 行像素写入不同颜色（圆角边框不应干扰内部纯色判断）
        solid[0] = 10;
        solid[1] = 20;
        solid[2] = 30;
        // 在内部像素注入幅度不超过 `>> 2`（同一 4 级桶 [240..243]）的轻微抖动
        let inner_offset = (10 * 24 + 10) * 4;
        solid[inner_offset] = 243;
        solid[inner_offset + 1] = 242;
        solid[inner_offset + 2] = 240;
        assert!(is_solid_interior_bgra(&solid, 24, 24));

        // 注入跨越量化桶边界的明显像素差，应判定为非纯色真实图案
        let mut patterned = solid.clone();
        patterned[inner_offset] = 120;
        patterned[inner_offset + 1] = 40;
        patterned[inner_offset + 2] = 200;
        assert!(!is_solid_interior_bgra(&patterned, 24, 24));
        assert!(!is_solid_interior_bgra(&[], 24, 24));
    }

    #[test]
    fn thread_dpi_awareness_guard_enters_and_restores_without_panicking() {
        let _guard = ThreadDpiAwarenessGuard::enter_per_monitor_v2();
    }
}
