//! 服务层常量定义模块：复用共享契约并补充服务端独有的协议与运维常量

pub use common_types::constants::{
    ACTION_CAN_SEND_VIDEO, ACTION_DOT_OCR_IMAGE, ACTION_FORWARD_FRIEND_SINGLE_MSG,
    ACTION_FORWARD_GROUP_SINGLE_MSG, ACTION_GET_AUDIT_EVENTS, ACTION_GET_FILE,
    ACTION_GET_FRIEND_LIST, ACTION_GET_GROUP_INFO, ACTION_GET_GROUP_LIST,
    ACTION_GET_GROUP_MEMBER_INFO, ACTION_GET_GROUP_MEMBER_LIST, ACTION_GET_LOGIN_INFO,
    ACTION_GET_MSG, ACTION_GET_RECENT_CONTACT, ACTION_GET_SCHEDULER_STATUS, ACTION_GET_STATUS,
    ACTION_GET_WORD_SLICES, ACTION_MARK_GROUP_MSG_AS_READ, ACTION_MARK_MSG_AS_READ,
    ACTION_MARK_PRIVATE_MSG_AS_READ, ACTION_OCR_IMAGE, ACTION_RESET_SECURITY_BREAKER,
    ACTION_SEND_GROUP_MSG, ACTION_SEND_MSG, ACTION_SEND_PRIVATE_MSG, ACTION_UPLOAD_GROUP_FILE,
    ACTION_UPLOAD_PRIVATE_FILE, BEARER_PREFIX, CQ_SEGMENT_IMAGE, CQ_SEGMENT_REPLY,
    DEFAULT_BOT_SELF_NAME, DEFAULT_ONEBOT_ID_BOUND, DEFAULT_RUNTIME_DIR, HEADER_AUTHORIZATION,
    HEADER_X_CLIENT_ROLE, HEADER_X_CLIENT_ROLE_UNIVERSAL, HEADER_X_SELF_ID,
    ONEBOT_MESSAGE_TYPE_PRIVATE, ONEBOT_SUB_TYPE_FRIEND, ROUTE_API_AUDIT_EVENTS,
    ROUTE_API_CACHE_CLEAN, ROUTE_API_DELETE_MSG, ROUTE_API_DIAG, ROUTE_API_ENSURE_QQ,
    ROUTE_API_FORWARD_MSG, ROUTE_API_FRIENDS, ROUTE_API_GROUPS, ROUTE_API_GROUP_INFO,
    ROUTE_API_GROUP_MEMBERS, ROUTE_API_HEALTH, ROUTE_API_MESSAGES, ROUTE_API_MESSAGES_HISTORY,
    ROUTE_API_OCR, ROUTE_API_OPEN_GROUP, ROUTE_API_OPEN_PRIVATE, ROUTE_API_RESTART,
    ROUTE_API_SCHEDULER, ROUTE_API_SCHEDULER_POLICY, ROUTE_API_SECURITY_RESET, ROUTE_API_SEND,
    ROUTE_API_SESSIONS, ROUTE_API_SESSIONS_READ, ROUTE_API_SHUTDOWN, ROUTE_API_STATUS,
    ROUTE_API_STORAGE_STATS, ROUTE_API_TREE, ROUTE_API_WINDOWS, ROUTE_ONEBOT_GET_FRIEND_LIST,
    ROUTE_ONEBOT_GET_GROUP_INFO, ROUTE_ONEBOT_GET_GROUP_LIST, ROUTE_ONEBOT_GET_GROUP_MEMBER_INFO,
    ROUTE_ONEBOT_GET_GROUP_MEMBER_LIST, ROUTE_ONEBOT_GET_LOGIN_INFO, ROUTE_ONEBOT_GET_MSG,
    ROUTE_ONEBOT_GET_STATUS, ROUTE_ONEBOT_SEND_GROUP_MSG, ROUTE_ONEBOT_SEND_MSG,
    ROUTE_ONEBOT_SEND_PRIVATE_MSG, ROUTE_ONEBOT_WS, ROUTE_WS_EVENTS, RUNTIME_CONNECTION_FILE,
};

/// 适配器应用名称标识
pub const ADAPTER_APP_NAME: &str = "qq-uia-adapter";
/// 当前实现的 OneBot 协议版本号
pub const ONEBOT_PROTOCOL_VERSION: &str = "v11";

/// OneBot 扩展路由：版本信息查询路径
pub const ROUTE_ONEBOT_GET_VERSION_INFO: &str = "/onebot/v11/get_version_info";
/// OneBot 扩展路由：图片发送能力检测路径
pub const ROUTE_ONEBOT_CAN_SEND_IMAGE: &str = "/onebot/v11/can_send_image";
/// OneBot 扩展路由：语音发送能力检测路径
pub const ROUTE_ONEBOT_CAN_SEND_RECORD: &str = "/onebot/v11/can_send_record";
/// OneBot 扩展路由：短视频发送能力检测路径
pub const ROUTE_ONEBOT_CAN_SEND_VIDEO: &str = "/onebot/v11/can_send_video";

