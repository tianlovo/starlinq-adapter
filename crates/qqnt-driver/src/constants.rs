//! QQNT UIA 驱动层共享常量与预编译正则表达式注册表，统一管理超时阈值、区域边界与节点匹配规则

use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;

/// 内部全树采集默认最大扫描深度
pub(crate) const INTERNAL_SCAN_DEPTH: usize = 26;

/// 驱动层广播事件通道缓冲区容量
pub(crate) const BROADCAST_CAPACITY: usize = 2048;

/// 突发期轮询时左侧群成员列表最小刷新间隔（毫秒），防止高频扫树反复重写入库
pub(crate) const SIDEBAR_MEMBER_REFRESH_INTERVAL_MS: u64 = 10_000;

/// 验证阶段 `ping` 消息自动回复默认文本
pub(crate) const AUTO_PING_REPLY_TEXT: &str = "pong (from qq-uia-adapter)";

/// QQ 仍停留在登录或扫码引导窗口时的提示信息
pub(crate) const LOGIN_SPLASH_ERROR: &str = "QQ appears to be at the login screen or not logged into the main chat window yet; please log into QQ first";

/// 持久化自账号昵称的 KV 元数据键名
pub(crate) const SELF_NICKNAME_META_KEY: &str = "self_nickname";

/// 持久化自账号 OneBot 识别号的 KV 元数据键名
pub(crate) const SELF_USER_ID_META_KEY: &str = "self_user_id";

/// 发送完成后轮询回显确认的重试间隔序列（毫秒）
pub(crate) const SEND_VERIFY_RETRY_DELAYS_MS: [u64; 6] = [80, 100, 140, 180, 220, 260];

/// WinEvent 事件触发后等待 UI 树稳定再采集的防抖延时（毫秒）
pub(crate) const POLLER_WINEVENT_SETTLE_MS: u64 = 50;

/// 生产轮询周期下的保底定时器最小间隔（毫秒）
pub(crate) const POLLER_FALLBACK_MIN_MS: u64 = 2000;

/// 用于识别集成测试高速轮询模式的配置阈值（毫秒）
pub(crate) const POLLER_FAST_TEST_THRESHOLD_MS: u64 = 500;

/// 测试高速轮询模式下的最小定时器保护下限（毫秒）
pub(crate) const POLLER_FAST_TEST_MIN_MS: u64 = 50;

/// 后台周期同步时单次消息拉取条数上限
pub(crate) const POLLER_SYNC_MESSAGE_LIMIT: usize = 50;

/// 触发主动回复与消息去重时的本端发送回显有效期
pub(crate) const OUTBOUND_TRACKER_TTL: Duration = Duration::from_secs(30);

/// `MessageTracker` 已见消息 ID 有界队列与哈希集合最大容量
pub(crate) const MESSAGE_TRACKER_MAX_SEEN_IDS: usize = 2048;

/// `MessageTracker` 本端已发送文本环形缓冲最大容量
pub(crate) const MESSAGE_TRACKER_MAX_OUTBOUND: usize = 256;

/// 唤醒并置顶消遮挡前等待宿主窗口响应的缓冲时间
pub(crate) const WAKE_PRE_UNOCCLUDE_DELAY: Duration = Duration::from_millis(200);

/// 执行置顶消遮挡后等待 Chromium 无障碍节点生成的时间
pub(crate) const WAKE_UNOCCLUDE_SETTLE_DELAY: Duration = Duration::from_millis(350);

/// 连续尝试唤醒 Chromium 无障碍节点之间的重试间隔
pub(crate) const WAKE_UNOCCLUDE_RETRY_DELAY: Duration = Duration::from_millis(350);

/// 单轮唤醒 Chromium 无障碍节点的最大尝试次数
pub(crate) const WAKE_UNOCCLUDE_MAX_ATTEMPTS: usize = 5;

/// 点击会话列表切换群聊后等待聊天区刷新的时间
pub(crate) const GROUP_SWITCH_SETTLE_DELAY: Duration = Duration::from_millis(150);

