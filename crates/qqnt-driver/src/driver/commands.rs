//! 异步驱动门面与专用 STA COM 工作线程之间的指令协议定义

use std::collections::VecDeque;

use common_types::{
    AdapterStatus, ChatMessage, ChatTargetKind, ForwardSendOutcome, ForwardSendRequest, FriendInfo,
    GroupInfoSummary, GroupMemberInfo, GroupTargetSpec, Result, SendOutcome, SendRequest,
    SessionSummary, SingleForwardRequest, UiNodeSnapshot, WindowDiagnosticReport, WindowInfo,
};
use parking_lot::{Condvar, Mutex};
use serde_json::Value;
use tokio::sync::oneshot;

/// 由 `QqntDriver` 投递至后台 COM 工作线程串行执行的内部控制命令
pub(crate) enum DriverCommand {
    /// 查询驱动当前运行状态并刷新进程及安全熔断检查
    Status(oneshot::Sender<Result<AdapterStatus>>),
    /// 发送前校验安全风控弹窗与进程状态
    CheckSecurity(oneshot::Sender<Result<()>>),
    /// 枚举当前桌面可见的 QQ 与允许的白名单测试窗口
    ListWindows(oneshot::Sender<Result<Vec<WindowInfo>>>),
    /// 检查并按配置拉起、唤醒 QQ 主窗口与无障碍控件树
    EnsureQqWindow {
        force_wake: bool,
        resp: oneshot::Sender<Result<Vec<WindowInfo>>>,
    },
    /// 采集指定窗口句柄的层级、文本与 Chromium 无障碍诊断报告
    DiagnoseWindow {
        hwnd: Option<isize>,
        resp: oneshot::Sender<Result<WindowDiagnosticReport>>,
    },
    /// 导出当前附加或指定窗口的语义 UI 节点树快照
    InspectTree {
        hwnd: Option<isize>,
        max_depth: usize,
        resp: oneshot::Sender<Result<UiNodeSnapshot>>,
    },
    /// 提取左侧会话列表并同步群摘要至嵌入式 SQLite 缓存
    ListSessions(oneshot::Sender<Result<Vec<SessionSummary>>>),
    /// 按指定群名或搜索策略打开目标群聊并校验小于 10 人安全护栏
    OpenGroup {
        spec: GroupTargetSpec,
        resp: oneshot::Sender<Result<AdapterStatus>>,
    },
    /// 打开并安全附着指定私聊好友会话（须先通过白名单校验）
    OpenPrivate {
        target: String,
        resp: oneshot::Sender<Result<AdapterStatus>>,
    },
    /// 采集目标群聊当前消息列表（返回全部快照消息与新到达外部消息）
    PollMessages {
        group: String,
        limit: usize,
        allow_unocclude: bool,
        resp: oneshot::Sender<Result<(Vec<ChatMessage>, Vec<ChatMessage>)>>,
    },
    /// 执行双阶段防误触与群安全护栏校验后发送群消息并校验回显
    SendGroupMessage {
        req: SendRequest,
        resp: oneshot::Sender<Result<SendOutcome>>,
    },
    /// 执行白名单与无群成员栏双校验后发送私聊消息并校验回显
    SendPrivateMessage {
        req: SendRequest,
        resp: oneshot::Sender<Result<SendOutcome>>,
    },
    /// 后台轮询周期内同步会话列表变化，并在已附着群聊或私聊时一并采集新消息
    SyncAndPoll {
        poll_limit: usize,
        resp: oneshot::Sender<Result<(Vec<SessionSummary>, Vec<ChatMessage>)>>,
    },
    /// 查询 OneBot 群列表缓存，支持指定 `no_cache` 主动刷新 UI
    GetGroupList {
        no_cache: bool,
        resp: oneshot::Sender<Result<Vec<GroupInfoSummary>>>,
    },
    /// 查询 OneBot 好友列表缓存，支持指定 `no_cache` 主动刷新左侧会话
    GetFriendList {
        no_cache: bool,
        resp: oneshot::Sender<Result<Vec<FriendInfo>>>,
    },
    /// 查询指定群元数据，并在缺少真实群号时触发群信息抽屉补全
    GetGroupInfo {
        group_id: Option<i64>,
        group_name: Option<String>,
        no_cache: bool,
        resp: oneshot::Sender<Result<GroupInfoSummary>>,
    },
    /// 查询指定群成员列表，支持指定 `no_cache` 从右侧成员栏重新提取
    GetGroupMemberList {
        group_id: Option<i64>,
        group_name: Option<String>,
        no_cache: bool,
        resp: oneshot::Sender<Result<Vec<GroupMemberInfo>>>,
    },
    /// 在真实客户端中通过多选气泡与合并转发目标弹窗投递真实聊天记录卡片
    SendForwardMessages {
        req: ForwardSendRequest,
        resp: oneshot::Sender<Result<ForwardSendOutcome>>,
    },
    /// 在真实客户端中通过气泡右键菜单“转发”将单条历史消息原生转发至目标会话
    ForwardSingleMessage {
        req: SingleForwardRequest,
        resp: oneshot::Sender<Result<SendOutcome>>,
    },
    /// 在确认桌面已无安全风控弹窗后受控重置安全熔断状态
    ResetSecurityBreaker(oneshot::Sender<Result<AdapterStatus>>),
    /// 在真实客户端中通过右键“撤回”菜单撤回已发送消息并同步清理数据库
    DeleteMessage {
        onebot_message_id: i64,
        resp: oneshot::Sender<Result<()>>,
    },
    /// 采集指定或当前附加 QQ 窗口的完整无边框 `PrintWindow` PNG 截图
    CaptureScreenshot {
        hwnd: Option<isize>,
        resp: oneshot::Sender<Result<Vec<u8>>>,
    },
    /// 执行只读且可自动恢复的 QQNT 弹窗或搜索状态校准探针
    ProbeUiAction {
        action: String,
        target: Option<String>,
        query: Option<String>,
        resp: oneshot::Sender<Result<Value>>,
    },
    /// 将指定群聊或私聊会话未读红点切换激活以清零，并同步更新多群调度器已读状态
    MarkSessionRead {
        target: String,
        target_kind: ChatTargetKind,
        resp: oneshot::Sender<Result<()>>,
    },
    /// 通知后台 COM 工作线程优雅退出消息循环
    Shutdown,
}