/// OneBot 扩展动作：获取版本信息
pub const ACTION_GET_VERSION_INFO: &str = "get_version_info";
/// OneBot 扩展动作：查询是否支持发送图片
pub const ACTION_CAN_SEND_IMAGE: &str = "can_send_image";
/// OneBot 扩展动作：查询是否支持发送语音
pub const ACTION_CAN_SEND_RECORD: &str = "can_send_record";

/// 原生存储综合概览路由路径
pub const ROUTE_API_STORAGE: &str = "/api/storage";
/// 无边框窗口截图诊断接口路径
pub const ROUTE_API_SCREENSHOT: &str = "/api/screenshot";
/// 只读 UI 弹窗探针诊断接口路径
pub const ROUTE_API_DIAG_PROBE_UI: &str = "/api/diag/probe-ui";

/// 反向 WebSocket 客户端上报的 User-Agent 请求头值
pub const REVERSE_WS_USER_AGENT: &str = concat!("qq-uia-adapter/", env!("CARGO_PKG_VERSION"));
/// 反向 WebSocket 指数退避重连的最大等待时间（毫秒）
pub const MAX_REVERSE_RECONNECT_MS: u64 = 30_000;
/// 默认界面元素树遍历最大深度
pub const DEFAULT_TREE_DEPTH: usize = 5;
/// 界面元素树遍历深度硬上限
pub const TREE_MAX_DEPTH_CAP: usize = 32;
/// 聊天历史单次拉取默认条数
pub const MESSAGES_DEFAULT_LIMIT: usize = 50;
/// 聊天历史单次拉取最大条数
pub const MESSAGES_MAX_LIMIT: usize = 500;
/// 触发异步优雅关闭前的响应冲刷等待时间（毫秒）
pub const SHUTDOWN_DELAY_MS: u64 = 50;
/// 触发异步进程重启前的响应冲刷等待时间（毫秒）
pub const RESTART_SPAWN_DELAY_MS: u64 = 100;

/// 守护进程热重启源发行版可执行文件相对路径
pub const RELEASE_BINARY_PATH: &str = r".cache\target\release\qq-uia-adapter.exe";
/// 守护进程热重启运行时可执行文件缓存目录
pub const RUNTIME_BIN_DIR: &str = r".runtime\bin";
/// 热重启暂存二进制文件名
pub const RESTARTER_EXE_NAME: &str = "qq-uia-adapter-restarter.exe";
/// 热重启过程日志输出文件路径
pub const RESTART_LOG_PATH: &str = r".runtime\restart.log";
/// Windows MinGW 运行库目录，用于保障脱离终端后子进程依赖加载
pub const MINGW_BIN_DIR: &str = r"C:\DevTools\mingw64\bin";

/// OneBot 协议通用请求与状态字符串常量
pub const HEADER_USER_AGENT: &str = "User-Agent";
/// OneBot 应答成功状态值
pub const ONEBOT_STATUS_OK: &str = "ok";
/// OneBot 应答失败状态值
pub const ONEBOT_STATUS_FAILED: &str = "failed";
/// OneBot 消息事件主类型
pub const ONEBOT_POST_TYPE_MESSAGE: &str = "message";
/// OneBot 元事件主类型
pub const ONEBOT_POST_TYPE_META: &str = "meta_event";
/// OneBot 生命周期元事件类型
pub const ONEBOT_META_LIFECYCLE: &str = "lifecycle";
/// OneBot 心跳元事件类型
pub const ONEBOT_META_HEARTBEAT: &str = "heartbeat";
/// OneBot 生命周期连接子类型
pub const ONEBOT_SUB_TYPE_CONNECT: &str = "connect";
/// OneBot 群消息常规子类型
pub const ONEBOT_SUB_TYPE_NORMAL: &str = "normal";
/// OneBot 群聊消息类型标识
pub const ONEBOT_MESSAGE_TYPE_GROUP: &str = "group";
/// CQ/OneBot 文本消息段类型
pub const CQ_SEGMENT_TEXT: &str = "text";
/// CQ/OneBot @ 提及消息段类型
pub const CQ_SEGMENT_AT: &str = "at";
/// CQ/OneBot @ 全体成员参数值
pub const CQ_AT_ALL_VALUE: &str = "all";
/// WebSocket 心跳上报允许的最小间隔（毫秒）
pub const MIN_HEARTBEAT_INTERVAL_MS: u64 = 500;
/// 反向 WebSocket 断线重连允许的最小间隔（毫秒）
pub const MIN_REVERSE_RECONNECT_MS: u64 = 100;

