//! UIA 与 Win32 底层引擎内部常量定义，并集中复用共享常量表中的进程名与限流阈值

use common_types::constants::paths::DEFAULT_QQ_EXECUTABLE_PATH;
use common_types::constants::processes::CHROME_RENDER_WIDGET_CLASS;
use windows_core::GUID;

/// 桌面对象名称查询索引（对应 Win32 `UOI_NAME`）
pub(crate) const USER_OBJECT_NAME_INDEX: i32 = 2;
/// 桌面窗口管理器隐匿属性查询标识（对应 `DWMWA_CLOAKED`）
pub(crate) const DWMWA_CLOAKED_ATTR: u32 = 14;
/// 将当前线程临时切换至每监视器高 DPI 感知（`DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2`）的伪句柄值
pub(crate) const DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2: isize = -4;
/// 创建独立后台 QQ 进程时传入的脱离控制台标志（对应 `DETACHED_PROCESS`）
pub(crate) const DETACHED_PROCESS_CREATION_FLAG: u32 = 0x0000_0008;
/// 异步按键状态返回值中的按下状态高位掩码
pub(crate) const KEY_PRESSED_HIGH_BIT_MASK: u16 = 0x8000;
/// MSAA 导航接口查询首个子节点的方向常量（对应 `NAVDIR_FIRSTCHILD`）
pub(crate) const NAVIGATE_FIRST_CHILD_DIRECTION: i32 = 7;
/// UIA `TextPattern` 文档范围全文读取长度哨兵值
pub(crate) const TEXT_PATTERN_READ_ALL: i32 = -1;

/// 表示目标窗口桌面名称未知的占位文本
pub(crate) const DESKTOP_UNKNOWN_NAME: &str = "unknown";
/// 沙盒隔离桌面名称前缀
pub(crate) const DESKTOP_SANDBOX_PREFIX: &str = "exebox";
/// 经典桌面版 QQ 中文窗口标题文本
pub(crate) const QQ_CHINESE_WINDOW_TITLE: &str = "腾讯QQ";
/// 后台事件监听线程命名标识
pub(crate) const EVENT_HOOK_THREAD_NAME: &str = "qq-winevent-watcher";

/// QQNT 客户端可执行文件标准安装路径探测列表
pub(crate) const QQ_EXECUTABLE_FALLBACK_PATHS: &[&str] = &[
    DEFAULT_QQ_EXECUTABLE_PATH,
    r"C:\Program Files\Tencent\QQNT\QQ.exe",
    r"D:\Program Files\Tencent\QQNT\QQ.exe",
    r"D:\Tencent\QQ\QQNT\QQ.exe",
];

/// Chromium 宿主窗口内部承载内容渲染的常见子窗口类名列表
pub(crate) const CHROMIUM_RENDER_CHILD_CLASSES: &[&str] = &[
    CHROME_RENDER_WIDGET_CLASS,
    "Chrome_WidgetWin_0",
    "Intermediate D3D Window",
];

/// MSAA 节点自身子项编号（对应 `CHILDID_SELF`）
pub const CHILDID_SELF: i32 = 0;
/// MSAA 选中标志：请求输入焦点
pub const SELFLAG_TAKEFOCUS: i32 = 1;
/// MSAA 选中标志：请求设置为当前选中项
pub const SELFLAG_TAKESELECTION: i32 = 2;

/// MSAA 状态掩码：控件不可用或已禁用
pub const STATE_SYSTEM_UNAVAILABLE: u32 = 0x0000_0001;
/// MSAA 状态掩码：控件处于只读状态
pub const STATE_SYSTEM_READONLY: u32 = 0x0000_0040;
/// MSAA 状态掩码：控件处于隐藏状态
pub const STATE_SYSTEM_INVISIBLE: u32 = 0x0000_8000;
/// MSAA 状态掩码：控件当前位于屏幕可见区域外
pub const STATE_SYSTEM_OFFSCREEN: u32 = 0x0001_0000;

/// 构造 `WM_KEYDOWN` 回车键消息时附带的扫描码与重复次数标志
pub(crate) const SCAN_KEYDOWN_RETURN_LPARAM: isize = 0x001C_0001;
/// 构造 `WM_KEYUP` 回车键消息时附带的扫描码与抬起标志
pub(crate) const SCAN_KEYUP_RETURN_LPARAM: isize = 0xC01C_0001u32 as isize;
/// 构造 `WM_KEYDOWN` 控制键消息时附带的扫描码与重复次数标志
pub(crate) const SCAN_KEYDOWN_CONTROL_LPARAM: isize = 0x001D_0001;
/// 构造 `WM_KEYUP` 控制键消息时附带的扫描码与抬起标志
pub(crate) const SCAN_KEYUP_CONTROL_LPARAM: isize = 0xC01D_0001u32 as isize;

