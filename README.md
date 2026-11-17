# starlinq-adapter / qq-uia-adapter (`v0.3.0`)

基于 **Windows UI Automation (UIA)**、**Win32 MSAA (`IAccessible`)** 与**安全光标/焦点守卫桌面交互**驱动桌面端 **QQNT (`9.9.31+`)** 的轻量、零注入机器人适配层，对外统一提供 **OneBot v11（HTTP / 正反向多角色 WebSocket / HMAC-SHA1 HTTP Webhook）**、原生 **REST / WebSocket** 与 **CLI (`qq-uia-adapter.exe`)** 接口。

> **公开副本说明**：本仓库使用独立脱敏历史，不含原始聊天截图、运行数据库或私有实机证据。测试身份均为合成值；当前版本在新电脑上的真机表现须重新核验。详见 [公开版本与验证说明](./docs/PUBLIC_RELEASE_NOTES.md)。
>
> **当前检查结果**：Release 构建、Clippy、self-test 通过；工作区测试有 1 个可重复的 Mock 焦点断言失败，QA 离线套件为 42 通过 / 1 跳过。完整范围见 [验证记录](./docs/PUBLIC_VALIDATION.json)，不能宣称全量或真机通过。
>
> **重要文档入口**：
> - **[新电脑接续开发提示词：先核验环境，再接手项目](./docs/DEVELOPMENT_HANDOFF_PROMPT.md)**（未知系统/工具链/QQ 条件的前置检查、首次克隆、路径适配、构建与分级验收）
> - **[分级 QA 测试与 Benchmark 基准工作流（L0–L4）](./docs/TESTING_AND_BENCHMARK_WORKFLOW.md)** & **[QA 缺陷审计日志](./docs/QA_DEFECT_LOG.md)**（5 级证据规范 / 13 类 43 用例无跳过全量验收与 Mock 洪峰测试 / 13 项基准 / `LIVE-BUG`、`REVERIFY-BUG`、`STAGE4-BUG`、`BIZ-RISK` 与 `LIVE-BUG-V6` 台账）
> - **[OneBot v11 协议完善进度与分层边界说明](./docs/OneBot协议完善进度.md)**（42 项支持动作 + 39 项显式 `10004` 边界 / 21 种段三层能力矩阵 / 实机证据目录）
> - **[系统架构设计](./docs/ARCHITECTURE.md)**（8 Crate 分层 / 并发锁序 / `composer/` 原生交互管线与 `.runtime/staging_uploads/` 暂存 / `uia_core` WinRT OCR / `scheduler/` 两级洪峰调度与多群轮巡 / 出站 `408/1408` 守卫）
> - **[私有证据与公开版说明](docs/PUBLIC_RELEASE_NOTES.md)**（附 [增量复验指引](./docs/gpt-live-verification/DELTA_REVERIFY_PROMPT.md)、[私有证据与公开版说明](docs/PUBLIC_RELEASE_NOTES.md) 与 [私有证据与公开版说明](docs/PUBLIC_RELEASE_NOTES.md)）
> - **[工程规范与安全红线](./AGENTS.md)**

---

## 核心特性与三层能力边界（`v0.3.0`）

### 1. OneBot v11 三层能力分层（明确拒绝静默文本伪造）

默认配置 `[onebot].allow_degraded_media_summary = false` 实行**严格模式**：凡本适配器未在 QQNT 桌面界面真实完成的富交互，默认直接返回 HTTP `501 UNSUPPORTED_ACTION` / `retcode = 10004`，绝不把媒体或指令静默伪装成纯文本发进聊天框。

