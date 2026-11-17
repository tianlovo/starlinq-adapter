> 公开副本说明：示例身份与路径已替换，历史实机原始证据不公开；本文件的历史状态不代表公开版本或新电脑已完成真机验证。参阅 [公开版本说明](PUBLIC_RELEASE_NOTES.md)。

# 新电脑接续开发提示词：先核验环境，再接手项目

把下方“可直接交给开发代理的完整提示词”作为新电脑上开发代理的任务输入。新电脑可以没有本项目、Git、Rust 或 QQ；先据实检查，再选择能执行的工作。不要直接照搬旧电脑的盘符、工具链路径、QQ 进程、消息 ID 或测试通过结论。

## 交接基线

- 远程仓库：`https://github.com/tianlovo/starlinq-adapter.git`
- 目标分支：`main`
- Git 提交身份：`tianlovo` / `tianluoqaq@gmail.com`，只写仓库级配置。这是提交作者信息，不是 GitHub 登录凭据。
- 交接文档日期：2026-11-17（Asia/Shanghai）
- 私有交接来源的业务代码基线（不在本公开 Git 历史中）：`42fb993215d86af413637ffcb39f6706a143ce6a`（Wave v6，提交于 2026-11-16）。本仓库已使用独立的脱敏导入历史；实际接手必须记录公开仓库的 HEAD，不得尝试 checkout 私有来源 SHA。
- 仓库远程名为 `starlinq-adapter`，当前软件、二进制及部分文档仍叫 `qq-uia-adapter`，版本声明为 `0.3.0`。不要因远程名称不同而自动批量改名。
- `.cache/`、`.runtime/`、编译产物和本地数据库不在常规 Git 交接范围。新机应从源码重建运行环境，不能假定旧机器的端口服务或运行状态会随 clone 迁移。
- 历史真机报告与新版本专题记录共同留档，不能把 2026-11-15 的失败报告当成最新代码全部仍失败，也不能把 2026-11-16 的部分通过当成新机器全部通过。

## 可直接交给开发代理的完整提示词

你要在这台环境未知、可能没有本项目的电脑上接续开发 Windows QQNT 桌面自动化适配器。请实际执行环境调查、拉取代码、建立可验证的开发基线，再继续有明确依据的开发工作。不要只提供建议清单。

远程仓库为 `https://github.com/tianlovo/starlinq-adapter.git`，默认分支为 `main`，提交用户名为 `tianlovo`，邮箱为 `tianluoqaq@gmail.com`。

### 第一层：开发前环境核验

先做只读调查，不要一上来安装依赖、运行仓库脚本、启动守护进程或操作 QQ。凡能通过本机工具探测的信息，不要让用户手动填写；探测失败时记录原始错误，不要猜测环境已经具备。

1. **系统与工作目录**
   - 检查操作系统、版本、CPU 架构、当前 shell、当前用户权限、是否处于容器/WSL/远程会话/锁屏或非交互桌面。
   - 选择用户有写权限、空间足够的项目目录，先检查已有内容。不要假设有 `F:` 或 `E:` 盘，也不要覆盖同名非空目录。
   - 若发现已有 Git checkout，先检查 HEAD、分支、远程和未提交改动，再决定复用或另建目录；禁止 reset/clean 丢弃用户工作。
   - 若并非可用的 Windows 桌面，只执行已确认兼容当前系统的源码阅读、静态检查或离线工作。Windows UIA/MSAA、WinRT OCR 和真实 QQ GUI 验收应明确标为当前环境不可执行；不能由 WSL/Linux/macOS 中的通过代签。

2. **网络与 Git**
   - 检查 Git 是否存在、版本与可执行文件实际位置；检查 GitHub 可达性、代理及证书错误。不要关闭 TLS 校验，也不要输出代理口令或令牌。
   - 用 `git ls-remote https://github.com/tianlovo/starlinq-adapter.git` 确认远程可访问且有可拉取分支。若仓库不存在、无权限或尚为空，说明事实并继续能做的环境核验；不能假装 clone 成功。
   - 公共读取与推送权限分别核验。用户名/邮箱只用于提交署名，不能替代认证；需要登录时使用用户已有凭据管理器或正常授权流程，不要求用户把密码/PAT 粘贴进提示词或项目文件。

