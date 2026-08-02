# Pir 开发计划 v0.1 — 任务索引

> 本计划基于 [`../../docs/01-requirements.md`](../../01-requirements.md)、
> [`../../docs/02-design.md`](../../02-design.md)、[ADR-0001](../../adr/0001-extension-and-config-dir.md) /
> [ADR-0002](../../adr/0002-baseline-decisions.md) / [ADR-0003](../../adr/0003-coverage-review-scope-decisions.md)、
> [`coding-standards.md`](../../coding-standards.md) 制定。
> 上游对照：`external/pi` @ `2efa728` / 0.82.1（见 [`UPSTREAM.md`](../../../UPSTREAM.md)）。
>
> 版本：v0.1　创建：2026-07-28　最近修订：2026-07-29（覆盖度审查后全面修订，见 §6）

---

## 1. 使用说明

- 每个任务一个文件（`TNN-<slug>.md`），可**独立开发、独立自测、独立验收**。
- 任务完成后必须通过门禁验收：通用门禁见 [`gates.md`](./gates.md)，任务特有验收标准见各任务文件「门禁验收」一节。**未通过门禁的任务不得标记为已完成。**
- 实现过程中与原始文档（需求 / 设计 / ADR / 编码规范）产生的任何偏离，必须：
  1. 登记到 [`deviations/`](./deviations/)（一事一记，流程见 [`deviations/README.md`](./deviations/README.md)）；
  2. **回写**到原始文档对应位置，保持文档与实现一致；
  3. 行为级偏离（影响对拍契约）不允许直接落地，须先立 ADR。
- 偏离未闭环（登记 + 回写）的任务，门禁验收不通过。

## 2. 进度标识约定

任务状态（各任务文件头部「状态」字段，本索引表同步维护）：

| 状态 | 含义 |
|------|------|
| `未开始` | 依赖未就绪或未排期 |
| `进行中` | 已开始实现 |
| `待验收` | 实现与自测完成，等待门禁验收 |
| `已完成` | 门禁验收通过，偏离已闭环 |
| `受阻` | 被外部条件阻塞（在任务文件记录阻塞原因） |

任务内部进度用五个阶段复选框跟踪：**设计细化 → 实现 → 自测 → 门禁验收 → 文档回写**。

## 3. 任务索引

| ID | 任务 | 里程碑 | 依赖 | 状态 | 验收日期 |
|----|------|--------|------|------|----------|
| T01 | [工程骨架与类型契约锁定](./T01-workspace-skeleton.md) | M0 | — | 已完成 | 2026-07-30 |
| T02 | [对拍基建与关键技术验证](./T02-parity-harness.md) | M0 | T01 | 已完成 | 2026-07-30 |
| T03 | [pir-ai 核心协议（Anthropic + OpenAI 系）](./T03-pir-ai-core-protocols.md) | M1 | T01 | 已完成 | 2026-07-30 |
| T04 | [pir-ai Auth 基础](./T04-pir-ai-auth.md) | M1 | T03 | 已完成 | 2026-07-31 |
| T05 | [pir-agent：agent_loop 与 Agent](./T05-pir-agent-loop.md) | M2 | T01、T02 | 已完成 | 2026-08-01 |
| T06 | [内置四工具与 ToolContext](./T06-builtin-tools.md) | M2 | T05 | 已完成 | 2026-08-01 |
| T07 | [SessionManager（JSONL 树）](./T07-session-manager.md) | M3 | T01、T05 | 已完成 | 2026-08-03 |
| T08 | [Compaction](./T08-compaction.md) | M3 | T07 | 未开始 | — |
| T09 | [Settings 与资源加载](./T09-settings-resources.md) | M3 | T01 | 未开始 | — |
| T16 | [pir-agent harness 层](./T16-agent-harness.md) | M3 | T05、T07、T08 | 未开始 | — |
| T10 | [Headless 模式：print / json / rpc](./T10-headless-modes.md) | M4 | T03、T04、T05、T06、T07、T08、T09 | 未开始 | — |
| T11 | [pir-tui 核心引擎](./T11-pir-tui-core.md) | M5 | T01 | 未开始 | — |
| T12 | [pir-tui 组件与 Interactive 模式](./T12-interactive-mode.md) | M5 | T10、T11 | 未开始 | — |
| T13 | [全量 Provider 与 OAuth](./T13-providers-oauth.md) | M6 | T03、T04 | 未开始 | — |
| T14 | [可选工具 / Packages / Trust / Export / llama / 更新](./T14-packages-trust-export.md) | M7 | T09、T10 | 未开始 | — |
| T15 | [扩展宿主 L0+L1 与 Parity Freeze](./T15-extension-host.md) | M8 | T02（spike）、T10、T12 | 未开始 | — |

