//! UI Automation 单线程 MTA COM 会话外观（Facade）与 RAII 守护模块，负责安全初始化并释放 `IUIAutomation` 实例、执行无障碍唤醒及采集窗口诊断报告

use std::time::Duration;

use common_types::{AdapterError, Result, UiNodeSnapshot, WakeReport, WindowDiagnosticReport};
use uiautomation::patterns::UILegacyIAccessiblePattern;
use uiautomation::types::{ControlType, Handle, Point, TreeScope, UIProperty};
use uiautomation::variants::Variant;
use uiautomation::{UIAutomation, UIElement, UITreeWalker};
use windows::Win32::Foundation::{RPC_E_CHANGED_MODE, S_FALSE, S_OK};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};

use crate::constants::{
    MAX_SEMANTIC_DIAG_SAMPLES, MAX_UIA_WAKE_DEPTH, MAX_UIA_WAKE_VISITS, POST_FIRST_WAKE_DIAG_MS,
    POST_SECOND_WAKE_DIAG_MS,
};
use crate::tree::{self, CollectedNode};
use crate::win32;

/// 诊断报告采集全量控件树时使用的最大遍历深度
const DIAGNOSTIC_TREE_MAX_DEPTH: usize = 28;

/// 管理当前线程 `CoInitializeEx` 生命周期并在析构时配对调用 `CoUninitialize` 的 RAII 守护对象
struct ComInitGuard {
    owns_com_init: bool,
}

impl Drop for ComInitGuard {
    fn drop(&mut self) {
        if self.owns_com_init {
            unsafe {
                CoUninitialize();
            }
        }
    }
}

/// 绑定于调用方 COM 初始化线程的单线程 UI Automation 会话外观（Facade）
///
/// `UIAutomation` 与 `UIElement` 均为 `!Send`，因此本会话必须在独立 MTA 工作线程内持有并调用
/// 字段声明顺序不可调换：`automation` 必须先于 `_com_guard` 声明，以确保 `IUIAutomation` 在 `CoUninitialize()` 前释放
pub struct UiaSession {
    automation: UIAutomation,
    _com_guard: ComInitGuard,
}

impl UiaSession {
    /// 以 `COINIT_MULTITHREADED` 安全初始化当前线程 COM 套间并创建 `UIAutomation` 实例
    pub fn new() -> Result<Self> {
        let hresult = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        let owns_com_init = if hresult == S_OK || hresult == S_FALSE {
            true
        } else if hresult == RPC_E_CHANGED_MODE {
            tracing::debug!(
                "当前线程已按其他套间模式初始化 COM，继续执行且不在析构时调用 CoUninitialize"
            );
            false
        } else if hresult.is_err() {
            return Err(AdapterError::UiaFailure(format!(
                "CoInitializeEx(COINIT_MULTITHREADED) 初始化失败: HRESULT 0x{:08x}",
                hresult.0
            )));
        } else {
            true
        };

        let com_guard = ComInitGuard { owns_com_init };
        let automation = UIAutomation::new_direct().map_err(|error| {
            AdapterError::UiaFailure(format!("UIAutomation::new_direct 创建失败: {error}"))
        })?;

        Ok(Self {
            automation,
            _com_guard: com_guard,
        })
    }

    /// 返回底层 `UIAutomation` 引用供树采集与动作模块复用
    pub fn automation(&self) -> &UIAutomation {
        &self.automation
    }

    /// 将原生 Win32 窗口句柄包装为 UI Automation 根元素
    pub fn element_from_hwnd(&self, window_hwnd: isize) -> Result<UIElement> {
        if !win32::is_window_valid(window_hwnd) {
            return Err(AdapterError::WindowNotFound(format!(
                "无效窗口句柄 HWND 0x{window_hwnd:x}"
            )));
        }
        self.automation
            .element_from_handle(Handle::from(window_hwnd))
            .map_err(|error| {
                AdapterError::UiaFailure(format!("element_from_handle(0x{window_hwnd:x}): {error}"))
            })
    }

    /// 采集 `window_hwnd` 深度不超过 `max_depth` 的分层 `UiNodeSnapshot` 控件树
    pub fn dump_tree(&self, window_hwnd: isize, max_depth: usize) -> Result<UiNodeSnapshot> {
        tree::dump_tree(&self.automation, window_hwnd, max_depth)
    }

