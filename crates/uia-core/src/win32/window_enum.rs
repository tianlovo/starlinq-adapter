//! Win32 顶层窗口、子窗口与安全风控弹窗枚举过滤模块，提供候选窗口筛选、Z 序还原与状态刷新能力

use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::mem::size_of;

use common_types::constants::processes::{
    CHROME_WIDGET_WINDOW_CLASS, MOCK_QQ_MAIN_CLASS, MOCK_QQ_SECURITY_CLASS, QQ_WINDOW_TITLE,
    TX_GUI_FOUNDATION_CLASS,
};
use common_types::{DiagWindowEntry, WindowInfo};
use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumChildWindows, EnumWindows, GetAncestor, GetClassNameW, GetWindow, GetWindowLongW,
    GetWindowRect, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsIconic,
    IsWindow, IsWindowVisible, ShowWindowAsync, GA_ROOT, GWL_EXSTYLE, GW_OWNER, SW_SHOWNOACTIVATE,
};
use windows_core::BOOL;

use super::process_lookup::{is_exact_qq_process_name, snapshot_process_map};
use crate::constants::{DWMWA_CLOAKED_ATTR, HWND_CLASS_BUFFER_LEN, QQ_CHINESE_WINDOW_TITLE};

#[link(name = "dwmapi")]
extern "system" {
    fn DwmGetWindowAttribute(
        hwnd: HWND,
        dwattribute: u32,
        pvattribute: *mut c_void,
        cbattribute: u32,
    ) -> windows_core::HRESULT;
}

/// 枚举子窗口按类名检索时使用的共享回调状态容器
struct ChildWindowSearchState<'a> {
    target_class: &'a str,
    matches: Vec<isize>,
}

/// 判断原始 `isize` 窗口句柄当前是否仍然指向有效的 Win32 窗口对象
pub fn is_window_valid(window_hwnd: isize) -> bool {
    if window_hwnd == 0 {
        return false;
    }
    unsafe { IsWindow(Some(HWND(window_hwnd as *mut c_void))).as_bool() }
}

/// 判断指定 Win32 窗口当前是否处于最小化（图标化）状态
pub fn is_window_minimized(window_hwnd: isize) -> bool {
    if !is_window_valid(window_hwnd) {
        return false;
    }
    unsafe { IsIconic(HWND(window_hwnd as *mut c_void)).as_bool() }
}

/// 通过 `GetWindowTextW` 读取指定窗口的标题文本
pub fn get_window_title(window_hwnd: isize) -> String {
    if window_hwnd == 0 {
        return String::new();
    }
    let raw_hwnd = HWND(window_hwnd as *mut c_void);
    let length = unsafe { GetWindowTextLengthW(raw_hwnd) };
    if length <= 0 {
        return String::new();
    }
    let mut buffer = vec![0u16; (length as usize) + 2];
    let copied = unsafe { GetWindowTextW(raw_hwnd, &mut buffer) };
    if copied <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buffer[..copied as usize])
}

/// 通过 `GetClassNameW` 读取指定窗口的 Win32 类名
pub fn get_class_name(window_hwnd: isize) -> String {
    if window_hwnd == 0 {
        return String::new();
    }
    let raw_hwnd = HWND(window_hwnd as *mut c_void);
    let mut buffer = [0u16; HWND_CLASS_BUFFER_LEN];
    let copied = unsafe { GetClassNameW(raw_hwnd, &mut buffer) };
    if copied <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buffer[..copied as usize])
}

/// 读取 Win32 窗口在屏幕坐标系中的外接矩形 `[left, top, right, bottom]`
pub fn get_window_rect(window_hwnd: isize) -> Option<[i32; 4]> {
    if window_hwnd == 0 {
        return None;
    }
    let mut rect = RECT::default();
    if unsafe { GetWindowRect(HWND(window_hwnd as *mut c_void), &mut rect) }.is_ok() {
        Some([rect.left, rect.top, rect.right, rect.bottom])
    } else {
        None
    }
}

