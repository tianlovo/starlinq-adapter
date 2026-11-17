> 公开副本说明：示例身份与路径已替换，历史实机原始证据不公开；本文件的历史状态不代表公开版本或新电脑已完成真机验证。参阅 [公开版本说明](../PUBLIC_RELEASE_NOTES.md)。

# QQNT 真实桌面端增量实机复验指引（DELTA_REVERIFY_PROMPT）

> **用途说明**：在完成 `LIVE-BUG-001` .. `LIVE-BUG-008` 及协议能力门禁整改、并通过 `scripts/cargo-local.ps1 build --release` 后，将本指引交付给宿主正常 Windows 活动 `Default` 桌面上的核验工程师或 GPT 执行实机回归验收。
> **工作目录**：`C:\Work\starlinq-adapter`
> **参考基线**：[私有证据与公开版说明](../PUBLIC_RELEASE_NOTES.md) 与 [私有证据与公开版说明](../PUBLIC_RELEASE_NOTES.md)

---

## 一、五级证据铁律与安全红线（不可越级）

1. **五级证据分级判定（严禁把 Mock/Stub 当作真机 PASS）**：
   - **`NATIVE_LIVE_VERIFIED`**：仅限真实 `QQ.exe` 端到端通过，**且具备独立端侧终态证据**（独立 UIA `/api/tree` 或窗口截图显示真实气泡位置/完整内容、DB 与气泡 1 对 1、`.runtime/media/*.png` 图像肉眼/像素验证确实是目标图片而非空白/相邻文字、外部前台窗口未失窃、剪贴板原文完好复原）。
   - **`DEGRADED_BY_DESIGN`**：按设计以可读文本摘要/存根呈现的段（例如 `face` / `at` / `reply` / `forward` / `share` / `file` / `xml` / `json`），在确认 `SendOutcome.degraded_segments` 与真实气泡文本一致后如实标注，**不得标为原生通过**。
   - **`OFFLINE_REAL_TREE_REPLAY`** / **`MOCK_PLUMBING_ONLY`**：不可用来宣布真机通过。
   - **`UNSUPPORTED_10004`**：非原生可实现动作/段（包含默认 `record` / `video` 与 19 项边界动作）实测严格返回 HTTP `501` / `retcode = 10004`。
2. **禁止仅凭 `retcode = 0` 或输入框清空判通过**：每条发送必须核对返回的真实持久化 `onebot_message_id`，并用独立 `/api/tree` 与 `get_msg` 验证气泡内容与作者完全一致。
3. **绝对安全红线**：
   - 严禁终止、重登或点击宿主机 `QQ.exe` 登录/安全窗口；若遇到 `423 / 1423` 或 `security_breaker_tripped == true` 立即停手。
   - 群聊发送严格锁定 **`Demo测试群`**（真实群号 `100000001`，每次发送前确认 `detected_member_count < 10`）。
   - 私聊发送严格锁定白名单好友 **`测试好友A`**；未授权探测仅用虚构名 `"__DELTA_UNAUTH_PROBE__"` 确认 `403 / 1403`。
   - 全程 `mouse_touched == false`，写请求之间间隔 `>= 1800ms`；禁止写入 `%USERPROFILE%\.cargo`；先热重启升级至最新 `release` 二进制（`.\.cache\target\release\qq-uia-adapter.exe restart`）。

---

## 二、热重启与前置环境基线确认（Step 0）

1. 记录旧守护 PID，执行热重启切换至最新编译的 Release 二进制：
   ```powershell
   .\.cache\target\release\qq-uia-adapter.exe restart
   ```
2. 查询 `GET http://127.0.0.1:18770/api/status` 与 `GET http://127.0.0.1:18770/api/windows`，确认：
   - 守护进程 PID 已轮换为新进程；
   - `desktop_name == "Default"` 且 `sandbox_isolated == false`；
   - `security_breaker_tripped == false`，识别到宿主真实 `QQ.exe` 窗口；
   - 默认配置下 `[onebot].allow_degraded_media_summary` 为 `false`。