/// 查询桌面名称时使用的静态 UTF-16 缓冲区长度
pub(crate) const DESKTOP_NAME_BUFFER_LEN: usize = 256;
/// 查询窗口类名时使用的静态 UTF-16 缓冲区长度
pub(crate) const HWND_CLASS_BUFFER_LEN: usize = 256;
/// 查询进程完整镜像路径时使用的缓冲区长度
pub(crate) const IMAGE_PATH_BUFFER_LEN: usize = 1024;
/// 置顶护栏查找原上方兄弟窗口的最大 Z 序遍历步数
pub(crate) const MAX_Z_ORDER_WALK_STEPS: usize = 64;

/// UIA 控件树与平铺列表采集允许的最大节点总数
pub(crate) const MAX_TREE_NODES: usize = 5000;
/// UIA 原始视图唤醒遍历允许的最大递归深度
pub(crate) const MAX_UIA_WAKE_DEPTH: usize = 12;
/// UIA 原始视图唤醒遍历允许访问的最大节点数
pub(crate) const MAX_UIA_WAKE_VISITS: usize = 64;
/// 窗口诊断报告采集语义控件样本的最大条数
pub(crate) const MAX_SEMANTIC_DIAG_SAMPLES: usize = 30;
/// MSAA 控件树采集允许的最大节点总数
pub const MAX_MSAA_TREE_NODES: usize = 5000;
/// 单个 MSAA 容器最多枚举的子节点数量
pub const MAX_CHILDREN_PER_CONTAINER: usize = 512;
/// MSAA 无障碍唤醒遍历允许的最大递归深度
pub(crate) const MAX_MSAA_WAKE_DEPTH: usize = 14;
/// MSAA 无障碍唤醒遍历允许访问的最大节点数
pub(crate) const MAX_MSAA_WAKE_VISITS: usize = 96;
/// MSAA 唤醒报告保留节点样本的最大条数
pub(crate) const MAX_MSAA_SAMPLES: usize = 30;
/// 判定 Chromium 窗口已成功暴露有效控件树的最小节点总数阈值
pub(crate) const MIN_POPULATED_TREE_NODES: usize = 5;
/// WIC 解码与剪贴板位图允许的单边最大像素尺寸（防范解压炸弹 OOM）
pub const MAX_DECODED_IMAGE_DIMENSION: u32 = 8192;
/// WIC 解码与剪贴板位图允许的最大像素缓冲区字节数（64 MiB）
pub const MAX_DECODED_IMAGE_BYTES: usize = 64 * 1024 * 1024;

