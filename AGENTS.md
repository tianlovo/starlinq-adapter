# qq-uia-adapter 工程规范与安全红线

本仓库遵循严格的模块化、可维护性与桌面自动化安全规范；所有编码与重构提交前必须通过 `scripts/check_coding_style.py` 与 Cargo 门禁。

## 1. 设计原则与代码复用

- **禁止无意义重复代码**：公共路径、UI 文本、协议路由与常量统一收敛至 `common_types::constants` 或各 crate 的 `constants.rs`，禁止在多处散落硬编码字符串或魔数；重复样板代码优先用 `macro_rules!` 或共享辅助函数消除
- **禁止造轮子**：优先选用 Workspace 已批准的成熟 Rust crate（`reqwest`、`tokio-tungstenite`、`strum`、`fnv`、`parking_lot`、`rusqlite`、`axum`、`clap`、`thiserror`、`tracing`）
- **编码前先确定设计模式**：每个 crate 入口 `lib.rs` 及核心模块 `//!` 文档头必须写明所采用的设计模式：
  - **Facade（门面模式）**：对外暴露聚合 API 与重导出边界（如 `common-types`、`Storage`）
  - **Adapter（适配器模式）**：Win32/UIA/MSAA 与 OneBot v11/REST 协议转换（如 `QqntDriver`、`onebot`）
  - **Proxy + RAII Guard（代理与作用域守卫）**：进程/窗口句柄托管与前台焦点/屏蔽状态恢复
  - **Strategy + Chain（策略与责任链）**：`BackgroundUia` -> `FocusRestoreUnicode` 输入回退链
  - **Command + Active Object（命令与活动对象）**：专用 COM/Win32 消息泵线程与通道调度
  - **Observer / Pub-Sub（观察者与发布订阅）**：`WinEventWatcher` 实时事件通知与 `broadcast` 总线
  - **Repository + Parameter Object（仓储与参数对象）**：数据访问拆分为领域仓储与结构体传参
  - **State Machine（状态机）**：安全熔断状态机与反向 WebSocket 自动重连循环
  - **Macro Codegen（声明宏代码生成）**：统一声明常量表、路由映射或测试夹具

## 2. 规模、命名与签名约束

以下命令由 `python scripts/check_coding_style.py [paths...]` 自动校验（默认检查 `crates/`，跳过 `.cache`、`.runtime`、`.git`）：

1. **单文件有效代码行 `<= 1000`**：去除空行与所有注释后不得超过 1000 行
2. **单目录直接文件数 `<= 15`**：超限时按业务领域拆分子目录组织
3. **单文件数据结构数 `<= 3`**：每个 `.rs` 文件内定义（含 `#[cfg(test)]` 测试模块）的 `struct`、`enum`、`trait`、`union` 总数不得超过 3 个
4. **单函数参数 `<= 6`**：所有 `fn` 签名形参数量（含 `self`、`&self`、`&mut self`）不得超过 6 个，超出时必须重构为 Parameter Object 结构体传参（Windows/C 外部 ABI 回调与 FFI 声明（`extern "system" fn` 等）按系统约定参数例外，其余所有 Rust 函数严格 `<= 6` 参数）
5. **禁止无意义缩写命名**：变量、字段、参数与函数名使用可读性完整的语义化名称（如 `adapter_config`、`window_handle`、`group_record`）

## 3. 简体中文注释与标点规范

- **文档注释强制**：所有 `struct`、`enum`、`trait`、`union` 正上方（跳过 `#[...]` 属性块）必须紧邻包含简体中文的 `///` 文档注释
- **行内注释要求**：关键算法、Win32/COM/UIA 机制及易歧义分支必须补充简体中文 `//` 行内注释
- **强制注释句号规则**：所有注释（`///`、`//!`、`//`、`/* ... */`）必须使用简体中文，且**每个连续注释块的最后一句禁止以句号（`。”`、`。` 或 `.`）结尾**；多句注释的前几句可用句号分隔，仅最后一句不得带句号（字符串常量与普通 Markdown 文档不受此限）

## 4. 性能与内存工程标准

- **`[profile.release]` 优化**：启用 `opt-level = 3`、`lto = "fat"`、`codegen-units = 1`、`panic = "abort"`、`strip = false`
- **正则预编译**：所有正则表达式必须使用 `std::sync::LazyLock<Regex>`，严禁在热路径内重复调用 `Regex::new`
- **有界缓存与 SQLite 调优**：事件去重队列与历史缓存保持有界容量；SQLite 启用 `WAL`、`synchronous = NORMAL`、`foreign_keys = ON`、`busy_timeout = 5000`、`temp_store = MEMORY` 与 `cache_size = -4000`

## 5. QQ 桌面自动化安全红线

