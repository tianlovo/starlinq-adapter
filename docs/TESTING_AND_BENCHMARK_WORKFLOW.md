> 公开副本说明：示例身份与路径已替换，历史实机原始证据不公开；本文件的历史状态不代表公开版本或新电脑已完成真机验证。参阅 [公开版本说明](PUBLIC_RELEASE_NOTES.md)。

# qq-uia-adapter 分级测试与行业标准基准工作流（L0 - L4）

本文档定义 `qq-uia-adapter` 的官方持续质量检验与性能基准流程。所有迭代发布前均须通过 `scripts/run-qa-suite.ps1` 按等级执行对应测试集与统计基准，并产出可视化报告（`.runtime/qa-reports/latest/index.html` 与 `qa-report.json`）。

---

## 1. 分级检验矩阵（L0 - L4）

| 等级 | 名称 | 包含测试项目（全 13 类 · 43 用例 + 13 项基准） | 触发时机 | 核心门禁阈值（SLA） |
| :--- | :--- | :--- | :--- | :--- |
| **L0** | **快速冒烟** | 1. **冒烟测试（Smoke）**（3 用例；`--skip-live` 时跳过 1 项在线探活） | 每次热重启、本地提交后 | 隔离服务端健康/WS 握手 `100%`；若 `127.0.0.1:18770` 运行中则只读探活且 `security_breaker_tripped == false && sandbox_isolated == false` |
| **L1** | **桩与边界门禁** | 2. **打桩测试（Stub/Mock）**<br>3. **数据边界测试（Data Boundary）**<br>4. **条件边界测试（Condition Boundary）**<br>5. **OneBot v11 扩展契约与 OCR/文件视频验证（CAT-12 · Protocol v4 & Wave v5 Guards）**<br>6. **Wave v6 协议与运维契约验证（CAT-13 · Deep History / Single Forward / Contacts / Audit / Cache / Breaker Reset）**<br>叠加全量 `--workspace` 单元测试 | 每次 PR / 功能修改 | 故障注入状态码精确映射（`423`/`403`/`401`/`408`/`413`/`500`/`10004`）；UTF-8 / `%XX` / `u64::MAX` / 巢状 CQ `< 10ms`；WinRT 离线 OCR（`"HOYO 2026"`）、原生 `[CQ:video]`/`upload_*_file`/`get_file` 沙箱、11 项新 `10004` 动作、Wave v5 风险守卫及 Wave v6 深分页/单条转发/最近联系人/审计脱敏/子目录清缓存/熔断复位（`423->200`）全达标 |
| **L2** | **并发与全链路集成** | 7. **并发测试（Concurrency）**<br>8. **综合集成测试（Integration）** | 合入主分支前 | `64` HTTP + `16` WS + `16` OS SQLite 线程 `0` 死锁 / `0` `SQLITE_BUSY`；`MockQqServer` + `QqntDriver` 含 500 人大群无损与 104 并发洪峰 P0 优先/滞回全绿 |
| **L3** | **极限与系统负载** | 9. **压力测试（Stress）**<br>10. **极限暴力测试（Brute-Force）**<br>11. **性能测试（Perf SLA）**<br>12. **内存负载测试（Memory Load）**<br>13. **CPU 负载测试（CPU Load）** | 版本发布 / 架构重构前 | `4,000` 高并发 + 3×600 超大群并发洪峰轮转 `0` 5xx 且 P0 全保活；畸形 TCP / Fuzz / `_async` 风暴恢复 `< 50ms`；HTTP `P50 < 3ms` / `P99 < 30ms`；内存增量 `< 64 MiB`；空闲 CPU `< 5.0%` |
| **L4** | **行业标准 Benchmark** | **13 项 Criterion/Google Benchmark 统计基准（`BM-01`..`BM-13`）** | 性能优化 / Release 归档 | `120ms` 预热 + 自适应校准 + `40` 样本 + `std::hint::black_box` + `1,000` 次 Bootstrap `95% CI` + `MAD` + `RME%` + Tukey `1.5*IQR` 离群分析 |

---

## 2. 13 大测试类别（43 用例）与 13 项 Benchmark 明细

