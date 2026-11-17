//! 洪峰调度器优先级分层、突发状态枚举与遥测快照 DTO 定义

use serde::{Deserialize, Serialize};

/// 消息响应优先级分层（P0 关键触发 > P1 高优先级上下文 > P2 普通闲聊）
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum MessagePriorityTier {
    /// P0 关键机器人触发消息（如 @机器人、引用回复机器人或命令前缀唤醒）
    CriticalBotTrigger,
    /// P1 高优先级上下文消息（如活跃对话线程、@全体成员或唤醒关键词命中）
    HighContextual,
    /// P2 普通闲聊消息（洪峰期优先进行确定性抽样与选择性丢弃）
    #[default]
    OrdinaryChatter,
}

/// 调度器当前的洪峰限流状态机阶段
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum BurstModeState {
    /// 常规平稳流量状态
    #[default]
    Normal,
    /// 突发消息洪峰激活状态（启用分层配额与选择性丢包保护）
    BurstActive,
}

/// 洪峰调度器与多群响应窗口选择器的运行期遥测快照
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SchedulerTelemetrySnapshot {
    /// 当前是否处于突发消息洪峰模式
    pub burst_state: BurstModeState,
    /// 累计进入洪峰模式的次数
    pub burst_enter_count: u64,
    /// 最近滑动窗口内到达的消息事件总数
    pub recent_window_arrivals: usize,
    /// 当前排队等待派发的事件总数
    pub pending_total: usize,
    /// 当前排队等待派发的 P0 关键消息数
    pub pending_p0: usize,
    /// 当前排队等待派发的 P1 上下文消息数
    pub pending_p1: usize,
    /// 当前排队等待派发的 P2 普通闲聊消息数
    pub pending_p2: usize,
    /// 调度器累计接收的入站消息总数
    pub received_total: u64,
    /// 累计已派发的 P0 关键消息总数
    pub dispatched_p0_total: u64,
    /// 累计已派发的 P1 上下文消息总数
    pub dispatched_p1_total: u64,
    /// 累计已派发的 P2 普通闲聊消息总数
    pub dispatched_p2_total: u64,
    /// 洪峰期通过抽样或深度阈值选择性丢弃的 P2 消息总数
    pub selective_shed_p2_total: u64,
    /// 因超出响应时间窗口而在派发前过期的消息总数
    pub expired_window_total: u64,
    /// 出站发送阶段因截止时间已过而被中止的回复总数
    pub outbound_deadline_aborted_total: u64,
    /// 多群轮巡或紧急抢占引发的会话窗口切换总次数
    pub multi_group_switches_total: u64,
    /// 最近一次派发消息所属的群聊名称
    pub last_dispatched_group: Option<String>,
    /// 当前处于活跃监控中的目标群聊数量
    pub active_monitored_groups: usize,
}