- **禁止杀死运行中的 `QQ.exe`**：不得终止用户已运行的桌面 QQ 进程
- **禁止未受控物理鼠标与切桌面 API**：严禁裸用 `UIElement::click()`、`mouse_event`、无守卫 `INPUT_MOUSE`/`SetCursorPos` 及桌面切换调用；所有真实桌面点选必须统一经由 `uia-core` 的 `click_point_cursor_restore_verified` 执行（内置用户空闲护栏与瞬时原位光标恢复）
- **安全验证即刻熔断**：检测到安全验证、滑块、验证码、登录保护、设备锁提示立即熔断并上报事件
- **群成员数上限保护**：严格校验 `strict_below_members < 10`，默认仅面向测试小群 `Demo测试群`
- **缓存与测试隔离**：单元测试数据库一律使用 `:memory:` 或受控临时路径；Rust/Cargo 缓存严禁写入用户主目录 `%USERPROFILE%\.cargo`

## 6. 门禁校验命令（Windows PowerShell 5.1）

```powershell
python scripts\check_coding_style.py crates
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\cargo-local.ps1 fmt --all -- --check
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\cargo-local.ps1 clippy --workspace --all-targets -- -D warnings
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\cargo-local.ps1 test --workspace
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\run-qa-suite.ps1 -Tier All
```

## 7. 官方分级 QA 测试与 Benchmark 门禁（`L0`–`L4`）

- 官方测试与基准文档：[docs/TESTING_AND_BENCHMARK_WORKFLOW.md](./docs/TESTING_AND_BENCHMARK_WORKFLOW.md)、缺陷审计日志：[docs/QA_DEFECT_LOG.md](./docs/QA_DEFECT_LOG.md)
- 分级执行入口：`scripts\run-qa-suite.ps1 -Tier L0|L1|L2|L3|L4|All`，产出 `.runtime\qa-reports\latest\index.html` 与 `qa-report.json`
- 在线守护 `127.0.0.1:18770` 仅允许执行 `L0` 只读健康与进程遥测探测；所有 `L1`–`L3` 压力/暴力/Mock 安全弹窗验证必须运行于独立 `:0` 临时端口与 `:memory:`/`TempDir` 隔离环境，严禁触碰正式守护与宿主桌面真实 `QQ.exe`

## 8. 强制证据分级与防自欺验收规范（5-Level Evidence Grading Rule）

所有开发与测试 Agent、QA 报告及发布说明均须严格遵守以下 **五级证据分级规范**，严禁跨级夸大验证结论：

| 证据等级 | 允许声明范围 | 必须具备的可复核证据 |
| :--- | :--- | :--- |
| **`NATIVE_LIVE_VERIFIED`** | 真实桌面端 `QQ.exe` 原生行为端到端通过 | 宿主 `Default` 桌面实机发送/采集成功，**且有独立端侧终态证据**：`/api/tree` 或独立 UIA 树显示正确气泡与位置、内容完整（无丢行/合并/错位）、1 对 1 DB 记录无重复、`.runtime/media/*.png` 像素图案肉眼/解码校验正确、外部前台焦点未失窃且剪贴板完整还原 |
| **`DEGRADED_BY_DESIGN`** | 经设计降级为文本摘要/本地存根的功能已按预期呈现 | 已确认 `SendOutcome.degraded_segments` 与真实 QQ 气泡中的实际降级文本一致，且明确标注“非原生 QQ 卡片/非原生媒体形态” |
| **`OFFLINE_REAL_TREE_REPLAY`** | 真实 QQNT UI 树快照回放回归通过 | 基于从真实 `QQ.exe` 捕获并脱敏/落盘的 JSON UI 树快照（如 `live_evidence_replay`）通过解析/提取断言；**不可单独替代新版本真机复测** |
| **`MOCK_PLUMBING_ONLY`** | 协议路由、并发锁序、错误映射、SQLite WAL 或 Win32 Mock 管道通过 | 由 `FakeDriver`、`ConfigurableStubDriver`、`MockQqServer`、`:memory:` SQLite 或 `qq-qa-harness` 覆盖（报告固定标注 `verification_scope = "OFFLINE_MOCK_AND_TREE_REPLAY (NOT_LIVE_QQ_E2E)"`、`live_e2e_verified = false`） |
| **`UNSUPPORTED_10004`** | 明确边界动作或非原生媒体默认拒绝 | 实测确认严格返回 HTTP `501` / `retcode = 10004` 且对真实 QQ 窗口零误操作（含 `record` / `video` 默认拒绝与 19 项边界动作） |

**防自欺硬性禁令**：
1. **严禁越级证明**：开发者与 Agent 严禁将 `MockQqServer`、`FakeDriver`、`ConfigurableStubDriver` 或 SQLite 存根自发自查（send-stub -> `get_msg`）表述为“真实 `QQ.exe` 功能已验证”或“全链路端到端通过”。
2. **禁止仅凭接口状态判成功**：HTTP `200`、`retcode = 0`、`mouse_touched = false` 或输入编辑框清空均**不能**作为消息内容正确或媒体截切正确的充分条件；实机验收必须核验端侧终态内容与唯一消息映射。
3. **跳过项与修复状态如实拆分**：`qq-qa-harness` 中被跳过的用例（`skipped = true`）计入 `skipped_cases` 而不计入 `passed_cases`；静态缺陷台账必须如实区分 `Code Fixed (Unit/Stub Regression)` 与 `Code Remediated — Pending Live QQ Re-Verify`。
