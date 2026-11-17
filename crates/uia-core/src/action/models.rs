//! 输入动作参数对象（Parameter Object）与目标身份作用域定义模块，收敛长参数列表并明确发送目标语义

use common_types::constants::limits::{DEFAULT_CHAR_DELAY_MAX_MS, DEFAULT_CHAR_DELAY_MIN_MS};
use common_types::SendShortcut;

use crate::msaa::UiNodeHandle;
use crate::win32::VerifiedWindowIdentity;

/// 模拟人工输入时的逐字符随机延迟区间参数对象（单位：毫秒）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TypingDelayRange {
    /// 单字符输入最小等待时间（毫秒）
    pub min_ms: u64,
    /// 单字符输入最大等待时间（毫秒）
    pub max_ms: u64,
}

impl TypingDelayRange {
    /// 构造指定毫秒边界的输入延迟区间参数对象
    pub const fn new(min_ms: u64, max_ms: u64) -> Self {
        Self { min_ms, max_ms }
    }

    /// 构造零延迟输入参数对象，常用于测试或即时文本注入场景
    pub const fn instant() -> Self {
        Self {
            min_ms: 0,
            max_ms: 0,
        }
    }

    /// 使用全局共享配置中的默认字符输入延迟范围构造参数对象
    pub const fn from_defaults() -> Self {
        Self {
            min_ms: DEFAULT_CHAR_DELAY_MIN_MS,
            max_ms: DEFAULT_CHAR_DELAY_MAX_MS,
        }
    }
}

impl Default for TypingDelayRange {
    fn default() -> Self {
        Self::from_defaults()
    }
}

/// 前台聚焦输入与提交操作的目标控件聚合参数对象，绑定所属窗口、输入框、可选发送按钮与提交快捷键
#[derive(Debug, Clone, Copy)]
pub struct ComposerSubmitTarget<'a> {
    /// 目标宿主窗口句柄；在 `*_verified` 接口中可传 0（自动取自绑定身份）或必须与绑定句柄一致
    pub window_hwnd: isize,
    /// 消息编辑框无障碍句柄引用
    pub editor: &'a UiNodeHandle,
    /// 可选的发送按钮无障碍句柄引用，优先通过后台模式激活点击
    pub send_button: Option<&'a UiNodeHandle>,
    /// 当未提供发送按钮或后台触发失败时回退使用的发送快捷键
    pub shortcut: SendShortcut,
}

/// 内部封装原始窗口句柄或已绑定的不可变目标身份作用域，消除无校验与有校验路径的逻辑重复
#[derive(Copy, Clone)]
pub(crate) enum TargetIdentityScope<'a> {
    /// 仅携带原始窗口句柄（不校验绑定身份）
    Raw(isize),
    /// 携带已锁定进程与类原子的不可变身份代理
    Verified(&'a VerifiedWindowIdentity),
}

impl<'a> TargetIdentityScope<'a> {
    /// 提取本次动作对应的宿主顶层窗口句柄
    pub(crate) fn window_hwnd(self) -> isize {
        match self {
            Self::Raw(window_hwnd) => window_hwnd,
            Self::Verified(identity) => identity.hwnd(),
        }
    }

    /// 提取可选的窗口身份保护代理引用
    pub(crate) fn identity(self) -> Option<&'a VerifiedWindowIdentity> {
        match self {
            Self::Raw(_) => None,
            Self::Verified(identity) => Some(identity),
        }
    }
}
