> 公开副本说明：示例身份与路径已替换，历史实机原始证据不公开；本文件的历史状态不代表公开版本或新电脑已完成真机验证。参阅 [公开版本说明](PUBLIC_RELEASE_NOTES.md)。

# Architecture — qq-uia-adapter

`qq-uia-adapter` 是一个面向 Windows 桌面端 **QQNT (`9.9.31+`)** 的轻量、零注入无障碍与安全桌面交互适配工作区（Rust Workspace），对外统一提供 **OneBot v11（HTTP API / 正向 WS / 反向 WS / HMAC-SHA1 HTTP Webhook）**、原生 **REST / WebSocket** 与 CLI 控制能力；工程上遵循 **非侵入后台 UIA 优先 + 富交互光标瞬时复原/前台交还守卫 + 安全红线熔断** 设计。

---

## 一、8-Crate 分层职责总览

```text
qq-uia-adapter/
├── crates/
│   ├── common-types/    # 共享配置模型（SchedulerConfig / GroupTargetSpec 多群监控守卫）、领域 DTO（OcrImageRequest / OcrImageResponse / SchedulerTelemetrySnapshot / MessagePriorityTier / BurstModeState）、21 段 CQ/JSON 宏编解码（含 File.size）、135 QQ 表情表、错误码（含 408/1408）与 BotDriver 契约（含 ocr_image）
│   ├── storage/         # 嵌入式 SQLite WAL 存储与 6 张领域表仓储（真实群号自愈、存根归并、has_structured_segments_json 保留 file/video、resolve_recalled_message 180s 被动撤回关联、delete_message_by_onebot_id、启动脏数据清洗、跨表映射 ID 统一、持久化单调递增 allocate_forward_nodes）
│   ├── uia-core/        # 单线程 COM UIA/MSAA 采集与后台键入引擎、ocr_engine.rs（WinRT OcrEngine 离线识图，CoIncrementMTAUsage + SoftwareBitmap）、VerifiedWindowIdentity、click_point_cursor_restore_verified / CursorPositionGuard、ForegroundRestoreGuard、ThreadDpiAwarenessGuard、WinEventWatcher、全局剪贴板守卫
│   ├── qqnt-driver/     # QQNT Profile、composer 原生桌面富交互编排（file_staging.rs 16 MiB/64 份有界暂存、原生 [CQ:video]/file/inline/forward/recall/probe）、parser（折行文件名拼接、diff_visible_uids + notice_enricher.rs 被动撤回与 6 类扩展通知）、segment_enricher 文件气泡、poller 上传通知、scheduler/ 两级洪峰调度与 PriorityCommandQueue
│   ├── adapter-server/  # Axum HTTP/WS 门面（OneBot 37/39 声明宏 + file_ocr_handlers.rs + REST /api/ocr、/api/scheduler(/policy)、/api/sessions/read + send 响应窗守卫）、发送真实性强校验、默认严格 10004 门禁、无锁心跳附带 stat/scheduler、多角色 WS、HMAC Webhook 与热重启器
│   ├── qq-cli/          # CLI 命令行入口（qq-uia-adapter.exe）、守护进程启停热重启、ocr / send --video / scheduler / mark-read / send --response-window-ms 与 14 步全链路自检（self-test）
│   ├── mock-qq-win/     # 离线可重复运行的模拟 QQNT Win32/MSAA 测试窗口与多群并发洪峰测试桩服务（MockQqServer::spawn_with_groups / inject_incoming_batch）
│   └── qa-harness/      # L0-L4 分级 QA 自动化验收引擎（qq-qa-harness，12 类 38 用例含 CAT-12 OCR/视频文件契约及 500/3×600 大群洪峰、12 项基准统计含 BM-11/BM-12 及 HTML 报告生成）
├── config/default.toml  # 默认服务端、安全守卫与 OneBot 多通道传输配置
└── scripts/             # 本地缓存隔离构建包装器（cargo-local.ps1 / env.ps1）、QA 套件入口（run-qa-suite.ps1）与代码规范检查（check_coding_style.py）
```