### 13 大测试类别（共计 43 条用例；`--skip-live` 下执行 `42/43` 通过、`1` 跳过）
1. **冒烟测试（Smoke · `CAT-01`，3 用例）**：隔离短命服务器全套基本探针 + 在线守护进程 `127.0.0.1:18770` 只读健康/状态/存储统计与内存/CPU 采集（传入 `--skip-live` 时单独计为跳过）。
2. **打桩测试（Stub/Mock · `CAT-02`，9 用例）**：通过可编程故障注入桩（`ConfigurableStubDriver`）验证安全熔断 `423/1423`、群人数超限 `403/1403`、UIA 失效或未确认入屏（`ok=false`）`500/1500`、私聊白名单拦截 `403/1403`、出站 `deadline_unix_ms` 与 `.handle_quick_operation` 陈旧 `captured_at_ms` 触发 `408/1408` 零发信、`mark_*_msg_as_read`（含非白名单私聊拦截）、`.get_word_slices`、`.get_scheduler_status`、`GET/POST /api/scheduler(/policy)`、升级事件/`get_msg`/`get_status.stat`/WS 心跳 `scheduler`、`record` 默认禁降级与边界 `10004` 矩阵（`set_msg_emoji_like` / `set_essence_msg` 等）、HMAC-SHA1 HTTP Webhook 签名与反向 WS 握手头。
   - **真实 UI 树离线重放补强（`REAL_TREE_REPLAY_VERIFIED` / `OFFLINE_REAL_TREE_REPLAY`）**：`crates/qqnt-driver/tests/live_evidence_replay.rs`（12 用例）与 `qqnt-driver::parser::replay_tests` 直接使用真实 QQNT UI 树固件与实机捕获回归样本（含 `03_tree.json` 原生引用气泡与未命中历史隔离、`06_tree.json` 原生合并转发卡提取与 `fwd-*` 幂等复用、`04_tree.json` 折行文件名合并、`06_probe_bubble_menu.json` 表情回应通知提取、多气泡切分 `LIVE-BUG-002`、会话边界 `LIVE-BUG-005`、视口裁剪 `LIVE-BUG-006`、`BUG-LIVE-V4-002`/`003`/`004` 文件视频防重及 `LIVE-BUG-V6-001` 顶边半裁无主贴图过滤）执行离线回放回归。
