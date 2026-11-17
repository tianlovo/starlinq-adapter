> 公开副本说明：示例身份与路径已替换，历史实机原始证据不公开；本文件的历史状态不代表公开版本或新电脑已完成真机验证。参阅 [公开版本说明](../PUBLIC_RELEASE_NOTES.md)。

# qq-uia-adapter 真实 QQ 客户端实机核验报告（模板）

> 本模板供 GPT 在宿主机真实 Windows `Default` 桌面上完成 `PROMPT_FOR_GPT.md` 核验后严格填充并输出至 `docs/gpt-live-verification/GPT_LIVE_VERIFICATION_REPORT.md`。
> **核验原则**：所有结论必须附带真实 QQNT 客户端可观测证据；未在真实客户端完成闭环验证的项目**必须标注 `CODE_ONLY_UNVERIFIED_LIVE`**，严禁将代码/单测通过等同于实机可用。

---

## 1. 核验环境与基线元数据

| 核验元数据项 | 实测观测值（请如实填写） |
| :--- | :--- |
| **核验执行时间** | `YYYY-MM-DD HH:MM:SS +08:00` |
| **核验人 / 执行模型** | `GPT（真实宿主桌面环境）` |
| **宿主机桌面 / 沙箱状态** | `desktop_name = ...` / `sandbox_isolated = ...` |
| **适配器服务地址与初始→结束 PID** | `http://127.0.0.1:18770` / `PID <initial> -> <final>` |
| **真实 `QQ.exe` PID / 主窗口句柄 / 标题** | `...` |
| **登录账号（`user_id` / `real_qq_id` / 昵称）** | `...` |
| **测试群（群名 / 真实群号 / 成员数）** | `Demo测试群` / `100000001` / `.../10` |
| **测试白名单好友** | `测试好友A` |
| **核验前后剪贴板守卫恢复测试** | `已复原 / 异常（附细节）` |
| **安全熔断是否触发 / `%USERPROFILE%\.cargo` 污染** | `security_breaker_tripped = false` / `False` |

---

## 2. 核验结论总览统计

| 状态类别 | 数量 | 占比 | 含义说明 |
| :--- | :---: | :---: | :--- |
| **`LIVE_PASS`** | `0` | `0%` | 真实 QQ 客户端端到端实测通过且有 UI 树/气泡/DB/磁盘实证 |
| **`LIVE_FAIL`** | `0` | `0%` | 真实 QQ 客户端实测失败、行为异常、乱码或违反无鼠标/焦点守护原则 |
| **`DEGRADED_BY_DESIGN`** | `0` | `0%` | 真实客户端行为已实测，但属桌面 UIA 设计降级形态（如可读文本摘要） |
| **`CODE_ONLY_UNVERIFIED_LIVE`** | `0` | `0%` | **代码/单测已实现但真实 QQ 客户端效果未获完整实证** |
| **`BOUNDARY_10004`** | `0` | `0%` | 显式禁止/边界动作，实测严格返回 `501` / `10004` 且零副作用 |
| **合计核验项** | `0` | `100%` | — |

---

## 3. 分模块逐项核验明细

### 3.1 连接与传输通道（HTTP / 正反向 WS / Webhook / 原生流）

| 功能项 | 核验状态 | 实测请求与参数摘要 | 真实端侧观测证据（UI / 日志 / 响应） | 备注或未实证原因 |
| :--- | :---: | :--- | :--- | :--- |
| OneBot HTTP (`/onebot/v11/{action}` & 根路径 `/{action}`) | | | | |
| HTTP GET / POST JSON / Form-UrlEncoded & GET 写拦截(`405`) | | | | |
| HTTP 鉴权（未开 token 默认态 vs 开启 `access_token` 拦截） | | | | |
| Forward WS Universal (`/onebot/v11/ws` & `/ws`, `lifecycle` + 实测 `heartbeat`) | | | | |
| Forward WS ApiOnly (`/onebot/v11/ws/api`) | | | | |
| Forward WS EventOnly (`/onebot/v11/ws/event` + 写动作 `10004` 拦截) | | | | |
| Reverse WS 三角色 (`reverse_ws_urls` / `api` / `event` + 握手头 + 重连) | | | | |
| HTTP POST Webhook (`http_post_urls` + `X-Signature: sha1=` + 快速操作) | | | | |
| 原生 `/ws/events` 实时事件流 | | | | |