---

## 二、多线程并发模型与严格有向无环锁层级（Lock Hierarchy）

桌面 UIA 与网络服务并发运行涉及 Axum Tokio 异步任务、独立 Win32 消息泵线程与 COM 工作线程，项目通过严格的有向无环锁序（DAG）与临界区最小化设计消除死锁与竞态条件：

1. **COM 线程隔离边界与双优先级队列（Worker Isolation & `PriorityCommandQueue`）**
   Windows UIA COM 对象（`IUIAutomation`、`UIElement`）具有公寓亲和性且为 `!Send`。`QqntDriver` 在专用系统线程 `qqnt-uia-worker` 中初始化 COM（`COINIT_MULTITHREADED`），上层异步协程通过 [`PriorityCommandQueue`](../crates/qqnt-driver/src/driver/commands.rs#L129-L182) + `tokio::sync::oneshot` 跨线程通信，保证跨越线程边界的只有纯 `Send` 值类型 DTO；队列内部维护 `urgent` 与 `background` 双双端队列及 `Condvar`，所有发送、状态探查、已读清除、切群与撤回指令严格抢占后台周期 `SyncAndPoll` 轮询执行。
2. **异步限流槽位预留（Slot-Reservation Rate Limiter）**
   针对 `_*_rate_limited` 异步排队动作（`crates/adapter-server/src/onebot/macros.rs`），`AppState::rate_limit_gate`（`tokio::sync::Mutex<Option<Instant>>`）**仅在微秒级临界区内计算并写入下一个执行时间戳槽位后即刻解锁**，随后调用方在不持锁状态下并行 `tokio::select! { wait_shutdown | sleep_until(deadline) }`，避免持锁睡眠造成后续并发请求队列卡死。
3. **写操作通道互斥锁（`AppState::send_gate` -> `SharedInner::send_gate`）**
   `adapter-server` 层的 `POST /api/open-group`、`POST /api/open-private`、`POST /api/send`、`POST /api/delete-msg`、`POST /api/diag/probe-ui` 与 OneBot 发送/撤回/合并转发动作统一先获取 `AppState::send_gate.lock().await`，将“状态检查 -> 条件切换窗口 -> 原生交互或撤回投递”作为原子复合流程串行化；底层 `QqntDriver` 写指令再获取 `SharedInner::send_gate` 协同人类节律冷却，外层绝不反向嵌套内层锁，彻底杜绝并发切换会话导致的发串群竞态。
4. **短临界区内存状态与锁外 SQLite WAL I/O**
   `SharedInner::state` 与 `MessageTracker` 使用短持有期 `parking_lot::Mutex`（仅在纯内存状态更新/切片读时加锁，**绝不在持有内存锁时调用 SQLite 磁盘 I/O 或 COM 遍历**）；嵌入式 SQLite 开启 `WAL` 与 `busy_timeout = 5000`，`Storage::allocate_forward_nodes` 等关键写事务使用 `TransactionBehavior::Immediate` 保证单调分配与幂等归并。
5. **跨线程全局剪贴板锁与 RAII 守卫（`GLOBAL_CLIPBOARD_LOCK`）**
   `uia-core` 在读写系统剪贴板写入 `CF_DIB` 位图（图片）或 `CF_HDROP` 文件列表（本地文件上传）前，获取进程级 `Mutex` 并创建 `ClipboardRestoreGuard`，确保多协程测试或重入时剪贴板内容完整备份与退出自动恢复。
6. **无丢失优雅停机与可取消热重启（Shutdown & Restart Cancellation）**
   - `AppState::wait_shutdown` 先 `AtomicBool::load(Acquire)`，注册并 `enable()` `tokio::sync::Notify::notified()` 后**再次二次核验标志**，彻底消除 `shutdown` 信号早于 `.await` 到达导致的丢失唤醒。
   - `schedule_daemon_restart` 在 `delay_ms` 等待窗内使用 `tokio::select!` 监听 `state.wait_shutdown()`；若收到停机指令则立即取消延时并复位 `restart_in_progress`。
   - 后台反向 WS 与 HTTP Webhook 协程全部托管于 `ReverseTaskGuard`，服务退出时由 `Drop` 统一 `abort()` 清理。

### 锁获取拓扑图（Strict DAG）

```text
[AppState::rate_limit_gate (O(1) 计算时间戳后立即解锁)]
         │ (不跨 await 睡眠持锁)
         ▼
[AppState::send_gate (序列化切窗、发送、撤回与 UI 探针)]
         │
         ▼
[SharedInner::send_gate (节律冷却与出站 deadline 校验)]
         │
         ▼ (PriorityCommandQueue 紧急队列 > SyncAndPoll 后台队列)
[qqnt-uia-worker COM 单线程] ──▶ [SharedInner::state / scheduler / MessageTracker (短持锁)]
         │                                       │ (先释放内存锁，再执行磁盘写入)
         ├──▶ [GLOBAL_CLIPBOARD_LOCK]            ▼
         └──▶ [Storage WAL Mutex] ◀──────────────┘
```

---

## 三、声明式宏代码生成体系（Declarative Macro Codegen）

为压缩机械性协议映射样板代码并保证“单点真源”（Single Source of Truth），工作区核心协议层广泛使用零外部过程宏依赖的 `macro_rules!` 生成代码：

| 宏名称 | 所在位置 | 生成产出与收益 |
| :--- | :--- | :--- |
| `define_string_constants!` | `crates/common-types/src/constants/routes_actions.rs` | 集中生成 OneBot/REST 路由（含 `/api/open-private`、`/api/delete-msg`、`/api/messages/history`）、动作与协议字符串常量 |
| `define_qq_faces!` | `crates/common-types/src/onebot/face_table.rs` | 由同一张 **135 项 QQ 经典表情表**（`0`..`326`）同步展开 `face_id_to_name` 与 `face_name_to_id`，确保出站面板点击与入站反查完全对称 |
| `cq_encode_tag!` / `json_opt_scalar!` | `crates/common-types/src/onebot/segment_codec.rs` | 统一展开 **21 种 OneBot 消息段** 的必填/可选 CQ 标签转义组装与 JSON 宽容标量字段读取 |
| `define_onebot_actions!` | [`crates/adapter-server/src/onebot/handlers.rs`](../crates/adapter-server/src/onebot/handlers.rs#L755-L951) & `macros.rs` | 单表同步生成 **37 项支持动作**（新增 `can_send_video`、`ocr_image`/`.ocr_image`、`upload_group_file`、`upload_private_file`、`get_file`）与 **39 项明确 `10004` 未支持边界**（移除 `ocr_image`/`.ocr_image`，新增 `set_msg_emoji_like`/`fetch_emoji_like`、`set_essence_msg`/`delete_essence_msg`、`_send_group_notice`/`_get_group_notice`、`delete_friend`、`set_qq_avatar`、`get_group_file_system_info`/`get_group_files_by_folder`/`get_group_file_url`；未注册动作返 `404 / 1404` `ACTION_NOT_FOUND`）；自动派生 `lookup_registered_action`、`_async`/`_rate_limited` 修饰分发、`execute_onebot_ws_envelope`、`http_onebot_action_dispatch` 与 `/onebot/v11/{action}` + `/{action}` 双路由 |
| `define_bracket_placeholder_rules!` | `crates/qqnt-driver/src/driver/segment_enricher.rs` | 由声明表统一生成入站 `[语音]`、`[视频]`、`[合并转发]` 与系统表情 `[标签]` 到 `MessageSegment` 的映射规则 |

---

## 四、`qqnt-driver/src/composer/` 原生桌面交互编排管线

`qqnt-driver/src/composer/` 将早先富媒体纯文本占位升级为面向 QQNT 桌面控件的真实交互执行引擎，同时对不支持的能力实行严格 `501 / 10004` 边界收口：

| 子模块 | 核心类型 / 入口 | 职责与执行流 |
| :--- | :--- | :--- |
| `plan.rs` | `build_composer_plan`、`ComposerExecutionPlan`、`InlineStep`、`StandaloneAction` | 根据 `[onebot].allow_degraded_media_summary`（默认 `false`）及请求段构造执行计划：<br>• `PurePlainTextOrImage`：纯文本 / 常规图片直接交由常规低扰动通道；<br>• `RichInline`：编排可组合序列（前置 `AttachReply` 右键回复、135 项 `InsertFace` 面板点击、群聊 `InsertAt` 成员弹窗、`TypeText` `Shift+Enter` 换行、`PasteImage` `CF_DIB`）；<br>• `Standalone`：独占消息（经 `file_staging` 暂存的本地/`base64://`/`data:` `File` 与 `Video` 走 `CF_HDROP`、魔法 `Rps`/`Dice`、私聊 `PrivateShake`），严禁与文本/其他段混发；<br>• 远端 `http(s)://` 文件/视频、非根目录 `folder`、非法混发、未知 `face id`、私聊 `@`、群内 `shake`、`poke`、`record`、`flash`、自定义未引用 `node` 与卡片段统一返回 `10004`；<br>• `DegradedLegacy`：仅当显式开启 `allow_degraded_media_summary = true` 时启用。 |
| `inline_ops.rs` | `execute_rich_inline_steps`、`RichComposeContext` | 按序执行内联步骤：`AttachReply` 定位可见气泡后右键点击 `回复` 挂载引用卡 -> 打开 `表情` 弹窗点选经典表情 -> 输入 `@` 匹配成员弹出菜单点击 -> 键入正文 -> 剪贴板写入 `CF_DIB` 贴图 -> 统一提交，并校验消息气泡在窗口确认入屏。 |
| `standalone_ops.rs` & `file_staging.rs` | `execute_standalone_action`、`stage_outbound_file` | `file_staging.rs` 将本地/`file://`/`base64://`/`data:` 媒体（`<=16 MiB`）有界暂存至 `.runtime/staging_uploads/`（超 `64` 个按修改时间淘汰，校验文件魔数并回填 `File.size`）；`execute_standalone_action` 经 `ClipboardRestoreGuard` + `write_files_to_clipboard_hdrop`（`CF_HDROP`）粘贴发送 `File` 与原生 `[CQ:video]`；`Rps`/`Dice` 尝试打开魔法面板（未暴露控件返 `10004`）；`PrivateShake` 点击私聊工具栏抖动入口。 |
| `forward_ops.rs` | `handle_send_forward_messages` | 严格模式要求全部节点均为同会话已入库的引用 `node(id)`：定位历史气泡 -> 右键“多选” -> 逐项勾选复选框（上限 `MAX_NATIVE_FORWARD_MESSAGES = 50`）-> 点击“合并转发” -> 在目标选择浮窗选取目标群/好友完成卡片发送 -> 调用 `Storage::allocate_forward_nodes` 生成全局唯一 `fwd-*` 并入库 `messages`。 |
| `recall_ops.rs` | `handle_delete_message` | 根据 `onebot_message_id` 查询记录与校验群/私聊权限 -> 在当前视口及最多 24 步上滚窗口内定位气泡 -> 右键气泡精确匹配 `ControlType::MenuItem && Name == "撤回"` -> 处理二次确认弹窗 -> 校验原气泡已移除及撤回提示出现 -> 同步 `Storage::delete_message_by_id`、写 `message_recalled` 审计事件并派发 `group_recall`/`friend_recall` 通知；超窗或无权气泡严格返 `10004` 且零副作用。 |
| `probe_ops.rs` & `popup_helpers.rs` | `handle_probe_ui_action` | 提供 `POST /api/diag/probe-ui` 所需的 7 类只读可自动清理界面探针（`clear_search` / `selfcheck_state` / `probe_emoji` / `probe_at` / `probe_bubble_menu` / `probe_multiselect` / `probe_forward_modal`）及浮层窗口扫描/点选辅助。 |

---

## 五、鼠标/焦点安全模型、高 DPI 截图与状态自愈

### 5.1 非侵入后台优先 + 光标瞬时复原与前台焦点守卫

- **背景 UIA 优先**：窗口刷新、`dump_tree` 树读取、会话列表/头部解析、以及无换行/无富交互面板的纯文本发送优先走 `ShowWindowAsync(SW_SHOWNOACTIVATE)`、`BackgroundUia` 与控件 Pattern，不移动用户物理鼠标也不抢占外部应用焦点。
- **光标瞬时复原守卫（`crates/uia-core/src/action/mouse_ops.rs`）**：
  在 `composer` 面板点选、气泡右键菜单或上滚定位必须注入鼠标事件时，统一通过 [`click_point_cursor_restore_verified`](../crates/uia-core/src/action/mouse_ops.rs#L192) 与 [`scroll_point_cursor_restore_verified`](../crates/uia-core/src/action/mouse_ops.rs#L237)：
  1. `prepare_verified_cursor_point` 循环检测用户鼠标是否正在移动或按键（`is_user_mouse_busy` / `are_mouse_buttons_pressed`），BUSY 时立即中止返错，绝不与真实用户抢夺鼠标；
  2. 验证 `VerifiedWindowIdentity` 未发生 PID 复用/窗口偏移，并通过 `WindowFromPoint` 校验目标坐标严格落于绑定 `QQ.exe` 进程与目标根/浮层 HWND 内；
  3. 构造 `CursorPositionGuard::capture()` 记录原光标位置，执行瞬时移动、点击/滚轮后在 `Drop` 时调用 `SetCursorPos` 立即把光标还原至操作前屏幕像素坐标。
- **跨操作前台焦点恢复（`ForegroundRestoreGuard`）**：
  [`focus_guard.rs`](../crates/uia-core/src/action/focus_guard.rs) 借助 `OUTER_RESTORE_DEPTH` 线程局部嵌套深度保护（防止内层步骤提前把焦点交还而打断外层复合发送/撤回），在全链路结束离开作用域时，若当前未按鼠标键且前台仍停留在 `QQ.exe` 进程组窗口上，则安全归还调用前的外部应用前景句柄。

### 5.2 高 DPI `PrintWindow` 截图与视口裁剪

- `crates/uia-core/src/dpi_capture.rs` 通过 `ThreadDpiAwarenessGuard::enter_per_monitor_v2()` 动态绑定 `user32.dll!SetThreadDpiAwarenessContext` 进入 `PerMonitorAwareV2`，完成任务析构恢复；
- `scale_crop_rect_for_dpi` 将逻辑 UIA 元素边界等比缩放对齐至物理 `PrintWindow` 位图坐标；
- `is_solid_interior_bgra` 检测全同色/空白占位渲染帧并在采图时重试避开未绘制完帧，同时支撑 `GET /api/screenshot` 导出无边框窗口 PNG 快照与入站图片气泡落地 `.runtime/media/`。

### 5.3 状态实时对账、自愈轮询与防半裁气泡标题误判

1. **实时状态对账（`window_discovery::handle_status` & `reconcile_status_from_tree`）**：
   - **WS 心跳慢发送隔离**：WS 定时器优先调用无锁 [`BotDriver::cached_status()`](../crates/common-types/src/driver_trait.rs)，保障慢速 UIA 发信期间心跳 RTT `< 1ms`。
   - **API 实时校准**：当 REST `/api/status` 或 OneBot `get_status` 主动查询状态时，驱动进入 [`handle_status`](../crates/qqnt-driver/src/driver/window_discovery.rs#L77)：检查附加窗口是否有效或最小化；未最小化时执行只读 `session.dump_tree` 交由 `reconcile_status_from_tree` 与 `resolve_live_chat_kind` 核对实时聊天标题、会话类型（群/私聊）与成员数；若主窗口处于空白页或缺乏消息列表容器，自动清空陈旧的 `active_group` 与 `detected_member_count`（修复 `REVERIFY-BUG-006`）。
2. **显式轮询自愈重开（`group_handlers::handle_poll_messages`）**：
   轮询拉取先通过 `inspect_poll_tree_target` 检查是否存在消息列表容器及顶部标题是否匹配请求目标；若发现容器缺失或标题已偏离当前请求会话，先调用 `reconcile_poll_tree_mismatch` 刷新内部实时状态，当处于显式唤醒切群模式且指定了目标会话名时，自动执行 `reopen_poll_target` 重新定位打开目标群或私聊窗格后再采样一次，彻底告别“窗口切走后显式轮询空返报错”。
3. **滚动半裁气泡标题守卫（`matcher/header_matcher.rs`）**：
   聊天历史向上滚动后，位于视口最顶部的半截长消息可能穿透并进入标题栏矩形采样区。`header_matcher` 现显式过滤 `Document` 类型、`automation_id` 以 `"msg-"` / `"ml-"` 开头的消息子树节点，以及水平重叠 `ml-root` 且满足 `snapshot.rect[3] > ml_rect[1] + 2 || snapshot.rect[1] >= ml_rect[1] - 6` 的顶部侵入文本节点，防止长气泡正文被误判为群标题而阻断后续发信。
4. **持久化不撞库转发存储（`Storage::allocate_forward_nodes`）**：
   [`meta_audit_repo.rs`](../crates/storage/src/repositories/meta_audit_repo.rs#L17) 在 `TransactionBehavior::Immediate` 事务内联合查询 `kv_meta["forward_seq"]` 与全部 `forward:fwd-%` 键最大后缀（保底 `10000`），循环探空后单调递增插入新转发包并更新序标（容量上限 512 淘汰），彻底根治跨进程重启复用覆盖旧转发记录的隐患（`REVERIFY-BUG-005`）。

---

## 六、高并发洪峰防雪崩调度、多群轮巡与出站截止期守卫（Two-Stage Flood Control）

面向单一数百人活跃大群瞬时刷屏以及多群并发洪峰场景，`qqnt-driver`、`common-types` 与 `adapter-server` 协作建立“采集侧轻量化减负 + 两级四模块响应窗调度 + 出站截止期拦截”闭环：

1. **采集侧双优先级抢占与洪峰减负**：
   - **`PriorityCommandQueue` 紧急抢占**（[`crates/qqnt-driver/src/driver/commands.rs`](../crates/qqnt-driver/src/driver/commands.rs#L129-L182)）：发送、撤回、已读清除、状态探查等人工/API 指令落入 `urgent` 队列，后台周期性 `SyncAndPoll` 轮询落入 `background` 队列，严格保障发信不被高密度采集饿死。
   - **P2 裁图预剥离与群成员同步节流**（[`group_handlers.rs`](../crates/qqnt-driver/src/driver/group_handlers.rs#L864-L937)）：轮询树提取富消息后先调用 `strip_shed_p2_image_rects_on_burst` -> `FloodScheduler::pre_classify_keep_image_mask`，对洪峰期即将选择性丢弃的 P2 普通闲聊提前清空图片矩形以跳过耗时 PerMonitorV2 DPI `PrintWindow` 截图落盘；右侧群成员栏持久化 `persist_sidebar_members_from_tree` 受 [`SIDEBAR_MEMBER_REFRESH_INTERVAL_MS`](../crates/qqnt-driver/src/constants.rs#L15)（`10_000 ms`）节流避免高频扫树重复写 SQLite。

2. **四模块 `FloodScheduler` 核心流水线**（[`crates/qqnt-driver/src/scheduler/mod.rs`](../crates/qqnt-driver/src/scheduler/mod.rs#L24-L169) 与 [`SchedulerConfig`](../crates/common-types/src/config/scheduler_config.rs#L8-L75)）：

| 模块 | 源码位置 | 职责、关键状态与默认策略参数 |
| :--- | :--- | :--- |
| `SlidingBurstDetector` | [`burst_detector.rs`](../crates/qqnt-driver/src/scheduler/burst_detector.rs#L9) | 在滑动窗口 `burst_window_ms`（`3000 ms`）内统计到达速率并结合单轮增量 `single_poll_burst_threshold`（`12`）触发 `BurstModeState::BurstActive`；通过滞回双阈值（`burst_enter_threshold = 16` / `burst_exit_threshold = 6`）避免流量边界抖动。 |
| `BotRelevanceClassifier` | [`classifier.rs`](../crates/qqnt-driver/src/scheduler/classifier.rs#L21) | 按相关性与上下文分为三层：`P0` `CriticalBotTrigger`（`@机器人`/引用机器人/命令前缀 `command_prefixes = ["/", "!", "！", ".", "。"]`，响应窗 `critical_response_window_ms = 30000 ms`）、`P1` `HighContextual`（活跃线程 `active_thread_ttl_ms = 20000 ms` / `@全体` / 唤醒词，响应窗 `contextual_response_window_ms = 20000 ms`）、`P2` `OrdinaryChatter`（普通闲聊，响应窗 `ordinary_response_window_ms = 8000 ms`）。 |
| `ResponseWindowSelector` | [`event_selector.rs`](../crates/qqnt-driver/src/scheduler/event_selector.rs#L21) | 严格执行 **`P0 > P1 > P2`** 优先顺序 + 同级 **最早截止优先（EDF）** + 跨群赤字轮转配额（DRR，`group_quantum_messages = 8`）；洪峰期对 P2 执行确定性抽样（`burst_ordinary_sample_stride = 5`）、超出 `hard_shed_queue_depth`（`64`）硬丢弃、总额封顶 `max_pending_events`（`1024`），并在 `drop_expired_events = true` 时自动淘汰超窗消息计入 `expired_window_total`。 |
| `MultiSessionHarvester` | [`session_harvester.rs`](../crates/qqnt-driver/src/scheduler/session_harvester.rs#L37) | 持续观测左侧会话未读增量、`has_at_me_hint`（`[有人@我]`）与 `has_at_all_hint`，遵循 `session_dwell_min_ms`（`1200 ms`）、P0 紧急抢占 `urgent_preempt_dwell_ms`（`350 ms`）与后台轮巡周期 `patrol_interval_ms`（`2500 ms`）在多个受监控群聊之间防饥饿轮转切换。 |

3. **出站响应窗守卫（`408 / 1408`）与大群受控监控边界**：
   - **双向截止期熔断**：`adapter-server` 发送入口（[`ensure_within_outbound_deadline`](../crates/adapter-server/src/onebot/handlers.rs#L35-L66)）及 `.handle_quick_operation` 上下文检查 `response_deadline_ms` / `captured_at_ms`、`POST /api/send` 检查 `response_window_ms`，叠加 `qqnt-driver` 执行前 [`SharedInner::check_outbound_deadline`](../crates/qqnt-driver/src/driver/runtime_state.rs#L238-L258) 复核 `deadline_unix_ms`；一旦过期立即中止并累加 `outbound_deadline_aborted_total`，返回 `AdapterError::ResponseWindowExpired`（HTTP `408 REQUEST_TIMEOUT` / OneBot `retcode = 1408`），**对 QQ 窗口零 UIA 误发**。
   - **受控多群监控放行**（[`session_target.rs`](../crates/common-types/src/messaging/session_target.rs#L58-L103)）：默认 `[target].allow_large_monitored_groups = false` 维持 `strict_below_members = 10` 安全守卫；仅当管理员将目标群明确写入 `[target].monitored_groups` 且显式置位 `allow_large_monitored_groups = true` 时，`GroupTargetSpec::member_guard_for_group` 才对名单内大群放开成员人数限制，名单外大群仍严格返 `403 / 1403` 拦截。

---

## 七、外部接口面、CLI 与本地隔离工具链

- **OneBot v11 动作与段能力**：`37` 项已支持动作（含 `can_send_video`、`ocr_image`/`.ocr_image`、`upload_group_file`、`upload_private_file`、`get_file`、`delete_msg`、`.handle_quick_operation`、`mark_*_msg_as_read`、`.get_word_slices`、`.get_scheduler_status`） + `39` 项显式 `10004` 边界（含 `set_msg_emoji_like`、`fetch_emoji_like`、`set_essence_msg`、`delete_essence_msg`、`_send_group_notice`、`_get_group_notice`、`delete_friend`、`set_qq_avatar`、`get_group_file_system_info`、`get_group_files_by_folder`、`get_group_file_url` 等；未知未注册动作返 `404 / 1404` `ACTION_NOT_FOUND`）；全量 21 种段分为 Tier 1 原生执行（含独立 `file`/`video`） / Tier 2 严格拒绝 `10004` / Tier 3 显式 `allow_degraded_media_summary = true` 降级摘要。
- **原生 REST 端点（`adapter-server/src/lib.rs`）**：
  - 系统与诊断：`GET /api/health`、`GET /api/status`、`GET /api/windows`、`GET /api/diag`、`GET /api/tree`、`GET /api/screenshot`、`POST /api/ocr`（WinRT 本地 OCR）、`POST /api/diag/probe-ui`、`POST /api/ensure-qq`、`POST /api/restart`、`POST /api/shutdown`；
  - 调度策略与已读控制：`GET /api/scheduler`、`GET/POST /api/scheduler/policy`、`POST /api/sessions/read`；
  - 会话、消息与撤回：`GET /api/sessions`、`GET /api/messages`、`GET /api/messages/history`（纯 SQLite 只读历史）、`POST /api/open-group`、`POST /api/open-private`、`POST /api/send`（支持可选 `response_window_ms`）、`POST /api/delete-msg`、`GET /ws/events`；
  - 存储与目录：`GET /api/friends?no_cache=<bool>`、`GET /api/groups?no_cache=<bool>`、`GET /api/group-info`、`GET /api/group-members`、`GET /api/storage/stats`、`GET /api/storage`。
- **CLI 控制命令（`crates/qq-cli/src/cli_args.rs`）**：新增 `ocr [--image] [--lang] [--json]`、`send --video <PATH>`、`scheduler [--json]`、`mark-read [--group|--private]`、`send --response-window-ms <ms>` 与 `self-test`（14 步全链路自测）。
- **本地隔离工具链（`.cache/` Containment）**：
  工作区 Cargo 缓存及编译产物全量隔离在 `.cache/cargo`、`.cache/target`、`.cache/tmp`：
  - 环境/编译入口：[`scripts/env.ps1`](../scripts/env.ps1)、[`scripts/cargo-local.ps1`](../scripts/cargo-local.ps1)
  - 静态风格门禁：[`scripts/check_coding_style.py`](../scripts/check_coding_style.py)（单文件有效行 `<= 1000`、单目录 `.rs` 文件 `<= 15`、单文件类型定义 `<= 3`、函数参数 `<= 6` 及简体中文文档注释检查）
  - QA 套件入口：[`scripts/run-qa-suite.ps1`](../scripts/run-qa-suite.ps1)（驱动 `qq-qa-harness` 生成 L0-L4 报告至 `.runtime/qa-reports/latest/index.html`）