3. **编译与脚本工具**
   - 检查 Python 3、PowerShell、Git、`rustup`、`rustc`、`cargo`，记录实际路径和版本。对尚未确认来源的同名工具不要直接信任。
   - Rust 需记录 `rustc -Vv` 的 host、活动 toolchain、已安装 targets，以及 rustfmt/clippy 是否存在。检查编译器和链接器架构一致。
   - 旧机器使用 `stable-x86_64-pc-windows-gnu` 和 MinGW。GNU 路径需检查 `gcc`/链接器与相关运行库；若采用 MSVC 路径，则核验 Visual C++ Build Tools、Windows SDK、`cl`/`link` 环境并实测兼容。不要把“PATH 中有 cargo”当成可以完成 Windows 构建。
   - 在运行会初始化 Rust/Cargo 的命令或安装器之前设置项目范围的缓存位置。默认 `CARGO_HOME=<项目>/.cache/cargo`、`CARGO_TARGET_DIR=<项目>/.cache/target`，临时目录使用 `<项目>/.cache/tmp`。需要新装 rustup 时，先显式设好 `RUSTUP_HOME` 与 `CARGO_HOME`，不得在 `%USERPROFILE%\.cargo` 新建或污染文件；用户本来已有的目录也不得删除。
   - 允许在项目目录准备可逆的配置与依赖。需要管理员权限、系统级安装、系统重启或用户登录时，说明检测结果和最小必要步骤；不要修改系统安全策略来绕过阻塞。

4. **QQ 与运行环境条件**
   - 只读检测真实 QQNT 的安装路径、版本、是否已运行、所在 Session/Desktop，以及当前代理能否访问宿主活动桌面。不要假设路径是 `C:\Program Files\Tencent\QQ\QQNT\QQ.exe`。
   - 检查 `127.0.0.1:18770` 是否被占用、监听进程是谁；只有确认是本项目服务后才读取其健康状态。不得终止未知监听进程。
   - 不操作登录/扫码/安全验证，不终止或强制重登 QQ。未登录、无桌面权限或工具只可访问隔离沙箱时，转为离线开发模式，明确记录真机缺口。
   - 旧电脑的 QQ 昵称、群号授权、映射用户 ID、消息 ID、HWND/PID 均不能直接套用。先核查新机账号及用户授权目标；如果换了账号或没有对应小群/白名单好友，再向用户询问缺失的测试目标。未确认前不得向真实联系人发测试消息。

输出一份本地环境报告，保存到 `.runtime/handoff/ENVIRONMENT_REPORT.md`（尚未 clone 时可先暂存在选定工作目录，随后归档）。报告至少列出“项目、实际观测、证据/命令、是否阻塞、下一步”，给出以下明确结论：

- 可执行的工作：例如源码阅读、离线测试、Windows Mock、真实 QQ 验证。
- 缺失依赖与具体失败原因，不把未知写成已满足。
- 工具安装/缓存方案、选定 workspace、compiler target 及权限要求。
- GitHub 可读取状态与可推送状态，分别报告。

环境满足后自动进入下一层，不要为了等待一个不影响后续工作的偏好问题停住整个任务。

### 第二层：获取代码并确认项目约束

1. 在确认空闲的新目录中 clone；以下是命令形态，先根据实际系统选择工作目录，不要机械执行到未知路径：

   ```text
   git clone https://github.com/tianlovo/starlinq-adapter.git starlinq-adapter
   cd starlinq-adapter
   git status --short
   git branch --show-current
   git remote -v
   git log -5 --oneline
   git rev-parse HEAD
   git config --local user.name tianlovo
   git config --local user.email tianluoqaq@gmail.com
   ```

