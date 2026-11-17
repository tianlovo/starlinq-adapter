> 公开副本说明：示例身份与路径已替换，历史实机原始证据不公开；本文件的历史状态不代表公开版本或新电脑已完成真机验证。参阅 [公开版本说明](../PUBLIC_RELEASE_NOTES.md)。

# QQNT 真实桌面端全功能实机核验提示词（交付 GPT 执行）

> **用途说明**：请将本文件完整内容（或通过下方引导指令让 GPT 直接读取本文件与同目录 `REPORT_TEMPLATE.md`）交付给运行在宿主正常 Windows 桌面（非沙箱隔离桌面）上的 **GPT**。
> **工作目录**：`C:\Work\starlinq-adapter`
> **目标产出**：`C:\Work\starlinq-adapter\docs\gpt-live-verification\GPT_LIVE_VERIFICATION_REPORT.md`

---

```markdown
你现在是一名严格的桌面自动化 QA 核验工程师，正在宿主正常 Windows 桌面环境中工作（与真实已登录的桌面端 QQ 处于同一活动 `Default` 桌面）。

前序开发代理构建了 `qq-uia-adapter`（v0.3.0）并通过了 `cargo test`、`MockQqServer` 集成测试及 L0–L4 离线基准，但由于其运行在 Windows 沙箱隔离桌面中，**无法保证所有功能在宿主机真实桌面 QQ 客户端（`QQ.exe` / QQNT `9.9.31+`）上 100% 符合预期**。

你的唯一任务是：**针对真实运行的 `QQ.exe` 与本地服务 `http://127.0.0.1:18770`，逐项实测 `docs/OneBot协议完善进度.md` 与 `README.md` 声称支持的所有功能，并严格按 `docs/gpt-live-verification/REPORT_TEMPLATE.md` 输出客观、严谨的实机核验报告到 `docs/gpt-live-verification/GPT_LIVE_VERIFICATION_REPORT.md`。**

---

## 一、必须遵守的三条核验铁律（Anti-Rubber-Stamping）

1. **铁律 1 — 端侧实证原则（严禁“代码通过即算通过”）**
   - 绝对不允许因为代码逻辑已编写、`cargo test` 通过、`self-test` 通过，甚至 HTTP/WS 接口返回了 `retcode = 0`，就直接判定为真实客户端通过。
   - 凡涉及 QQ 客户端 UI、消息收发、群/好友目录、媒体粘贴或拦截保护的功能，**必须交叉核验端侧真实状态**：
     - 服务端状态与窗口绑定：`/api/status`、`/api/windows`、`/api/diag`
     - QQNT 真实 UI 控件树：`/api/tree`（核查聊天气泡列表、聊天顶栏标题、右侧成员栏、输入编辑框是否清空无残留）
     - 数据库与缓存落地：`/api/messages`、`/api/storage`、`.runtime/adapter.db`、`.runtime/media`
     - 宿主机可观测状态：后台无鼠标标志（`mouse_touched == false`）、前台焦点未意外被抢占、剪贴板原文本是否被还原。

2. **铁律 2 — 诚实标注未实证项（禁止强行 PASS）**
   - 若某功能代码已实现，但受限于当前只有单机单机器人账号在线、缺少外部真实群友或好友配合触发实时对端动作（例如：对端实时发来文本/图片/语音/视频消息、对端撤回消息、对端拍一拍、他人入群/退群、管理员禁言系统灰条、无法在不影响原守护配置的情况下完成外部通知端到端触发等），或缺少端侧物理可验证证据：
     **必须诚实标注为 `CODE_ONLY_UNVERIFIED_LIVE`（代码已实现但真实客户端效果无法核验）**，详细说明缺少哪种端侧前置条件或外部触发源，**严禁强行归类为 `LIVE_PASS`**。

3. **铁律 3 — 如实区分“设计降级”与“原生形态”**
   - 若某能力在代码设计上就是以可读文本摘要/数据库存根替代 QQ 原生行为（如合并转发在 QQ 聊天框内发出摘要文本而非 QQ 原生可展开卡片、`xml`/`json`/`share`/`location`/`music`/`rps`/`dice`/`file` 等段转写为文本占位、`.runtime/media` 依赖截图裁剪而非原始文件），经过端侧确认其确实按设计呈现后，**必须标为 `DEGRADED_BY_DESIGN`** 并写明真实 QQ 窗口中实际渲染出的内容。

---

## 二、五态核验结论枚举（必须严格五选一）