/// 查询指定 Win32 窗口所属的操作系统进程 PID
pub fn get_window_pid(window_hwnd: isize) -> u32 {
    if window_hwnd == 0 {
        return 0;
    }
    let mut pid: u32 = 0;
    unsafe {
        GetWindowThreadProcessId(HWND(window_hwnd as *mut c_void), Some(&mut pid));
    }
    pid
}

/// 查询窗口或子控件句柄对应的最顶层根祖先窗口句柄（`GA_ROOT`）
pub fn get_root_ancestor(window_hwnd: isize) -> isize {
    if window_hwnd == 0 {
        return 0;
    }
    let root = unsafe { GetAncestor(HWND(window_hwnd as *mut c_void), GA_ROOT) };
    root.0 as isize
}

/// 过滤调用方额外传入的类名白名单，剥离通用 Chromium 与经典 QQ 类名以避免匹配第三方无关宿主窗口
pub(crate) fn build_explicit_extra_classes(extra_classes: &[String]) -> HashSet<String> {
    extra_classes
        .iter()
        .map(|item| item.trim())
        // 通用 Electron/Chromium 或旧版窗口类会被外部无关程序共享，禁止通过扩展类名直接放行非 QQ 进程
        .filter(|item| {
            !item.is_empty()
                && !item.eq_ignore_ascii_case(CHROME_WIDGET_WINDOW_CLASS)
                && !item.eq_ignore_ascii_case(TX_GUI_FOUNDATION_CLASS)
        })
        .map(ToString::to_string)
        .collect()
}

/// 判定单个顶层窗口是否应当作为 QQ 客户端或测试模拟窗口的候选目标返回
pub(crate) fn should_include_candidate_window(
    pid: u32,
    pid_set: &HashSet<u32>,
    process_name: &str,
    class_name: &str,
    title: &str,
    explicit_extra: &HashSet<String>,
) -> bool {
    if pid_set.contains(&pid) {
        return class_name == CHROME_WIDGET_WINDOW_CLASS
            || class_name == TX_GUI_FOUNDATION_CLASS
            || class_name == MOCK_QQ_MAIN_CLASS
            || explicit_extra.contains(class_name)
            || !title.trim().is_empty();
    }

    if class_name.eq_ignore_ascii_case(CHROME_WIDGET_WINDOW_CLASS)
        || class_name.eq_ignore_ascii_case(TX_GUI_FOUNDATION_CLASS)
    {
        return is_exact_qq_process_name(process_name);
    }

    explicit_extra.contains(class_name)
}

