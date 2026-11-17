//! Chromium 与 Electron 无障碍树唤醒模块，应用 RAII 守护对象模式管理 `VARIANT` 内存及 `SPI_SETSCREENREADER` 系统状态恢复

use std::ffi::c_void;
use std::time::Duration;

use common_types::{MsaaNodeSample, WakeReport};
use parking_lot::{Mutex, MutexGuard};
use windows::Win32::Foundation::{HWND, LPARAM, POINT, WPARAM};
use windows::Win32::System::Com::IServiceProvider;
use windows::Win32::System::Variant::{
    VariantClear, VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_DISPATCH, VT_I4,
};
use windows::Win32::UI::Accessibility::{
    AccessibleChildren, AccessibleObjectFromPoint, AccessibleObjectFromWindow, IAccessible,
    UiaRootObjectId,
};
use windows::Win32::UI::WindowsAndMessaging::{
    SendMessageTimeoutW, SystemParametersInfoW, OBJID_CLIENT, SMTO_ABORTIFHUNG,
    SPI_GETSCREENREADER, SPI_SETSCREENREADER, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, WM_GETOBJECT,
};
use windows_core::Interface;

use super::window_enum::{find_child_windows_by_class, get_window_rect, is_window_valid};
use crate::constants::{
    CHILDID_SELF, CHROMIUM_RENDER_CHILD_CLASSES, IID_IACCESSIBLE2, IID_IACCESSIBLE_TEXT,
    IID_ISIMPLE_DOM_NODE, MAX_MSAA_SAMPLES, MAX_MSAA_WAKE_DEPTH, MAX_MSAA_WAKE_VISITS,
    NAVIGATE_FIRST_CHILD_DIRECTION, POST_WAKE_SETTLE_MS, WAKE_MESSAGE_TIMEOUT_MS,
};

/// 串行化全局屏幕阅读器系统标志脉冲操作的非毒化互斥锁
static SCREEN_READER_PULSE_LOCK: Mutex<()> = Mutex::new(());

/// 自动批量释放 `VARIANT` 切片资源的 RAII 守护集合
pub(crate) struct VariantVecGuard(pub(crate) Vec<VARIANT>);

impl Drop for VariantVecGuard {
    fn drop(&mut self) {
        for variant_item in &mut self.0 {
            unsafe {
                let _ = VariantClear(variant_item);
            }
        }
    }
}

/// 自动释放单个 `VARIANT` COM 变体资源的 RAII 守护对象
pub(crate) struct SingleVariantGuard(pub(crate) VARIANT);

impl Drop for SingleVariantGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = VariantClear(&mut self.0);
        }
    }
}

/// 临时激活系统 `SPI_SETSCREENREADER` 标志并在析构时可靠恢复原值的 RAII 守护对象
struct ScreenReaderPulseGuard {
    _mutex_guard: Option<MutexGuard<'static, ()>>,
    was_active: bool,
    pulsed: bool,
}

impl ScreenReaderPulseGuard {
    /// 按需加锁并读取当前屏幕阅读器状态；当开启脉冲且系统原本未启用时临时打开标志
    fn acquire(pulse: bool) -> Self {
        let mutex_guard = if pulse {
            Some(SCREEN_READER_PULSE_LOCK.lock())
        } else {
            None
        };
        let mut active: i32 = 0;
        let query_result = unsafe {
            SystemParametersInfoW(
                SPI_GETSCREENREADER,
                0,
                Some(&mut active as *mut i32 as *mut c_void),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            )
        };
        let was_active = query_result.is_ok() && active != 0;
        let pulsed = pulse
            && query_result.is_ok()
            && active == 0
            && unsafe {
                SystemParametersInfoW(
                    SPI_SETSCREENREADER,
                    1,
                    None,
                    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
                )
            }
            .is_ok();
        Self {
            _mutex_guard: mutex_guard,
            was_active,
            pulsed,
        }
    }
}

