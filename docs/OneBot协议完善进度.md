> 公开副本说明：示例身份与路径已替换，历史实机原始证据不公开；本文件的历史状态不代表公开版本或新电脑已完成真机验证。参阅 [公开版本说明](PUBLIC_RELEASE_NOTES.md)。

# qq-uia-adapter OneBot v11 协议完善进度与能力实态

> **当前版本**：`v0.3.0`（Workspace 8 Crates：覆盖 `uia_core/ocr_engine.rs` WinRT 离线 OCR、`qqnt-driver/src/composer/{file_staging,single_forward_ops}.rs` 有界暂存/原生 `[CQ:video]`/文件上传与原生右键单条转发、`parser/{row_cleaner,nested_msaa,notice_enricher}.rs` 顶边无主贴图过滤/原生引用与合并转发提取/被动撤回与 6 类扩展通知、`storage/src/repositories/{history_query_repo,message_repo,meta_audit_repo}.rs` 深分页过滤检索/出站防吞并新 ID 与幂等 `fwd-*` 复用、`scheduler/` 两级洪峰调度、`delete_msg` 实机右键撤回、`.handle_quick_operation` / 出站 `408/1408` 截止期拦截、`.get_audit_events` 脱敏及 `.reset_security_breaker` 受控恢复）
> **真实定位**：基于 Windows UI Automation + WinRT OCR + 剪贴板快照 + 光标守卫桌面交互的 QQNT `9.9.31` 适配器。支持 **42 个已实现/可执行 OneBot v11 动作**、**39 个显式边界拒绝动作（HTTP `501` / `retcode = 10004`；未注册未知动作返 `404` / `retcode = 1404` `ACTION_NOT_FOUND`）**，以及覆盖 **21 种 OneBot v11 消息段** 的三层诚实能力边界（`Tier 1 原生桌面 GUI 执行` / `Tier 2 严格模式默认 10004 拒收` / `Tier 3 显式配置降级摘要`）。

---

## 一、底层安全模型与核心架构能力（Stage 4 实机闭环已验，详见第六章分级台账）

1. **三层能力执行架构与严格禁伪造门禁**：
   - **Tier 1（原生桌面 GUI 执行与本地能力）**：纯文本换行（`Shift+Enter`）、图片（`CF_DIB` 粘贴 + PerMonitorV2 DPI `PrintWindow` 截图回采）、经典表情（135 个系统表情通过表情面板点选）、群 `@` 提及（成员选择器弹窗点选）、气泡引用回复（右键菜单 `回复` 挂载原生引用卡片）、本地/内联暂存文件（`CF_HDROP` 粘贴 + 发送确认，支持 `local`/`file://`/`base64://`/`data:` 上限 `16 MiB` 落盘 `.runtime/staging_uploads/` 并记录 `File.size`）、原生 `[CQ:video]` 独立发送（`can_send_video = true`，经 `file_staging` + `CF_HDROP`）、离线 WinRT `ocr_image`/`.ocr_image`（`POST /api/ocr`）、魔法猜拳/骰子（工具栏面板探测点击，桌面控件未暴露时安全拒绝）、私聊窗口抖动、引用 ID 多选合并转发卡片（右键 `多选 -> 合并转发`）、以及原生撤回（`delete_msg` 右键 `撤回`）。
   - **Tier 2（严格默认拒绝 `501 / 10004`）**：默认 `[onebot].allow_degraded_media_summary = false`，严禁静默伪造纯文本摘要；`send_poke` / `group_poke` / `friend_poke`、`[CQ:poke]`、`record`、`image(type=flash)`、自定义未引用 `node` 内容、`share`、`contact`、`location`、`music`、`xml`、`json`、`anonymous`、`http(s)://` 远端媒体 URL、非根群文件夹 `folder`、`file`/`video`/`rps`/`dice`/`shake` 与其他段混发，以及 11 项新增无可用 UIA 入口动作（`set_msg_emoji_like` 等），一律返回 HTTP `501` / `retcode = 10004`。
   - **Tier 3（显式降级摘要模式）**：仅当运维在配置文件显式开启 `[onebot].allow_degraded_media_summary = true` 时，才对非原生支持的富媒体/自定义节点退化为可读文本占位并在 `SendOutcome.degraded_segments` 如实标注。