    /// 采集 `window_hwnd` 深度不超过 `max_depth` 的前序平铺 `CollectedNode` 列表
    pub fn collect_flat(&self, window_hwnd: isize, max_depth: usize) -> Result<Vec<CollectedNode>> {
        tree::collect_flat(&self.automation, window_hwnd, max_depth)
    }

    /// 主动唤醒指定窗口的 Chromium/Electron 无障碍提供程序，可选脉冲激活 `SPI_SETSCREENREADER` 并探测 `IAccessible2` 接口，返回 `WakeReport` 遥测摘要
    pub fn wake_uia_subtree_ext(
        &self,
        window_hwnd: isize,
        pulse_screen_reader: bool,
    ) -> WakeReport {
        let report = win32::wake_chromium_accessibility_ext(window_hwnd, pulse_screen_reader);
        let Ok(root_element) = self.element_from_hwnd(window_hwnd) else {
            return report;
        };

        if pulse_screen_reader {
            for (probe_x, probe_y) in win32::diagnostic_probe_points(window_hwnd) {
                let _ = self
                    .automation
                    .element_from_point(Point::new(probe_x, probe_y));
            }
        }

        if let Ok(document_condition) = self.automation.create_property_condition(
            UIProperty::ControlType,
            Variant::from(ControlType::Document as i32),
            None,
        ) {
            let _ = root_element.find_first(TreeScope::Subtree, &document_condition);
        }

        if let Ok(walker) = self.automation.get_raw_view_walker() {
            let mut visited_count = 0usize;
            probe_raw_view_subtree(&walker, &root_element, 0, &mut visited_count);
        }

        report
    }

    /// 通过 Win32 MSAA/UIA 探针、`Document` 子树检索与受限原始视图遍历唤醒指定窗口的无障碍子树
    pub fn wake_uia_subtree(&self, window_hwnd: isize) {
        let _ = self.wake_uia_subtree_ext(window_hwnd, false);
    }

    /// 为指定窗口采集覆盖 Win32、MSAA、`IA2` 与 UIA（`ControlView`、`RawView`、`FindAll`）的全维度诊断报告
    pub fn diagnose_hwnd(
        &self,
        window_hwnd: isize,
        qq_pids: &[u32],
        unocclude_and_wake: bool,
    ) -> Result<WindowDiagnosticReport> {
        let _ = self.element_from_hwnd(window_hwnd)?;
        let all_qq_windows = win32::enumerate_all_windows_for_pids(qq_pids);
        let _topmost_guard = if unocclude_and_wake {
            win32::TopmostUnoccludeGuard::acquire(window_hwnd)
        } else {
            None
        };

        let child_windows_before = win32::enumerate_all_child_windows(window_hwnd);
        let wake_report = if unocclude_and_wake {
            let _first_report = self.wake_uia_subtree_ext(window_hwnd, true);
            std::thread::sleep(Duration::from_millis(POST_FIRST_WAKE_DIAG_MS));
            let second_report = self.wake_uia_subtree_ext(window_hwnd, true);
            std::thread::sleep(Duration::from_millis(POST_SECOND_WAKE_DIAG_MS));
            second_report
        } else {
            WakeReport::default()
        };
        let child_windows_after = win32::enumerate_all_child_windows(window_hwnd);

        let control_tree =
            tree::dump_tree_control(&self.automation, window_hwnd, DIAGNOSTIC_TREE_MAX_DEPTH)?;
        let uia_control_count = tree::count_snapshot_nodes(&control_tree);

        let raw_tree =
            tree::dump_tree_raw(&self.automation, window_hwnd, DIAGNOSTIC_TREE_MAX_DEPTH).ok();
        let uia_raw_count = raw_tree
            .as_ref()
            .map(tree::count_snapshot_nodes)
            .unwrap_or(0);

        let sample_tree = if !tree::snapshot_needs_chromium_wake(&control_tree) {
            &control_tree
        } else if let Some(ref raw_snapshot) = raw_tree {
            if !tree::snapshot_needs_chromium_wake(raw_snapshot)
                || uia_raw_count > uia_control_count
            {
                raw_snapshot
            } else {
                &control_tree
            }
        } else {
            &control_tree
        };

        let mut uia_semantic_count = 0usize;
        let mut sample_semantic_nodes = Vec::new();
        collect_semantic_samples(
            sample_tree,
            0,
            &mut uia_semantic_count,
            &mut sample_semantic_nodes,
        );

        let uia_find_all_count = self
            .element_from_hwnd(window_hwnd)
            .ok()
            .and_then(|root| {
                self.automation
                    .create_true_condition()
                    .and_then(|condition| root.find_all(TreeScope::Descendants, &condition))
                    .ok()
            })
            .map(|elements| elements.len())
            .unwrap_or(0);

        Ok(WindowDiagnosticReport {
            target_hwnd: window_hwnd,
            all_qq_windows,
            child_windows_before,
            child_windows_after,
            wake_report,
            uia_control_count,
            uia_semantic_count,
            uia_raw_count,
            uia_find_all_count,
            sample_semantic_nodes,
        })
    }
}