/// 枚举当前桌面上所有匹配 `pids`、精确 `QQ.exe` / `QQEX.exe` 进程或显式扩展类名的可见与最小化顶层窗口
pub fn enumerate_candidate_windows(pids: &[u32], extra_classes: &[String]) -> Vec<WindowInfo> {
    let pid_set: HashSet<u32> = pids.iter().copied().collect();
    let explicit_extra = build_explicit_extra_classes(extra_classes);

    let process_map = snapshot_process_map();
    let mut hwnds: Vec<isize> = Vec::new();

    unsafe extern "system" fn collect_hwnd_callback(window: HWND, user_data: LPARAM) -> BOOL {
        let output = &mut *(user_data.0 as *mut Vec<isize>);
        output.push(window.0 as isize);
        BOOL(1)
    }

    unsafe {
        let _ = EnumWindows(
            Some(collect_hwnd_callback),
            LPARAM(&mut hwnds as *mut Vec<isize> as isize),
        );
    }

    let mut results = Vec::new();
    for window_hwnd in hwnds {
        let raw_hwnd = HWND(window_hwnd as *mut c_void);
        let is_visible = unsafe { IsWindowVisible(raw_hwnd).as_bool() };
        let is_iconic = unsafe { IsIconic(raw_hwnd).as_bool() };
        if !is_visible && !is_iconic {
            continue;
        }

        let rect = get_window_rect(window_hwnd).unwrap_or([0, 0, 0, 0]);
        let width = rect[2] - rect[0];
        let height = rect[3] - rect[1];
        if !is_iconic && (width <= 0 || height <= 0) {
            continue;
        }

        let pid = get_window_pid(window_hwnd);
        let class_name = get_class_name(window_hwnd);
        let title = get_window_title(window_hwnd);
        let process_name = process_map.get(&pid).cloned().unwrap_or_default();

        if !should_include_candidate_window(
            pid,
            &pid_set,
            &process_name,
            &class_name,
            &title,
            &explicit_extra,
        ) {
            continue;
        }

        let trimmed = title.trim();
        let is_standalone_chat = is_visible
            && !trimmed.is_empty()
            && trimmed != QQ_WINDOW_TITLE
            && trimmed != QQ_CHINESE_WINDOW_TITLE;

        results.push(WindowInfo {
            hwnd: window_hwnd,
            pid,
            process_name,
            class_name,
            title,
            is_visible,
            is_iconic,
            rect,
            is_standalone_chat,
        });
    }

    // 确定性排序：优先非最小化的大尺寸主窗口，再按句柄值稳定排序
    results.sort_by(|left, right| {
        let left_area =
            ((left.rect[2] - left.rect[0]) as i64) * ((left.rect[3] - left.rect[1]) as i64);
        let right_area =
            ((right.rect[2] - right.rect[0]) as i64) * ((right.rect[3] - right.rect[1]) as i64);
        left.is_iconic
            .cmp(&right.is_iconic)
            .then_with(|| right_area.cmp(&left_area))
            .then_with(|| left.hwnd.cmp(&right.hwnd))
    });

    results
}

/// 判断给定窗口及其宿主关系是否属于目标进程，或在 `allow_mock=true` 时允许命中模拟安全弹窗
pub(crate) fn is_security_window_candidate_ext(
    pid_set: &HashSet<u32>,
    pid: u32,
    owner_pid: u32,
    class_name: &str,
    owner_class: &str,
    allow_mock: bool,
) -> bool {
    pid_set.contains(&pid)
        || (owner_pid != 0 && pid_set.contains(&owner_pid))
        || (allow_mock
            && (class_name == MOCK_QQ_SECURITY_CLASS || owner_class == MOCK_QQ_MAIN_CLASS))
}

/// 判断给定窗口及其宿主关系是否属于目标进程或测试用安全弹窗类
#[cfg(test)]
pub(crate) fn is_security_window_candidate(
    pid_set: &HashSet<u32>,
    pid: u32,
    owner_pid: u32,
    class_name: &str,
    owner_class: &str,
) -> bool {
    is_security_window_candidate_ext(pid_set, pid, owner_pid, class_name, owner_class, true)
}