### 3.2 OneBot v11 支持动作（25 项）及修饰后缀

| # | 动作 / 特性 | 核验状态 | 实测方法与参数 | 真实 QQ 客户端端侧观测证据 | 备注或未实证原因 |
| :---: | :--- | :---: | :--- | :--- | :--- |
| 1 | `get_status` | | | | |
| 2 | `get_login_info` | | | | |
| 3 | `get_version_info` | | | | |
| 4 | `send_group_msg` | | | | |
| 5 | `send_private_msg`（含白名单放行与未授权 `403/1403` 拦截） | | | | |
| 6 | `send_msg` | | | | |
| 7 | `get_msg` | | | | |
| 8 | `get_group_msg_history` | | | | |
| 9 | `get_friend_msg_history` | | | | |
| 10 | `send_group_forward_msg` | | | | |
| 11 | `send_private_forward_msg` | | | | |
| 12 | `send_forward_msg` | | | | |
| 13 | `get_forward_msg` | | | | |
| 14 | `get_friend_list` | | | | |
| 15 | `get_stranger_info` | | | | |
| 16 | `get_group_list`（含真实群号提取） | | | | |
| 17 | `get_group_info`（含双通道群号探测与容量） | | | | |
| 18 | `get_group_member_list` | | | | |
| 19 | `get_group_member_info` | | | | |
| 20 | `get_image`（含合法文件与 `../` 越界拒绝） | | | | |
| 21 | `can_send_image` | | | | |
| 22 | `can_send_record` | | | | |
| 23 | `clean_cache` | | | | |
| 24 | `set_restart` | | | | |
| 25 | `.handle_quick_operation`（回复与 `delete`→`10004`） | | | | |
| S1 | 后缀 `_async`（`retcode=1` + 实际异步上屏） | | | | |
| S2 | 后缀 `_rate_limited` & `_async_rate_limited` | | | | |

### 3.3 明确不支持的边界动作（19 项，预期 `501` / `10004`）

| 类别 | 动作范围 | 核验状态 | 实测抽检 HTTP / retcode | 真实 QQ 是否零副作用 |
| :--- | :--- | :---: | :--- | :--- |
| 消息与媒体底层流 | `delete_msg`, `get_record` | | | |
| 资料点赞与网页凭证 | `send_like`, `get_cookies`, `get_csrf_token`, `get_credentials` | | | |
| 高危群管理写操作 | `set_group_kick`, `set_group_ban`, `set_group_anonymous_ban`, `set_group_whole_ban`, `set_group_admin`, `set_group_anonymous`, `set_group_card`, `set_group_name`, `set_group_leave`, `set_group_special_title`, `set_group_portrait` | | | |
| 加好友/加群审批 | `set_friend_add_request`, `set_group_add_request` | | | |

### 3.4 21 种消息段（Message Segments）端侧出站 & 入站真实表现

| 序号 | 段类型 | 出站核验状态 | 出站真实 QQ 聊天窗呈现效果 | 入站核验状态 | 入站识别与落库说明 |
| :---: | :--- | :---: | :--- | :---: | :--- |
| 1 | `text`（含中文/Emoji/多行） | | | | |
| 2 | `face`（系统表情 `[CQ:face,id=...]`） | | | | |
| 3 | `image`（`base64://` / `data:` / `file://` / 闪照 / `PrintWindow` 截切） | | | | |
| 4 | `record`（语音） | | | | |
| 5 | `video`（视频） | | | | |
| 6 | `at`（`@成员` / `@all`） | | | | |
| 7 | `rps`（猜拳） | | | | |
| 8 | `dice`（骰子） | | | | |
| 9 | `shake`（窗口抖动） | | | | |
| 10 | `poke`（戳一戳） | | | | |
| 11 | `anonymous`（匿名） | | | | |
| 12 | `share`（链接分享） | | | | |
| 13 | `contact`（推荐好友/群名片） | | | | |
| 14 | `location`（位置） | | | | |
| 15 | `music`（音乐卡片） | | | | |
| 16 | `reply`（引用回复） | | | | |
| 17 | `forward`（合并转发段） | | | | |
| 18 | `node`（合并转发节点） | | | | |
| 19 | `xml`（XML 卡片） | | | | |
| 20 | `json`（JSON 卡片） | | | | |
| 21 | `file`（文件） | | | | |