impl Drop for ScreenReaderPulseGuard {
    fn drop(&mut self) {
        if self.pulsed {
            unsafe {
                let _ = SystemParametersInfoW(
                    SPI_SETSCREENREADER,
                    0,
                    None,
                    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
                );
            }
        }
    }
}

/// 构造 `VT_I4` 整数型 COM `VARIANT`
pub(crate) fn variant_i4(value: i32) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: std::mem::ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_I4,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 { lVal: value },
            }),
        },
    }
}

/// 构造指向元素自身的 `CHILDID_SELF` 变体参数
pub(crate) fn childid_self_variant() -> VARIANT {
    variant_i4(CHILDID_SELF)
}

/// 向目标 `IAccessible` 查询 `IAccessible2`、`IAccessibleText` 与 `ISimpleDOMNode` 扩展接口以触发 Chromium 懒加载节点实例化
pub(crate) unsafe fn query_ia2_interfaces(
    accessible: &IAccessible,
    first_hresult: &mut Option<i32>,
    report: &mut WakeReport,
) {
    match accessible.cast::<IServiceProvider>() {
        Ok(service_provider) => {
            for (service_guid, interface_id) in [
                (&IAccessible::IID, &IID_IACCESSIBLE2),
                (&IID_IACCESSIBLE2, &IID_IACCESSIBLE2),
                (&IAccessible::IID, &IID_IACCESSIBLE_TEXT),
                (&IAccessible::IID, &IID_ISIMPLE_DOM_NODE),
            ] {
                let mut output_pointer: *mut c_void = std::ptr::null_mut();
                let hresult = (Interface::vtable(&service_provider).QueryService)(
                    Interface::as_raw(&service_provider),
                    service_guid,
                    interface_id,
                    &mut output_pointer,
                );
                report.ia2_first_hr = *first_hresult.get_or_insert(hresult.0);
                if hresult.is_ok() && !output_pointer.is_null() {
                    report.ia2_query_service_ok += 1;
                    drop(windows_core::IUnknown::from_raw(output_pointer));
                }
            }
        }
        Err(error) => {
            report.ia2_first_hr = *first_hresult.get_or_insert(error.code().0);
        }
    }

    let child_self = childid_self_variant();
    if let Ok(variant_item) = accessible.accFocus() {
        let _guard = SingleVariantGuard(variant_item);
    }
    if let Ok(variant_item) = accessible.accNavigate(NAVIGATE_FIRST_CHILD_DIRECTION, &child_self) {
        let _guard = SingleVariantGuard(variant_item);
    }
}

/// 递归触摸 MSAA 无障碍子树并记录节点样本，迫使 Electron/Chromium 按需构建完整可访问树
unsafe fn touch_iaccessible_subtree(
    accessible: &IAccessible,
    depth: usize,
    visited_count: &mut usize,
    first_hresult: &mut Option<i32>,
    report: &mut WakeReport,
) {
    if *visited_count >= MAX_MSAA_WAKE_VISITS {
        return;
    }
    *visited_count += 1;
    report.visited_msaa_nodes = *visited_count;
    report.max_msaa_depth = report.max_msaa_depth.max(depth);

    query_ia2_interfaces(accessible, first_hresult, report);

    let child_count = accessible.accChildCount().unwrap_or(0);
    if report.msaa_samples.len() < MAX_MSAA_SAMPLES {
        let child_self = childid_self_variant();
        let role = match accessible.get_accRole(&child_self) {
            Ok(variant_item) => {
                let guard = SingleVariantGuard(variant_item);
                if guard.0.Anonymous.Anonymous.vt == VT_I4 {
                    guard.0.Anonymous.Anonymous.Anonymous.lVal
                } else {
                    0
                }
            }
            Err(_) => 0,
        };
        let name = accessible
            .get_accName(&child_self)
            .map(|text| text.to_string())
            .unwrap_or_default();
        report.msaa_samples.push(MsaaNodeSample {
            depth,
            role,
            name,
            child_count,
        });
    }

    if depth >= MAX_MSAA_WAKE_DEPTH || child_count <= 0 || *visited_count >= MAX_MSAA_WAKE_VISITS {
        return;
    }

    let fetch_count = (child_count as usize).min(MAX_MSAA_WAKE_VISITS);
    let mut children = VariantVecGuard((0..fetch_count).map(|_| VARIANT::default()).collect());
    let mut obtained: i32 = 0;
    if AccessibleChildren(accessible, 0, &mut children.0, &mut obtained).is_err() {
        return;
    }

    let count = (obtained.max(0) as usize).min(children.0.len());
    for variant_item in &children.0[..count] {
        if *visited_count >= MAX_MSAA_WAKE_VISITS {
            break;
        }
        let variant_type = variant_item.Anonymous.Anonymous.vt;
        if variant_type == VT_DISPATCH {
            if let Some(dispatch) = variant_item.Anonymous.Anonymous.Anonymous.pdispVal.as_ref() {
                if let Ok(child_accessible) = dispatch.cast::<IAccessible>() {
                    touch_iaccessible_subtree(
                        &child_accessible,
                        depth + 1,
                        visited_count,
                        first_hresult,
                        report,
                    );
                }
            }
        }
    }
}