/// 搜索框输入群名后等待搜索结果下拉刷新完成的等待时间
pub(crate) const SEARCH_SETTLE_DELAY: Duration = Duration::from_millis(450);

/// 群信息抽屉未提取到真实群号时的重查冷却窗口（10 分钟，毫秒）
pub(crate) const DRAWER_RECHECK_COOLDOWN_MS: u64 = 10 * 60 * 1000;

/// 关闭群信息抽屉后验证右侧抽屉已消失前的等待时长（毫秒）
pub(crate) const DRAWER_CLOSE_WAIT_MS: u64 = 120;

/// 群信息抽屉面板位于窗口右侧区域的最小相对横坐标比例
pub(crate) const DRAWER_PANEL_MIN_REL_X: f64 = 0.45;

/// 聊天标题栏右上角“更多”扩展抽屉按钮的最小相对横坐标比例
pub(crate) const HEADER_MORE_MIN_REL_X: f64 = 0.70;

/// 主窗口内嵌群信息浮层新增节点的最小相对横坐标比例
pub(crate) const IN_WINDOW_POPOVER_MIN_REL_X: f64 = 0.25;

/// 展开右侧群信息抽屉后轮询等待渲染出数据的最大尝试次数
pub(crate) const DRAWER_POLL_ATTEMPTS: usize = 5;

/// 展开右侧群信息抽屉后每轮轮询间隔时长（毫秒）
pub(crate) const DRAWER_POLL_INTERVAL_MS: u64 = 120;

/// 触发群标题按钮后轮询等待“资料卡”弹窗就绪的最大尝试次数
pub(crate) const POPUP_POLL_ATTEMPTS: usize = 6;

/// 触发群标题按钮后每轮轮询等待“资料卡”弹窗的间隔时长（毫秒）
pub(crate) const POPUP_POLL_INTERVAL_MS: u64 = 120;

/// 抽屉与资料卡审计日志最多记录的新增候选文本摘要条数
pub(crate) const AUDIT_CANDIDATE_SAMPLE_LIMIT: usize = 12;

/// 后台投递 `VK_ESCAPE` 按下消息的标准硬件扫描码参数
pub(crate) const SCAN_KEYDOWN_ESCAPE_LPARAM: isize = 0x0001_0001;

/// 后台投递 `VK_ESCAPE` 抬起消息的标准硬件扫描码参数
pub(crate) const SCAN_KEYUP_ESCAPE_LPARAM: isize = 0xC001_0001u32 as isize;

/// 真实 QQ 群号的最小合法数值下界（至少 5 位数字）
pub(crate) const MIN_VALID_REAL_GROUP_ID: i64 = 10_000;

/// UIA 树中自定义控件的扩展类型标识（未在 `uia_core` 标准表中导出）
pub(crate) const CONTROL_TYPE_CUSTOM: &str = "Custom";

/// 顶部搜索群聊时优先排查的输入框关键词
pub(crate) const SEARCH_KEYWORD: &str = "搜索";

/// 群信息抽屉标题及群号标签关键词集合
pub(crate) const DRAWER_GROUP_NUMBER_LABELS: &[&str] = &["群号", "群聊号码", "QQ群号", "Group ID"];

/// 群信息抽屉中用于关闭抽屉的常见按钮标签
pub(crate) const DRAWER_CLOSE_BUTTON_KEYWORDS: &[&str] = &["关闭", "收起", "返回"];

/// 消息链接节点中应被忽略的纯协议或网址前缀
pub(crate) const IGNORED_LINK_URL_PREFIXES: &[&str] =
    &["app://", "mqqapi://", "http://", "https://", "javascript:"];

/// QQNT 常见反诈预警提示文案特征子串
pub(crate) const ANTI_FRAUD_NOISE_PHRASES: &[&str] = &[
    "涉及金钱交易请电话核实身份",
    "谨防境外诈骗",
    "该账号疑似非大陆地区",
];