- **Tier 1：真实桌面 GUI 原生执行（Native Desktop GUI Execution）**
  - **`text`**：单行优先后台 UIA；含换行符时通过 `ForegroundRestoreGuard` + `Shift+Enter` 保留硬换行。
  - **`image`**：通过 `ClipboardRestoreGuard` + `CF_DIB` 剪贴板粘贴发送真实图片气泡；入站通过 `PerMonitorAwareV2` DPI 对齐 `PrintWindow` 视口裁剪落盘 `.runtime/media/`。
  - **`face`**：支持 `define_qq_faces!` 收录的 **135 项 QQ 经典表情**（`0`..`326`），点击工具栏表情图标并在弹出面板真实点选；未知表情 ID 返回 `10004`。
  - **`at`**：群聊内输入 `@` 触发成员选择器弹窗并点选目标成员或 `@全体成员`；私聊严格返 `10004`。
  - **`reply`**：定位当前可见聊天气泡并右键点击 `回复` 挂载原生引用卡片（固定前置为首步，单条消息限 1 个；找不到气泡严格报错而非伪造文本引用）；入站解析经 `try_collect_native_quote_bubble` 抽取引用头与正文并在匹配历史时还原 `reply(id)`（未命中历史仅保留纯文本引文，不误触发 `@`）。
  - **`file`**（独占发送）：接受本地文件路径、`file://` URI、`base64://` 与 `data:*;base64,...`（上限 `16 MiB`，经 `.runtime/staging_uploads/` 有界暂存 `<=64` 份并做魔数识别；拒绝 `http(s)://`，不可与文本混发），通过剪贴板 `CF_HDROP` 粘贴后点击发送确认按钮，并回填 `MessageSegment::File.size` 供 `get_msg` 查询；对应 `upload_group_file`（非根目录 `folder` 返 `10004`）、`upload_private_file`（白名单校验）与 `.runtime/media` / `.runtime/staging_uploads` 沙箱 `get_file`。
  - **`video`**（独占发送）：`can_send_video` 返回 `{"yes": true}`；接受本地路径、`file://`、`base64://` 与 `data:*;base64,...` 经 `file_staging` + `CF_HDROP` 原生发送；远端 `http(s)://` 或与文本混发严格返回 `501 / 10004`。
  - **`ocr_image` / `.ocr_image` & `POST /api/ocr`**：基于 Windows WinRT `Windows.Media.Ocr.OcrEngine` 离线识别图片（支持 `.runtime/media/`、`.runtime/staging_uploads/`、`file://`、`base64://`、`data:` 与本地路径，返回 CJK 感知文本与 4 角点边界框）；拒绝 `http(s)://`（`501 / 10004`）与 `..` 目录穿越（`400 / 1400`）。
  - **`rps` / `dice`**（独占发送）：尝试打开魔法表情面板定位真实“猜拳 / 掷骰子”按钮点击发送；若被测 QQNT `9.9.31` UIA 未暴露对应控件，诚实返回 `501 / 10004`。
  - **`shake`**（私聊独占发送）：点击私聊工具栏窗口抖动入口；群聊或未暴露控件严格返回 `10004`。
  - **`send_*_forward_msg` / `forward_*_single_msg`**：多条合并转发要求传入同会话已入库消息引用 `node(id)`，由驱动执行**桌面右键“多选” -> 逐条勾选 -> “合并转发” -> 目标选择弹窗**真实发出卡片并持久化跨重启唯一的 `fwd-*`；单条转发（`forward_group_single_msg` / `forward_friend_single_msg`）通过气泡右键 `转发` -> 目标选择弹窗发出并入库返回新 `message_id`；入站合并转发卡经 `try_parse_forward_card_segment` + `Storage::get_or_allocate_forward_for_uid` 幂等落库，供 `get_forward_msg`（支持 `fwd-*` 与数字 `message_id`）回查。
  - **`delete_msg` 与 `.handle_quick_operation({"delete": true})`**：定位当前视口及上滚窗口内气泡 -> 右键精确点击 `MenuItem("撤回")` -> 处理确认弹窗 -> 验证气泡消失与撤回提示 -> 删除 SQLite 记录、写 `message_recalled` 审计并派发 `group_recall` / `friend_recall` 通知；被动撤回横幅通过 `MessageTracker::diff_visible_uids` + `180s` 存储回溯窗口反查恢复真实 `message_id`。