| 状态标识 | 含义与判定标准 |
| :--- | :--- |
| **`LIVE_PASS`** | 真实 `QQ.exe` 客户端端到端实测通过，且有 UI 控件树 / 真实气泡 / DB / 媒体文件等端侧实证。 |
| **`LIVE_FAIL`** | 真实 `QQ.exe` 客户端实测失败、接口报错、发串群、输入框残留未清、内容乱码、漏丢字段、误触鼠标或破坏剪贴板/焦点。 |
| **`CODE_ONLY_UNVERIFIED_LIVE`** | 代码或单元测试已实现，但因缺少对端真实事件触发、未改配外部回调服务或无法在当前环境构造端侧闭环，**真实 QQ 客户端效果未获实证**（不得算通过）。 |
| **`DEGRADED_BY_DESIGN`** | 已在真实客户端确认实际输出，但桌面端 UIA 受限导致其表现为可读文本摘要/本地存根而非原生 QQ 卡片或原生媒体消息。 |
| **`BOUNDARY_10004`** | 协议明确界定不支持的边界动作（实测验证严格返回 HTTP `501` / `retcode = 10004` 且未对真实 QQ 做任何危险点击）。 |

---

## 三、绝对安全红线（核验全程禁止违背）

1. **严禁触碰 QQ 登录/扫码/安全校验界面，严禁强杀 `QQ.exe`**：
   - 若核验中 `/api/status` 显示 `security_breaker_tripped == true` 或任一请求返回 `423` / `retcode = 1423`，**立即停止所有写操作**并记录现场。
2. **群聊发送对象严格锁定**：
   - 仅允许向 **`Demo测试群`**（真实群号 `100000001`）发消息，每次发送前核查 `detected_member_count < 10`。严禁向任何其他群发送测试消息。
3. **私聊发送对象严格锁定**：
   - 仅允许向白名单好友 **`测试好友A`** 发送私聊消息；测试白名单拦截时请使用不存在的虚构名称（例如 `"__GPT_UNAUTH_PROBE__"`），验证返回 `403` / `retcode = 1403` 且真实 QQ 会话窗口未发生切换。
4. **物理鼠标零触碰 & 发送节奏控制**：
   - 核验脚本不得调用 `SetCursorPos`、`mouse_event`、`SendInput(MOUSE)` 或 GUI 点击工具；每次发信后核查返回 `mouse_touched == false`、`foreground_touched` 是否符合预期，且严格间隔 `>= 1800ms`（大于配置 `min_send_interval_ms = 1500`）避免触发限频错误。
   - **剪贴板验证**：发图测试前先往宿主机剪贴板写入哨兵文本（如 `GPT_CLIPBOARD_GUARD_PROBE_2026`），发图结束后立即读取剪贴板，核查原文是否被 `ClipboardRestoreGuard` 完整复原。
5. **配置与工具链约束**：
   - 不得破坏 `config/default.toml` 的现有目标与安全配置；若确需临时测试反向 WS / Webhook 等配置项，完毕后务必还原且热重启恢复。
   - 仅使用现有 `.\.cache\target\release\qq-uia-adapter.exe` 或 `scripts\cargo-local.ps1`，严禁在 `%USERPROFILE%\.cargo` 产生文件。
   - **不得修改工作区 Rust 源码**：本次你的职责是“独立核验并出报告”，若发现真实缺陷，请在报告“缺陷与修复建议”章节详尽记录复现步骤与疑点位置，不要擅自改码掩盖问题。

---

## 四、请先阅读以下项目文档与配置（建立核验基线）

1. `config/default.toml`
2. `docs/OneBot协议完善进度.md`
3. `docs/ARCHITECTURE.md`
4. `README.md`

---

## 五、分阶段真实 QQNT 核验清单（Phase 0 ～ Phase 6）

> **建议做法**：所有发向 QQ 群/好友的测试文本均附带唯一标记（如 `[GPT-QA-HHMMSS]`），便于通过 `/api/tree` 与 `/api/messages` 精确检索回读。

### Phase 0：宿主机环境与真实 QQNT 附着基线检查
1. 调用 `GET http://127.0.0.1:18770/api/health`，确认守护进程在线；若未启动，先执行根目录 `start-daemon.bat` 或 `.\.cache\target\release\qq-uia-adapter.exe start-daemon`。
2. 调用 `GET /api/status` 与 `GET /api/windows`，确认：
   - `desktop_name == "Default"` 且 `sandbox_isolated == false`；
   - `qq_pid` 对应宿主真实的 `QQ.exe`，`security_breaker_tripped == false`；
   - 当前附着群为 `Demo测试群`，人数 `< 10`。
