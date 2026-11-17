//! 网络监听端口、安全人数上限、防抖时间与 SQLite 性能阈值常量

/// 默认本地 HTTP 服务绑定地址
pub const DEFAULT_SERVER_HOST: &str = "127.0.0.1";
/// 默认本地 HTTP 服务监听端口
pub const DEFAULT_SERVER_PORT: u16 = 18770;
/// 默认机器人内部识别号
pub const DEFAULT_SELF_ID: i64 = 10001;
/// 默认严格限制的群成员人数上限（仅允许严格小于该值）
pub const DEFAULT_STRICT_BELOW_MEMBERS: u32 = 10;

/// 默认消息轮询间隔（毫秒）
pub const DEFAULT_POLL_INTERVAL_MS: u64 = 800;
/// 默认单字符输入最小延迟（毫秒）
pub const DEFAULT_CHAR_DELAY_MIN_MS: u64 = 35;
/// 默认单字符输入最大延迟（毫秒）
pub const DEFAULT_CHAR_DELAY_MAX_MS: u64 = 90;
/// 默认连续两次发送之间的最小节流间隔（毫秒）
pub const DEFAULT_MIN_SEND_INTERVAL_MS: u64 = 1500;
/// 默认等待 QQ 客户端启动就绪的超时时长（毫秒）
pub const DEFAULT_QQ_STARTUP_TIMEOUT_MS: u64 = 15_000;

/// 默认 OneBot 心跳上报周期（毫秒）
pub const DEFAULT_ONEBOT_HEARTBEAT_MS: u64 = 15_000;
/// 默认 OneBot 反向 WebSocket 断线重连间隔（毫秒）
pub const DEFAULT_ONEBOT_RECONNECT_MS: u64 = 3_000;

/// 计算确定性 OneBot 整数标识允许的最小上界
pub const MIN_ONEBOT_ID_BOUND: i64 = 1000;
/// 默认生成确定性 OneBot 整数标识的取模上界
pub const DEFAULT_ONEBOT_ID_BOUND: i64 = 1_000_000_000;
/// 消息自增编号的初始序列基数（首条自增结果为 10001）
pub const INITIAL_MESSAGE_SEQUENCE: i64 = 10_000;
/// SQLite 忙等待锁超时时间（毫秒）
pub const SQLITE_BUSY_TIMEOUT_MS: u32 = 5000;
/// SQLite 页缓存大小（负值表示 KiB，即约 4 MiB 内存缓存）
pub const SQLITE_CACHE_SIZE_KIB: i32 = -4000;
/// WinEventHook 事件合并防抖窗口（毫秒）
pub const WIN_EVENT_DEBOUNCE_MS: u64 = 35;

/// QQ 群容量标准档位（200 人群）
pub const QQ_GROUP_TIER_200: i32 = 200;
/// QQ 群容量标准档位（500 人群）
pub const QQ_GROUP_TIER_500: i32 = 500;
/// QQ 群容量标准档位（1000 人群）
pub const QQ_GROUP_TIER_1000: i32 = 1_000;
/// QQ 群容量标准档位（2000 人群）
pub const QQ_GROUP_TIER_2000: i32 = 2_000;
/// QQ 群容量标准档位（3000 人群）
pub const QQ_GROUP_TIER_3000: i32 = 3_000;
