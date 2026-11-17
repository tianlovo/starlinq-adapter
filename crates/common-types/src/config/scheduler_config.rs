//! 洪峰调度器、响应窗口选择器与多群轮巡策略配置结构体定义

use serde::{Deserialize, Serialize};

/// 单群及多群并发洪峰消息的防雪崩调度、优先级分层与响应窗口选择策略配置
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SchedulerConfig {
    /// 是否启用洪峰调度器与响应窗口选择器
    pub enabled: bool,
    /// 判定进入或退出洪峰态的滑动时间窗口长度（毫秒）
    pub burst_window_ms: u64,
    /// 窗口到达消息数达到或超过该阈值时进入洪峰模式
    pub burst_enter_threshold: usize,
    /// 窗口到达消息数回落至该阈值及以下时退出洪峰模式
    pub burst_exit_threshold: usize,
    /// 单次 UIA 轮询增量条数达到该阈值时直接触发洪峰保护
    pub single_poll_burst_threshold: usize,
    /// P0 关键触发消息（@机器人、引用机器人、命令前缀）的最大响应窗口（毫秒）
    pub critical_response_window_ms: u64,
    /// P1 上下文活跃线程消息（@全体、唤醒词、连续对话）的最大响应窗口（毫秒）
    pub contextual_response_window_ms: u64,
    /// P2 普通闲聊消息的最大响应窗口（毫秒）
    pub ordinary_response_window_ms: u64,
    /// 洪峰期 P2 普通闲聊消息的确定性抽样步长（每 N 条保留 1 条）
    pub burst_ordinary_sample_stride: usize,
    /// 队列积压达到该深度时对未派发 P2 普通闲聊执行硬丢弃
    pub hard_shed_queue_depth: usize,
    /// 调度队列允许保留的最大待处理事件总数
    pub max_pending_events: usize,
    /// 派发或出站发送前是否丢弃已超出响应窗口的过期事件
    pub drop_expired_events: bool,
    /// 多群切换后在当前会话窗口保持的最小驻留时长（毫秒）
    pub session_dwell_min_ms: u64,
    /// 遇 P0 紧急触发消息时允许抢占当前群会话的最短驻留保护时长（毫秒）
    pub urgent_preempt_dwell_ms: u64,
    /// 多群轮巡探测后台未读会话的时间间隔（毫秒）
    pub patrol_interval_ms: u64,
    /// 单轮切入某个群聊后最多连续派发的消息配额条数
    pub group_quantum_messages: usize,
    /// P1 活跃对话线程的记忆生存期（毫秒）
    pub active_thread_ttl_ms: u64,
    /// 识别为机器人命令的文本前缀集合
    pub command_prefixes: Vec<String>,
    /// 命中后可提升为高优先级的自定义唤醒关键词集合
    pub wake_keywords: Vec<String>,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            burst_window_ms: 3000,
            burst_enter_threshold: 16,
            burst_exit_threshold: 6,
            single_poll_burst_threshold: 12,
            critical_response_window_ms: 30_000,
            contextual_response_window_ms: 20_000,
            ordinary_response_window_ms: 8_000,
            burst_ordinary_sample_stride: 5,
            hard_shed_queue_depth: 64,
            max_pending_events: 1024,
            drop_expired_events: true,
            session_dwell_min_ms: 1200,
            urgent_preempt_dwell_ms: 350,
            patrol_interval_ms: 2500,
            group_quantum_messages: 8,
            active_thread_ttl_ms: 20_000,
            command_prefixes: ["/", "!", "！", ".", "。"]
                .iter()
                .map(|prefix| (*prefix).to_string())
                .collect(),
            wake_keywords: Vec::new(),
        }
    }
}