3. 调用 `GET /api/diag` 与 `GET /api/tree`，验证 QQNT Chromium 无障碍控件树已正常唤醒（能够看见真实会话列表/消息列表/输入区域节点，而非空渲染壳）。
4. 调用 `GET /api/storage/stats` 与 `GET /api/storage`，记录当前 SQLite 6 张表的初始统计基线。

### Phase 1：账号信息、目录探测与真实群号实机核验
依次实测并对照真实 QQ 客户端 UI 核对数据准确性：
1. **基础状态与路由双形态**：
   - `get_version_info`、`get_status`、`get_login_info`（同时测试 `/onebot/v11/...` 与根路径 `/get_version_info`、`/get_login_info`），核对 `user_id`、`real_qq_id`、`nickname` 与真实登录 QQ 账号是否一致。
2. **群列表与真实群号探测**：
   - `POST /onebot/v11/get_group_list`（分别测 `no_cache: false` 与 `no_cache: true`）与 `POST /onebot/v11/get_group_info`（分别用 `group_id: 100000001` 与 `group_name: "Demo测试群"` 调用）。
   - 核对是否准确抓到真实数字群号 `100000001`、成员数与 `max_member_count`，同步对照 `/api/groups`、`/api/group-info?group_id=100000001`。
3. **群成员与好友、陌生人信息**：
   - `get_group_member_list`（`no_cache: true`）、`get_group_member_info`（使用列表返回的真实 `user_id`）、`get_friend_list`（`no_cache: true`）、`get_stranger_info`，对照 `/api/group-members`、`/api/friends`、`/api/sessions` 核查真实 QQ 右侧成员列表与左侧会话列表是否吻合（注意是否存在群名称混入好友列表、昵称缺漏或头像占位错乱）。

### Phase 2：`Demo测试群` 群聊发送、消息段与回查实机核验
（注意每次发送之间 `Start-Sleep -Milliseconds 1800`，防止触发节律防抖）
1. **纯文本 + 中文/Emoji/换行发送**：
   - 通过 `send_group_msg`（按 `group_id: 100000001`）发送 `[GPT-QA-<ts>-TEXT] 中文/Emoji🚀/多行测试`。
   - **端侧取证**：立即查询 `/api/tree`（检查聊天窗口最新气泡文本与输入框是否已清空、无残字）及 `/api/messages`。
2. **`face` 表情与 `at`、`reply` 组合段发送**：
   - 分别用结构化 `message` 数组与 CQ 码串 `[CQ:face,id=14][CQ:at,qq=...]` + `[CQ:reply,id=<刚才拿到的msg_id>]` 调用 `send_group_msg` 与 `send_msg`。
   - **端侧取证**：检查 `outcome.mouse_touched == false`、`verified_in_chat`，并在 `/api/tree` / `/api/messages` 核查在真实 QQ 客户端中：`face` 是否转换为真实表情图标还是裸 `[微笑]` 文字？`@群成员` 是否真触发群内艾特高亮还是纯文本？`reply` 是否真呈引用回复还是摘要文字？（请如实记录实际渲染效果！）
3. **出站图片粘贴发送（`image`）与剪贴板恢复**：
   - 先往宿主机剪贴板写入 `"GPT_CLIPBOARD_GUARD_PROBE_2026"`。
   - 生成一张可辨识的小 PNG 图片，分别测试 `base64://...`、`data:image/png;base64,...` 与本地临时文件 `file:///...` 调用 `send_group_msg`（如支持可附 `type: "flash"` 检查表现）。
   - **端侧取证**：检查真实 QQ 聊天窗口气泡是否真正发出图片（`/api/tree` / `/api/messages`）、`mouse_touched == false`、且宿主机剪贴板内容是否还原为 `"GPT_CLIPBOARD_GUARD_PROBE_2026"`。
4. **其余扩展/特殊消息段端侧表现检查**：
   - 核查 `record`、`video`、`rps`、`dice`、`shake`、`poke`、`anonymous`、`share`、`contact`、`location`、`music`、`xml`、`json`、`file` 在群发接口中的实际行为与 QQ 客户端内实际显示（通常为摘要文本，请实测至少代表性若干项并逐项准确标为 `DEGRADED_BY_DESIGN` 或 `CODE_ONLY_UNVERIFIED_LIVE`）。