/// 检索标题命中任意风控关键词的可见顶层窗口或其附属弹窗，并按 `allow_mock` 控制是否隔离模拟窗口
pub fn find_windows_with_keywords_ext(
    pids: &[u32],
    keywords: &[&str],
    allow_mock: bool,
) -> Vec<(isize, String)> {
    if keywords.is_empty() {
        return Vec::new();
    }

    let pid_set: HashSet<u32> = pids.iter().copied().collect();
    let mut hwnds: Vec<isize> = Vec::new();

    unsafe extern "system" fn collect_hwnd_callback(window: HWND, user_data: LPARAM) -> BOOL {
        let output = &mut *(user_data.0 as *mut Vec<isize>);
        output.push(window.0 as isize);
        BOOL(1)
    }

    unsafe {
        let _ = EnumWindows(
            Some(collect_hwnd_callback),
            LPARAM(&mut hwnds as *mut Vec<isize> as isize),
        );
    }

    let mut matches = Vec::new();
    for window_hwnd in hwnds {
        let raw_hwnd = HWND(window_hwnd as *mut c_void);
        if !unsafe { IsWindowVisible(raw_hwnd).as_bool() } {
            continue;
        }
        let title = get_window_title(window_hwnd);
        if title.trim().is_empty() {
            continue;
        }
        let lower_title = title.to_ascii_lowercase();
        let keyword_hit = keywords.iter().any(|keyword| {
            !keyword.is_empty()
                && (title.contains(keyword) || lower_title.contains(&keyword.to_ascii_lowercase()))
        });
        if !keyword_hit {
            continue;
        }

        let pid = get_window_pid(window_hwnd);
        let class_name = get_class_name(window_hwnd);
        let owner = unsafe { GetWindow(raw_hwnd, GW_OWNER) }
            .ok()
            .unwrap_or_default();
        let owner_isize = owner.0 as isize;
        let owner_pid = if owner_isize != 0 {
            get_window_pid(owner_isize)
        } else {
            0
        };
        let owner_class = if owner_isize != 0 {
            get_class_name(owner_isize)
        } else {
            String::new()
        };

        if is_security_window_candidate_ext(
            &pid_set,
            pid,
            owner_pid,
            &class_name,
            &owner_class,
            allow_mock,
        ) {
            matches.push((window_hwnd, title));
        }
    }

    matches
}

/// 检索标题命中任意风控关键词的可见顶层窗口或其附属弹窗，仅限 `pids` 归属窗口及测试模拟窗口
pub fn find_windows_with_keywords(pids: &[u32], keywords: &[&str]) -> Vec<(isize, String)> {
    find_windows_with_keywords_ext(pids, keywords, true)
}