3. **数据边界测试（Data Boundary · `CAT-03`，4 用例）**：覆盖空串/纯空白、NUL/RTL/4 字节 Emoji/`64 KiB` 负载、多字节 `%XX` 截断（`QA-BUG-001`）、非 ASCII `Authorization`（`QA-BUG-002`）、病理巢状 CQ（`QA-BUG-007`）、`u64::MAX` 时间戳饱和（`QA-BUG-008`）、`>8 MiB` `Content-Length` 预检拒收 `413`（`QA-BUG-011`）、超大 `16384x16384` WIC 图像头（`QA-BUG-004`）、`MAX_FORWARD_NODES + 1`（`QA-BUG-009`）。
4. **条件边界测试（Condition Boundary · `CAT-04`，2 用例）**：覆盖 `strict_below_members` 临界 `9` 与 `10`、`require_member_count_verified` 开关、私聊白名单空/大小写/规范化匹配、Token/Bearer/Query 鉴权矩阵、`Universal`/`ApiOnly`/`EventOnly` 三角色 WS 边界、`_async`/`_rate_limited` 修饰后缀、`allow_mock=false` 安全弹窗隔离（`QA-BUG-010`）。
5. **OneBot v11 扩展契约与 OCR/文件视频验证（Protocol v4 · `CAT-12`，5 用例）**：归类为 `CONTRACT_VERIFIED` + `WINRT_LOCAL_OCR_VERIFIED`——覆盖（a）`ocr_image`/`.ocr_image`/`POST /api/ocr` 调用真实 Windows WinRT OCR 识别合成 `"HOYO 2026"` PNG `base64://`，校验文本与 4 角点框，同时验证 `http(s)://` 返 `501/10004`、`..` 穿越返 `400/1400`；（b）`can_send_video` 返回 `yes: true`、本地/ `base64://` `[CQ:video]` 发送入库回显与远端 `http://` / 图文混排视频返 `501/10004`；（c）`upload_group_file`/`upload_private_file`（含非白名单 `403/1403` 与非根 `folder` `501/10004`）及 `get_file` 沙箱读取（`.runtime/staging_uploads` 返回 `base64`）与 `../` / `http://` 拦截；（d）全部 11 项新增显式 `10004` 动作（`set_msg_emoji_like`、`fetch_emoji_like`、`set_essence_msg`、`delete_essence_msg`、`_send_group_notice`、`_get_group_notice`、`delete_friend`、`set_qq_avatar`、`get_group_file_system_info`、`get_group_files_by_folder`、`get_group_file_url`）严格返回 `501/10004`，区别于未注册未知动作的 `404/1404` `ACTION_NOT_FOUND`；（e）`business_logic_risk_guards_v5` 覆盖 Wave v5 业务风险防线：非白名单私聊 `get_msg` 与 `/api/messages/history` `403/1403`、同名 `Group` vs `Private` 历史按 `chat_type` 隔离（截断前过滤不互相挤占）、`.handle_quick_operation` 组合管理标志 `501/10004` 零发信 / 过期 `delete` `408/1408` / 独占媒体防混插、`POST /api/ocr` 阻断 `\\127.0.0.1\share\x.png`（`400/1400`）并放行大写 `BASE64://`、`/api/scheduler/policy` 5 项非零字段传 `0` 拒绝（`400/1400`），以及 `post_message_sent = true` 回填真实存储内容与失败发送零上报。
6. **Wave v6 协议与运维契约验证（Wave v6 · `CAT-13`，4 用例）**：归类为 `CONTRACT_VERIFIED`——覆盖（a）`deep_history_seq0_and_reverse_order_contracts`（`Storage::query_messages_filtered` >200 条深分页、`LIKE ... ESCAPE '\'` 字面转义、`message_seq=0` 视作最新页、`reverseOrder`/`reverse_order` 倒序与 REST `/api/messages/history` 多条件游标过滤）；（b）`single_forward_group_private_and_deadline_contracts`（`forward_group_single_msg`、`forward_friend_single_msg`、`POST /api/forward-msg`、WS `MessageSent` 派发、私聊白名单 `403/1403`、群护栏 `403`、过期 `408/1408`、缺失 `400/1400` 与未确认 `500/1500`）；（c）`recent_contact_audit_and_full_cache_clean_contracts`（`get_recent_contact` 过滤大群与非白名单私聊、`.get_audit_events` / `GET /api/audit-events` 剥离私聊泄密项、使用进程独有测试文件校验 `POST /api/cache/clean` 与 `clean_cache` 全清 `.runtime/media` 与 `.runtime/staging_uploads` 子目录且保留根目录）；（d）`security_breaker_reset_denied_and_recovery_contracts`（`StubFaultMode::SecurityBreaker` 下 `.reset_security_breaker` 与 `POST /api/security/reset` 均返 `423/1423` 并记 `security_breaker_reset_denied` 审计，转健康态后复位返 `200`、写 `security_breaker_reset` 审计并恢复发信）。
7. **并发测试（Concurrency · `CAT-05`，3 用例）**：`64` 路并发发信序列化验证、`16` WS 订阅者接收 `200` 广播、`Ping/Pong` 与快 RPC 穿透慢 `send_gate`（`QA-BUG-006`）、`16` OS 线程共享 SQLite WAL 并发读写零错误。
8. **综合集成测试（Integration · `CAT-06`，4 用例）**：真实 Win32 `MockQqServer` + `QqntDriver`（`:memory:` SQLite）+ 正/反向 WS + HTTP Webhook 全链路验证，并在触发 Mock 安全弹窗 `423` 时确认在线守护 `:18770` 未被污染；叠加 **500 人大群 `< burst_enter_threshold` 常规无损**与 **500 人单群 104 并发洪峰（100 闲聊 + 4 P0 触发）优先保活及滞回恢复 `Normal`** 验证（`MOCK_WIN32_VERIFIED`）。
9. **压力测试（Stress · `CAT-07`，3 用例）**：`4,000` 混合 HTTP 请求（48 并发工作协程）+ `800` WS 广播事件（10 订阅者）+ `2,000` SQLite WAL 写入 + **`3 × 600` 人超大群并发洪峰防饥饿抢占轮转验证**（`allow_large_monitored_groups = true` 下由侧边栏 `[有人@我]` / 未读增量驱使跨群切换并投递 P0 关键触发）。
10. **极限暴力测试（Brute-Force · `CAT-08`，3 用例）**：`50` 个半开/中途断连原始 TCP Socket、`400` 组确定性 PRNG 模糊畸形 JSON/CQ/表单载荷、`80` 次高频 WS 连接-畸形帧-二进制帧-断开冲击、`500` 并发 `_async` 队列风暴（`MAX_INFLIGHT_ASYNC_ACTIONS=256` 背压与回收）。
11. **性能测试（Performance SLA · `CAT-09`，1 用例）**：`1,500` 次独立计时环回 RPC，统计 `Min` / `P50` / `P90` / `P95` / `P99` / `P99.9` / `Max`。
12. **内存负载测试（Memory Load · `CAT-10`，1 用例）**：通过 Win32 `K32GetProcessMemoryInfo` 实时监控 `10,000` 条消息追踪去重、`1,500` 合并转发（`<=512` 有界淘汰）、`7,500` 审计事件（`<=5000` 有界裁剪）与 `200` 次 `128x128` WIC BGRA<->PNG 编解码周期前/峰/后驻留与泄漏检测。
13. **CPU 负载测试（CPU Load · `CAT-11`，1 用例）**：通过 Win32 `GetProcessTimes` 测量 `1,200ms` 多连接空闲唤醒功耗（严格 `<5.0%`）与 `800ms` 4 线程密集计算饱和效率。