5. **消息回查准确性**：
   - 对上面真实发出的消息调用 `get_msg`（使用发信返回的 `message_id`）与 `get_group_msg_history`（测 `message_seq` 游标与 `count` 分页），核对字段、`group_id`、`raw_message`、`message` 段数组与真实气泡一致。

### Phase 3：`测试好友A` 私聊白名单、发送与合并转发核验
1. **私聊未授权拦截**：
   - 对 `"__GPT_UNAUTH_PROBE__"`（及虚构不存在 `user_id`）调用 `send_private_msg` 与 `get_friend_msg_history`，确认均返回 `403` / `retcode = 1403` / 相应拒绝，且真实 QQ 未切换聊天窗口。
2. **白名单私聊发送与切窗回归**：
   - 向好友 `"测试好友A"` 调用 `send_private_msg`（支持按 `nickname` 与按 `get_friend_list` 获取到的 `user_id` 两种方式各测一次，含文本+`face`）。
   - **端侧取证**：检查返回 `mouse_touched == false`、`target_kind == "private"`、`verified_in_chat == true`；检查 `/api/tree` 确认真实 QQ 已切换进与 `测试好友A` 的私聊窗口（无右侧群成员列表）且气泡上屏、输入框已清空。
   - 调用 `get_friend_msg_history` 验证私聊消息可完整回查。
3. **合并转发（`forward` / `node`）端侧实测**：
   - 测试 `send_group_forward_msg`（发往 `Demo测试群`）、`send_private_forward_msg`（发往 `测试好友A`）及 `send_forward_msg`，再用返回的 `forward_id` / `message_id` 调用 `get_forward_msg`。
   - **端侧取证**：核查 `get_forward_msg` 还原的 `messages` 节点树准确性，并**如实记录 QQ 真实聊天窗口内显示的是逐条文本摘要还是原生合并转发卡片**（若是前者，标 `DEGRADED_BY_DESIGN`）。
4. **群←→私聊跨上下文连续切换可靠性**：
   - 从 `测试好友A` 私聊发信结束后，紧接着向 `Demo测试群` 再发一条群消息，核查适配器是否准确从私聊切回 `Demo测试群` 而不发错对象。

### Phase 4：多角色正向 WebSocket、HTTP 表单与后缀修饰器实测
针对真实 `127.0.0.1:18770` 服务端执行（建议测试前再调用 `/api/open-group` 切回 `Demo测试群`）：
1. **正向 WS 三角色实测**：
   - `/onebot/v11/ws` & `/ws`（`Universal`）：验证首帧 `meta_event.lifecycle.connect`（核对 `self_id` 是否与 `get_login_info` 一致）；下发 `get_status`、`send_group_msg` 验证 `echo` 回执；**等待至少 `16` 秒**验证真实 `meta_event.heartbeat` 定时心跳是否按配置推送且 `good == true`。
   - `/onebot/v11/ws/api`（`ApiOnly`）：验证能发动作拿回执，且不主动推送心跳/事件。
   - `/onebot/v11/ws/event`（`EventOnly`）：验证能收到事件，且尝试发送动作时返回 `status="failed"`、`retcode=10004`。
   - **原生事件流**：连一下 `/ws/events` 确认连接握手正常。
2. **HTTP Query / Form / 鉴权兼容**：
   - 测试 `GET /onebot/v11/get_group_list`（读操作允许 GET）、`GET /onebot/v11/send_group_msg`（写操作必须返回 `405`）、`POST` `application/x-www-form-urlencoded` 调用动作。
   - 测试 `config/default.toml` 未开启 `access_token` 时放行状态；如无法在不重启前提下测开启态鉴权，请标注其鉴权部分仅在单测覆盖或热重启场景注明（不得擅自丢弃原有配置）。
3. **动作后缀修饰器实测**：
   - 对 `get_version_info_async`、`send_group_msg_async`、`send_group_msg_rate_limited`、`send_group_msg_async_rate_limited` 实测，核查同步返 `retcode = 1 / status = "async"` 以及真实 QQ 窗口中消息是否真正确实延后上屏。
4. **`.handle_quick_operation` 实测**：
   - 传入真实群事件 `context`（`message_type: "group"`, `group_id: 100000001`, `sender: ...`, `message_id: ...`）与 `operation: { reply: "[GPT-QA-QUICKOP]", auto_escape: false }`，核查真实 QQ 群内是否发出对应回复，并测传入 `delete: true` 时是否返回 `10004`。