/// 枚举 `parent_hwnd` 下所有类名等于 `target_class` 的子窗口句柄
pub fn find_child_windows_by_class(parent_hwnd: isize, target_class: &str) -> Vec<isize> {
    if parent_hwnd == 0 || target_class.is_empty() {
        return Vec::new();
    }

    unsafe extern "system" fn child_callback(window: HWND, user_data: LPARAM) -> BOOL {
        let state = &mut *(user_data.0 as *mut ChildWindowSearchState<'_>);
        let class_name = get_class_name(window.0 as isize);
        if class_name == state.target_class {
            state.matches.push(window.0 as isize);
        }
        BOOL(1)
    }

    let mut state = ChildWindowSearchState {
        target_class,
        matches: Vec::new(),
    };
    unsafe {
        let _ = EnumChildWindows(
            Some(HWND(parent_hwnd as *mut c_void)),
            Some(child_callback),
            LPARAM(&mut state as *mut ChildWindowSearchState<'_> as isize),
        );
    }
    state.matches
}

/// 返回 `parent_hwnd` 下首个类名等于 `target_class` 的子窗口句柄
pub fn find_child_window_by_class(parent_hwnd: isize, target_class: &str) -> Option<isize> {
    find_child_windows_by_class(parent_hwnd, target_class)
        .into_iter()
        .next()
}

/// 原地刷新 `window_info` 中的标题、可见性、最小化状态及外接矩形；若句柄已失效则返回 `false`
pub fn refresh_window_info(window_info: &mut WindowInfo) -> bool {
    if !is_window_valid(window_info.hwnd) {
        return false;
    }
    let raw_hwnd = HWND(window_info.hwnd as *mut c_void);
    window_info.title = get_window_title(window_info.hwnd);
    window_info.is_visible = unsafe { IsWindowVisible(raw_hwnd).as_bool() };
    window_info.is_iconic = unsafe { IsIconic(raw_hwnd).as_bool() };
    if let Some(rect) = get_window_rect(window_info.hwnd) {
        window_info.rect = rect;
    }
    true
}

/// 为指定窗口构造包含扩展样式与 DWM 隐匿属性在内的诊断摘要条目
fn build_diag_window_entry(
    window_hwnd: isize,
    process_map: &HashMap<u32, String>,
) -> DiagWindowEntry {
    let raw_hwnd = HWND(window_hwnd as *mut c_void);
    let pid = get_window_pid(window_hwnd);
    let process_name = process_map.get(&pid).cloned().unwrap_or_default();
    let class_name = get_class_name(window_hwnd);
    let title = get_window_title(window_hwnd);
    let is_visible = unsafe { IsWindowVisible(raw_hwnd).as_bool() };
    let is_iconic = unsafe { IsIconic(raw_hwnd).as_bool() };
    let ex_style = unsafe { GetWindowLongW(raw_hwnd, GWL_EXSTYLE) as u32 };
    let mut cloaked: u32 = 0;
    let dwm_result = unsafe {
        DwmGetWindowAttribute(
            raw_hwnd,
            DWMWA_CLOAKED_ATTR,
            &mut cloaked as *mut u32 as *mut c_void,
            size_of::<u32>() as u32,
        )
    };
    let dwm_cloaked = if dwm_result.is_ok() { cloaked } else { 0 };
    let rect = get_window_rect(window_hwnd).unwrap_or([0, 0, 0, 0]);

    DiagWindowEntry {
        hwnd: window_hwnd,
        pid,
        process_name,
        class_name,
        title,
        is_visible,
        is_iconic,
        dwm_cloaked,
        ex_style,
        rect,
    }
}

/// 枚举指定 `pids` 拥有的全部顶层窗口（含隐藏、隐匿或零尺寸窗口）以供无障碍诊断
pub fn enumerate_all_windows_for_pids(pids: &[u32]) -> Vec<DiagWindowEntry> {
    if pids.is_empty() {
        return Vec::new();
    }
    let pid_set: HashSet<u32> = pids.iter().copied().collect();
    let process_map = snapshot_process_map();
    let mut hwnds: Vec<isize> = Vec::new();

    unsafe extern "system" fn collect_callback(window: HWND, user_data: LPARAM) -> BOOL {
        let output = &mut *(user_data.0 as *mut Vec<isize>);
        output.push(window.0 as isize);
        BOOL(1)
    }

    unsafe {
        let _ = EnumWindows(
            Some(collect_callback),
            LPARAM(&mut hwnds as *mut Vec<isize> as isize),
        );
    }

    hwnds
        .into_iter()
        .filter(|&window_hwnd| pid_set.contains(&get_window_pid(window_hwnd)))
        .map(|window_hwnd| build_diag_window_entry(window_hwnd, &process_map))
        .collect()
}

/// 枚举 `parent_hwnd` 下的全部子窗口及其扩展 Win32/DWM 元数据
pub fn enumerate_all_child_windows(parent_hwnd: isize) -> Vec<DiagWindowEntry> {
    if !is_window_valid(parent_hwnd) {
        return Vec::new();
    }
    let process_map = snapshot_process_map();
    let mut hwnds: Vec<isize> = Vec::new();

    unsafe extern "system" fn child_callback(window: HWND, user_data: LPARAM) -> BOOL {
        let output = &mut *(user_data.0 as *mut Vec<isize>);
        output.push(window.0 as isize);
        BOOL(1)
    }

    unsafe {
        let _ = EnumChildWindows(
            Some(HWND(parent_hwnd as *mut c_void)),
            Some(child_callback),
            LPARAM(&mut hwnds as *mut Vec<isize> as isize),
        );
    }

    hwnds
        .into_iter()
        .map(|window_hwnd| build_diag_window_entry(window_hwnd, &process_map))
        .collect()
}

/// 若目标窗口处于最小化状态，则以 `SW_SHOWNOACTIVATE` 在不窃取前台焦点的前提下异步恢复显示
pub fn restore_no_activate(window_hwnd: isize) {
    if !is_window_valid(window_hwnd) {
        return;
    }
    let raw_hwnd = HWND(window_hwnd as *mut c_void);
    unsafe {
        if IsIconic(raw_hwnd).as_bool() {
            let _ = ShowWindowAsync(raw_hwnd, SW_SHOWNOACTIVATE);
        }
    }
}