### 13 项行业标准 Benchmark（Release `--profile release` + `LTO=fat`）
1. `BM-01`：`stable_onebot_id` FNV-1a ID 散列吞吐（`64 B`）
2. `BM-02`：CQ 文本 -> `MessageSegment` 结构化解析吞吐
3. `BM-03`：`MessageSegment` -> CQ 文本 + 实体转义序列化吞吐
4. `BM-04`：`MessageTracker::observe_poll` 50 条滑动窗口去重增量对齐
5. `BM-05`：系统提示横幅正则引擎 `parse_notice_text_to_onebot_json`
6. `BM-06`：RFC 2104 Webhook `hmac_sha1_hex` 签名吞吐（`1 KiB`）
7. `BM-07`：原生 WIC 硬件引擎 `128x128` BGRA <-> PNG 双向编解码（`64 KiB`）
8. `BM-08`：SQLite WAL `record_message` + `get_message_by_id` 单条读写往返
9. `BM-09`：SQLite WAL `upsert_groups_batch`（20 群/事务）批量持久化
10. `BM-10`：Axum HTTP Loopback `POST /onebot/v11/get_version_info` 全栈 JSON RPC
11. `BM-11`：`FloodScheduler::ingest_and_select` 多群（3×90 条/批）并发洪峰分级与响应窗口选择吞吐及 P99 延迟
12. `BM-12`：扩展通知分类（`group_msg_emoji_like`/`group_admin`/`essence`/`friend_add`/`lucky_king`/全员禁言及解禁）、`04_tree.json` 折行文件名气泡回放与 `diff_visible_uids` 被动撤回差分综合吞吐与延迟
13. `BM-13`：`Storage::query_messages_filtered` 深分页过滤检索、`03_tree.json`/`06_tree.json` 原生引用与合并转发卡提取（`enrich_parsed_messages_with_storage`）及 `Storage::get_or_allocate_forward_for_uid` 幂等查找综合吞吐与延迟

---

## 3. 执行命令与日常操作

```powershell
# 运行全套 L0-L4 质量检验与基准测试（包含代码规范检查、clippy、单元测试与可视化报告生成）
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\run-qa-suite.ps1 -Tier All

# 按场景运行指定等级
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\run-qa-suite.ps1 -Tier L0
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\run-qa-suite.ps1 -Tier L1
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\run-qa-suite.ps1 -Tier L2
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\run-qa-suite.ps1 -Tier L3
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\run-qa-suite.ps1 -Tier L4

# 快速重跑 Harness（跳过已验证的静态检查与单元测试）
.\.cache\target\release\qq-qa-harness.exe --tier All
```

---

## 4. 在线 QQ 安全隔离守则

- `127.0.0.1:18770` 若运行中，**严禁**对其发起任何会触发发信、切换群聊、修改数据库或模拟弹窗的压测操作；所有 `L0` 在线探测仅限只读 `GET /api/health`、`/api/status`、`/api/storage/stats`、`POST /onebot/v11/get_status`、`POST /onebot/v11/get_version_info` 与进程遥测查询。
- 所有破坏性测试、Fuzz、并发风暴及 `MockQqServer` 安全验证弹窗均在**独立临时 `:0` 端口 + `:memory:` / 临时目录 SQLite + `allow_mock=true` 沙箱实例**内运行，物理隔离宿主桌面真实 `QQ.exe` 与正式守护进程。

---

## 5. 五级证据分级规范与报告诚实性约束（5-Level Evidence Grading Rule）

为杜绝将本地桩测试或协议字段成功误报为真实桌面 QQ 端到端通过，本工作流强制采用五级证据分级：