- **Tier 2：严格模式默认 `10004` 边界（`allow_degraded_media_summary = false`）**
  - **出站戳一戳、表情回应与云端管理动作**：`send_poke`、`group_poke`、`friend_poke`、`[CQ:poke]` 与 11 项新增显式 `10004` 边界（含 `set_msg_emoji_like`、`fetch_emoji_like`、`set_essence_msg`、`delete_essence_msg`、`_send_group_notice`、`_get_group_notice`、`delete_friend`、`set_qq_avatar`、`get_group_file_system_info`、`get_group_files_by_folder`、`get_group_file_url`，依据 `06_probe_bubble_menu.json` 等实测无可用 UIA 入口）统一明确返回 `501 / 10004`（未在路由表注册的未知动作返 `404 / 1404` `ACTION_NOT_FOUND`）；同时 `parser/notices.rs` 扩展解析入站 `notice.notify.poke`、`group_msg_emoji_like`、`group_admin`、`essence`、`friend_add`、`lucky_king`、`group_upload` / `offline_file` 及全员禁言/解禁（`duration: -1 / 0`）。
  - **非原生媒体与结构化卡片**：`record`（`can_send_record = false`）、`image(type=flash)`、自定义未引用 `node` 内容（含内置于 `send_msg` 的 `forward`/`node`）、`share`、`contact`、`location`、`music`、`xml`、`json`、`anonymous`，以及将 `file`/`video`/`rps`/`dice`/`shake` 与文本或内联段混发、或传入 `http(s)://` 远端媒体 URL，默认统一返回 `501 / 10004`。

- **Tier 3：显式配置降级摘要模式（`allow_degraded_media_summary = true`）**
  - 仅当下游在 `config/default.toml` 主动设置 `allow_degraded_media_summary = true` 时，才对非原生支持的媒体/卡片段退化输出可读文本摘要（例如 `[语音]` / `[视频]` / `[聊天记录]` / `[戳一戳]`），并在 `SendOutcome.degraded_segments` 如实标注。

### 2. 非侵入优先与光标瞬时复原 / 焦点安全模型（Mouse & Focus Safety Model）