/// 递归统计树快照中的语义内容节点总数并收集前若干条诊断样本
fn collect_semantic_samples(
    node: &UiNodeSnapshot,
    depth: usize,
    semantic_count: &mut usize,
    samples: &mut Vec<String>,
) {
    if tree::is_semantic_content_control(&node.control_type) {
        *semantic_count += 1;
        if samples.len() < MAX_SEMANTIC_DIAG_SAMPLES {
            samples.push(format!(
                "d={} {} name={} rect={:?}",
                depth, node.control_type, node.name, node.rect
            ));
        }
    }
    for child in &node.children {
        collect_semantic_samples(child, depth + 1, semantic_count, samples);
    }
}

/// 在 `RawViewWalker` 上执行有界深度遍历并读取 `Pane` 节点的 `LegacyIAccessible` 属性以刺激宿主构建树
fn probe_raw_view_subtree(
    walker: &UITreeWalker,
    element: &UIElement,
    depth: usize,
    visited_count: &mut usize,
) {
    if *visited_count >= MAX_UIA_WAKE_VISITS {
        return;
    }
    *visited_count += 1;

    if element.get_control_type().ok() == Some(ControlType::Pane) {
        if let Ok(legacy) = element.get_pattern::<UILegacyIAccessiblePattern>() {
            let _ = legacy.get_name();
            let _ = legacy.get_role();
        }
    }

    if depth >= MAX_UIA_WAKE_DEPTH || *visited_count >= MAX_UIA_WAKE_VISITS {
        return;
    }

    if let Ok(mut child) = walker.get_first_child(element) {
        loop {
            if *visited_count >= MAX_UIA_WAKE_VISITS {
                break;
            }
            probe_raw_view_subtree(walker, &child, depth + 1, visited_count);
            match walker.get_next_sibling(&child) {
                Ok(next) => child = next,
                Err(_) => break,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uia_session_drops_cleanly_on_dedicated_mta_thread() {
        let handle = std::thread::spawn(|| {
            for _ in 0..3 {
                let session = UiaSession::new().expect("在独立 MTA 线程创建 UiaSession");
                drop(session);
            }
        });
        handle.join().expect("UiaSession 工作线程不应 panic");
    }

    #[test]
    fn diagnose_hwnd_captures_win32_msaa_and_uia_counts_on_mock_window() {
        let mock_server = mock_qq_win::MockQqServer::spawn().expect("启动 MockQqServer");
        let window_hwnd = mock_server.hwnd();
        let session = UiaSession::new().expect("创建 UiaSession");

        let report = session
            .diagnose_hwnd(window_hwnd, &[std::process::id()], true)
            .expect("执行 diagnose_hwnd 诊断采集");

        assert_eq!(report.target_hwnd, window_hwnd);
        assert!(
            report
                .all_qq_windows
                .iter()
                .any(|window| window.hwnd == window_hwnd),
            "all_qq_windows 必须包含模拟窗口句柄 {window_hwnd}"
        );
        assert!(report.child_windows_before.len() >= 5);
        assert!(report.child_windows_after.len() >= 5);
        assert!(report.wake_report.visited_msaa_nodes > 0);
        assert!(report.uia_control_count >= 5);
        assert!(report.uia_raw_count >= 5);
        assert!(report.uia_semantic_count > 0);
        assert!(report.uia_find_all_count >= 5);
        assert!(!report.sample_semantic_nodes.is_empty());

        mock_server.shutdown();
    }
}
