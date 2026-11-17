//! 命令行参数解析模块（Clap）：定义所有子命令、全局参数及十六进制/十进制 HWND 解析器

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use common_types::InputMode;

use crate::constants::{DEFAULT_CONFIG_PATH, DEFAULT_STRICT_BELOW_MEMBERS};

/// Windows UI Automation QQNT 机器人适配器顶层命令行参数结构
#[derive(Debug, Parser)]
#[command(
    name = "qq-uia-adapter",
    version,
    about = "Windows UI Automation QQNT Bot Adapter (REST / WS / OneBot v11)"
)]
pub struct Cli {
    /// TOML 配置文件路径
    #[arg(long, global = true, default_value = DEFAULT_CONFIG_PATH)]
    pub config: PathBuf,

    /// 远程适配器守护进程地址（例如 http://127.0.0.1:18770）
    #[arg(long, global = true)]
    pub remote: Option<String>,

    /// 强制在当前桌面本地执行 UIA，不探测或桥接桌面守护进程
    #[arg(long, global = true)]
    pub local_only: bool,

    #[command(subcommand)]
    pub command: Commands,
}

/// 适配器 CLI 支持的所有子命令枚举
#[derive(Debug, Subcommand)]
pub enum Commands {
    /// 启动 HTTP / WebSocket / OneBot v11 常驻适配服务
    Serve {
        /// 覆盖配置中的监听端口
        #[arg(long)]
        port: Option<u16>,
        /// 覆盖配置中的监听主机地址
        #[arg(long)]
        host: Option<String>,
        /// 为接收到的 ping 消息启用自动回复 pong
        #[arg(long)]
        auto_ping: bool,
    },
    /// 查看当前驱动运行状态（桌面名、QQ 进程号、可执行路径与当前附加群）
    Status,
    /// 唤醒或拉起本地 QQNT 桌面窗口并确保对 UIA 可见
    EnsureQq {
        /// 即使已检测到窗口也强制重新触发无障碍树唤醒序列
        #[arg(long)]
        force_wake: bool,
    },
    /// 对候选 QQ 窗口执行 Win32、MSAA、IA2 与 UIA 综合无障碍诊断
    Diag {
        /// 十六进制 (0x...) 或十进制目标窗口句柄
        #[arg(long)]
        hwnd: Option<String>,
    },
    /// 向后台桌面守护进程请求纯 Rust 热重启并等待新进程上线
    Restart,
    /// 停止已有守护进程、暂存运行时副本并脱离终端启动后台常驻服务
    StartDaemon,
    /// 枚举候选 QQ 窗口并递归导出 UIA 元素树快照
    Inspect {
        /// 十六进制 (0x...) 或十进制目标窗口句柄
        #[arg(long)]
        hwnd: Option<String>,
        /// UIA 树最大抓取深度
        #[arg(long, default_value_t = 5)]
        depth: usize,
        /// 可选的 JSON 快照输出文件路径
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// 列出 QQ 左侧会话列表中的可见条目
    Sessions,
    /// 打开目标群聊（校验人数阈值）、私聊窗口或仅查询离线 SQLite 历史消息并打印记录
    Messages {
        /// 目标群名称或私聊好友昵称（默认读取 config.target.group_name）
        #[arg(long)]
        group: Option<String>,
        /// 以白名单好友私聊模式先切换会话再拉取消息（需配合 --group 指定好友）
        #[arg(long)]
        private: bool,
        /// 仅从本地 SQLite 历史表读取消息，不触发 UIA 或切群
        #[arg(long)]
        history: bool,
        /// 最多拉取的历史消息条数
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// 向前翻页上界游标（仅返回 `onebot_message_id <= before_id` 的历史记录）
        #[arg(long)]
        before_id: Option<i64>,
        /// 向后增量游标（仅返回 `onebot_message_id > after_id` 的历史记录）
        #[arg(long)]
        after_id: Option<i64>,
        /// 按发送者昵称不区分大小写子串过滤
        #[arg(long)]
        sender: Option<String>,
        /// 按消息正文关键词不区分大小写子串过滤
        #[arg(long)]
        keyword: Option<String>,
        /// 按消息编号从新到旧倒序输出
        #[arg(long)]
        reverse_order: bool,
        /// 以格式化的原始 JSON 输出消息列表
        #[arg(long)]
        json: bool,
    },
    /// 打开目标群聊或白名单好友私聊并发送结构化复合消息（支持回复、艾特、表情、文本、图片与文件）
    Send {
        /// 目标群名称或私聊好友昵称（群聊默认读取 config.target.group_name）
        #[arg(long)]
        group: Option<String>,
        /// 强制以好友私聊模式发送（需通过 target.allowed_private_friends 白名单校验）
        #[arg(long)]
        private: bool,
        /// 待发送的纯文本内容（搭配其他消息段时可省略）
        #[arg(long)]
        text: Option<String>,
        /// 可选附带的图片源（支持本地路径、file://、base64:// 或 http://）
        #[arg(long)]
        image: Option<String>,
        /// 可选附带的本地文件路径或文件 URI
        #[arg(long)]
        file: Option<String>,
        /// 可选附带的本地视频路径、file:// 或 base64://（严格模式下需独立发送）
        #[arg(long)]
        video: Option<String>,
        /// 追加 QQ 经典系统表情（支持数字编号如 14 或中文名称如 微笑，可多次指定）
        #[arg(long)]
        face: Vec<String>,
        /// 追加 `@` 群成员消息段（支持数字 QQ 号、成员名称或 `all`，可多次指定）
        #[arg(long)]
        at: Vec<String>,
        /// 引用回复指定历史消息 ID
        #[arg(long)]
        reply: Option<i64>,
        /// 可选输入模式覆盖：auto | background_uia | focus_restore_unicode
        #[arg(long)]
        mode: Option<InputMode>,
        /// 可选出站发送响应时间窗（毫秒，超过窗口仍未送达将中止发送）
        #[arg(long)]
        response_window_ms: Option<u64>,
    },
    /// 查看洪峰调度器实时状态、队列遥测与响应窗口策略
    Scheduler {
        /// 以格式化的原始 JSON 输出调度配置与遥测快照
        #[arg(long)]
        json: bool,
    },
    /// 将指定群聊或白名单好友私聊会话的未读消息角标标记为已读
    MarkRead {
        /// 目标群名称（若与 --private 均省略则默认使用 config.target.group_name）
        #[arg(long)]
        group: Option<String>,
        /// 白名单私聊好友昵称（不可与 --group 同时传入）
        #[arg(long)]
        private: Option<String>,
    },
    /// 运行真实 QQNT 目标群验证流程并输出 `.runtime/latest-report.json`
    VerifyGroup {
        /// 目标群名称（默认读取 config.target.group_name）
        #[arg(long)]
        group: Option<String>,
        /// 严格限制群成员人数必须严格小于该上限
        #[arg(long, default_value_t = DEFAULT_STRICT_BELOW_MEMBERS)]
        below_members: u32,
        /// 附着成功后可选发送的一条验证消息文本
        #[arg(long)]
        send_text: Option<String>,
    },
    /// 查看嵌入式 SQLite 存储库统计计数与已缓存群/好友概要
    Storage,
    /// 查询已缓存或刷新后的好友列表元数据
    Friends {
        /// 是否跳过缓存并强制刷新好友目录
        #[arg(long)]
        no_cache: bool,
    },
    /// 撤回或删除指定消息 ID 对应的消息记录
    #[command(visible_alias = "delete-msg")]
    Recall {
        /// 待撤回消息的 OneBot 数字消息 ID
        #[arg(long, alias = "id")]
        message_id: i64,
    },
    /// 查询已缓存群列表元数据并支持跳过缓存强制刷新
    Groups {
        /// 是否跳过缓存并强制重新采集群列表
        #[arg(long)]
        no_cache: bool,
    },
    /// 查询目标群详情并探测真实数字群号
    GroupInfo {
        /// 目标群名称（缺省使用配置目标群）
        #[arg(long)]
        group: Option<String>,
        /// 目标群号（支持真实数字群号或兼容逻辑群号）
        #[arg(long)]
        group_id: Option<i64>,
        /// 是否跳过缓存并强制重新探测真实群号
        #[arg(long)]
        no_cache: bool,
    },
    /// 查询目标群成员列表并支持跳过缓存强制刷新
    GroupMembers {
        /// 目标群名称（缺省使用当前活跃群或配置目标群）
        #[arg(long)]
        group: Option<String>,
        /// 目标群号（支持真实数字群号或兼容逻辑群号）
        #[arg(long)]
        group_id: Option<i64>,
        /// 是否跳过缓存并强制重新探测成员列表
        #[arg(long)]
        no_cache: bool,
    },
    /// 对本地图片、file:// URI 或 base64:// 载荷执行离线 Windows OCR 文字识别
    Ocr {
        /// 待识别图片路径、file:// URI 或 base64:// 载荷
        #[arg(long)]
        image: String,
        /// 可选 OCR 识别语言标签（如 zh-Hans-CN、en-US）
        #[arg(long)]
        lang: Option<String>,
        /// 以格式化的原始 JSON 输出识别结果与坐标框
        #[arg(long)]
        json: bool,
    },
    /// 将单条历史消息原生转发至目标群聊或白名单好友私聊
    Forward {
        /// 待转发的历史 OneBot 消息数字编号
        #[arg(long)]
        message_id: i64,
        /// 目标群名称（与 --private 互斥，均省略时使用配置默认目标群）
        #[arg(long)]
        group: Option<String>,
        /// 目标白名单好友昵称（与 --group 互斥）
        #[arg(long)]
        private: Option<String>,
        /// 可选出站响应时间窗（毫秒，超时将中止转发）
        #[arg(long)]
        response_window_ms: Option<u64>,
    },
    /// 按 `fwd-*` 转发编号或消息数字编号查询合并转发节点明细
    ForwardGet {
        /// 合并转发编号（如 `fwd-10001`）或包含转发卡的消息数字 ID
        #[arg(long)]
        id: String,
        /// 以格式化的原始 JSON 输出转发节点明细
        #[arg(long)]
        json: bool,
    },
    /// 检索已脱敏的安全熔断、撤回与污染清理持久化审计事件流水
    Audit {
        /// 可选事件类型过滤标识
        #[arg(long)]
        kind: Option<String>,
        /// 返回的最大记录条数（1..=200）
        #[arg(long, default_value_t = 50)]
        limit: usize,
        /// 以格式化的原始 JSON 输出审计事件列表
        #[arg(long)]
        json: bool,
    },
    /// 清理 `.runtime/media` 与 `.runtime/staging_uploads` 运行时缓存文件
    CleanCache {
        /// 以格式化的原始 JSON 输出清理统计结果
        #[arg(long)]
        json: bool,
    },
    /// 核验风控弹窗关闭后手动重置安全验证熔断器
    ResetBreaker {
        /// 以格式化的原始 JSON 输出重置后的状态快照
        #[arg(long)]
        json: bool,
    },
    /// 在当前桌面运行端到端 Win32 模拟窗口、UIA、正反向 WS 与 OneBot v11 全链路自测
    SelfTest,
}

/// 解析十六进制（`0x...`）或十进制字符串格式的 Win32 窗口句柄
pub fn parse_hwnd(raw: &str) -> Result<isize> {
    let trimmed = raw.trim();
    if let Some(hex) = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
    {
        isize::from_str_radix(hex, 16).with_context(|| format!("invalid hex hwnd '{trimmed}'"))
    } else {
        trimmed
            .parse::<isize>()
            .with_context(|| format!("invalid decimal hwnd '{trimmed}'"))
    }
}
