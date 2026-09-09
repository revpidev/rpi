# subagents target-track recorded fixtures（pi-subagents v0.66.0）

> **目标轨，pin 未切换**（TE13，ADR-0025 状态「提议」）。本目录的锚点取自
> `external/pi-subagents` @ `0fc0eebb9604970c506708b7508d6aa38921fde2`（v0.66.0，只读复核）；
> `external/` 未被写入、submodule HEAD 仍为旧 pin `56f97234`（v0.48.0，TE27 才切换）。

## 用途与消费者

| 文件 | 形状内容 | 需求 | 承接任务 |
|------|----------|------|----------|
| `events/*.jsonl` + `events/expected.json` | 子进程 stdout 事件流三组（willRetry 失败后成功 / 空终态文本 / 工具错误+空回复）与期望终态 | R7.1.1.3 | TE14 |
| `discovery/agents-tree/` + `discovery/materialize.json` | 发现目录树：坏 frontmatter、嵌套、`sync-backups` 剪枝、`.rpi` 剪枝、符号链接目录（含环） | R7.1.3.1–.4 | TE15 |
| `terminal-classification.json` | 子步终态分类向量 + async step effective thinking | R7.1.6.1/.2 | TE16 |
| `notify-fields.json` | 完成通知字段/行格式黄金 | R7.1.7.2 | TE17 |

## 事件流（`events/`）

- JSONL 每行 = 子进程 stdout 的一行 JSON 事件（与
  `crates/rpi-ext-subagents/tests/fixtures/child_stream.jsonl` 同形：
  `session` 头 + `agent_start`/`turn_start`/`message_start`/`message_update`
  （`assistantMessageEvent`）/`message_end`/`turn_end`/`tool_execution_*`/
  `tool_result_end`/`agent_end`/`agent_settled`）。
- `expected.json` 的 `expected` 字段由 v0.66 源码实读推导（每条 `anchors`
  给出文件:行）；`currentRpiAtM0` 记录 TE13 时的 rpi 现状（即目标轨报告
  中被归因的差异）。TE14 用 `ChildRunState` 回放 JSONL 并断言
  `finalOutput`/`error`/`exitCode` 与 `expected` 一致。
- **三组的语义**：
  1. `will-retry-then-success.jsonl`：第一次 provider 尝试 `errorMessage` +
     `stopReason:"error"` + `agent_end willRetry:true`，随后成功——恢复后
     `errorMessage` 不得残留（R7.1.1.1 #1919）；
  2. `empty-terminal-text.jsonl`：`stopReason:"stop"` 且 content 为空文本、
     `usage.output==0`——空终态按 empty-output 诊断（R7.1.1.2 #1921）；
  3. `tool-error-empty-reply.jsonl`：探索性工具报错后模型空回复——
     empty-output 诊断优先于旧工具错（R7.1.1.2 #1921）。

## 发现目录（`discovery/`）

- `agents-tree/` 提交的文件覆盖：合法 agent、未闭合 frontmatter、
  无冒号行、**fatal frontmatter 两例**（`async: maybe` / `timeoutMs: not-a-number`）、
  嵌套 agent、`sync-backups/`（应剪枝）。
- `.rpi/` 与符号链接**不提交**，由 `materialize.json` 在测试临时目录重建：
  `.rpi/` 被仓库 `.gitignore` 全局忽略（`.rpi/`），符号链接在 Windows
  checkout 不可靠。`materialize.json` 钉死路径、目标、环与期望
  （可见 agent 集合、剪枝路径、静默跳过集合、诊断集合）。
- **TE15 复核修正（2026-09-09）**：TE13 初版把 `broken-frontmatter.md` /
  `broken-no-colon.md` 列为诊断，与上游 v0.66.0 实读不符——两文件在
  `loadAgentsFromDefinitionFiles` 里因缺 name/description 走 `continue`
  （静默跳过，无诊断）；TE15 新增两例 fatal frontmatter 作为真正的诊断
  来源，并把原两例改列 `expected_silent_skips`。
- 消费方式：crate 单测按 `materialize.json` 物化后断言；目标轨 harness 的
  `discovery` 模式用同一树用例与上游 `discoverAgents` 逐字段对拍
  （`scripts/subagents-parity/README.md`「目标轨 discovery 腿」）。

## 终态分类与通知

- `terminal-classification.json`：`child_status_union`/`projection_status_union`
  取自 v0.66 `src/shared/types.ts:398/537`；聚合语义锚点
  `src/runs/foreground/subagent-executor.ts:4228-4237`（stopped/timedOut/
  interrupted）。TE16 补齐完整向量后本文件可扩面（不得静默改语义）。
- `notify-fields.json`：行格式锚点 `src/runs/background/notify.ts:213-232`。
  TE17 落地后 `cases.expected_lines` 作为 renderer 往返断言输入。

## 形状钉死与变更口径

本目录的形状（字段名、目录结构、状态集合）在 TE13 钉死，TE14–TE17 直接
消费。实现期若发现形状需要调整（例如上游字段名与实读不符），按 G2 口径
登记「旧形状 → 新形状 + 上游依据」，并同步本 README 与对应任务文档；
**不得静默改写**。