1. **`NATIVE_LIVE_VERIFIED`**：仅限宿主 `Default` 桌面实机验证通过，且已核查独立 `/api/tree` 终态气泡、完整内容（含换行/Emoji）、气泡与 DB 1 对 1 归并、`.runtime/media/*.png` 真实图案像素、非 QQ 前台焦点恢复及剪贴板完整复原。
2. **`DEGRADED_BY_DESIGN`**：按设计以可读文本摘要/占位或 SQLite 存根呈现的功能（例如 `face` / `at` / `reply` / `forward` / `share` / `file` / `xml` / `json` 等，出站会记录于 `SendOutcome.degraded_segments`），实机确认展示符合降级设计，不得宣称为 QQ 原生气泡或原生卡片能力。
3. **`OFFLINE_REAL_TREE_REPLAY` / `REAL_TREE_REPLAY_VERIFIED`**：基于真实 `QQ.exe` 保存 UI 树样本（含 `03_tree.json`、`04_tree.json`、`06_tree.json` 与 `06_probe_bubble_menu.json`）的离线重放回归（如 `qqnt-driver --test live_evidence_replay` 与 `qqnt-driver::parser::replay_tests`），用于验证解析/过滤/折行拼接/原生引用与合并转发提取算法回归，不可单独替代真机重测。
4. **`CONTRACT_VERIFIED` / `WINRT_LOCAL_OCR_VERIFIED` / `MOCK_PLUMBING_ONLY` / `MOCK_WIN32_VERIFIED`**：`qq-qa-harness`（`ConfigurableStubDriver` / `:memory:` SQLite）用于验证协议路由、并发互斥、背压限流、错误码与数据库事务管道（`CONTRACT_VERIFIED` / `MOCK_PLUMBING_ONLY`）；`CAT-12` 与 `uia-core` OCR 测试直接调用本机 Windows WinRT `OcrEngine` 完成离线识图回归（`WINRT_LOCAL_OCR_VERIFIED`）；真实 Win32 `MockQqServer::spawn_with_groups` 500/3×600 大群并发洪峰、多群轮转及安全熔断复位 `423->200` 回归测试（`qqnt-driver --test flood_selector_e2e`、`wave6_driver_tests`、Release `self-test` Step 13 与 `qq-qa-harness` CAT-06/CAT-07）标明为 **`MOCK_WIN32_VERIFIED`**（及 **`Self-Test Verified`**），且整份报告仍固定写入 `verification_scope = "OFFLINE_MOCK_AND_TREE_REPLAY (NOT_LIVE_QQ_E2E)"` 与 `live_e2e_verified = false`，绝不可充当 `NATIVE_LIVE_VERIFIED` 真机通过证据。
5. **`UNSUPPORTED_10004`**：非原生可实现或被安全红线禁行的动作/段（包含 39 项显式边界动作、默认状态下的 `record`/`flash`、混发 `file`/`video`、`http(s)://` 媒体及非根群文件目录 `folder`），实测严格返 HTTP `501` / `retcode = 10004`（未在路由表注册的未知动作返 `404` / `retcode = 1404` `ACTION_NOT_FOUND`；仅当显式配置 `[onebot].allow_degraded_media_summary = true` 时才放行降级占位文本）。

**报告统计与缺陷状态刚性要求**：
- **跳过绝不计入通过**：`CategoryBuilder::push_skipped` 严格标记 `passed: false, skipped: true`，由 `skipped_cases` 单独统计，`passed_cases` 仅统计实际运行且断言通过的用例（`--skip-live` 下为 `42/43 passed, 1 skipped`）。
- **缺陷台账如实标注范围**：`QA-BUG-*` 统一标注为 `Code Fixed (Unit/Stub Regression)`，`LIVE-BUG-001..008`、Stage 1–3 Track 1–4 实机联动项、`STAGE4-BUG-001..007`、Wave v5 `BIZ-RISK-001..010` 与 Wave v6 `LIVE-BUG-V6-001..002` / `FEAT-V6-002..005` 均严格依据端侧留存档案分级标注（已获宿主真机闭环者附 [私有证据与公开版说明](PUBLIC_RELEASE_NOTES.md)、[私有证据与公开版说明](PUBLIC_RELEASE_NOTES.md) 与 [私有证据与公开版说明](PUBLIC_RELEASE_NOTES.md) 截面截图升级为 `NATIVE_LIVE_VERIFIED`，未实机触发的被动通知、`upload_private_file`、`forward_friend_single_msg`、`.reset_security_breaker`（标为 `MOCK_WIN32_VERIFIED` & `Self-Test Verified`）、同名冲突及大群洪峰维持 `UNIT_TESTED` / `CONTRACT_VERIFIED` / `REAL_TREE_REPLAY_VERIFIED` / `MOCK_WIN32_VERIFIED` 诚实分级）。
- **严禁误导表述**：禁止在 PR、汇报或文档中把 `MockQqServer`、`FakeDriver` 或 SQLite 存根自查作为 `QQ.exe` 实机正确性依据；真机发布准入必须以端侧 UI/内容核验为准。