3. 记录 `Test-Path "$env:USERPROFILE\.cargo"` 为 `False`。

---

## 三、逐项增量实机复验清单（Checklist）

> 建议为每条消息携带唯一时间戳前缀（例如 `[REVERIFY-<HHMMSS>-<TAG>]`），严禁复用旧消息原文，避免与历史记录混淆。

### 1. LIVE-BUG-001：发送真值强门禁、持久化 `message_id` 与无伪 ID 撞库
- **测试对象**：`Demo测试群`（`group_id = 100000001`）
- **步骤与断言**：
  1. 通过 `POST /onebot/v11/send_group_msg` 发送文本段与混合段（如 `text` + `face(id=14)`），以及通过 `POST /api/send` 发送一条纯文本测试消息。
  2. 检查每次返回：
     - `retcode == 0` 时 **必须存在持久化的 `data.message_id`**（且与 DB 最新自增主键连续一致，绝不出现缺失回采却返回 `10000` / `10001` 撞历史消息的伪 ID）；
     - 立即调用 `POST /onebot/v11/get_msg`（传入返回的 `message_id`），验证返回的 `raw_message`、`message` 段、`group_id = 100000001` 与**刚才发出的唯一标记完全一致**；
     - 核对返回的 `data.outcome.degraded_segments`（发送 `face` 时应如实包含 `"face"` 等降级段提示，发纯文本应为空数组 `[]`）；
     - 若人为制造失败/超时条件（例如超限/白名单拦截或底层确认失败），`send_group_msg`、`send_msg`、WS `send_group_msg` 与 `/api/send` **均不得返回 HTTP 200 或 `retcode = 0`**。

### 2. `record` / `video` 默认拒绝 HTTP `501` / `retcode = 10004` 能力诚实门禁
- **测试对象**：`Demo测试群`（确保 `[onebot].allow_degraded_media_summary = false`）
- **步骤与断言**：
  1. 调用 `POST /onebot/v11/can_send_record`，确认返回 `{"yes": false}`。
  2. 调用 `POST /onebot/v11/send_group_msg` 发送 `[{"type": "record", "data": {"file": "test.amr"}}]`。
  3. 调用 `POST /onebot/v11/send_group_msg` 发送 `"[CQ:video,file=test.mp4]"`。
  4. **预期结果**：两次请求均**严格返回 HTTP `501` / `retcode = 10004` (`UNSUPPORTED_ACTION`)**；检查 `/api/tree` 与 `QQ.exe` 窗口，确认聊天记录内**没有发出 `[语音]` 或 `[视频]` 占位文字**，且输入框保持干净。

### 3. LIVE-BUG-002：群聊气泡精准切分（不返回假空、不跨头像串合并）
- **测试对象**：`Demo测试群`（`100000001`）
- **步骤与断言**：
  1. 连续发送 3 条短文本消息（每条间隔 `>= 1800ms`）：`[REVERIFY-SPLIT-1]`、`[REVERIFY-SPLIT-2]`、`[REVERIFY-SPLIT-3]`。
  2. 立即调用 `GET /api/tree?depth=32`、查看 `GET /api/messages` 与 `POST /onebot/v11/get_group_msg_history`（`count = 10`）。
  3. **预期结果**：
     - 3 条标记对应 **3 条各自独立的 DB 记录与历史记录项**；
     - **严禁出现数组突变为 `[]`**，也**严禁出现一条记录内混入多条气泡正文、多个昵称或 `LV` 等级前缀的跨气泡合并串行**。

### 4. LIVE-BUG-003：私聊气泡与 DB 1 对 1 归并及结构化段保留
- **测试对象**：白名单好友 `测试好友A`
- **步骤与断言**：
  1. 通过 `POST /onebot/v11/send_private_msg` 向 `测试好友A` 发送一条复合消息：`"REVERIFY-PRIV "` + `face(id=21)` + 唯一时间戳标记。
  2. 间隔 `>= 2500ms` 等待后台轮询回采完成，再按 `user_id` 发送第 2 条私聊测试消息并再等待一轮轮询。
  3. 调用 `POST /onebot/v11/get_friend_msg_history`（`count = 10`）与 `GET /api/messages?session=测试好友A`，并对照 `/api/tree` 真实私聊气泡数。
  4. **预期结果**：
     - QQ 屏幕新增 2 个私聊气泡 -> 数据库与历史接口**严格只对应 2 条记录**（无成对重复 Stub+Poll 双份入库）；
     - 对应记录的 `message` 段数组中**完整保留 `face` 结构段**，未被回采覆盖退化为纯文本。

