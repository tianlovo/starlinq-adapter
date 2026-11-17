> 公开副本说明：示例身份与路径已替换，历史实机原始证据不公开；本文件的历史状态不代表公开版本或新电脑已完成真机验证。参阅 [公开版本说明](../PUBLIC_RELEASE_NOTES.md)。

# GPT 真实 QQ 客户端核验套件使用说明

本目录用于让运行在**宿主机正常桌面**上的 GPT 对 `qq-uia-adapter`（`v0.3.0`）进行**端到端真实客户端实测与反水分核验**，弥补隔离沙箱中仅能依托单元测试与 `MockQqServer` 推演的盲区。

## 文件清单

- [PROMPT_FOR_GPT.md](./PROMPT_FOR_GPT.md)：交付 GPT 执行的完整核验指令、安全红线、5 态结论定义（严禁“代码通过即算通过”，必须明确列出 `CODE_ONLY_UNVERIFIED_LIVE` 与 `DEGRADED_BY_DESIGN`）及 Phase 0–6 逐项实测步骤。
- [REPORT_TEMPLATE.md](./REPORT_TEMPLATE.md)：标准化核验报告模板。
- `GPT_LIVE_VERIFICATION_REPORT.md`（GPT 执行后生成）：最终实机核验报告文件，完成后可交回给我继续定位和修复真机缺陷。

## 核验前 10 秒检查

1. 确认宿主机真实 QQ 客户端已在正常 Windows `Default` 桌面登录上线，且测试群 **`Demo测试群`**（成员 `< 10` 人）与测试好友 **`测试好友A`** 可正常通信。
2. 确认适配器守护进程已启动（监听 `http://127.0.0.1:18770`，未启动可先运行仓库根目录 [start-daemon.bat](../../start-daemon.bat)）。

## 可直接复制给 GPT 的启动指令

```text
请进入目录 C:\Work\starlinq-adapter，完整阅读并使用 docs/gpt-live-verification/PROMPT_FOR_GPT.md 作为你的执行规范，配合同目录下的 REPORT_TEMPLATE.md，针对当前宿主机上正在运行的真实 QQ 客户端与 http://127.0.0.1:18770 服务端逐项核验全部已声明功能。严禁“代码通过即算通过”；凡无法用真实 QQ 客户端端侧观测闭环的功能必须如实标为 CODE_ONLY_UNVERIFIED_LIVE。核验结束后请将完整报告写入 docs/gpt-live-verification/GPT_LIVE_VERIFICATION_REPORT.md，并在回复中给出统计摘要和发现的所有问题。
```