### Phase 5：入站事件感知、媒体缓存、Notice 横幅与反向回调的诚实边界核验
1. **`get_image` 与 `clean_cache`**：
   - 先核查 `.runtime/media` 当前是否有历史/新截取图片；若存在，用真实文件名调用 `get_image` 验证返回合法路径及 `../` 路径穿越拦截；最后（或先确认无需保留再）调用 `clean_cache` 测试清理统计与磁盘释放。
   - 若目前聊天记录中没有真实对端发来的图片可供触发入站截切，请**如实将“对端发图入站 `PrintWindow` 自动截图落盘并经 `get_image` 读取”标为 `CODE_ONLY_UNVERIFIED_LIVE`**，只将 `get_image` 参数/穿越与 `clean_cache` 文件清理标为实测项。
2. **对端实时入站消息、6 类 Notice 横幅、反向 WS 与 HTTP Webhook**：
   - 检查单机环境限制：由于只有当前已登录账号在本机运行，若无真实好友/群友向你发送实时消息、撤回、拍一拍、入/退群或禁言：
     - **严禁将 `message.group`（他人实时入站）、`message.private`（他人实时私聊入站）、`notice.group_recall`、`notice.friend_recall`、`notice.notify.poke`、`notice.group_increase`、`notice.group_decrease`、`notice.group_ban`、`notice.notify.system_tip` 虚报为 `LIVE_PASS`**！必须明确列为 `CODE_ONLY_UNVERIFIED_LIVE`。
     - 若存在本地现成的历史消息节点中含横幅或 `[语音]`/`[视频]`/`[合并转发]`/`[微笑]` 入站样本，可在报告中注明检索结果；若无，如实标 `CODE_ONLY_UNVERIFIED_LIVE`。
   - 至于 `[onebot].reverse_ws_*`、`[onebot].http_post_urls`（HMAC-SHA1 签名）与 `[onebot].post_message_sent`：若你不修改 `config/default.toml` 就无法触发实际出向建连/POST 投递，要么先显式标注 `CODE_ONLY_UNVERIFIED_LIVE`，要么仅在确保**完整备份并恢复原 `config/default.toml` 后重启回来**的前提下做本机临时监听验证，并在报告里清楚写出是否实测过。

### Phase 6：19 项 `10004` 边界、CLI 子命令与热重启收尾检查
1. **边界动作抽检（确认严格 `501` / `10004`）**：
   - 实测代表性边界动作：`delete_msg`、`get_record`、`send_like`、`get_cookies`、`set_group_kick`、`set_group_ban`、`set_friend_add_request`、`set_group_add_request`，确认均返 HTTP `501` 与 `retcode = 10004`，且真实 QQ 客户端无弹窗/无右键点击/无任何副作用。
2. **CLI 工具实地核验**：
   - 依次运行只读命令确认与服务返回一致：
     - `.\.cache\target\release\qq-uia-adapter.exe status`
     - `.\.cache\target\release\qq-uia-adapter.exe sessions`
     - `.\.cache\target\release\qq-uia-adapter.exe messages --limit 10`
     - `.\.cache\target\release\qq-uia-adapter.exe friends`
     - `.\.cache\target\release\qq-uia-adapter.exe group-info --group-id 100000001`
     - `.\.cache\target\release\qq-uia-adapter.exe storage`
     - `.\.cache\target\release\qq-uia-adapter.exe verify-group --below-members 10`
3. **热重启（`restart` / `set_restart`）核验**：
   - 记录当前守护 `PID`（如 `516`），调用 `.\.cache\target\release\qq-uia-adapter.exe restart`（或 `POST /onebot/v11/set_restart`），等待新进程就绪后核查：PID 已轮换、`/api/status` 仍显示 `desktop_name == "Default"` & `sandbox_isolated == false` & `security_breaker_tripped == false`、能继续识别真实 `QQ.exe` 并正常执行一次 `get_status`。
4. **最终收尾检查**：
   - 确认 `%USERPROFILE%\.cargo` 依然不存在；
   - 确认没有多余进程残留，`:18770` 守护进程处于健康态并切回 `Demo测试群`；
   - 按 `docs/gpt-live-verification/REPORT_TEMPLATE.md` 的结构生成最终完整报告到 `docs/gpt-live-verification/GPT_LIVE_VERIFICATION_REPORT.md`。
```
