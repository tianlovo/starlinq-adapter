//! 运行时路径与客户端启动参数常量定义

/// 默认 TOML 配置文件相对路径
pub const DEFAULT_CONFIG_PATH: &str = "config/default.toml";
/// 默认运行时目录名称
pub const DEFAULT_RUNTIME_DIR: &str = ".runtime";
/// 默认嵌入式 SQLite 数据库路径
pub const DEFAULT_DB_PATH: &str = ".runtime/adapter.db";
/// 后台服务连接信息文件相对路径
pub const RUNTIME_CONNECTION_FILE: &str = ".runtime/connection.json";
/// 群验证结果报告文件相对路径
pub const RUNTIME_LATEST_REPORT_FILE: &str = ".runtime/latest-report.json";
/// 界面元素树导出文件相对路径
pub const RUNTIME_UIA_TREE_FILE: &str = ".runtime/uia-tree.json";
/// 默认本地桌面版 QQNT 可执行文件路径
pub const DEFAULT_QQ_EXECUTABLE_PATH: &str = r"C:\Program Files\Tencent\QQ\QQNT\QQ.exe";

/// 默认注入给 QQNT Chromium 容器的无障碍与防休眠启动参数
pub const DEFAULT_QQ_LAUNCH_ARGS: &[&str] = &[
    "--force-renderer-accessibility",
    "--disable-renderer-backgrounding",
    "--disable-backgrounding-occluded-windows",
    "--disable-features=AutoDisableAccessibility,CalculateNativeWinOcclusion",
];