- **纯读与常规纯文本路径默认非侵入**：状态轮询、元素树读取、会话历史查库与单行纯文本发送优先采用后台 UIA / `WM_CHAR`，**不移动物理光标、不主动抢占外部前景窗口**。
- **富交互动作安全守卫链**：仅当执行表情/成员弹窗点选、气泡右键菜单、多选合并转发、上滚撤回、换行（`Shift+Enter`）或剪贴板贴图/文件时启用三重保护：
  1. **光标守卫点击（[`click_point_cursor_restore_verified`](./crates/uia-core/src/action/mouse_ops.rs#L192) / [`scroll_point_cursor_restore_verified`](./crates/uia-core/src/action/mouse_ops.rs#L237)）**：先检查物理鼠标未移动且无按键按下（用户忙碌即中止），再调用 `WindowFromPoint` 验证命中坐标严格归属已绑定的 `QQ.exe` 进程及目标根/浮层句柄，并通过 `CursorPositionGuard` 在点击/滚轮后瞬时将鼠标坐标还原至操作前屏幕像素位置；
  2. **跨操作前台焦点守卫（`ForegroundRestoreGuard`）**：采用外层嵌套深度保护，在整轮发送、撤回或群资料探测结束退出作用域时，自动把焦点交还调用前的外部前台窗口；
  3. **进程级全局剪贴板守卫（`GLOBAL_CLIPBOARD_LOCK` + `ClipboardRestoreGuard`）**：贴图（`CF_DIB`）或贴文件（`CF_HDROP`）前后备份并恢复原剪贴板内容。

### 3. 实时状态对齐、两级洪峰防雪崩调度与安全护栏

- **两级洪峰防雪崩与多群轮巡（`scheduler/` + `PriorityCommandQueue`）**：
  - **`FloodScheduler` 四模块协作**：`SlidingBurstDetector`（滑动时间窗 + 滞回双阈值）判定 `Normal` / `BurstActive`，`BotRelevanceClassifier` 按 `P0`（`@机器人`/引用/命令前缀，默认 30s 窗口）> `P1`（活跃线程/`@全体`/唤醒词，20s）> `P2`（普通闲聊，8s）分层，`ResponseWindowSelector` 执行 EDF + 跨群赤字轮转（DRR）+ 洪峰期 P2 抽样/硬丢弃，`MultiSessionHarvester` 监控左侧未读与 `[有人@我]` 防饥饿切换多群；
  - **采集减负与双优先级抢占**：`PriorityCommandQueue` 保证发送/撤回/已读/状态命令严格抢占后台 `SyncAndPoll`；突发批处理自动跳过将被丢弃的 P2 图片截图并对右侧群成员同步按 10s 节流；
  - **出站截止期拦截（`408 / 1408`）**：`POST /api/send`（`response_window_ms`）、OneBot 发送与 `.handle_quick_operation` 在入口与驱动层双向核验截止期，超时立即熔断计数 `outbound_deadline_aborted_total` 并零操作返回。
- **实时 `status()` 对账与自愈轮询**：`GET /api/status` 与 `get_status` 前执行只读 UIA 树核对（空白页清零陈旧 `active_group`）；`handle_poll_messages` 在显式指定会话且检测到界面偏离或缺失容器时可自愈重开目标聊天窗格；`header_matcher` 严格排除 `Document`、`msg-`/`ml-` 及向上滚动半裁气泡防止长消息误判为群标题。
- **防误发、越权拦截与会话隔离护栏**：默认维持 `< 10` 人小群门槛（`strict_below_members = 10`、`allow_large_monitored_groups = false`）；仅当配置显式将目标群填入 `[target].monitored_groups` 并置位 `allow_large_monitored_groups = true` 时，才对受监控大群放行（未列入群仍返 `403 / 1403`）；叠加 `VerifiedWindowIdentity`、私聊好友白名单（`[target].allowed_private_friends`，覆盖发送、`get_msg`、`/api/messages/history` 与后台静默轮询）、同名群/私聊 `chat_type` 复合隔离、出站 `[CQ:reply,id=...]` 跨会话拒发（`400 / 1400`）、UNC 路径阻断、Mock 窗隔离、风控弹窗 `423 / 1423` 熔断及登录页只读禁止触碰。
- **身份 ID 语义**：已探测群的 `group_id`（如 `Demo测试群` `100000001`）在 `real_group_id != null` 时为真实 QQ 群号；当桌面无障碍树无法提供 `real_qq_id` 时，`user_id` 为 SQLite 跨表统一归并的 **FNV-1a 兼容映射 ID**，并非腾讯真实数字 QQ 号。

---

## 工作区 8-Crate 模块结构

| Crate | 路径 | 职责说明 |
| :--- | :--- | :--- |
| `common-types` | `crates/common-types` | 配置模型（含 `SchedulerConfig` 与 `GroupTargetSpec` 多群受控守卫）、领域 DTO（含 `OcrImageRequest`/`OcrImageResponse`、`SchedulerTelemetrySnapshot`/`MessagePriorityTier`/`BurstModeState`）、21 段 CQ/JSON 宏编解码（含 `File.size`）、135 QQ 表情表、408/1408 错误码与 `BotDriver` 契约（含 `ocr_image`） |
| `storage` | `crates/storage` | 嵌入式 SQLite WAL 引擎、6 张表仓储、`has_structured_segments_json`（保留 `file`/`video` 段）、`query_messages_filtered` 深页多维检索、`get_or_allocate_forward_for_uid` 幂等转发包、`resolve_recalled_message`（180s 被动撤回关联）与 `delete_message_by_onebot_id`、`cleanup_historical_pollution` 脏数据清理、跨表映射 ID 统一、跨重启唯一 `allocate_forward_nodes` 及同分无时间戳防并锚去重 |
| `uia-core` | `crates/uia-core` | Win32/UIA/MSAA 引擎、`ocr_engine.rs`（Windows WinRT `OcrEngine` 离线识图 `recognize_bgra_image`/`recognize_image_bytes`）、`VerifiedWindowIdentity`、`click_point_cursor_restore_verified`、`ForegroundRestoreGuard`、PerMonitorV2 DPI 截图裁剪、`WinEventWatcher` 与 `GLOBAL_CLIPBOARD_LOCK` |
| `qqnt-driver` | `crates/qqnt-driver` | QQNT 窗口绑定、`composer/`（`file_staging.rs` 有界暂存、原生 `[CQ:video]`/`file`/富交互、单条右键 `forward_single_message`、`reset_security_breaker` 与撤回）、`parser/`（折行文件名拼接、`try_collect_native_quote_bubble`、`diff_visible_uids`、顶边半截 `Unknown` 气泡抑制、`notice_enricher.rs` 被动撤回与扩展 `notices.rs` 分类）、`segment_enricher`（入站 `File`/`try_parse_forward_card_segment` 解析）、`poller` `group_upload`/`offline_file` 通知、`scheduler/` 两级洪峰调度与 `PriorityCommandQueue` |
| `adapter-server` | `crates/adapter-server` | Axum HTTP/WS 门面、42/39 声明式 OneBot 分发（`file_ocr_handlers.rs`、`wave6_handlers.rs`、`forward_*_single_msg`、`get_recent_contact`、`.get_audit_events` 脱敏、`.reset_security_breaker`、`can_send_video`、`ocr_image`/`.ocr_image`、`upload_*_file`、`get_file` 沙箱、`clean_cache` 双缓存清理与 `408/1408` 保护）、REST `/api/ocr`、`/api/scheduler(/policy)`、`/api/sessions/read`、`/api/forward-msg`、`/api/audit-events`、`/api/security/reset`、`/api/cache/clean`、`/api/messages/history` 多维过滤、`/api/send` 响应窗守卫、并发 WS、HMAC Webhook 与纯 Rust 热重启器 |
| `qq-cli` | `crates/qq-cli` | `qq-uia-adapter.exe` CLI 命令行（含 `ocr`、`send --video`、`scheduler`、`mark-read`、`forward`、`forward-get`、`audit`、`clean-cache`、`reset-breaker`、`messages --history` 多维过滤与 `send --response-window-ms`）、守护进程生命周期管理及 14 步全流程自检 |
| `mock-qq-win` | `crates/mock-qq-win` | 离线可重复运行的模拟 QQNT Win32/MSAA 测试窗口与多群大群（500 / 3×600）洪峰压测桩服务 |
| `qa-harness` | `crates/qa-harness` | `qq-qa-harness` L0–L4 分级质量验收（13 类 43 用例含 `CAT-12`/`CAT-13` OneBot v11/OCR/文件视频/单条转发/深页历史/审计脱敏/全缓存清理/熔断重置契约、Wave v5/v6 业务风险守卫及 500/3×600 大群洪峰 + 13 项基准含 `BM-11`/`BM-12`/`BM-13`，支持 HTML/JSON 报告输出） |

---

## 接口总览

### 1. OneBot v11 接口（`42` 支持动作 + `39` 项明确 `10004` 边界）

- **HTTP API**：`/onebot/v11/{action}` 与根路径 `/{action}`（支持 GET Query、POST JSON、`POST application/x-www-form-urlencoded` 及 `_async` / `_rate_limited` / `_async_rate_limited` 后缀），支持 `Authorization: Bearer <token>`、`Token <token>` 与 `?access_token=<token>`。未在注册表中的未知动作统一返回 HTTP `404` / `retcode = 1404`（`ACTION_NOT_FOUND`）。
- **Forward & Reverse WebSocket**：`/onebot/v11/ws`（`/ws`）、`/onebot/v11/ws/api`（`/api/ws`）、`/onebot/v11/ws/event`（`/event/ws`、`/ws/event`），及 `[onebot]` 反向 WS 三角色地址配置；心跳事件同步携带 `status.stat` 与 `status.scheduler`。
- **HTTP Webhook + 快速操作**：`[onebot].http_post_urls` + `X-Signature: sha1=<hex>`，应答 JSON 支持 `.handle_quick_operation`（含 `reply`、`at_sender`、`auto_escape`、`{"delete": true}` 原生撤回及过期 `408/1408` 保护）。

| 分类 | 动作列表 | 契约说明 |
| :--- | :--- | :--- |
| **状态 / 发送 / 转发 / 撤回 / 已读（13 项）** | `get_status`、`get_login_info`、`get_version_info`、`send_group_msg`、`send_private_msg`、`send_msg`、`forward_group_single_msg`、`forward_friend_single_msg`、`delete_msg`、`.handle_quick_operation`、`mark_msg_as_read`、`mark_group_msg_as_read`、`mark_private_msg_as_read` | `send_*` 需气泡确认入屏否则 `500/1500`，超时返 `408/1408`；`forward_*_single_msg` 原生右键 `转发` 并校验私聊白名单与群 guard；`delete_msg` 右键撤回并触发 `*_recall`；`mark_*` 清除会话未读 |
| **查询 / 历史 / 转发读 / 运维 / OCR / 文件视频（29 项）** | `get_msg`、`get_friend_list`、`get_group_list`、`get_group_info`、`get_group_member_list`、`get_group_member_info`、`get_recent_contact`、`can_send_image`（`true`）、`can_send_record`（`false`）、`can_send_video`（`true`）、`ocr_image`、`.ocr_image`、`upload_group_file`、`upload_private_file`、`get_file`、`get_stranger_info`、`get_image`、`get_group_msg_history`、`get_friend_msg_history`、`send_group_forward_msg`、`send_private_forward_msg`、`send_forward_msg`、`get_forward_msg`、`clean_cache`、`set_restart`、`.get_word_slices`、`.get_scheduler_status`、`.get_audit_events`、`.reset_security_breaker` | 含 `message_seq=0`/`reverseOrder` 深页历史、`get_recent_contact` 过滤非白名单私聊与大群、`.get_audit_events` 脱敏检索、`.reset_security_breaker` 风控弹窗核验（`423/1423` -> `200`）、`clean_cache` 双缓存目录清理、Windows WinRT 离线 OCR、`16 MiB` 文件暂存与 `4 MiB` Base64 `get_file`、`[CQ:video]` 及 `.get_scheduler_status`；详见 [OneBot协议完善进度.md](./docs/OneBot协议完善进度.md) |
| **明确 `10004` 未支持边界（39 项）** | `send_poke`、`group_poke`、`friend_poke`、`get_record`、`send_like`、`get_cookies`、`get_csrf_token`、`get_credentials`、`get_group_honor_info`、`get_group_system_msg`、`get_group_at_all_remain`、`get_essence_msg_list`、`get_group_root_files`、`download_file`、`set_qq_profile`、`set_group_kick`、`set_group_ban`、`set_group_anonymous_ban`、`set_group_whole_ban`、`set_group_admin`、`set_group_anonymous`、`set_group_card`、`set_group_name`、`set_group_leave`、`set_group_special_title`、`set_group_portrait`、`set_friend_add_request`、`set_group_add_request`、`set_msg_emoji_like`、`fetch_emoji_like`、`set_essence_msg`、`delete_essence_msg`、`_send_group_notice`、`_get_group_notice`、`delete_friend`、`set_qq_avatar`、`get_group_file_system_info`、`get_group_files_by_folder`、`get_group_file_url` | 严格返 HTTP `501` / `retcode = 10004`（`UNSUPPORTED_ACTION`），对 QQ 窗口零操作 |

### 2. 原生 REST & WebSocket 端点

| 分组 | 方法与路径 | 参数 / 请求体与行为说明 |
| :--- | :--- | :--- |
| **系统、诊断与运维重置** | `GET /api/health`、`GET /api/status`、`GET /api/windows`<br>`GET /api/diag?hwnd=`、`GET /api/tree?hwnd=&depth=`<br>`GET /api/screenshot?hwnd=`<br>`POST /api/ocr`、`POST /api/diag/probe-ui`<br>`POST /api/ensure-qq?force_wake=`、`POST /api/restart`、`POST /api/shutdown`<br>`POST /api/security/reset`、`POST /api/cache/clean` | `/api/status` 返回前执行只读 UIA 对账；`/api/screenshot` 返回无边框 `PrintWindow` PNG；`POST /api/ocr` 调用 WinRT OCR；`POST /api/security/reset` 核验风控弹窗关闭后重置熔断（仍在返 `423`）；`POST /api/cache/clean` 清理 `.runtime/media` 与 `.runtime/staging_uploads` 子目录 |
| **调度策略与已读控制** | `GET /api/scheduler`<br>`GET /api/scheduler/policy`、`POST /api/scheduler/policy`<br>`POST /api/sessions/read` | 查询当前 `SchedulerConfig` + `SchedulerTelemetrySnapshot` + 多群监控参数；支持 JSON 部分或全量热更新调度参数（校验合法阈值）；`POST /api/sessions/read` 按群聊或白名单私聊标记会话已读 |
| **会话、历史、转发与撤回** | `GET /api/sessions`<br>`GET /api/messages?group=&limit=`<br>`GET /api/messages/history`（支持 `group`/`session`/`chat_type`/`after_id`/`before_id`/`sender`/`keyword`/`limit`/`reverse_order`）<br>`POST /api/open-group`、`POST /api/open-private`<br>`POST /api/send`、`POST /api/forward-msg`、`POST /api/delete-msg` | `/api/messages/history` 为**纯 SQLite 只读历史检索**；`POST /api/send` 与 `POST /api/forward-msg` 均支持可选 `response_window_ms`（超时返 `408/1408`）；`POST /api/open-private` 校验私聊白名单；`POST /api/delete-msg` 执行原生撤回并清库 |
| **通讯录、审计与存储** | `GET /api/friends?no_cache=<bool>`、`GET /api/groups?no_cache=<bool>`<br>`GET /api/group-info`、`GET /api/group-members`<br>`GET /api/audit-events?event_kind=&limit=`<br>`GET /api/storage/stats`、`GET /api/storage`、`GET /ws/events` | `no_cache=true` 跳过缓存触发界面刷新与污点清洗；`/api/audit-events` 自动剥离非白名单私聊用户名及敏感信息；`GET /ws/events` 推送内部适配事件流 |

---

## 快速开始与常用命令

### 1. 根目录快捷脚本

- [start-adapter.bat](./start-adapter.bat)：前台启动适配服务端（读取 [config/default.toml](./config/default.toml)）。
- [start-daemon.bat](./start-daemon.bat) / [stop-daemon.bat](./stop-daemon.bat)：启动或优雅停止后台最小化本地守护进程。
- [inspect-qq.bat](./inspect-qq.bat)：导出当前 QQ 窗口 UI 树快照至 `.runtime\uia-tree.json`。
- [verify-hoyo-group.bat](./verify-hoyo-group.bat)：验证 `Demo测试群` 附着与 `< 10` 人安全护栏。

### 2. CLI 子命令总览（`.\.cache\target\release\qq-uia-adapter.exe`）

全局参数：`--config <PATH>`（默认 `config/default.toml`）、`--remote <URL>`、`--local-only`。

| 子命令 | 常用参数与说明 |
| :--- | :--- |
| `serve` / `start-daemon` / `restart` | 启动前台服务（`--port`、`--host`、`--auto-ping`）、启动后台守护或热重启 |
| `status` / `ensure-qq` / `diag` / `inspect` / `ocr` / `sessions` | 实时状态、`--force-wake` 唤醒、`--hwnd` 诊断、`--depth`/`--out` 导出树、本地 WinRT 图片 OCR（`ocr --image [--lang] [--json]`）与枚举左侧会话 |
| `messages` | `--group <名>`、`--limit <条数>`（默认 20）；`--private` 切私聊后再采集；`--history` 仅查本地 SQLite 离线历史，支持 `--before-id`、`--after-id`、`--sender`、`--keyword`、`--reverse-order` 与 `--json` |
| `send` | `--group <目标>`、`--private`（白名单好友）；支持：`--text`、`--image`、`--video`、`--file`、`--face <编号或中文名>`、`--at <QQ/昵称/all>`、`--reply <message_id>`、`--mode <auto\|background_uia\|focus_restore_unicode>`、`--response-window-ms <毫秒>` |
| `forward` / `forward-get` | `forward --message-id <ID> [--group \| --private] [--response-window-ms <毫秒>]` 执行单条原生转发；`forward-get --id <fwd-*\|ID> [--json]` 回查合并转发节点 |
| `recall`（可见别名 `delete-msg`） | `--message-id <ID>`（别名 `--id`），执行原生右键撤回并清理 SQLite 记录 |
| `scheduler` / `mark-read` | `scheduler [--json]` 查看洪峰调度器遥测与策略；`mark-read [--group <名> \| --private <好友>]` 将目标会话未读角标标记为已读 |
| `audit` / `clean-cache` / `reset-breaker` | `audit [--kind <类型>] [--limit <条数>] [--json]` 查脱敏审计流水；`clean-cache [--json]` 清空双运行缓存；`reset-breaker [--json]` 核验关窗后重置安全熔断器 |
| `friends` / `groups` / `group-info` / `group-members` | 支持 `--no-cache` 强制刷新；`group-info` 与 `group-members` 支持 `--group` / `--group-id` |
| `verify-group` / `storage` / `self-test` | 小群校验（`--below-members`、`--send-text`）、SQLite 存储统计与离线 14 步模拟窗口全链自测 |

常用示例：

```powershell
# 查看状态、洪峰调度遥测、好友与群目录、脱敏审计流水及纯离线 SQLite 历史检索
.\.cache\target\release\qq-uia-adapter.exe status
.\.cache\target\release\qq-uia-adapter.exe scheduler --json
.\.cache\target\release\qq-uia-adapter.exe audit --limit 5 --json
.\.cache\target\release\qq-uia-adapter.exe mark-read --group "Demo测试群"
.\.cache\target\release\qq-uia-adapter.exe friends --no-cache
.\.cache\target\release\qq-uia-adapter.exe groups --no-cache
.\.cache\target\release\qq-uia-adapter.exe group-members --group "Demo测试群"
.\.cache\target\release\qq-uia-adapter.exe messages --group "Demo测试群" --history --keyword "WAVE6" --reverse-order --limit 20 --json
.\.cache\target\release\qq-uia-adapter.exe messages --private --group "测试好友A" --limit 10

# 发送纯文本（带响应窗守卫）、原生表情/@/引用组合消息、单条转发、回查转发节点与撤回消息
.\.cache\target\release\qq-uia-adapter.exe send --group "Demo测试群" --text "hello from cli" --response-window-ms 15000
.\.cache\target\release\qq-uia-adapter.exe send --group "Demo测试群" --reply 10145 --at "测试好友A" --face 微笑 --text "收到"
.\.cache\target\release\qq-uia-adapter.exe forward --message-id 10192 --group "Demo测试群"
.\.cache\target\release\qq-uia-adapter.exe forward-get --id fwd-10005 --json
.\.cache\target\release\qq-uia-adapter.exe send --private --group "测试好友A" --file ".\sample.txt"
.\.cache\target\release\qq-uia-adapter.exe recall --message-id 10149
```

### 3. 核心配置（[config/default.toml](./config/default.toml)）

```toml
[server]
host = "127.0.0.1"
port = 18770

[target]
group_name = "Demo测试群"
strict_below_members = 10
require_member_count_verified = true
allowed_private_friends = []
monitored_groups = []
allow_large_monitored_groups = false

[onebot]
# allow_degraded_media_summary = false
reverse_ws_urls = []
# reverse_ws_api_urls = ["ws://127.0.0.1:6199/ws/api"]
# reverse_ws_event_urls = ["ws://127.0.0.1:6199/ws/event"]
# http_post_urls = ["http://127.0.0.1:6200/onebot/event"]
# secret = "webhook-hmac-sha1-secret"
# post_message_sent = false
# rate_limit_interval_ms = 250
heartbeat_interval_ms = 15000
reconnect_interval_ms = 3000

[scheduler]
enabled = true
burst_window_ms = 3000
burst_enter_threshold = 16
burst_exit_threshold = 6
single_poll_burst_threshold = 12
critical_response_window_ms = 30000
contextual_response_window_ms = 20000
ordinary_response_window_ms = 8000
burst_ordinary_sample_stride = 5
hard_shed_queue_depth = 64
max_pending_events = 1024
drop_expired_events = true
session_dwell_min_ms = 1200
urgent_preempt_dwell_ms = 350
patrol_interval_ms = 2500
group_quantum_messages = 8
active_thread_ttl_ms = 20000
command_prefixes = ["/", "!", "！", ".", "。"]
wake_keywords = []
```

---

## 构建门禁、QA 套件与验收实态声明

在 Windows PowerShell 下通过环境隔离包装器（[scripts/cargo-local.ps1](./scripts/cargo-local.ps1) 将 Cargo/Target/TMP 全量隔离至 `.cache/`）执行：

```powershell
python scripts\check_coding_style.py crates
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\cargo-local.ps1 fmt --all -- --check
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\cargo-local.ps1 clippy --workspace --all-targets -- -D warnings
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\cargo-local.ps1 test --workspace
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\cargo-local.ps1 build --release
.\.cache\target\release\qq-uia-adapter.exe self-test

# 运行 L0-L4 分级 QA 全量验收套件与 13 项 Benchmark（离线 Mock/契约/树回放门禁，报告输出至 .runtime/qa-reports/latest/index.html）
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\run-qa-suite.ps1 -Tier All -SkipLiveSmoke
```

**本公开副本的验证结果**：参阅 [公开版本与验证说明](./docs/PUBLIC_RELEASE_NOTES.md)。离线回归、Win32 Mock 与 WinRT OCR 分别报告；真实 QQ 端侧能力须在新机重新留证，不能由这里的通过代签。
