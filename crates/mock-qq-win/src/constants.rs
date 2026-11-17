//! 模拟 QQNT 窗口类名、控件标识、布局尺寸、自定义窗口消息及默认测试会话常量

use windows::Win32::UI::WindowsAndMessaging::WM_APP;

pub use common_types::constants::{
    DEFAULT_BOT_SELF_NAME, IMAGE_PLACEHOLDER_TOKEN, MOCK_QQ_MAIN_CLASS, MOCK_QQ_SECURITY_CLASS,
    QQ_WINDOW_TITLE, UI_LABEL_SEND_BUTTON,
};

/// 模拟主窗口线程名称
pub const MOCK_THREAD_NAME: &str = "mock-qq-win";

/// 标准 Win32 子控件窗口类名常量
pub const CLASS_LISTBOX: &str = "LISTBOX";
pub const CLASS_STATIC: &str = "STATIC";
pub const CLASS_EDIT: &str = "EDIT";
pub const CLASS_BUTTON: &str = "BUTTON";

/// 子控件整型标识符常量
pub const CONTROL_ID_SESSION_LIST: i32 = 101;
pub const CONTROL_ID_HEADER_STATIC: i32 = 102;
pub const CONTROL_ID_CHAT_HISTORY: i32 = 103;
pub const CONTROL_ID_COMPOSER_EDIT: i32 = 104;
pub const CONTROL_ID_SEND_BUTTON: i32 = 105;

/// 按钮点击通知码
pub const BUTTON_CLICKED_NOTIFY_CODE: u16 = 0;

/// 自定义窗口消息编号常量
pub const WM_APP_REFRESH_CHAT: u32 = WM_APP + 1;
pub const WM_APP_OPEN_SECURITY: u32 = WM_APP + 2;
pub const WM_APP_CLOSE_SECURITY: u32 = WM_APP + 3;
pub const WM_APP_SYNC_SELECTION: u32 = WM_APP + 4;
pub const WM_APP_SHUTDOWN: u32 = WM_APP + 5;

/// 保存会话列表原始窗口过程指针的窗口属性名
pub const PROP_ORIGINAL_LISTBOX_WNDPROC: &str = "MockQqOrigLbWndProc";
/// 保存输入框原始窗口过程指针的窗口属性名
pub const PROP_ORIGINAL_EDIT_WNDPROC: &str = "MockQqOrigEditWndProc";
/// 自检与离线私聊验证使用的预设白名单好友名称
pub const SELF_TEST_PRIVATE_FRIEND: &str = "测试好友";

/// 左侧会话预览中“有人@我”提示标签前缀
pub const SIDEBAR_AT_ME_TAG: &str = "[有人@我] ";
/// 左侧会话预览中“全体成员消息”提示标签前缀
pub const SIDEBAR_AT_ALL_TAG: &str = "[有全体消息] ";
/// 模拟左侧会话单行摘要使用的默认 HH:MM 时间戳
pub const SIDEBAR_MOCK_TIME: &str = "12:00";
/// 左侧会话预览摘要允许保留的最大字符数上限
pub const PREVIEW_SNIPPET_MAX_CHARS: usize = 160;
/// 触发 `[有人@我]` 预览标签的机器人默认英文艾特关键字
pub const MENTION_ME_QQ_BOT: &str = "@qq-bot";
/// 触发 `[有人@我]` 预览标签的机器人默认中文昵称艾特关键字
pub const MENTION_ME_SANDRONE: &str = "@测试机器人";
/// 触发 `[有全体消息]` 预览标签的全体成员艾特关键字
pub const MENTION_ALL_TOKEN: &str = "@全体成员";

/// 主窗口与安全弹窗默认位置尺寸（像素）
pub const MAIN_WINDOW_X: i32 = 80;
pub const MAIN_WINDOW_Y: i32 = 80;
pub const MAIN_WINDOW_WIDTH: i32 = 800;
pub const MAIN_WINDOW_HEIGHT: i32 = 640;
pub const SECURITY_DIALOG_X: i32 = 180;
pub const SECURITY_DIALOG_Y: i32 = 180;
pub const SECURITY_DIALOG_WIDTH: i32 = 360;
pub const SECURITY_DIALOG_HEIGHT: i32 = 220;

/// 预设模拟群聊与私聊好友会话名称与初始聊天消息
pub const INITIAL_MOCK_GROUPS: &[(&str, &str)] = &[
    ("测试大群 (128)", "系统提示 [13:55]: 欢迎来到测试大群"),
    ("Demo测试群 (26)", "大群成员 [14:00]: 26人大群测试"),
    ("Demo测试群 (5)", "群友A [14:00]: 欢迎来到Demo测试群"),
    ("测试好友", "测试好友 [14:02]: 你好，这是私聊测试"),
];
