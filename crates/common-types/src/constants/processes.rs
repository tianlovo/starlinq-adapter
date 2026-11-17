//! QQ 客户端进程名与 Win32 窗口类名常量定义

/// 允许绑定的标准 QQNT 进程可执行文件名列表
pub const QQ_PROCESS_NAMES: &[&str] = &["QQ.exe", "QQex.exe"];
/// 小写规范化后的 QQNT 进程可执行文件名列表
pub const QQ_PROCESS_NAMES_LOWER: &[&str] = &["qq.exe", "qqex.exe"];

/// QQNT 主窗口常用的 Chromium 宿主窗口类名
pub const CHROME_WIDGET_WINDOW_CLASS: &str = "Chrome_WidgetWin_1";
/// 经典腾讯窗口框架类名
pub const TX_GUI_FOUNDATION_CLASS: &str = "TXGuiFoundation";
/// Chromium 渲染器子窗口类名
pub const CHROME_RENDER_WIDGET_CLASS: &str = "Chrome_RenderWidgetHostHWND";
/// 单元测试与自测使用的模拟 QQ 主窗口类名
pub const MOCK_QQ_MAIN_CLASS: &str = "MockQQNTWndClass";
/// 模拟 QQ 安全弹窗窗口类名
pub const MOCK_QQ_SECURITY_CLASS: &str = "MockQQSecurityWndClass";
/// QQ 主窗口默认标题文本
pub const QQ_WINDOW_TITLE: &str = "QQ";