2. **非侵入背景 UIA 优先与光标瞬时恢复点击安全模型（Mouse & Focus Safety Model）**：
   - 纯读取与常规纯文本发送默认优先使用**非侵入后台 UIA / `WM_CHAR`**，尽量不触碰物理鼠标与外部前台焦点（`mouse_touched = false`）。
   - 仅当触发富交互编排（点击表情/魔法面板、`@` 列表、气泡右键 `回复`/`多选`/`撤回`、`CF_HDROP` 文件确认框、合并转发模态框）或包含多行换行（`Shift+Enter`）与剪贴板粘贴时，才启用显式安全守卫链：
     - [`click_point_cursor_restore_verified`](../crates/uia-core/src/action/mouse_ops.rs#L192) / [`scroll_point_cursor_restore_verified`](../crates/uia-core/src/action/mouse_ops.rs#L237)：先由 `prepare_verified_cursor_point` 检查用户鼠标按键及最近空闲窗口（`is_user_mouse_busy`），再通过 `WindowFromPoint` 验证命中像素严格归属绑定的 `QQ.exe` PID 与根/浮层 HWND，配合 `CursorPositionGuard` 在点击/滚轮前后瞬时保存并还原原始物理光标坐标（典型停驻微秒级至数毫秒）。
     - `ForegroundRestoreGuard`（`crates/uia-core/src/action/focus_guard.rs`）：覆盖整个富交互与发送/撤回周期，退出作用域时在确认用户未手动切换窗口的前提下自动恢复调用前外部前台窗口焦点。
     - `ClipboardRestoreGuard`：写入 `CF_DIB` / `CF_HDROP` 前备份原始剪贴板文本并在操作结束时自动还原。
3. **跨重启不撞库的合并转发存储（`Storage::allocate_forward_nodes`）**：
   - 修复了早先进程重启后进程内原子计数器归零覆盖旧 `fwd-10000+` 的缺陷（`REVERIFY-BUG-005`）：[`allocate_forward_nodes`](../crates/storage/src/repositories/meta_audit_repo.rs) 在 SQLite `TransactionBehavior::Immediate` 事务中同时读取 `kv_meta["forward_seq"]` 与现有 `forward:fwd-%` 最大数字后缀（起始不低于 `10000`），自增到不存在的新键后原子写入转发节点，并保留最多 `512` 条有界淘汰；`get_forward_msg` 同时支持 `fwd-*` 字符串 ID 与数字 `message_id` 关联回查。
4. **并发 WebSocket、长发送心跳与背压保护**：
   - 正向/反向 WS 采用并发 `JoinSet` + `Semaphore(32)` + 有界 `mpsc` 汇总通道，慢发送锁（`send_gate`）期间 `Ping/Pong` 与快查询零阻塞。
   - HTTP 路由接入 `8 MiB` `DefaultBodyLimit`、Auth 中间件前置 `Content-Length` 检查（超限返回 `413/1413`）及 `MAX_INFLIGHT_ASYNC_ACTIONS = 256` 异步信号量背压。
5. **高安全边界、响应窗守卫与多维度风控隔离**：
   - **群聊安全门槛与多群受控放行**：默认按成员数阈值（`strict_below_members = 10`）校验标题或右侧边栏人数防误触大群；仅当配置将群列入 `[target].monitored_groups` 且显式设置 `allow_large_monitored_groups = true` 时才放宽受监控大群人数限制（未在监控列表的大群仍一律 `403 / 1403`）。
   - **出站响应截止期保护（`408 / 1408`）**：`send_*`、`POST /api/send`（`response_window_ms`）与 `.handle_quick_operation`（`response_deadline_ms` / `captured_at_ms`）均支持响应窗约束；超时立即返回 HTTP `408 REQUEST_TIMEOUT` / `retcode = 1408`（`ResponseWindowExpired`）并累加 `outbound_deadline_aborted_total`，零 UIA 误发。
   - **私聊白名单与跨会话护栏**：出站私聊、`get_msg`、`/api/messages/history`、`mark_private_msg_as_read` 与合并转发均强制核验 `[target].allowed_private_friends`（未命中即返 `403 / 1403`）；`BotDriver::get_recent_messages_by_kind` 与 `Storage` 复合索引 `idx_messages_group_type_recent` 严格隔离同名群聊与私聊记录；出站 `[CQ:reply,id=...]` 拒绝跨会话引用（`400 / 1400` `"belongs to another session"`）；OCR/暂存阻断 UNC 与远端共享路径（`400 / 1400`）并兼容大小写 `BASE64://`/`DATA:`。
   - **生产风控弹窗隔离**：`allow_mock = false` 时严禁匹配 Mock 安全窗类（`QA-BUG-010`）；发现真实风控/验证码弹窗立即熔断（`423 / 1423`）。
6. **标识符诚实性声明**：
   - `real_user_id` / `real_group_id` 为 `None` 时，`user_id`、`group_id` 均为本地 SQLite 通过一致性 FNV-1a 散列生成的**适配器级兼容映射 ID**（已由 `Storage::reconcile_member_mapped_ids` 在登录态、好友与群成员表间统一），并非声称从 QQ 私有协议抓取的真实数字 QQ 号。

---

## 二、多协议传输通道与鉴权实现矩阵

| 通道 / 特性 | 实现文件 | 实现状态 | 验证等级与备注 |
| :--- | :--- | :--- | :--- |
| **HTTP API（`/onebot/v11/:action` 与根路径 `/:action`）** | `adapter-server/src/onebot/{macros.rs,handlers.rs}` | 已完成 | 支持 JSON、`x-www-form-urlencoded`（UTF-8 字节安全解码）与 Query；写动作禁止 `GET`（`405/1405`） |
| **正向 WebSocket（Universal / API / Event）** | `adapter-server/src/onebot/{forward_ws.rs,ws_runner.rs}` | 已完成 | `/onebot/v11/ws`（`/ws`）、`/onebot/v11/ws/api`（`/api/ws`）、`/onebot/v11/ws/event`（`/event/ws`、`/ws/event`），Event-only 调用写/读动作拒绝 |
| **反向 WebSocket（三角色主动重连）** | `adapter-server/src/onebot/reverse_ws.rs` | 已完成 | 支持 `reverse_ws_urls` / `api` / `event` 拆分配置、`X-Self-ID`/`X-Client-Role` 及指数退避；`MOCK_PLUMBING_ONLY` 已覆盖 |
| **HTTP POST Webhook + 快速操作回调** | `adapter-server/src/onebot/{http_webhook.rs,quick_ops.rs}` | 已完成 | 纯 Rust RFC 2104 `HMAC-SHA1` (`X-Signature: sha1=...`)，应答 JSON 直接驱动 `.handle_quick_operation`（支持 `408/1408` 过期保护） |
| **Access Token 双位置鉴权** | `adapter-server/src/auth.rs` | 已完成 | `Authorization: Bearer <token>` 与 `?access_token=<token>`（含 `%XX` 解码），恒定时间比较防计时旁路 |
| **动作修饰符与限流队列** | `adapter-server/src/onebot/macros.rs` | 已完成 | 全部 42 项支持动作均自动支持 `_async`、`_rate_limited`、`_async_rate_limited` 后缀修饰 |

---

## 三、全量事件与 Notice 推送能力矩阵

| 事件分类 | `post_type` / 子类型 | 触发源与字段规范 | 说明 |
| :--- | :--- | :--- | :--- |
| **群消息 / 私聊消息** | `message.group.normal` / `message.private.friend` | 后台 WinEventHook + UIA 轮询采集 | 保留 `sender`（含 `role`/`card`）、`group_id`、`user_id`、`message_id`、`font: 0`、`message_seq`、`real_id`、`anonymous: null`（群聊）、`priority_tier`、`response_deadline_ms`、`captured_at_ms` 与结构化 `MessageSegment` 数组（含入站 `File { file, name, size }`，已修复 `row_cleaner` 折行文件名拼接） |
| **本端自发上报** | `message_sent` | 显式开启 `[onebot].post_message_sent = true` 时 | 自发气泡确认落地后经 `translate_adapter_event_enriched` 从存储与登录态回填真实 `raw_message`、`message` 段数组与机器人昵称；`ok = false` 或缺失 ID 严禁发射（`CONTRACT_VERIFIED`） |
| **消息撤回事件** | `notice.group_recall` / `notice.friend_recall` | **主动执行** `delete_msg` / `.handle_quick_operation({"delete": true})` 撤回成功；或被动横幅 + `diff_visible_uids` 捕获 | 主动撤回直接生成标准 `*_recall`；被动撤回由 `MessageTracker::diff_visible_uids` 与 `notice_enricher.rs` 调用 `Storage::resolve_recalled_message`（`180s` 兜底窗口）恢复真实 `message_id`/`user_id` 并通过 `delete_message_by_onebot_id` 删库 |
| **文件上传通知（入站）** | `notice.group_upload` / `notice.offline_file` | `poller.rs` `process_polled_incoming` 解析入站文件气泡（仅 `!msg.is_self`） | 群文件派发 `group_upload`（`{ id, name, size, busid: 0 }`），私聊文件派发 `offline_file`（`{ name, size, url: "" }`） |
| **群友戳一戳事件（入站）** | `notice.notify.poke` | `parser/notices.rs` 解析 QQNT 系统横幅 | 仅支持**入站系统提示捕获**；出站主动戳一戳见动作表 `10004` 边界 |
| **扩展群/好友交互与管理通知** | `notice.group_msg_emoji_like` / `group_admin` / `essence` / `friend_add` / `notify.lucky_king` / `group_ban` | `parser/notices.rs` + `notice_enricher.rs` | 识别表情回应（支持从 `06_probe_bubble_menu.json` 扁平横幅解析并回填自发 `message_id`）、管理员任免（`set`/`unset`）、精华消息（`add`/`delete`）、新增好友、红包运气王及全员禁言/解禁（`user_id: 0`, `duration: -1 / 0`） |
| **群成员变动 / 禁言 / 提示** | `notice.group_increase` / `group_decrease` / `group_ban` / `notify.system_tip` | `parser/notices.rs` 解析群内灰色系统提示横幅 | 正则识别入群、退群/踢出、单人/全员禁言/解禁及其他系统提示 |
| **元事件（生命周期 / 心跳）** | `meta_event.lifecycle.connect` / `meta_event.heartbeat` | WS 建连与独立心跳定时器 | `status` 同步返回 `online`、`good`、`stat`（`packet_received`/`packet_sent`/`packet_lost`/`message_received`/`message_sent`/`lost_times`/`last_message_time` 等）与 `scheduler`（`SchedulerTelemetrySnapshot`）；慢发送期间心跳 RTT 实测 `< 1ms` |

---

## 四、42 个受支持 OneBot v11 动作与 39 个显式 `10004` 边界动作

来源单一真值表：[`crates/adapter-server/src/onebot/handlers.rs`](../crates/adapter-server/src/onebot/handlers.rs#L755-L951) 中的 `define_onebot_actions!` 声明宏（未在此表中注册的未知动作返 HTTP `404` / `retcode = 1404` `ACTION_NOT_FOUND`）。

### 4.1 42 个已实现 OneBot v11 动作

| # | 动作名 | HTTP 方法 | 核心实现与能力边界说明 |
| :---: | :--- | :--- | :--- |
| 1 | `get_status` | GET / POST | 返回驱动在线状态、当前附加会话、进程 PID、安全熔断标识、标准 `stat` 统计（`packet_lost = expired_window_total + outbound_deadline_aborted_total`、`lost_times = outbound_deadline_aborted_total` 等）及 `scheduler` 实时洪峰遥测 |
| 2 | `get_login_info` | GET / POST | 返回当前已登录账号昵称与映射 `user_id` |
| 3 | `get_version_info` | GET / POST | 返回 `qq-uia-adapter` 版本（`0.3.0`）与 `protocol_version: "v11"` |
| 4 | `send_group_msg` | POST | 按三层能力编排执行群聊消息发送（群成员阈值/受监控大群放行校验 + 响应窗 `deadline_unix_ms` / `response_window_ms` 守卫 + 气泡落地回采确认后才返回 `retcode=0`） |
| 5 | `send_private_msg` | POST | 私聊白名单校验 + 响应窗截止期守卫 + 原生桌面/后台输入执行与落地确认 |
| 6 | `send_msg` | POST | 根据 `message_type` / `group_id` / `user_id` 路由至群聊或白名单私聊发送，同步执行出站 `408/1408` 截止期保护 |
| 7 | `get_msg` | GET / POST | 从 SQLite 持久化记录查询指定 `message_id` 结构化消息及 `sender`（含 `role`）、`font: 0`、`message_seq`、`real_id` 元数据 |
| 8 | `get_friend_list` | GET / POST | 查询或刷新（`no_cache`）已清洗过滤的真实好友目录 |
| 9 | `get_group_list` | GET / POST | 查询或刷新（`no_cache`）已清洗过滤的群目录与人数摘要 |
| 10 | `get_group_info` | GET / POST | 返回单群信息，并在缺少真实数字群号时安全触发“更多”抽屉与资料卡双通道探测 |
| 11 | `get_group_member_list` | GET / POST | 提取右侧群成员栏与历史缓存中的成员名单及群内统一映射 ID |
| 12 | `get_group_member_info` | GET / POST | 按 `group_id` + `user_id` 返回单群成员详情与头衔/昵称 |
| 13 | `can_send_image` | GET / POST | 返回 `{"yes": true}`（支持基于 `CF_DIB` 剪贴板的原生图片发送） |
| 14 | `can_send_record` | GET / POST | **返回 `{"yes": false}`**（严格默认拒绝语音段，与 Tier 2 `10004` 门禁一致） |
| 15 | `get_stranger_info` | GET / POST | 跨好友与群成员表解析展示指定 `user_id` 昵称与映射属性 |
| 16 | `get_image` | GET / POST | 校验安全白名单路径 `.runtime/media/`（阻止路径穿越 `400/1400`）并返回本地图片截图属性 |
| 17 | `get_group_msg_history` | GET / POST | 基于 SQLite 深度游标分页（支持 `message_seq`、`message_seq: 0` 从最新回溯、`count` 与 `reverseOrder`）回查群聊历史记录 |
| 18 | `get_friend_msg_history` | GET / POST | 校验 `[target].allowed_private_friends` 白名单后基于 SQLite 深度游标分页（`message_seq`、`message_seq: 0`、`count`、`reverseOrder`）回查私聊历史记录 |
| 19 | `send_group_forward_msg` | POST | 严格模式提取同群已入库引用 `node(id)` 执行真实桌面“多选 -> 合并转发”，持久化唯一 `fwd-*` 包 |
| 20 | `send_private_forward_msg` | POST | 白名单校验后执行原生私聊多选合并转发（或显式开启降级时输出文本摘要） |
| 21 | `send_forward_msg` | POST | 通用合并转发分发入口，兼容 `group_id` 与 `user_id` |
| 22 | `get_forward_msg` | GET / POST | 同时支持 `fwd-10000+` 字符串转发 ID 与关联数字 `message_id` 回查节点数组 |
| 23 | `clean_cache` | POST | 递归清理 `.runtime/media` 与 `.runtime/staging_uploads` 及其子目录运行缓存，并保留两大缓存根目录 |
| 24 | `set_restart` | POST | 通过 `restart_ops` 执行纯 Rust 二进制暂存热重启 |
| 25 | `.handle_quick_operation` | POST | 支持 `reply` / `at_sender` / `auto_escape` **及 `{"delete": true}` 原生撤回**：组合 `kick`/`ban`/`approve`/`set_card`/`reject_add_request`/`ban_duration` 优先原子返 `501/10004` 零发信；`reply` 与 `delete` 均校验 `response_deadline_ms` / `captured_at_ms` 过期返 `408/1408`；独占媒体段禁止前置 `Reply`/`At` 并校验同群归属 |
| 26 | `delete_msg` | POST | 实机原生撤回：滚屏查找对应消息气泡 -> 右键点击精确 `MenuItem("撤回")` -> 弹窗确认 -> 验证气泡移除/撤回横幅 -> 同步删除 SQLite 记录、写 `message_recalled` 审计并派发 `group_recall`/`friend_recall` |
| 27 | `mark_msg_as_read` | GET / POST | 按 `message_id` 定位所属群聊或私聊会话并标记已读清零未读计数；非白名单私聊严格返 `403 / 1403` |
| 28 | `mark_group_msg_as_read` | GET / POST | 按 `group_name`/`group`/`group_id` 解析目标群并将该群会话标记为已读 |
| 29 | `mark_private_msg_as_read` | GET / POST | 按 `user_id`/`nickname`/`group_name` 严格核验 `[target].allowed_private_friends` 后将私聊标记为已读 |
| 30 | `.get_word_slices` | GET / POST | 对 `content` 字符串执行确定性的 CJK 字符、ASCII 单词与标点切分并返回 `slices` 数组 |
| 31 | `.get_scheduler_status` | GET / POST | 返回当前生效 `SchedulerConfig`（`config`）与运行期洪峰调度器遥测快照（`telemetry`） |
| 32 | `can_send_video` | GET / POST | **返回 `{"yes": true}`**（支持基于 `.runtime/staging_uploads/` + `CF_HDROP` 的本地/`base64://` 原生 `[CQ:video]` 独占发送） |
| 33 | `ocr_image` | GET / POST | 调用 `uia_core` Windows WinRT `OcrEngine` 离线识别图片（`image`/`file` + 可选 `language`/`lang`），支持 `.runtime/media/`、`.runtime/staging_uploads/`、`file://`、`base64://`、`data:` 与本地路径；拒绝 `http(s)://`（`501/10004`）与 `..`（`400/1400`） |
| 34 | `.ocr_image` | GET / POST | `ocr_image` 同语义标准别名（同时配套 REST `POST /api/ocr` 与 CLI `ocr` 子命令） |
| 35 | `upload_group_file` | POST | 校验 `http(s)://` 与非根目录 `folder`（`!= "" && != "/"` 返 `501/10004`），经 `file_staging`（`<=16 MiB`）与群发送截止期发出原生 `MessageSegment::File` 并回填 `size` |
| 36 | `upload_private_file` | POST | 校验 `[target].allowed_private_friends` 白名单（`403/1403`）与 `http(s)://`（`501/10004`），经 `file_staging` 向好友发送本地/`base64://`/`data:` 文件 |
| 37 | `get_file` | GET / POST | 沙箱严格限定 `.runtime/media` 与 `.runtime/staging_uploads`（含 1 级哈希子目录）；拦截 `..`（`400/1400`）与 `http(s)://`（`501/10004`），返回 `{file, file_name, file_size, base64}`（`<=4 MiB` 附带 Base64） |
| 38 | `forward_group_single_msg` | POST | 通过群聊气泡右键菜单原生 `转发` 单条历史消息至目标群（校验消息存在、私聊来源白名单与群人数守卫），入库返回新 `message_id`（超时返 `408/1408`） |
| 39 | `forward_friend_single_msg` | POST | 校验 `[target].allowed_private_friends` 白名单与截止期后，通过原生右键 `转发` 将单条历史消息发至目标好友并入库返回新 `message_id` |
| 40 | `get_recent_contact` | GET / POST | 按 `count` 聚合活跃会话与近期 SQLite 消息，严格过滤大群及非白名单私聊并按最近活跃倒序返回 |
| 41 | `.get_audit_events` | GET / POST | 按可选 `event_kind` / `limit` 检索持久化安全审计流水，并在返回前剥离非白名单私聊目标与敏感详情 |
| 42 | `.reset_security_breaker` | POST | 在发送互斥锁保护下重新核验风控弹窗：未关弹窗返 `423/1423` 并记审计，确认关闭后重置熔断器并写审计返回最新状态 |

### 4.2 39 个显式边界拒绝动作（HTTP `501 UNSUPPORTED_ACTION` / `retcode = 10004`）

| 分类 | 明确 `10004` 拒绝动作列表（合计 39 项） | 边界理由与端侧保障 |
| :--- | :--- | :--- |
| **出站戳一戳（3 项）** | `send_poke`、`group_poke`、`friend_poke` | QQNT `9.9.31` 桌面客户端在 UIA 模式下不支持可靠的原生出站头像戳一戳（实机多次尝试无可靠动作入口），明确拒收且对 QQ 窗口零操作；仅保留入站 `notice.notify.poke` |
| **语音流与远端下载（2 项）** | `get_record`、`download_file` | QQNT UIA 不提供加密语音提取能力；基于安全红线禁止通过 OneBot 动作触发任意远程 URL 下载落盘（本地缓存读回已改由受限沙箱 `get_file` 承接，OCR 已由 `ocr_image`/`.ocr_image` 承接） |
| **个人资料修改、点赞与凭据（5 项）** | `send_like`、`set_qq_profile`、`get_cookies`、`get_csrf_token`、`get_credentials` | 资料修改与高频资料卡点赞超出非侵入安全边界，且严禁窃取 QQ Web Cookie/Skey |
| **群只读扩展枚举（5 项）** | `get_group_honor_info`、`get_group_system_msg`、`get_group_at_all_remain`、`get_essence_msg_list`、`get_group_root_files` | 群荣誉榜、加群/邀请通知、@全体剩余次数、精华消息与群共享文件目录均需协议级接口或私有 RPC，非侵入 UIA 不伪造返回 |
| **群管理与群设置（11 项）** | `set_group_kick`、`set_group_ban`、`set_group_anonymous_ban`、`set_group_whole_ban`、`set_group_admin`、`set_group_anonymous`、`set_group_card`、`set_group_name`、`set_group_leave`、`set_group_special_title`、`set_group_portrait` | 高权限群管理/退群修改操作超出当前自动化安全边界 |
| **加友加群审批（2 项）** | `set_friend_add_request`、`set_group_add_request` | 通知盒子验证审批流未纳入原生操作范围 |
| **互动回应、精华、群公告与文件目录管理（11 项）** | `set_msg_emoji_like`、`fetch_emoji_like`、`set_essence_msg`、`delete_essence_msg`、`_send_group_notice`、`_get_group_notice`、`delete_friend`、`set_qq_avatar`、`get_group_file_system_info`、`get_group_files_by_folder`、`get_group_file_url` | 真实探针 [私有证据与公开版说明](PUBLIC_RELEASE_NOTES.md) 证明 QQNT `9.9.31` 右键弹出菜单仅含无名图标按钮（无可用 UIA 贴表情动作入口），故仅保留入站 `group_msg_emoji_like` 捕获而显式拒绝出站 `set_msg_emoji_like`；其余精华、群公告、删好友、改头像与群云盘枚举均属私有/云端 RPC，统一返 `501 / 10004` |

---

## 五、全量 21 种 OneBot v11 消息段三层能力矩阵

依据 `crates/qqnt-driver/src/composer/plan.rs`（`build_composer_plan` / `validate_strict_segments`）与 `crates/adapter-server/src/onebot/handlers.rs`（`validate_strict_onebot_segments`）：

| # | 消息段 `type` | Tier 1（默认严格模式行为） | Tier 2 / Tier 3（拒绝或开启降级开关后行为） | 入站采集与结构化能力 |
| :---: | :--- | :--- | :--- | :--- |
| 1 | `text` | **原生支持**：单行优先后台 UIA；多行自动切至 `ForegroundRestoreGuard` + `Shift+Enter` 保留硬换行 | 无降级 | 原生文本精确提取与去重归一化 |
| 2 | `image`（普通） | **原生支持**：`ClipboardRestoreGuard` + `CF_DIB` + `Ctrl+V` 粘贴真实图片气泡 | 入站非原图下载，而是 PerMonitorV2 DPI `PrintWindow` 视口截图裁剪入 `.runtime/media/` | 解析气泡内图片坐标并自动保存本地路径 |
| 3 | `image(type=flash)` | **Tier 2 默认拒绝 `501/10004`**（桌面 UIA 无法构造原生闪照） | 仅开启 `allow_degraded_media_summary = true` 时按普通图片或摘要发送 | 解析已知占位 |
| 4 | `face` | **原生支持**：支持 `135` 个 QQ 经典表情（`face_table.rs`）；点击工具栏 `表情` -> 面板真实点选；未知 `id` 返回 `10004` | 开启 `allow_degraded_media_summary = true` 可退化为 `[表情:<name>]` | 双向识别 `[表情:名称]` 与结构化 `face` 段 |
| 5 | `at` | **群聊原生支持**：编辑器输入 `@` 触发成员选择器弹窗并真实点选成员或 `@全体成员`；**私聊拒绝 `10004`** | 开启降级开关可退化为 `@昵称` 纯文本 | 解析 `@昵称` / `@全体成员` 与映射 `qq` |
| 6 | `reply` | **原生支持**：定位可见历史气泡后右键 `回复` 挂载 QQNT 原生引用卡片（前置第 0 步，单条限 1 个） | 找不到气泡时严格失败（不伪造引用文本）；开启降级可转为 `「回复 @...」` | `try_collect_native_quote_bubble` 解析原生引用头部与正文，`segment_enricher` 回查同会话历史并生成 `reply(id)`；未命中历史仅保留纯文本引文（不误触发 `@`） |
| 7 | `file` | **原生独占动作**：接受本地路径/`file://`/`base64://`/`data:`（`<=16 MiB` 暂存 `.runtime/staging_uploads/`，拒绝 `http(s)://`，不可与文本混发），写入 `CF_HDROP` 粘贴并点击发送确认框，同时写入 `File.size` | 混发或 `http(s)://` 默认返 `10004`；开启降级可发文件文本提示 | `row_cleaner` 修复折行拼接；`segment_enricher` 抽取 `File { file, name, size }` 并触发 `group_upload`/`offline_file` |
| 8 | `rps` | **原生独占动作（受 客户端暴露限制）**：尝试点击表情面板中的猜拳魔法入口；若 QQNT `9.9.31` UIA 未暴露按钮则返回 `501/10004`，绝不发假文本 | 混发默认返 `10004`；开启降级可发 `[猜拳]` 文本占位 | 解析 `[猜拳]` 文本或对应气泡状态 |
| 9 | `dice` | **原生独占动作（受客户端暴露限制）**：尝试点击表情面板中的骰子魔法入口；若 QQNT `9.9.31` UIA 未暴露按钮则返回 `501/10004` | 混发默认返 `10004`；开启降级可发 `[掷骰子]` 文本占位 | 解析 `[掷骰子]` 文本或对应气泡状态 |
| 10 | `shake` | **私聊原生独占动作**：点击私聊工具栏 `窗口抖动` / `抖一抖`；**群聊或控件缺失返回 `501/10004`** | 混发默认返 `10004`；开启降级可发 `[窗口抖动]` 文本占位 | 视 QQNT 横幅而定 |
| 11 | `forward` / `node` | `send_*_forward_msg` 对**同会话已入库引用 `node(id)`** 执行原生多选气泡合并转发卡片；`forward_*_single_msg` 对单条已入库消息执行桌面右键 `转发`；内置于 `send_msg` 或**自定义未引用 `node` 内容**默认返回 **`501/10004`** | 仅当显式设置 `allow_degraded_media_summary = true` 时才退化为 `[聊天记录]` 摘要发送 | `try_parse_forward_card_segment` 自动抽取入站合并转发气泡并经 `Storage::get_or_allocate_forward_for_uid` 幂等落库为 `fwd-*`，供 `get_forward_msg`（支持 `fwd-*` 与数字 `message_id`）完整读取节点 |
| 12 | `poke` | **Tier 2 默认拒绝 `501/10004`**（与 `send_poke` 一致，QQNT `9.9.31` 无可靠原生出站戳一戳 UIA 路径） | 仅开启 `allow_degraded_media_summary = true` 时退化为 `[戳一戳]` 摘要 | `parser/notices.rs` 支持捕获**入站** `notice.notify.poke` |
| 13 | `record` | **Tier 2 默认拒绝 `501/10004`**（`can_send_record = false`） | 仅开启降级开关时输出 `[语音]` 文本占位 | 解析 `[语音]` 占位符 |
| 14 | `video` | **原生独占动作**（`can_send_video = true`）：接受本地路径/`file://`/`base64://`/`data:`（经 `file_staging` 落盘 `.runtime/staging_uploads/` 后复用 `CF_HDROP` 粘贴发送，不可与文本混发） | `http(s)://` 远端地址或图文混发默认返 `501/10004`；开启降级开关时输出 `[视频]` 文本占位 | `row_cleaner` 与 `segment_enricher` 抽取 `.mp4`/`.mov`/`.mkv`/`.webm` 等结构化 `Video { file }` 段（兼容折行与 `0%..100%` 进度），并解析 `[视频]` 占位符 |
| 15-21 | `share`、`contact`、`location`、`music`、`xml`、`json`、`anonymous` | **Tier 2 默认拒绝 `501/10004`**（无法由桌面 UIA 构造原生卡片或匿名开关） | 仅开启 `allow_degraded_media_summary = true` 时折叠输出可读链接/名片/位置/卡片摘要 | 解析入站分享/位置/卡片链接与占位符 |

---

## 六、五级证据分级规范与端侧验证实态（2026-11-16）

本仓库严格执行 [`AGENTS.md`](../AGENTS.md#8-强制证据分级与防自欺验收规范5-level-evidence-grading-rule) 与 [`TESTING_AND_BENCHMARK_WORKFLOW.md`](./TESTING_AND_BENCHMARK_WORKFLOW.md) 的五级验证口径：**单元测试、打桩与 Mock 大群流水线绝不冒充真实 `QQ.exe` 端到端通过**。

| 验证等级 | 标准定义 | 当前覆盖基线与核心证据路径 |
| :--- | :--- | :--- |
| **L1 · `UNIT_TESTED` / `CONTRACT_VERIFIED` / `WINRT_LOCAL_OCR_VERIFIED` / `MOCK_WIN32_VERIFIED`** | Workspace 单元测试、`ConfigurableStubDriver`、`:memory:` SQLite 与 `qq-qa-harness` L0–L4（13 类 43 用例 + 13 项 Criterion 风格基准）；`CAT-12`/`CAT-13` 验证 WinRT 离线 OCR（`WINRT_LOCAL_OCR_VERIFIED`）、42/39 扩展契约、深页历史/单条转发/审计脱敏/全缓存清理与 Wave v5/v6 守卫（`CONTRACT_VERIFIED`）；**`MOCK_WIN32_VERIFIED` & `Self-Test Verified`** 涵盖真实 Win32 `MockQqServer::spawn_with_groups` 多大群（500 / 3×600 人）并发洪峰集成回归及 `.reset_security_breaker` 弹窗 `423 -> 200` 重置闭环（`self-test` 14 步验证，报告固定写明 `live_e2e_verified = false`） | `.\scripts\run-qa-suite.ps1 -Tier All`（`overall_passed=true, live_daemon_detected=true, categories=13/13, passed=43/43, skipped=0, benchmarks=13`）、`.\.cache\target\release\qq-uia-adapter.exe self-test`（`14/14 SELF_TEST_PASSED`）、`qqnt-driver --test flood_selector_e2e`、`adapter-server --test scheduler_onebot_tests`、`adapter-server --lib biz_risk_tests`、`CAT-06`/`CAT-07`/`CAT-12`/`CAT-13` 与 `BM-11`/`BM-12`/`BM-13` |
| **L2 · `OFFLINE_REAL_TREE_REPLAY` / `REAL_TREE_REPLAY_VERIFIED`** | 直接对真实 `QQ.exe` 捕获落盘的 UIA JSON 元素树固件进行解析、容器解包、折行文件名合并、扩展通知分类、会话污染过滤、入站引用/合并转发提取与视口图片裁切离线重放回归 | `crates/qqnt-driver/tests/live_evidence_replay.rs`（12 用例覆盖 `LIVE-BUG-002`/`005`/`006`、`BUG-LIVE-V4-002`/`003`/`004`、`03_tree.json` 原生引用气泡与未匹配降级、`06_tree.json` 原生合并转发卡及 `get_or_allocate_forward_for_uid` 幂等落库、`LIVE-BUG-V6-001` 顶边幽灵零行）与 `qqnt-driver::parser::replay_tests`（含 `04_tree.json` 与 `06_probe_bubble_menu.json`） |
| **L3 · `UNSUPPORTED_10004`** | 明确边界动作（39 项，含 `send_poke`/`group_poke`/`friend_poke`、新增 `set_msg_emoji_like`/`fetch_emoji_like`/`set_essence_msg` 等 11 项及 `get_group_honor_info`/`set_qq_profile`/`download_file` 等；与 `record`/`flash`/混发/`http(s)://`/非根 `folder`/过期不可撤回 `delete_msg`）实测严格返 HTTP `501` / `retcode = 10004`（未注册未知动作返 `404/1404`），且端侧 UI 树与库表零污染 | `qq-qa-harness` CAT-02 & CAT-12（`all_11_new_explicit_10004_actions_reject_not_10002`）、[私有证据与公开版说明](PUBLIC_RELEASE_NOTES.md)、[私有证据与公开版说明](PUBLIC_RELEASE_NOTES.md) 与 Stage 4 实机验证 [私有证据与公开版说明](PUBLIC_RELEASE_NOTES.md)（`set_msg_emoji_like` 及过期视频 `delete_msg(10168)` 均实机返 `501/10004`） |
| **L4 · `DEGRADED_BY_DESIGN`** | 图片回采截图裁剪（DPI `PrintWindow` `.runtime/media/*.png`，像素色板一致而非原始文件）以及显式配置 `allow_degraded_media_summary = true` 时的降级文本摘要 | [私有证据与公开版说明](PUBLIC_RELEASE_NOTES.md) 第 5 章、[私有证据与公开版说明](PUBLIC_RELEASE_NOTES.md) |
| **L5 · `NATIVE_LIVE_VERIFIED`** | **仅限**宿主 `Default` 桌面真实 `QQ.exe` `9.9.31` 针对目标群（如 `Demo测试群`）与白名单好友（如 `测试好友A`）端到端闭环通过，且具独立 `/api/tree` + 无边框截图 + SQLite 1:1 对账 + 前台/剪贴板恢复证据（绝不用 `MOCK_WIN32_VERIFIED`/`CONTRACT_VERIFIED`/`WINRT_LOCAL_OCR_VERIFIED` 替代） | Stage 1 单项实机闭环：`file`（`10117`）、`forward`（`fwd-10005`）、`priv_face`（`10134`）、`priv_shake`（`10135`）、`delete_msg`（`10149`）、`.handle_quick_operation` 撤回（`10150`）、慢发送心跳保活与剪贴板恢复；**Stage 4 Wave v4 实机端到端全闭环**（[私有证据与公开版说明](PUBLIC_RELEASE_NOTES.md)）；**Wave v5 业务风险闭环**（[私有证据与公开版说明](PUBLIC_RELEASE_NOTES.md)）；**Wave v6 协议完善与顶边去重宿主实机闭环**（[私有证据与公开版说明](PUBLIC_RELEASE_NOTES.md)）：`Demo测试群`（`100000001`，`9` 成员，`security_breaker_tripped=false`）发锚点 `10192` + `get_group_msg_history`（`message_seq=0, reverseOrder=true`）/REST/CLI 深翻页检索 + 原生右键 `转发` `forward_group_single_msg` 生成 `10193 != 10192`（同步修复并实机验证 `LIVE-BUG-V6-002` 零并锚、2s 轮询后 `1:1` 仅 2 行）+ `get_recent_contact`（4 条无私聊泄漏）+ `.get_audit_events`/`forward-get` + 双气泡 `delete_msg` 撤回后 `recalled_self_rows_remaining=0`、`ghost_rows_count=0`（`LIVE-BUG-V6-001` 实机归零）与输入框清空；`.reset_security_breaker` 维持 `MOCK_WIN32_VERIFIED` & `Self-Test Verified`，`forward_friend_single_msg`/`get_friend_msg_history` 维持 `CONTRACT_VERIFIED` & `UNIT_TESTED`，入站 `03_tree`/`06_tree` 引用转发提取维持 `REAL_TREE_REPLAY_VERIFIED` |

### 已留存 2026-11-15 / 2026-11-16 实机证据目录索引

- **Wave v6 协议完善与原生单条转发/顶边去重宿主实机闭环档案（2026-11-16）**：[私有证据与公开版说明](PUBLIC_RELEASE_NOTES.md)（`01_live_wave6_single_forward_sent.png`、`02_live_wave6_recalled_clean.png`、`summary.json`，覆盖 `baseline_max_id=10191`、`Demo测试群` 锚点 `10192`、`get_group_msg_history(message_seq=0, reverseOrder=true)` + `/api/messages/history` + CLI `messages --history --keyword`、原生右键 `forward_group_single_msg` -> `10193`、`get_recent_contact`（4 联系人，0 非白名单私聊泄漏）、`/api/audit-events` / CLI `audit` / `forward-get --id fwd-10005`、`delete_msg` 双撤回后 `recalled_self_rows_remaining=0`、`ghost_rows_count=0` 与空输入框/搜索框终态；同步按诚实分级标定 `.reset_security_breaker` 为 `MOCK_WIN32_VERIFIED` & `Self-Test Verified`（`self-test` 14/14），`03_tree`/`06_tree` 提取为 `REAL_TREE_REPLAY_VERIFIED`）

- **Wave v5 业务风险加固宿主实机闭环档案（2026-11-16）**：[私有证据与公开版说明](PUBLIC_RELEASE_NOTES.md)（`01_live_biz_verify_sent.png`、`02_live_biz_recalled_clean.png`、`summary.json`，实测覆盖非白名单私聊历史 `403/1403`、UNC OCR `400/1400` 与大写 `BASE64://` WinRT 识图 `200`、`.handle_quick_operation` 组合禁令 `501/10004` 零发信与过期撤回 `408/1408`、`POST /api/scheduler/policy` 5 项非零传 `0` 拒收 `400/1400`、跨会话 `[CQ:reply,id=10159]` `400/1400` 拒发、`Demo测试群` 实时 `.handle_quick_operation` 引用回复（`10185` -> `10186`）`get_msg`/SQLite 1:1 核验、`delete_msg` 双气泡撤回与空输入框终态；同名群/私聊混合冲突及 `post_message_sent = true` WS 推送保持 `UNIT_TESTED` / `CONTRACT_VERIFIED` 诚实分级）

- **Wave v4 Stage 4 实机全闭环档案（2026-11-16）**：[私有证据与公开版说明](PUBLIC_RELEASE_NOTES.md)（`01_live_file_uploaded.png`、`02_live_video_sent.png`、`03_live_recalled_clean.png`、`summary.json`，覆盖能力/10004、WinRT OCR、`upload_group_file` + `get_msg` + `get_file` + `delete_msg`、原生 `[CQ:video]` + 同名/离屏防重 + `delete_msg` 与终态树/库核验）

- **第二轮独立复验报告（发现 `REVERIFY-BUG-001`..`006`）**：[私有证据与公开版说明](PUBLIC_RELEASE_NOTES.md)（原始档案位于 `docs/gpt-live-verification/evidence/20261115-reverify/`）
- **Native GUI Composer 7 场景与 13 探针实机审计档案**：[私有证据与公开版说明](PUBLIC_RELEASE_NOTES.md)
  - 基线与探针快照：`00_baseline_after_restart.png`、`00b_after_probes.png`、`probes/01_clear_search.json` .. `13_probe_forward_modal.json`
  - 分场景端侧截图与 UI 树：`01_session_baseline.png` / `01_tree.json` .. `08_final_hoyo_state.png` / `08_tree.json`、`sample_live_verify.png` / `sample_live_verify.txt`、`09_audit_summary.json`
  - 实测结论提示：`file` 上传、`forward` 引用 ID 多选合并转发卡片、私聊 `face` / `shake` 与严格 `10004` 门禁已在该轮留存证据；其中 `rps`/`dice` 因被测 QQNT `9.9.31` 面板未暴露 UIA 可点击按钮而诚实返 `501/10004`。
- **Poke `10004` 边界与 `delete_msg` / `.handle_quick_operation` 实机撤回专题实证**：
  - 初期对比与探索：[私有证据与公开版说明](PUBLIC_RELEASE_NOTES.md)、[私有证据与公开版说明](PUBLIC_RELEASE_NOTES.md)（确认群/私聊右键撤回可行，同时暴露出站 poke 无法稳定在 UIA 触发）
  - 最终收口边界 + 撤回验收：[私有证据与公开版说明](PUBLIC_RELEASE_NOTES.md)、[私有证据与公开版说明](PUBLIC_RELEASE_NOTES.md)（实证 `send_poke`/`group_poke`/`friend_poke` 与 `[CQ:poke]` 返回 `501/10004` 且零消息误发；`delete_msg` 撤回群消息 `10149`、`.handle_quick_operation` `{"delete": true}` 撤回私聊 `10150` 均同步清理 UI 与 SQLite；超窗老消息 `10137` 安全返回 `501/10004` 并保留原气泡；`audit_events` 记录 `message_recalled` 且最终恢复 `Demo测试群`）
