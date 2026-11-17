//! QQNT 9.9.31 简体中文界面标签与安全风控关键词常量

/// 左侧会话列表容器节点名称
pub const UI_LABEL_SESSION_LIST: &str = "会话列表";
/// 右侧聊天消息历史容器节点名称
pub const UI_LABEL_MESSAGE_LIST: &str = "消息列表";
/// 群聊成员列表容器节点名称
pub const UI_LABEL_GROUP_MEMBER_LIST: &str = "群成员列表";
/// 聊天发送按钮默认显示文本
pub const UI_LABEL_SEND_BUTTON: &str = "发送";
/// 聊天标题栏右侧扩展抽屉触发按钮标签
pub const UI_LABEL_MORE_BUTTON: &str = "更多";
/// QQNT 群聊及个人资料浮动弹窗标准标题文本
pub const UI_LABEL_PROFILE_CARD: &str = "资料卡";
/// 资料卡或抽屉中群号/账号旁的复制按钮文本
pub const UI_LABEL_COPY_BUTTON: &str = "复制";
/// 弹出窗口与抽屉面板中常见的关闭按钮文本
pub const UI_LABEL_CLOSE_BUTTON: &str = "关闭";
/// 默认受限测试目标群名称
pub const DEFAULT_TARGET_GROUP_NAME: &str = "Demo测试群";
/// 内部模拟或默认识别的机器人自身名称
pub const DEFAULT_BOT_SELF_NAME: &str = "BotSelf";
/// 默认用于自回显过滤的机器人昵称候选列表
pub const DEFAULT_BOT_NICKNAME_CANDIDATES: &[&str] = &["BotSelf", "测试机器人"];
/// 测试账号关联的常见群昵称标识
pub const UI_LABEL_SANDRONE: &str = "测试机器人";
/// 右侧群聊成员抽屉与列表标题文本
pub const UI_LABEL_GROUP_CHAT_MEMBER: &str = "群聊成员";
/// 群主角色界面标签
pub const UI_ROLE_OWNER_LABEL: &str = "群主";
/// 管理员角色界面标签
pub const UI_ROLE_ADMIN_LABEL: &str = "管理员";
/// 头像无障碍名称常见后缀
pub const UI_AVATAR_SUFFIX: &str = "的头像";
/// 界面噪声节点占位名称
pub const UI_NOISE_UNKNOWN: &str = "Unknown";
/// 群等级气泡常见前缀
pub const UI_NOISE_LEVEL_PREFIX: &str = "LV";
/// 窗口标题栏控制区域干扰节点名称
pub const UI_LABEL_WINDOW_CONTROL_AREA: &str = "窗口控制区域";
/// 图片消息在纯文本会话流与 UI 输入框中的统一占位标记
pub const IMAGE_PLACEHOLDER_TOKEN: &str = "[图片]";

/// 触发安全熔断的高风险弹窗与提示关键词集合
pub const SECURITY_KEYWORDS: &[&str] = &["安全验证", "滑块", "验证码", "登录保护", "设备锁"];