/// 支撑紧急指令严格抢占后台轮询的双优先级并发命令队列（紧急通道 > 后台 `SyncAndPoll`）
pub(crate) struct PriorityCommandQueue {
    inner: Mutex<(VecDeque<DriverCommand>, VecDeque<DriverCommand>, bool)>,
    notify: Condvar,
}

impl PriorityCommandQueue {
    /// 创建一个带高/低双队列及条件唤醒原语的命令调度通道
    pub(crate) fn new() -> Self {
        Self {
            inner: Mutex::new((VecDeque::new(), VecDeque::new(), false)),
            notify: Condvar::new(),
        }
    }

    /// 投递一条驱动控制指令：`SyncAndPoll` 落入后台队列，其余所有发送/控制/查询指令落入紧急抢占队列
    pub(crate) fn push(&self, cmd: DriverCommand) -> std::result::Result<(), DriverCommand> {
        let mut guard = self.inner.lock();
        if guard.2 {
            return Err(cmd);
        }
        if matches!(cmd, DriverCommand::SyncAndPoll { .. }) {
            guard.1.push_back(cmd);
        } else {
            guard.0.push_back(cmd);
        }
        self.notify.notify_one();
        Ok(())
    }

    /// 阻塞等待下一条可执行命令，总是优先消费紧急队列，全部为空且已关闭时返回 `None`
    pub(crate) fn pop_blocking(&self) -> Option<DriverCommand> {
        let mut guard = self.inner.lock();
        loop {
            if let Some(urgent) = guard.0.pop_front() {
                return Some(urgent);
            }
            if let Some(bg) = guard.1.pop_front() {
                return Some(bg);
            }
            if guard.2 {
                return None;
            }
            self.notify.wait(&mut guard);
        }
    }

    /// 优雅关闭命令通道：追加退出指令并置位关停标记，唤醒沉睡工作线程
    pub(crate) fn close_with_shutdown(&self) {
        let mut guard = self.inner.lock();
        guard.2 = true;
        guard.0.push_back(DriverCommand::Shutdown);
        self.notify.notify_all();
    }

    /// 返回当前紧急控制队列中排队指令数（仅测试验证未授权目标未入队）
    #[cfg(test)]
    pub(crate) fn urgent_len(&self) -> usize {
        self.inner.lock().0.len()
    }
}