### 5. LIVE-BUG-004：多行硬换行保真（`Shift+Enter` + 端侧气泡校验）
- **测试对象**：`Demo测试群`（`100000001`）
- **步骤与断言**：
  1. 调用 `POST /onebot/v11/send_group_msg` 发送：
     `"[REVERIFY-NEWLINE]\nLINE_TWO\n第三行 🚀"`。
  2. **端侧核验**：读取独立 `/api/tree`（及必要时受控截图/目视确认）并查询 `get_msg`。
  3. **预期结果**：
     - QQ 真实聊天气泡内显示为**明显三行折行排版**（不再合并粘连成单行 `[REVERIFY-NEWLINE]LINE_TWO第三行 🚀`）；
     - 数据库 `raw_message` 与 `get_msg` 返回完整保留 `\n`、中文与 Emoji；
     - `mouse_touched == false`，输入编辑框发送后完全清空。

### 6. LIVE-BUG-005：好友/群/会话列表去噪防污染与旧污点清除
- **步骤与断言**：
  1. 分别调用 `POST /onebot/v11/get_friend_list`（`no_cache: true`）、`POST /onebot/v11/get_group_list`（`no_cache: true`）与 `GET /api/sessions`。
  2. 核查返回全集并与真实 QQ 左侧 `"会话列表"` 逐条对照。
  3. **预期结果**：
     - 列表中**不得出现** `"截图 Ctrl + Alt + A"`、`"文件"`、`"04:35"`、`"消息列表"`、`"在线状态"`、`"测试机器人的头像"` 或聊天正文段落等 UI 按钮/噪点；
     - 此前遗留在 `adapter.db` 中的非法污染历史项已在刷新后被自动剔除（`purge_invalid_*`），只保留真实会话/联系人。

### 7. LIVE-BUG-006：DPI 对齐真实图片回采与图像内容核验
- **测试对象**：`Demo测试群`（`100000001`）
- **步骤与断言**：
  1. 先往宿主机剪贴板写入哨兵文本 `"DELTA_CLIPBOARD_SENTINEL_2026"`。
  2. 在脚本临时生成一张**带显著高对比图案/色块的 240×120 PNG 测试图**（例如四象限红蓝黄绿拼块+文字标号，禁止纯白/纯黑单色图），通过 `base64://` 发送至 `Demo测试群`。
  3. 间隔 `>= 2500ms` 触发入站/回采与视口裁剪，调用 `GET /api/messages` 找到该图对应的 `Image` 段文件名，并调用 `POST /onebot/v11/get_image` 取回本地 `.runtime/media/*.png` 路径。
  4. **像素与端侧校验（严禁只查文件大小）**：
     - 读取宿主机剪贴板，确认原文已还原为 `"DELTA_CLIPBOARD_SENTINEL_2026"`；
     - 解码 `.runtime/media/*.png` 检查宽高、非单色像素分布（远高于 465B 单色背景）、以及图片内容确实对应发出的四象限测试图（**而非相邻气泡文字或纯黑/纯灰背景**）；
     - 检查 `/api/tree` 中该图片处于历史视口内且完整露出（验证 DPI 缩放与 `clip_rect_to_history`）。