2. 完整阅读仓库 `AGENTS.md`。随后阅读：
   - `README.md`
   - `docs/ARCHITECTURE.md`
   - `docs/TESTING_AND_BENCHMARK_WORKFLOW.md`
   - `docs/QA_DEFECT_LOG.md`
   - `docs/OneBot协议完善进度.md`
   - `config/default.toml`、`.cargo/config.toml`、`scripts/env.ps1`、`scripts/cargo-local.ps1`、`scripts/run-qa-suite.ps1`
   - 做真机核验前，再完整阅读 `docs/gpt-live-verification/PROMPT_FOR_GPT.md`、`REPORT_TEMPLATE.md`、`DELTA_REVERIFY_PROMPT.md`，并与当前 AGENTS、当前能力矩阵核对版本差异；老提示中的动作数量和原生能力可能已过期。

3. 先阅读 `docs/PUBLIC_RELEASE_NOTES.md`，再按时间和当前源码理解现状。以下资料属于未公开的私有历史，仅列出名称帮助辨认历史文档引用；公开仓库不提供这些原始文件，不能虚构读取结果。使用缺陷台账和脱敏夹具建立新基线，并重新补测：
   - `docs/gpt-live-verification/GPT_TEST_LIVE_GAP_ROOT_CAUSE_ANALYSIS.md`
   - `docs/gpt-live-verification/GPT_LIVE_REVERIFICATION_REPORT_20261115.md`
   - `docs/gpt-live-verification/live-runs/protocol_v4_live_20261116/summary.json`
   - `docs/gpt-live-verification/live-runs/biz_risk_live_20261116/summary.json`
   - `docs/gpt-live-verification/live-runs/wave6_live_20261116/summary.json`

   旧报告曾发现假消息入库、重复 ID、目录污染、焦点丢失、转发 ID 冲突、标题误判；后续提交包含针对性整改。逐项按当前代码和最新证据核对，不预设“全部仍坏”或“全部已修好”。README 记载的 42/39 动作、13 类/43 用例/13 基准属于当时基线；以当前源码声明和实际运行结果为准。

4. 当前包含 8 个 crate：`common-types`、`storage`、`uia-core`、`qqnt-driver`、`adapter-server`、`qq-cli`、`mock-qq-win`、`qa-harness`。先理解要修改的调用链，不做无关重命名、全仓重构或依赖批量升级。

### 第三层：适配本机环境并建立构建基线

1. **先修正路径假设，再运行包装器。** 公开版 `scripts/env.ps1` / `scripts/cargo-local.ps1` 支持 PATH 探测及 `QQ_UIA_TOOLCHAIN_BIN`、`QQ_UIA_CARGO_EXE`、`QQ_UIA_MINGW_BIN`、`QQ_UIA_RUSTUP_HOME`、`QQ_UIA_CARGO_HOME`。根据第一层结果选择现有工具链并保留 `.cache` 隔离；不要把某台机器的固定盘符重新写成默认值。
2. 检查 `.cargo/config.toml` 的 rsproxy 源在本机是否可用；若调整源，说明原因、保留 `Cargo.lock`，不要顺便升级依赖或关闭证书检查。
3. 先复制生成 `.runtime/handoff/local.toml` 作为本机配置，按需修正 QQ 路径和已获授权目标，命令使用全局 `--config <PATH>` 传入。不要把令牌、QQ 数据库、截图、聊天原文或本机私有目录提交到公开远程。
4. 在工具链适配完成后运行基线检查。PowerShell 命令如下；每步检查退出码，失败先定位，不堆叠后续命令掩盖错误：

   ```powershell
   python scripts\check_coding_style.py crates
   powershell -NoProfile -ExecutionPolicy Bypass -File scripts\cargo-local.ps1 fmt --all -- --check
   powershell -NoProfile -ExecutionPolicy Bypass -File scripts\cargo-local.ps1 clippy --workspace --all-targets -- -D warnings
   powershell -NoProfile -ExecutionPolicy Bypass -File scripts\cargo-local.ps1 test --workspace
   powershell -NoProfile -ExecutionPolicy Bypass -File scripts\cargo-local.ps1 build --release
   ```

   按影响范围和仓库规范进行测试；建立新机基线时记录各门禁，后续文档或局部修改不要无理由反复全量测试。

