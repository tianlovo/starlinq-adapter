//! 命令行工具常量模块：复用共享路由路径与服务契约并集中定义 CLI 运维超时阈值

pub use adapter_server::constants::{
    ACTION_GET_VERSION_INFO, ROUTE_API_STORAGE, ROUTE_ONEBOT_CAN_SEND_IMAGE,
    ROUTE_ONEBOT_CAN_SEND_VIDEO, ROUTE_ONEBOT_GET_VERSION_INFO,
};
pub use common_types::constants::{
    ACTION_GET_FRIEND_LIST, ACTION_GET_GROUP_LIST, ACTION_GET_MSG, ACTION_GET_SCHEDULER_STATUS,
    ACTION_GET_WORD_SLICES, ACTION_MARK_GROUP_MSG_AS_READ, DEFAULT_CONFIG_PATH,
    DEFAULT_STRICT_BELOW_MEMBERS, HEADER_AUTHORIZATION, HEADER_X_CLIENT_ROLE,
    HEADER_X_CLIENT_ROLE_UNIVERSAL, HEADER_X_SELF_ID, MOCK_QQ_MAIN_CLASS, ROUTE_API_DELETE_MSG,
    ROUTE_API_DIAG, ROUTE_API_ENSURE_QQ, ROUTE_API_FRIENDS, ROUTE_API_GROUPS, ROUTE_API_GROUP_INFO,
    ROUTE_API_GROUP_MEMBERS, ROUTE_API_HEALTH, ROUTE_API_MESSAGES, ROUTE_API_MESSAGES_HISTORY,
    ROUTE_API_OCR, ROUTE_API_OPEN_GROUP, ROUTE_API_OPEN_PRIVATE, ROUTE_API_RESTART,
    ROUTE_API_SCHEDULER, ROUTE_API_SCHEDULER_POLICY, ROUTE_API_SEND, ROUTE_API_SESSIONS,
    ROUTE_API_SESSIONS_READ, ROUTE_API_SHUTDOWN, ROUTE_API_STATUS, ROUTE_API_STORAGE_STATS,
    ROUTE_API_TREE, ROUTE_API_WINDOWS, ROUTE_ONEBOT_GET_FRIEND_LIST, ROUTE_ONEBOT_GET_STATUS,
    ROUTE_ONEBOT_SEND_GROUP_MSG, ROUTE_ONEBOT_SEND_MSG, ROUTE_ONEBOT_SEND_PRIVATE_MSG,
    ROUTE_ONEBOT_WS, RUNTIME_CONNECTION_FILE, RUNTIME_LATEST_REPORT_FILE,
};

/// 后台守护进程运行时存放目录名称
pub const RUNTIME_DIR_NAME: &str = ".runtime";
/// 后台守护进程暂存可执行文件子目录名
pub const RUNTIME_BIN_SUBDIR: &str = "bin";
/// 后台守护进程暂存副本文件名
pub const DAEMON_EXE_NAME: &str = "qq-uia-adapter-daemon.exe";
/// 发行版构建产物相对路径
pub const RELEASE_CLI_BIN_REL: &str = r".cache\target\release\qq-uia-adapter.exe";
/// 守护进程标准输出日志文件名
pub const DAEMON_OUT_LOG_NAME: &str = "daemon.out.log";
/// 守护进程标准错误日志文件名
pub const DAEMON_ERR_LOG_NAME: &str = "daemon.err.log";
/// Windows MinGW 运行库目录，用于确保后台守护进程脱离终端仍可加载运行库
pub const MINGW_BIN_DIR: &str = r"C:\DevTools\mingw64\bin";

/// 自动探测本地运行中守护进程的短超时（毫秒）
pub const FAST_PROBE_TIMEOUT_MS: u64 = 350;
/// 显式指定 `--remote` 时探活的超时（毫秒）
pub const EXPLICIT_PROBE_TIMEOUT_MS: u64 = 1000;
/// 常规远程业务调用 HTTP 超时（秒）
pub const NORMAL_HTTP_TIMEOUT_SECS: u64 = 60;
/// `restart` 等待新进程替换接管的最大等待时间（秒）
pub const RESTART_POLL_TIMEOUT_SECS: u64 = 25;
/// `start-daemon` 健康检查轮询最大次数
pub const HEALTH_POLL_ATTEMPTS: usize = 50;
/// `start-daemon` 健康检查轮询间隔（毫秒）
pub const HEALTH_POLL_INTERVAL_MS: u64 = 250;
/// 复制守护进程副本遇文件锁时的最大重试次数
pub const COPY_RETRY_ATTEMPTS: usize = 15;
/// 复制守护进程副本重试间隔（毫秒）
pub const COPY_RETRY_DELAY_MS: u64 = 200;

/// 自测场景中大于人数上限且必须被跳过的模拟群名称
pub const SELF_TEST_BIG_GROUP: &str = "Demo测试群 (26)";
/// 自测场景中符合人数安全阈值并应当被附着的模拟群名称
pub const SELF_TEST_SMALL_GROUP: &str = "Demo测试群 (5)";
/// 自测场景中的白名单私聊好友名称
pub const SELF_TEST_PRIVATE_FRIEND: &str = "测试好友";
/// 自测场景中触发安全熔断的模拟安全验证弹窗标题
pub const SELF_TEST_SECURITY_TITLE: &str = "QQ安全验证";
/// 自测图片粘贴发送使用的最小合法 1x1 PNG Base64 载荷
pub const SELF_TEST_PNG_BASE64: &str = "base64://iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+ip1sAAAAASUVORK5CYII=";