/// OneBot Token 前缀（除 Bearer 外兼容常见 OneBot 实现）
pub const ONEBOT_TOKEN_PREFIX: &str = "Token ";
/// 异步/限速队列默认最小间隔兜底毫秒数
pub const DEFAULT_RATE_LIMIT_INTERVAL_MS: u64 = 250;
/// HTTP POST Event Webhook 请求超时秒数
pub const WEBHOOK_HTTP_TIMEOUT_SECS: u64 = 10;
/// HTTP POST Event Webhook 签名请求头
pub const HEADER_X_SIGNATURE: &str = "X-Signature";
/// OneBot 反向 WebSocket 仅接口角色值
pub const HEADER_X_CLIENT_ROLE_API: &str = "API";
/// OneBot 反向 WebSocket 仅事件角色值
pub const HEADER_X_CLIENT_ROLE_EVENT: &str = "Event";
/// OneBot 异步受理应答状态值
pub const ONEBOT_STATUS_ASYNC: &str = "async";
/// OneBot 自发消息上报主类型
pub const ONEBOT_POST_TYPE_MESSAGE_SENT: &str = "message_sent";
/// OneBot 异步操作回执提示码（规范定义 retcode=1）
pub const ONEBOT_RETCODE_ASYNC: i64 = 1;
/// OneBot 未实现或不支持动作的标准返回码
pub const ONEBOT_RETCODE_UNSUPPORTED: i64 = 10004;
/// 入站图片本地媒体缓存目录（与驱动侧保持一致）
pub const MEDIA_CACHE_SUBDIR: &str = ".runtime/media";
/// 出站文件与视频暂存目录（与驱动侧保持一致）
pub const STAGING_UPLOADS_SUBDIR: &str = ".runtime/staging_uploads";
/// `get_file` 内联返回 Base64 内容的最大文件字节阈值（4 MiB）
pub const GET_FILE_MAX_BASE64_BYTES: u64 = 4 * 1024 * 1024;

/// OneBot 扩展正向 WebSocket 路由常量
pub const ROUTE_ONEBOT_WS_ROOT: &str = "/ws";
/// OneBot 正向 API-only WebSocket 路由
pub const ROUTE_ONEBOT_WS_API: &str = "/onebot/v11/ws/api";
/// OneBot 正向 API-only 简写 WebSocket 路由
pub const ROUTE_ONEBOT_API_WS: &str = "/onebot/v11/api";
/// OneBot 正向 Event-only WebSocket 路由
pub const ROUTE_ONEBOT_WS_EVENT: &str = "/onebot/v11/ws/event";
/// OneBot 正向 Event-only 简写 WebSocket 路由
pub const ROUTE_ONEBOT_EVENT_WS: &str = "/onebot/v11/event";
/// OneBot 根级 Event-only 简写 WebSocket 路由
pub const ROUTE_ONEBOT_ROOT_EVENT_WS: &str = "/ws/event";

/// OneBot 参数化 HTTP 动作分发路由模板
pub const ROUTE_ONEBOT_V11_ACTION: &str = "/onebot/v11/{action}";
/// OneBot 根路径 HTTP 动作分发路由模板
pub const ROUTE_ONEBOT_ROOT_ACTION: &str = "/{action}";

/// HTTP 异步/限速动作全局并发排队许可上限（防止任务风暴耗尽内存）
pub const MAX_INFLIGHT_ASYNC_ACTIONS: usize = 256;
/// HTTP 请求体最大允许字节数（8 MiB）
pub const HTTP_MAX_BODY_BYTES: usize = 8 * 1024 * 1024;
/// 单个 WebSocket 会话内最大并发执行动作数
pub const MAX_WS_INFLIGHT_ACTIONS: usize = 32;
/// 单个合并转发消息允许携带的最大节点数
pub const MAX_FORWARD_NODES: usize = 200;

/// 新增 OneBot 扩展支持动作常量
pub const ACTION_GET_STRANGER_INFO: &str = "get_stranger_info";
/// OneBot 扩展动作：获取图片详情与本地缓存路径
pub const ACTION_GET_IMAGE: &str = "get_image";
/// OneBot 扩展动作：获取群历史消息记录
pub const ACTION_GET_GROUP_MSG_HISTORY: &str = "get_group_msg_history";
/// OneBot 扩展动作：获取私聊历史消息记录
pub const ACTION_GET_FRIEND_MSG_HISTORY: &str = "get_friend_msg_history";
/// OneBot 扩展动作：发送群合并转发消息
pub const ACTION_SEND_GROUP_FORWARD_MSG: &str = "send_group_forward_msg";
/// OneBot 扩展动作：发送私聊合并转发消息
pub const ACTION_SEND_PRIVATE_FORWARD_MSG: &str = "send_private_forward_msg";
/// OneBot 扩展动作：通用合并转发消息
pub const ACTION_SEND_FORWARD_MSG: &str = "send_forward_msg";
/// OneBot 扩展动作：查询合并转发节点详情
pub const ACTION_GET_FORWARD_MSG: &str = "get_forward_msg";
/// OneBot 扩展动作：清理本地媒体缓存
pub const ACTION_CLEAN_CACHE: &str = "clean_cache";
/// OneBot 扩展动作：触发适配器热重启
pub const ACTION_SET_RESTART: &str = "set_restart";
/// OneBot 扩展动作：执行事件快速操作
pub const ACTION_HANDLE_QUICK_OPERATION: &str = ".handle_quick_operation";
/// OneBot 标准动作：撤回消息
pub const ACTION_DELETE_MSG: &str = "delete_msg";