/// 生成用于诊断与无障碍命中测试的代表性窗口坐标点（仅用于只读命中查询，绝不移动或点击鼠标）
pub(crate) fn diagnostic_probe_points(window_hwnd: isize) -> Vec<(i32, i32)> {
    let Some(rect) = get_window_rect(window_hwnd) else {
        return Vec::new();
    };
    let width = rect[2] - rect[0];
    let height = rect[3] - rect[1];
    if width <= 0 || height <= 0 {
        return Vec::new();
    }
    vec![
        (rect[0] + width / 5, rect[1] + height / 2),
        (rect[0] + width * 2 / 3, rect[1] + height / 2),
        (rect[0] + width * 2 / 3, rect[1] + height * 5 / 6),
    ]
}

/// 收集顶层窗口自身及内部常见的 Chromium 渲染器子窗口句柄列表
fn collect_wake_targets(window_hwnd: isize) -> Vec<isize> {
    let mut targets = vec![window_hwnd];
    for child_class in CHROMIUM_RENDER_CHILD_CLASSES {
        for child_hwnd in find_child_windows_by_class(window_hwnd, child_class) {
            if !targets.contains(&child_hwnd) {
                targets.push(child_hwnd);
            }
        }
    }
    targets
}

/// 向单个窗口句柄发送 `WM_GETOBJECT` 唤醒信号并获取其根 `IAccessible` 对象遍历子树
unsafe fn probe_hwnd_for_wake(
    target_hwnd: isize,
    visited_count: &mut usize,
    first_hresult: &mut Option<i32>,
    report: &mut WakeReport,
) -> Option<IAccessible> {
    let raw_hwnd = HWND(target_hwnd as *mut c_void);
    for object_id in [OBJID_CLIENT.0 as isize, UiaRootObjectId as isize] {
        let mut message_result: usize = 0;
        let _ = SendMessageTimeoutW(
            raw_hwnd,
            WM_GETOBJECT,
            WPARAM(0),
            LPARAM(object_id),
            SMTO_ABORTIFHUNG,
            WAKE_MESSAGE_TIMEOUT_MS,
            Some(&mut message_result),
        );
    }

    let mut accessible_pointer: *mut c_void = std::ptr::null_mut();
    let result = AccessibleObjectFromWindow(
        raw_hwnd,
        OBJID_CLIENT.0 as u32,
        &IAccessible::IID,
        &mut accessible_pointer,
    );
    match result {
        Ok(()) if !accessible_pointer.is_null() => {
            let accessible = IAccessible::from_raw(accessible_pointer);
            if *visited_count < MAX_MSAA_WAKE_VISITS {
                touch_iaccessible_subtree(&accessible, 0, visited_count, first_hresult, report);
            } else {
                query_ia2_interfaces(&accessible, first_hresult, report);
                let _ = accessible.accChildCount();
            }
            Some(accessible)
        }
        Ok(()) => None,
        Err(error) => {
            tracing::debug!(
                hwnd = target_hwnd,
                error = %error,
                "AccessibleObjectFromWindow 返回非 OK 状态（可忽略的非致命错误）"
            );
            None
        }
    }
}