/// 发送 `WM_GETOBJECT` 唤醒消息时的阻塞超时时间（毫秒）
pub(crate) const WAKE_MESSAGE_TIMEOUT_MS: u32 = 500;
/// 发送无障碍唤醒信号后的短暂等待稳定时长（毫秒）
pub(crate) const POST_WAKE_SETTLE_MS: u64 = 40;
/// 诊断模式下首次执行无障碍唤醒后的等待时长（毫秒）
pub(crate) const POST_FIRST_WAKE_DIAG_MS: u64 = 350;
/// 诊断模式下二次执行无障碍唤醒后的等待时长（毫秒）
pub(crate) const POST_SECOND_WAKE_DIAG_MS: u64 = 250;
/// 通过模式接口直接写入文本后等待编辑器刷新的时长（毫秒）
pub(crate) const POST_VALUE_WRITE_SETTLE_MS: u64 = 20;
/// 通过字符消息逐字输入后等待编辑器提交文本状态的时长（毫秒）
pub(crate) const POST_CHAR_INPUT_SETTLE_MS: u64 = 45;
/// 后台点击提交按钮失败后的重试间隔（毫秒）
pub(crate) const RETRY_INVOKE_DELAY_MS: u64 = 35;
/// 后台发送组合键时按下与抬起之间的保持时长（毫秒）
pub(crate) const KEY_DOWN_UP_INTERVAL_MS: u64 = 15;
/// 前台切换前检测鼠标忙碌状态的单轮采样时长（毫秒）
pub(crate) const MOUSE_IDLE_SAMPLE_MS: u64 = 80;
/// 判定用户正在移动鼠标光标的位移容忍阈值（像素）
pub(crate) const MOUSE_MOVE_TOLERANCE_PX: i32 = 2;
/// 前台切换前避让用户鼠标活动的最大轮询次数
pub(crate) const MAX_FOREGROUND_MOUSE_WAIT_ROUNDS: usize = 6;
/// 发送 Alt 抬起脉冲解锁前台切换锁后的短暂等待时长（毫秒）
pub(crate) const FOREGROUND_ALT_PULSE_SETTLE_MS: u64 = 25;
/// 尝试将目标窗口置为前台后的稳定等待时长（毫秒）
pub(crate) const FOREGROUND_SWITCH_SETTLE_MS: u64 = 20;
/// 窗口置前完成后将焦点定位至输入框前的等待时长（毫秒）
pub(crate) const PRE_FOCUS_EDITOR_DELAY_MS: u64 = 50;
/// 输入框聚焦完成后等待光标就绪的时长（毫秒）
pub(crate) const POST_FOCUS_EDITOR_DELAY_MS: u64 = 45;
/// 前台输入完成后触发提交动作前的短暂稳定等待（毫秒）
pub(crate) const PRE_SUBMIT_FOREGROUND_DELAY_MS: u64 = 55;
/// 前台全选组合键按下后等待选区完成的时长（毫秒）
pub(crate) const POST_SELECT_ALL_DELAY_MS: u64 = 20;
/// 前台退格清空按键发送后的短暂等待时长（毫秒）
pub(crate) const POST_BACKSPACE_DELAY_MS: u64 = 25;
/// 前台注入 `Shift+Enter` 换行组合键后等待富文本编辑器完成换行的时长（毫秒）
pub(crate) const NEWLINE_CHORD_SETTLE_MS: u64 = 20;
/// 离线截图首次检测到纯单色空白帧后的短暂重试等待时长（毫秒）
pub(crate) const TRANSIENT_BLANK_CAPTURE_RETRY_MS: u64 = 80;
/// 事件钩子初始时间偏移基数，用于避免首条事件与零值哨兵冲突（毫秒）
pub(crate) const EVENT_HOOK_DEBOUNCE_OFFSET_MS: u64 = 100;

/// 未知控件类型统一回退名称
pub const CONTROL_TYPE_UNKNOWN: &str = "Unknown";
/// 文本输入框控件类型名称
pub const CONTROL_TYPE_EDIT: &str = "Edit";
/// 只读静态文本控件类型名称
pub const CONTROL_TYPE_TEXT: &str = "Text";
/// 按钮控件类型名称
pub const CONTROL_TYPE_BUTTON: &str = "Button";
/// 富文本可编辑分组控件类型名称
pub const CONTROL_TYPE_GROUP: &str = "Group";
/// 文档容器控件类型名称
pub const CONTROL_TYPE_DOCUMENT: &str = "Document";
/// 组合下拉框控件类型名称
pub const CONTROL_TYPE_COMBO_BOX: &str = "ComboBox";
/// 列表子项控件类型名称
pub const CONTROL_TYPE_LIST_ITEM: &str = "ListItem";
/// 列表容器控件类型名称
pub const CONTROL_TYPE_LIST: &str = "List";
/// 弹出菜单控件类型名称
pub const CONTROL_TYPE_MENU: &str = "Menu";
/// 菜单项控件类型名称
pub const CONTROL_TYPE_MENU_ITEM: &str = "MenuItem";
/// 选项卡子项控件类型名称
pub const CONTROL_TYPE_TAB_ITEM: &str = "TabItem";
/// 选项卡容器控件类型名称
pub const CONTROL_TYPE_TAB: &str = "Tab";
/// 树形列表容器控件类型名称
pub const CONTROL_TYPE_TREE: &str = "Tree";
/// 树形列表子项控件类型名称
pub const CONTROL_TYPE_TREE_ITEM: &str = "TreeItem";
/// 复选框控件类型名称
pub const CONTROL_TYPE_CHECK_BOX: &str = "CheckBox";
/// 单选按钮控件类型名称
pub const CONTROL_TYPE_RADIO_BUTTON: &str = "RadioButton";
/// 超链接控件类型名称
pub const CONTROL_TYPE_LINK: &str = "Link";
/// 图像控件类型名称
pub const CONTROL_TYPE_IMAGE: &str = "Image";
/// 对话框控件类型名称
pub const CONTROL_TYPE_DIALOG: &str = "Dialog";
/// 滚动条控件类型名称
pub const CONTROL_TYPE_SCROLL_BAR: &str = "ScrollBar";
/// 进度条控件类型名称
pub const CONTROL_TYPE_PROGRESS_BAR: &str = "ProgressBar";
/// 滑块控件类型名称
pub const CONTROL_TYPE_SLIDER: &str = "Slider";
/// 微调数字框控件类型名称
pub const CONTROL_TYPE_SPINNER: &str = "Spinner";
/// 表格控件类型名称
pub const CONTROL_TYPE_TABLE: &str = "Table";
/// 工具栏控件类型名称
pub const CONTROL_TYPE_TOOL_BAR: &str = "ToolBar";
/// 提示气泡控件类型名称
pub const CONTROL_TYPE_TOOL_TIP: &str = "ToolTip";
/// 分割线控件类型名称
pub const CONTROL_TYPE_SEPARATOR: &str = "Separator";
/// 通用容器面板控件类型名称
pub const CONTROL_TYPE_PANE: &str = "Pane";
/// 顶层或子窗口控件类型名称
pub const CONTROL_TYPE_WINDOW: &str = "Window";
/// 窗口标题栏控件类型名称
pub const CONTROL_TYPE_TITLE_BAR: &str = "TitleBar";
/// 窗口菜单栏控件类型名称
pub const CONTROL_TYPE_MENU_BAR: &str = "MenuBar";
/// 滑块拖拽手柄控件类型名称
pub const CONTROL_TYPE_THUMB: &str = "Thumb";
/// 底部状态栏控件类型名称
pub const CONTROL_TYPE_STATUS_BAR: &str = "StatusBar";
/// 列表表头项控件类型名称
pub const CONTROL_TYPE_HEADER_ITEM: &str = "HeaderItem";
/// 数据表格单元项控件类型名称
pub const CONTROL_TYPE_DATA_ITEM: &str = "DataItem";