## 4. 里程碑映射与并行建议

```
M0: T01 → T02
M1: T03 → T04          ┐ 并行
M2: T05 → T06          ┘
M3: T07 → T08,  T09,  T16   ┐ 与 M5（T11→T12）尽早重叠
M4: T10                     ┘
M5: T11 → T12          （TUI 为硬性交付，ADR-0002 §3，不可压后）
M6: T13                （与 M3–M5 并行）
M7: T14
M8: T15                （Parity Freeze）
```

并行口径沿用设计文档 §11：T03∥T05；T07–T10 与 T11–T12 尽早重叠；T13∥T07–T12；T16 在 T07/T08 就绪后插入，可与 T09 并行。

## 5. 目录结构

```
docs/plan/v0.1/
├── index.md            # 本文件：任务索引与进度跟踪
├── gates.md            # 门禁验收标准与流程（所有任务共用）
├── deviations/         # 偏离登记目录（一事一记 + 登记表）
│   ├── README.md       # 偏离管理流程
│   └── TEMPLATE.md     # 偏离记录模板
└── TNN-*.md            # 任务文件（T01–T16）
```

## 6. 变更记录

| 日期 | 变更 | 说明 |
|------|------|------|
| 2026-07-28 | v0.1 创建 | 初始 15 任务划分（M0–M8） |
| 2026-07-28 | 选型收口 | Bedrock 接入 / OAuth 回调 / 动态库 ABI / 事件通道 / 工具并行原语钉死（设计文档 §14），同步 T04/T05/T13/T15 与编码规范 |
| 2026-07-29 | 覆盖度审查修订 | 依据 2026-07-29 覆盖度审查与 ADR-0003 全面修订：新增 T16（harness 层，M3）；T05 循环语义 9→19 条；T06 补 output_accumulator/bash_executor 与全常数锚点；T07 修正 session 无锁、补延迟落盘/id 规则/条目全集；T10 补 RPC 30 命令与 CLI 全标志语义；T13 provider 清单更新为 39 工厂 + 7 OAuth + compat 矩阵；grep/find/ls 原生实现（ADR-0003 §2）归入 T14；T15 能力面更新为 27 事件 + 27 API + 29 UI；gates 补红线与逐条对拍基准 |
| 2026-07-29 | 二次覆盖度复核修订 | 对上一轮审查报告逐项回查上游源码后修订：修正系统性计数错误（RPC 30→32、provider 39→38、扩展事件 27→33、API 方法 27→24、UI 方法 29→28、Context 两级→三级补 ReplacedSessionContext、harness 事件 21→22）；修正与源码相反的描述（originator 字面值 "pi"、-p 吞噬条件、bash 输出不清洗、settings 单层浅合并、agent loop terminate 语义、theme `/` 为 light/dark 分隔符、vertex `{location}` 占位符丢弃、theme colors 51 必填、diagnostics 三种字面值、扩展同名冲突分项规则）；补协议字面量（Claude Code 伪装 2.1.75/beta 头/system 前缀、17 条 canonical 工具名、compat 21 字段与 thinkingFormat 10 取值、call_id\|item_id 复合格式、Codex WS 续传、Azure/mistral/Google/Bedrock 字段级清单）、OAuth 遗漏流程（device code 5 家、codex deviceauth 旁路、copilot policy-enable、ANTHROPIC_AUTH_TOKEN 走 Bearer）、compaction 第 4 个 prompt 与格式串、harness 语义（emitRunFailure 失败路径、subscribe/on 双订阅、entryTransforms/entryProjectors、leaf 重放、proxy 12 事件、JSONL 硬要 v3）、终端自省四件套与 auto light/dark、工具 P2 细节（read 图像拒绝子规则/路径变体、edit JSON-string 强转、grep `:`/`-` 分隔、schema 强转表、retry-after 优先级链、calculateCost tier 口径），并标注 Google/Bedrock SDK 委托来源空白 |