/// 唤醒 Chromium/Electron 无障碍提供程序并可选脉冲式激活 `SPI_SETSCREENREADER`，返回包含诊断计数与节点样本的 `WakeReport`
pub fn wake_chromium_accessibility_ext(
    window_hwnd: isize,
    pulse_screen_reader: bool,
) -> WakeReport {
    let mut report = WakeReport::default();
    if !is_window_valid(window_hwnd) {
        return report;
    }

    report.target_hwnds_before = collect_wake_targets(window_hwnd);
    let _screen_reader_guard = ScreenReaderPulseGuard::acquire(pulse_screen_reader);
    report.screen_reader_flag_before = _screen_reader_guard.was_active;
    report.pulsed_screen_reader = _screen_reader_guard.pulsed;

    let mut visited_count = 0usize;
    let mut first_hresult: Option<i32> = None;
    let mut root_accessible: Option<IAccessible> = None;

    for &target_hwnd in &report.target_hwnds_before.clone() {
        let accessible = unsafe {
            probe_hwnd_for_wake(
                target_hwnd,
                &mut visited_count,
                &mut first_hresult,
                &mut report,
            )
        };
        if target_hwnd == window_hwnd {
            root_accessible = accessible;
        }
    }

    for (probe_x, probe_y) in diagnostic_probe_points(window_hwnd) {
        if let Some(ref root_node) = root_accessible {
            if let Ok(variant_item) = unsafe { root_node.accHitTest(probe_x, probe_y) } {
                let guard = SingleVariantGuard(variant_item);
                report.acc_hit_test_ok += 1;
                unsafe {
                    if guard.0.Anonymous.Anonymous.vt == VT_DISPATCH {
                        if let Some(dispatch) =
                            guard.0.Anonymous.Anonymous.Anonymous.pdispVal.as_ref()
                        {
                            if let Ok(hit_accessible) = dispatch.cast::<IAccessible>() {
                                query_ia2_interfaces(
                                    &hit_accessible,
                                    &mut first_hresult,
                                    &mut report,
                                );
                            }
                        }
                    }
                }
            }
        }

        if pulse_screen_reader {
            let mut optional_accessible: Option<IAccessible> = None;
            let mut variant_child = SingleVariantGuard(VARIANT::default());
            if unsafe {
                AccessibleObjectFromPoint(
                    POINT {
                        x: probe_x,
                        y: probe_y,
                    },
                    &mut optional_accessible,
                    &mut variant_child.0,
                )
            }
            .is_ok()
            {
                if let Some(hit_accessible) = optional_accessible {
                    report.acc_from_point_ok += 1;
                    unsafe {
                        query_ia2_interfaces(&hit_accessible, &mut first_hresult, &mut report);
                    }
                }
            }
        }
    }

    std::thread::sleep(Duration::from_millis(POST_WAKE_SETTLE_MS));
    report.target_hwnds_after = collect_wake_targets(window_hwnd);
    let new_targets: Vec<isize> = report
        .target_hwnds_after
        .iter()
        .copied()
        .filter(|candidate_hwnd| !report.target_hwnds_before.contains(candidate_hwnd))
        .collect();
    for target_hwnd in new_targets {
        let _ = unsafe {
            probe_hwnd_for_wake(
                target_hwnd,
                &mut visited_count,
                &mut first_hresult,
                &mut report,
            )
        };
    }

    report
}

/// 向主窗口及 Chromium 渲染器子窗口发送 `WM_GETOBJECT` 并遍历 MSAA 子树以唤醒无障碍树
pub fn wake_chromium_accessibility(window_hwnd: isize) {
    let _ = wake_chromium_accessibility_ext(window_hwnd, false);
}