/// QQNT 常见群系统通知文本特征子串
pub(crate) const SYSTEM_NOTICE_PHRASES: &[&str] = &[
    "邀请你加入了群聊",
    "加入了群聊。",
    "撤回了一条消息",
    "退出了群聊。",
    "移出了群聊。",
    "回应了你的",
    "拍了拍",
    "戳了戳",
    "设为管理员",
    "设置管理员",
    "取消管理员",
    "被取消管理员",
    "设为精华消息",
    "取消精华消息",
    "移出精华消息",
    "设为群精华",
    "已经成为好友",
    "已经是好友",
    "已添加你为好友",
    "手气王",
    "运气王",
    "开启了全员禁言",
    "关闭了全员禁言",
];

/// QQNT 聊天窗口文件气泡状态标签词表
pub(crate) const FILE_BUBBLE_STATUS_KEYWORDS: &[&str] = &[
    "已发送",
    "已上传",
    "已接收",
    "未下载",
    "已下载",
    "正在发送",
    "正在接收",
    "发送中",
    "上传中",
    "接收中",
    "取消",
    "天内有效",
    "天后过期",
];

/// 单次发送在驱动队列锁内自动等待前序冷却的最大毫秒上限
pub(crate) const MAX_SEND_COOLDOWN_WAIT_MS: u64 = 10_000;

/// 左侧会话标题中常见明确指向群聊的结尾关键词集合
pub(crate) const GROUP_TITLE_SUFFIX_HINTS: &[&str] = &[
    "测试群",
    "聊天群",
    "交流群",
    "通知群",
    "官方群",
    "粉丝群",
    "工作群",
    "讨论群",
    "群聊",
];

/// 左侧会话预览文本中可明确识别为群聊会话的特征短语集合
pub(crate) const GROUP_PREVIEW_NOTICE_HINTS: &[&str] = &[
    "[有人@我]",
    "[有全体消息]",
    "[全体成员]",
    "@全体成员",
    "邀请你加入了群聊",
    "加入了群聊",
    "移出了群聊",
    "退出了群聊",
    "[群公告]",
    "[群签到]",
    "[群投票]",
];

/// V3 入站图片切片与富文本消息持久化目录及前缀集中常量
pub(crate) const MEDIA_CACHE_SUBDIR: &str = ".runtime/media";
pub(crate) const MEDIA_PNG_EXTENSION: &str = "png";
pub(crate) const MEDIA_URL_FILE_SCHEME: &str = "file://";
pub(crate) const IMAGE_SRC_BASE64_PREFIX: &str = "base64://";
pub(crate) const IMAGE_SRC_DATA_URL_PREFIX: &str = "data:";
pub(crate) const IMAGE_SRC_HTTP_PREFIX: &str = "http://";
pub(crate) const IMAGE_SRC_HTTPS_PREFIX: &str = "https://";

/// QQNT UI 文本、表情反应角标与引用头解析阈值集中定义
pub(crate) const UI_IMAGE_CONTROL_NAME: &str = "图片";
pub(crate) const ONEBOT_AT_ALL_QQ: &str = "all";
pub(crate) const AT_ALL_DISPLAY_NAME: &str = "全体成员";
pub(crate) const REPLY_QUOTE_OPEN_CN: &str = "「回复 ";
pub(crate) const REPLY_QUOTE_OPEN_EN: &str = "[回复 ";
pub(crate) const AUDIT_PRIVATE_WHITELIST_ABORT: &str = "private_whitelist_abort";
pub(crate) const REACTION_COUNTER_MAX_LEN: usize = 5;
pub(crate) const REACTION_GROUP_MAX_HEIGHT: i32 = 28;
pub(crate) const REACTION_BOTTOM_TOLERANCE: i32 = 2;
pub(crate) const MIN_VISIBLE_IMAGE_DIM: i32 = 8;
pub(crate) const MIN_UNLABELED_IMAGE_DIM: i32 = 32;
pub(crate) const MEDIA_CACHE_KEEP_LIMIT: usize = 512;
pub(crate) const HTTP_IMAGE_FETCH_TIMEOUT_SECS: u64 = 15;

/// 消息行中“昵称 [时间]: 内容”格式的预编译正则
pub(crate) static FORMATTED_MESSAGE_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?P<sender>.+?)\s*\[(?P<time>[^\]]{1,32})\]:\s*(?P<content>[\s\S]*)$")
        .expect("valid formatted message regex")
});