/// 不计入语义内容控件的纯容器与窗口装饰类型集合
pub(crate) const NON_SEMANTIC_CONTROL_TYPES: &[&str] = &[
    CONTROL_TYPE_PANE,
    CONTROL_TYPE_WINDOW,
    CONTROL_TYPE_TITLE_BAR,
    CONTROL_TYPE_MENU_BAR,
];

/// UIA 调用模式名称标识
pub(crate) const PATTERN_INVOKE: &str = "Invoke";
/// UIA 值读写模式名称标识
pub(crate) const PATTERN_VALUE: &str = "Value";
/// UIA 选择项模式名称标识
pub(crate) const PATTERN_SELECTION_ITEM: &str = "SelectionItem";
/// UIA 传统无障碍接口模式名称标识
pub(crate) const PATTERN_LEGACY_IACCESSIBLE: &str = "LegacyIAccessible";
/// UIA 文本读取模式名称标识
pub(crate) const PATTERN_TEXT: &str = "Text";
/// UIA 滚动控制模式名称标识
pub(crate) const PATTERN_SCROLL: &str = "Scroll";

/// Windows 输入法语音提示前缀文本（等价于空输入框）
pub(crate) const VOICE_INPUT_PLACEHOLDER_PREFIX: &str = "按住 Win + Alt";

/// 通过 UIA `InvokePattern` 完成后台点击的审计标识
pub(crate) const ACTION_METHOD_UIA_INVOKE: &str = "uia_invoke";
/// 通过 UIA `SelectionItemPattern` 完成后台选中的审计标识
pub(crate) const ACTION_METHOD_UIA_SELECT: &str = "uia_select";
/// 通过 UIA `LegacyIAccessiblePattern` 完成后台触发的审计标识
pub(crate) const ACTION_METHOD_UIA_LEGACY_DEFAULT: &str = "uia_legacy_default";
/// 通过 MSAA `accDoDefaultAction` 完成后台触发的审计标识
pub(crate) const ACTION_METHOD_MSAA_DEFAULT: &str = "msaa_default_action";
/// 通过 MSAA `accSelect` 完成后台选中的审计标识
pub(crate) const ACTION_METHOD_MSAA_SELECT: &str = "msaa_select";

/// IAccessible2 核心接口扩展 GUID
pub(crate) const IID_IACCESSIBLE2: GUID = GUID::from_u128(0xe89f726e_c4f4_4c19_bb19_b647d7fa8478);
/// IAccessibleText 扩展文本接口 GUID
pub(crate) const IID_IACCESSIBLE_TEXT: GUID =
    GUID::from_u128(0x24fd2ffb_3aad_4a08_8335_a3ad89c0fb4b);
/// Gecko/Chromium ISimpleDOMNode 无障碍接口 GUID
pub(crate) const IID_ISIMPLE_DOM_NODE: GUID =
    GUID::from_u128(0x0c539790_12e4_11cf_b661_00aa004cd6d8);