### 8. LIVE-BUG-007：非 QQ 外部前台窗口焦点恢复守护
- **步骤与断言**：
  1. 将宿主一个**非 QQ 外部窗口**（例如记事本或终端或浏览器窗口，记录其 HWND 与窗口标题）设为当前前台活动窗口（`GetForegroundWindow()`），**保持 QQ 窗口非前台**。
  2. 从命令行脚本调用 `POST /onebot/v11/get_group_info`（`{"group_id": 100000001, "no_cache": true}`）。
  3. 调用结束后立刻（以及 300ms 后）再次查询宿主 `GetForegroundWindow()` 句柄与标题，并与调用前对比。
  4. **预期结果**：群信息与真实群号 `100000001` 探测成功，且结束时系统前台焦点已由 `ForegroundRestoreGuard` 自动归还至调用前的非 QQ 外部窗口（不得长期停留在 `QQ.exe`）。

### 9. LIVE-BUG-008：身份 ID 跨表一致性与实号语义核对
- **步骤与断言**：
  1. 依次调用 `get_login_info`、`get_friend_list`、`get_group_member_list`（`group_id: 100000001`）、`get_stranger_info`。
  2. 对照同一昵称（例如登录账号 `测试机器人` 在 `get_login_info.user_id` 与 `get_group_member_list` 同名成员的 `user_id`；白名单好友 `测试好友A` 在 `get_friend_list.user_id` 与 `get_group_member_list` 的 `user_id`）。
  3. **预期结果**：
     - `Demo测试群` 的 `group_id = 100000001` 且 `real_group_id = 100000001`（真群号）；
     - 若 `real_qq_id == null` / `real_user_id == null`，**同一昵称在 `get_login_info`、`get_friend_list` 与 `get_group_member_list` 中的 `user_id` 完全统一**（不再同人多号），同时报告严格如实标注其为“适配器作用域 FNV-1a 统一映射 ID”，不得谎称为真实 QQ 号。

### 10. 数字 `message_id` 回查 `get_forward_msg`
- **测试对象**：`Demo测试群`（或 `测试好友A`）
- **步骤与断言**：
  1. 调用 `POST /onebot/v11/send_group_forward_msg`（`group_id: 100000001`，传入 2 条自定义 `node` 消息），记录返回的 `data.forward_id`（`fwd-...`）和 `data.message_id`（整数，如 `100xx`）。
  2. 分别使用以下参数调用 `POST /onebot/v11/get_forward_msg`：
     - `{"id": "<forward_id>"}`
     - `{"id": <numeric_message_id>}`（JSON 整数）
     - `{"id": "<numeric_message_id>"}`（数字字符串）
     - `{"message_id": <numeric_message_id>}`
  3. **预期结果**：4 种调用全部返回 `retcode = 0` 与一致的 `data.messages` 2 节点数组；同时如实记录 QQ 聊天气泡显示为 `[聊天记录]` 摘要文本（归类 `DEGRADED_BY_DESIGN`）。

### 11. 慢速发信期间 WebSocket 心跳非阻塞（`cached_status()`）
- **步骤与断言**：
  1. 连接正向 WS `ws://127.0.0.1:18770/onebot/v11/ws`（`Universal`），读取 `lifecycle.connect` 与初始 `heartbeat` 时间戳，核对配置 `[onebot].heartbeat_interval_ms`（默认 `15000ms`）。
  2. 在独立连接发起一次需要焦点切回/图片发送或连续发信动作的同时，在 WS 监听端持续记录 `meta_event.heartbeat` 帧到达时间及 `Ping` -> `Pong` 延迟。
  3. **预期结果**：心跳帧按配置间隔准时到达、携带 `status.good == true`，**不再因 UIA 发送锁等待出现数十秒抖动/堵死**，且 `Ping/Pong` 毫秒级响应。

---

## 四、验收产出与收尾要求

1. 复验结果按 **五级证据分级** 逐条附上可核对证据（HTTP JSON、`/api/tree`、DB 查询、`.runtime/media/*.png` 像素尺寸与色值校验、`GetForegroundWindow()` 前后句柄），切勿整份报告用单一笼统 PASS 总结。
2. 结束后确认：
   - `%USERPROFILE%\.cargo` 不存在；
   - `127.0.0.1:18770` 守护进程在线健康、停留在 `Demo测试群`、输入框空、无遗留测试进程；
   - 将增量复验报告输出或追加记录供最终上线决策。