/// 消息行中“发送者: 内容”格式的预编译正则
pub(crate) static COLON_MESSAGE_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?P<sender>[^:\[\]\r\n]{1,32}):\s+(?P<content>[\s\S]+)$")
        .expect("valid colon message regex")
});

/// 纯时间或带相对日期的时间戳预编译正则
pub(crate) static TIMESTAMP_ONLY_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:(?:今天|昨天|前天)\s*)?(?:\d{4}[-/年]\d{1,2}[-/月]\d{1,2}日?\s*)?\d{1,2}:\d{2}(?::\d{2})?$",
    )
    .expect("valid timestamp regex")
});

/// 纯日期分界提示文本预编译正则
pub(crate) static DATE_ONLY_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:\d{4}[-/年]\d{1,2}[-/月]\d{1,2}日?|昨天|今天|前天)$")
        .expect("valid date regex")
});

/// 群标题尾部成员数量括号后缀预编译正则
pub(crate) static GROUP_COUNT_SUFFIX_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\s*[（(][0-9]{1,5}人?[)）]\s*$").expect("valid group suffix regex")
});

/// 标题栏与会话列表中的成员数提取预编译正则
pub(crate) static MEMBER_COUNT_PAREN_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[（(]([0-9]{1,5})人?[)）]").expect("valid member count regex"));

/// 会话列表中未读消息数量提取预编译正则
pub(crate) static UNREAD_COUNT_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"([0-9]{1,4})\s*条未读").expect("valid unread count regex"));

/// 右侧群成员栏标题计数（如 `群聊成员 9`）提取预编译正则
pub(crate) static MEMBER_SIDEBAR_COUNT_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:群聊?成员|Members?)\s*[:：(\[（]?\s*([0-9]{1,5})\s*[)\]）]?")
        .expect("valid sidebar count regex")
});

/// 群等级徽章文本（如 `LV 12`）过滤预编译正则
pub(crate) static LEVEL_BADGE_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^LV\s*[0-9]{1,3}$").expect("valid level badge regex"));

/// 群信息抽屉中独立纯数字群号节点（5 至 12 位）预编译正则
pub(crate) static DRAWER_GROUP_ID_STANDALONE_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?P<id>[1-9]\d{4,11})$").expect("valid drawer group id regex"));

/// 群信息抽屉中带“群号”标签的文本提取预编译正则
pub(crate) static DRAWER_GROUP_ID_LABELED_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:群号|群聊号码|QQ\s*群号?|Group\s*ID)\s*[:：]?\s*(?P<id>[1-9]\d{4,11})")
        .expect("valid labeled group id regex")
});

/// 群信息抽屉及资料卡中内嵌在括号或“复制”字样旁的 5 至 12 位群号词元预编译正则
pub(crate) static DRAWER_GROUP_ID_TOKEN_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:^|[\s(（:：#])(?P<id>[1-9]\d{4,11})(?:$|[\s)）复制])")
        .expect("valid token group id regex")
});

/// 左侧会话列表单行拼接字符串中拆分纯群名与时间/消息预览的预编译正则
pub(crate) static SESSION_INLINE_SPLIT_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?P<title>.+?)\s+(?P<preview>(?:昨天|前天|星期[一二三四五六日天]|\d{1,2}:\d{2}|\d{1,2}/\d{1,2}|\d{4}[/-]\d{1,2}[/-]\d{1,2})(?:\s+.*)?)$",
    )
    .expect("valid session inline split regex")
});

/// 群信息抽屉中群规模上限比值（如 `9/200`）预编译正则
pub(crate) static DRAWER_CAPACITY_RATIO_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?P<current>\d{1,4})\s*/\s*(?P<max>\d{2,5})\s*人?")
        .expect("valid drawer capacity ratio regex")
});

/// QQNT 文件气泡体积与单位匹配预编译正则
pub(crate) static FILE_BUBBLE_SIZE_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?P<num>\d+(?:\.\d+)?)\s*(?P<unit>TB|GB|MB|KB|B)\b")
        .expect("valid file bubble size regex")
});