5. 若 Windows Mock 条件满足，可执行 release `self-test` 和 QA。没有已确认的真实守护时，采用：

   ```powershell
   .\.cache\target\release\qq-uia-adapter.exe self-test
   powershell -NoProfile -ExecutionPolicy Bypass -File scripts\run-qa-suite.ps1 -Tier All -SkipLiveSmoke
   ```

   `-SkipLiveSmoke` 引起的 skipped 必须计为跳过；`overall_passed` 或 self-test 通过不能证明真实 QQ 端到端通过。L1–L3 压力、Mock 风控和洪峰测试须在临时端口、临时/内存数据库中运行，不能冲击正式 `:18770` 或真实联系人。只有确认真实服务适用且获得对应范围授权后，才做 L0 真实只读探测。

6. 启动真实守护前检查本机配置、端口归属、登录状态与已授权目标；用该版本实际支持的 `--help` 确认命令。记录源码 SHA、可执行文件路径/哈希及真正运行的 PID，分别证明“构建完成”和“运行的确实是该构建”。

### 第四层：接续开发与真机验收规则

1. 未指定下一项新功能时，先交付新机环境及基线报告，整理当前缺陷/未实证能力清单。优先解决有当前证据且阻塞开发的环境或构建问题；功能目标仍有歧义时，只询问最关键目标，同时继续不依赖答案的工作，不自行扩张产品范围。
2. 结合当前代码、缺陷台账及可复现证据选择修复点，先复现，再做最小修改，再按调用传播路径验证。默认单代理执行；不要为了“看起来通过”删除断言、制造成功 ID、写发送存根来替代端侧观测，或把不支持的卡片偷偷转换成普通文字。
3. 遵守 AGENTS 的当前桌面交互规范。任何真机操作都须核验真实进程与授权会话，保持成员上限、私聊白名单、焦点/光标/剪贴板守卫；不裸用鼠标注入或桌面切换。遇到 `423/1423` 或 `security_breaker_tripped=true`，立即停写并记录，不自动重置安全熔断继续发送。
4. 实际发送前再次核验当前标题、真实群号/目标、人数和空编辑器，使用唯一标记，保持至少 1800ms 的测试发送间隔。切窗、轮询、滚动、时间标签变化和重启后都要复查去重与历史关联，不能只查发送后的第一帧。
5. 分清证据范围：
   - `NATIVE_LIVE_VERIFIED`：真实 QQ 原生动作闭环，有独立 UI 树/截图、正确气泡内容、DB 1:1、适用的媒体像素及焦点/光标/剪贴板恢复证据。
   - `DEGRADED_BY_DESIGN`：明确降级且端侧呈现与 `degraded_segments` 一致。
   - `OFFLINE_REAL_TREE_REPLAY` / `MOCK_PLUMBING_ONLY`：分别仅证明真实树离线回放或 Mock/协议管道；相关契约、Win32 Mock、WinRT OCR 标签同样不得升级为 QQ 真机通过。
   - `UNSUPPORTED_10004`：明确不支持，实测严格拒绝且无误操作。
   - 未取得真实端侧闭环、缺少外部账号/事件源或桌面访问条件：标为 `CODE_ONLY_UNVERIFIED_LIVE`。`retcode=0`、单测通过、输入框清空均不是充分证据；`[红包] 恭喜发财` 普通文字绝不能证明红包功能。
6. 新机验收记录写入按实际日期/提交命名的新目录，保留历史报告。真实数据优先存本地 `.runtime`，需要入库的资料先脱敏；不要把旧电脑的记录 ID、外部目标或访问令牌复用到新账户。
7. 完成后报告：实际环境、检出的 SHA、改动与原因、运行过的命令和结果、通过/失败/跳过/未实证范围、剩余问题、运行中服务与界面收尾状态、下一步。提交只包含本次必要改动；推送前检查远程分支与数据范围，禁止强推覆盖他人提交。

从第一层的只读环境核验开始，边检查边推进。不要在未知系统上预先宣布“环境可用”“已迁移完成”或“全链路通过”。