### 3.5 事件上报与 6 类系统提示横幅（Notice）

| 事件 / Notice 类型 | 核验状态 | 实测情况或缺少的前置条件 | 证据 / 未实证说明 |
| :--- | :---: | :--- | :--- |
| `meta_event.lifecycle.connect` | | | |
| `meta_event.heartbeat` | | | |
| `message.group`（他人入站实时推送 + 防自回显） | | | |
| `message.private`（白名单好友入站未读监测与推送） | | | |
| `message_sent`（本端已发消息上报） | | | |
| `notice.group_recall` / `friend_recall` | | | |
| `notice.notify.poke`（拍一拍） | | | |
| `notice.group_increase`（邀人/入群） | | | |
| `notice.group_decrease`（踢出/退群） | | | |
| `notice.group_ban`（禁言/解禁与时长） | | | |
| `notice.notify.system_tip`（通用提示兜底） | | | |
| `request`（加友/加群请求事件透传） | | | |

### 3.6 原生 REST/WS 接口、CLI 工具与 8 重安全护栏

| 检查项目 | 核验状态 | 端侧观测证据 | 备注 |
| :--- | :---: | :--- | :--- |
| 原生 REST 诊断 (`/api/health`, `/status`, `/windows`, `/diag`, `/tree`, `/ensure-qq`) | | | |
| 原生会话/通讯录/存储 (`/api/sessions`, `/messages`, `/friends`, `/groups`, `/group-info`, `/group-members`, `/storage*`) | | | |
| CLI 只读命令 & `verify-group --below-members 10` | | | |
| CLI / API 热重启 `restart`（PID 轮换 & 桌面无损恢复） | | | |
| 八重安全护栏 1–2：真实 `QQ.exe` 身份绑定 & 全程 `mouse_touched == false` | | | |
| 八重安全护栏 3–4：发前人数(`<10`)/标题护栏 & 未触碰登录/安全窗口 | | | |
| 八重安全护栏 5–8：`WinEventWatcher`、SQLite WAL、私聊白名单与剪贴板 RAII 恢复 | | | |

---

## 4. 专章一：代码已实现但真实 QQ 客户端效果未能核验项（`CODE_ONLY_UNVERIFIED_LIVE`）

> **强制要求**：凡无法在真实 QQNT 客户端形成 100% 端侧动作→观测闭环的能力，必须全部汇总于此，并明确给后续人工/双账号联调提供触发前提。

1. `<功能名称>` — 未能核验原因：`<如：单机单账号无法构造对端实时发信/撤回/拍一拍/禁言事件>` — 补测建议：`...`

---

## 5. 专章二：桌面端设计降级项实测表现（`DEGRADED_BY_DESIGN`）

> 如实说明在真实 QQ 聊天窗口中的实际可见呈现与 OneBot 理想原生效果的差异。

1. `<功能/消息段>` — 真实 QQ 聊天窗口实际显示内容：`...` — 是否符合预期降级说明：`...`

---

## 6. 专章三：真实客户端发现的缺陷、偏差与修复建议（`LIVE_FAIL` / 瑕疵记录）

> 每条缺陷请至少包含：`缺陷编号`、`对应功能`、`复现步骤`、`实际表现与错误响应`、`预期表现`、`怀疑代码文件或模块`。若本次核验发现 0 个 `LIVE_FAIL`，请明确写明并列出体验层面的优化观察点。

1. **`<LIVE-BUG-001>`（严重级：Critical / High / Medium / Low）**
   - **影响功能**：`...`
   - **复现步骤**：`1. ... 2. ...`
   - **实际表现 / 原始响应 / UI 树证据**：`...`
   - **预期表现**：`...`
   - **定位线索**：`...`

---

## 7. 总结与可立即上线 / 需补测范围建议

- **可直接信任投产的核心链路**：`...`
- **仍需双账号/真实互动补测的链路**：`...`
- **优先修复建议（按优先级排序）**：`...`
