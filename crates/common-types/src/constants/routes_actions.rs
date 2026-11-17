//! 原生 REST 路由、OneBot v11 路由、动作名称与反向 WebSocket 请求头常量

/// 批量生成共享字符串常量，避免重复声明样板代码
macro_rules! define_string_constants {
    ($( $(#[$meta:meta])* $name:ident = $value:expr; )*) => {
        $(
            $(#[$meta])*
            pub const $name: &str = $value;
        )*
    };
}

define_string_constants! {
    /// 健康检查接口路径
    ROUTE_API_HEALTH = "/api/health";
    /// 驱动状态查询接口路径
    ROUTE_API_STATUS = "/api/status";
    /// 候选窗口列表接口路径
    ROUTE_API_WINDOWS = "/api/windows";
    /// 唤醒或拉起桌面 QQ 接口路径
    ROUTE_API_ENSURE_QQ = "/api/ensure-qq";
    /// 窗口与无障碍诊断接口路径
    ROUTE_API_DIAG = "/api/diag";
    /// 界面元素树抓取接口路径
    ROUTE_API_TREE = "/api/tree";
    /// 左侧会话列表接口路径
    ROUTE_API_SESSIONS = "/api/sessions";
    /// 撤回指定消息接口路径
    ROUTE_API_DELETE_MSG = "/api/delete-msg";
    /// 聊天消息查询接口路径
    ROUTE_API_MESSAGES = "/api/messages";
    /// 离线 SQLite 历史消息查询接口路径
    ROUTE_API_MESSAGES_HISTORY = "/api/messages/history";
    /// 切换并打开指定群聊接口路径
    ROUTE_API_OPEN_GROUP = "/api/open-group";
    /// 切换并打开指定好友私聊接口路径
    ROUTE_API_OPEN_PRIVATE = "/api/open-private";
    /// 发送群消息接口路径
    ROUTE_API_SEND = "/api/send";
    /// 热重启后台守护进程接口路径
    ROUTE_API_RESTART = "/api/restart";
    /// 优雅关闭服务接口路径
    ROUTE_API_SHUTDOWN = "/api/shutdown";
    /// 已缓存群列表查询接口路径
    ROUTE_API_GROUPS = "/api/groups";
    /// 已缓存好友列表查询接口路径
    ROUTE_API_FRIENDS = "/api/friends";
    /// 单群详情查询接口路径
    ROUTE_API_GROUP_INFO = "/api/group-info";
    /// 群成员列表查询接口路径
    ROUTE_API_GROUP_MEMBERS = "/api/group-members";
    /// 存储统计信息查询接口路径
    ROUTE_API_STORAGE_STATS = "/api/storage/stats";
    /// 洪峰调度器与响应窗口遥测状态查询接口路径
    ROUTE_API_SCHEDULER = "/api/scheduler";
    /// 洪峰调度器运行策略动态查询与热更新接口路径
    ROUTE_API_SCHEDULER_POLICY = "/api/scheduler/policy";
    /// 会话未读红点同步标记已读接口路径
    ROUTE_API_SESSIONS_READ = "/api/sessions/read";
    /// 图片本地 OCR 识别接口路径
    ROUTE_API_OCR = "/api/ocr";
    /// 单条历史消息原生转发接口路径
    ROUTE_API_FORWARD_MSG = "/api/forward-msg";
    /// 审计事件流水检索接口路径
    ROUTE_API_AUDIT_EVENTS = "/api/audit-events";
    /// 安全验证熔断器手动重置接口路径
    ROUTE_API_SECURITY_RESET = "/api/security/reset";
    /// 本地媒体与历史缓存清理接口路径
    ROUTE_API_CACHE_CLEAN = "/api/cache/clean";
    /// 原生事件流 WebSocket 路径
    ROUTE_WS_EVENTS = "/ws/events";

    /// OneBot v11 状态查询路径
    ROUTE_ONEBOT_GET_STATUS = "/onebot/v11/get_status";
    /// OneBot v11 登录信息查询路径
    ROUTE_ONEBOT_GET_LOGIN_INFO = "/onebot/v11/get_login_info";
    /// OneBot v11 发送群消息路径
    ROUTE_ONEBOT_SEND_GROUP_MSG = "/onebot/v11/send_group_msg";
    /// OneBot v11 发送私聊消息路径
    ROUTE_ONEBOT_SEND_PRIVATE_MSG = "/onebot/v11/send_private_msg";
    /// OneBot v11 通用发送消息路径
    ROUTE_ONEBOT_SEND_MSG = "/onebot/v11/send_msg";
    /// OneBot v11 好友列表查询路径
    ROUTE_ONEBOT_GET_FRIEND_LIST = "/onebot/v11/get_friend_list";
    /// OneBot v11 群列表查询路径
    ROUTE_ONEBOT_GET_GROUP_LIST = "/onebot/v11/get_group_list";
    /// OneBot v11 单群信息查询路径
    ROUTE_ONEBOT_GET_GROUP_INFO = "/onebot/v11/get_group_info";
    /// OneBot v11 群成员列表查询路径
    ROUTE_ONEBOT_GET_GROUP_MEMBER_LIST = "/onebot/v11/get_group_member_list";
    /// OneBot v11 单群成员信息查询路径
    ROUTE_ONEBOT_GET_GROUP_MEMBER_INFO = "/onebot/v11/get_group_member_info";
    /// OneBot v11 单条消息查询路径
    ROUTE_ONEBOT_GET_MSG = "/onebot/v11/get_msg";
    /// OneBot v11 正向 WebSocket 入口路径
    ROUTE_ONEBOT_WS = "/onebot/v11/ws";

    /// OneBot 动作：获取运行状态
    ACTION_GET_STATUS = "get_status";
    /// OneBot 动作：获取登录号信息
    ACTION_GET_LOGIN_INFO = "get_login_info";
    /// OneBot 动作：发送群消息
    ACTION_SEND_GROUP_MSG = "send_group_msg";
    /// OneBot 动作：发送私聊消息
    ACTION_SEND_PRIVATE_MSG = "send_private_msg";
    /// OneBot 动作：发送消息
    ACTION_SEND_MSG = "send_msg";
    /// OneBot 动作：获取好友列表
    ACTION_GET_FRIEND_LIST = "get_friend_list";
    /// OneBot 动作：获取群列表
    ACTION_GET_GROUP_LIST = "get_group_list";
    /// OneBot 动作：获取群信息
    ACTION_GET_GROUP_INFO = "get_group_info";
    /// OneBot 动作：获取群成员列表
    ACTION_GET_GROUP_MEMBER_LIST = "get_group_member_list";
    /// OneBot 动作：获取群成员信息
    ACTION_GET_GROUP_MEMBER_INFO = "get_group_member_info";
    /// OneBot 动作：获取历史消息
    ACTION_GET_MSG = "get_msg";
    /// OneBot 动作：按消息编号标记会话已读
    ACTION_MARK_MSG_AS_READ = "mark_msg_as_read";
    /// OneBot 扩展动作：按群号标记群聊消息已读
    ACTION_MARK_GROUP_MSG_AS_READ = "mark_group_msg_as_read";
    /// OneBot 扩展动作：按用户号标记私聊消息已读
    ACTION_MARK_PRIVATE_MSG_AS_READ = "mark_private_msg_as_read";
    /// OneBot 扩展动作：获取文本分词切片
    ACTION_GET_WORD_SLICES = ".get_word_slices";
    /// OneBot 扩展动作：获取洪峰调度器状态快照
    ACTION_GET_SCHEDULER_STATUS = ".get_scheduler_status";
    /// OneBot 扩展动作：图片 OCR 文字识别
    ACTION_OCR_IMAGE = "ocr_image";
    /// OneBot 扩展动作：点号前缀图片 OCR 文字识别
    ACTION_DOT_OCR_IMAGE = ".ocr_image";
    /// OneBot 动作：检查是否支持发送视频
    ACTION_CAN_SEND_VIDEO = "can_send_video";
    /// OneBot 扩展动作：上传群文件
    ACTION_UPLOAD_GROUP_FILE = "upload_group_file";
    /// OneBot 扩展动作：上传私聊文件
    ACTION_UPLOAD_PRIVATE_FILE = "upload_private_file";
    /// OneBot 扩展动作：获取文件下载或本地路径信息
    ACTION_GET_FILE = "get_file";
    /// OneBot 扩展动作：单条消息转发至群聊
    ACTION_FORWARD_GROUP_SINGLE_MSG = "forward_group_single_msg";
    /// OneBot 扩展动作：单条消息转发至好友私聊
    ACTION_FORWARD_FRIEND_SINGLE_MSG = "forward_friend_single_msg";
    /// OneBot 扩展动作：获取最近联系人会话列表
    ACTION_GET_RECENT_CONTACT = "get_recent_contact";
    /// OneBot 扩展动作：查询持久化审计事件流水
    ACTION_GET_AUDIT_EVENTS = ".get_audit_events";
    /// OneBot 扩展动作：手动重置安全验证熔断状态
    ACTION_RESET_SECURITY_BREAKER = ".reset_security_breaker";

    /// OneBot 私聊消息类型字段值
    ONEBOT_MESSAGE_TYPE_PRIVATE = "private";
    /// OneBot 好友私聊消息子类型字段值
    ONEBOT_SUB_TYPE_FRIEND = "friend";
    /// OneBot 引用回复消息段类型名
    CQ_SEGMENT_REPLY = "reply";
    /// OneBot 图片消息段类型名
    CQ_SEGMENT_IMAGE = "image";

    /// HTTP 与 WebSocket 鉴权请求头名称
    HEADER_AUTHORIZATION = "Authorization";
    /// OneBot 反向 WebSocket 自身账号标识头
    HEADER_X_SELF_ID = "X-Self-ID";
    /// OneBot 反向 WebSocket 客户端角色头
    HEADER_X_CLIENT_ROLE = "X-Client-Role";
    /// OneBot 反向 WebSocket 通用客户端角色值
    HEADER_X_CLIENT_ROLE_UNIVERSAL = "Universal";
    /// Bearer 令牌前缀
    BEARER_PREFIX = "Bearer ";
}
