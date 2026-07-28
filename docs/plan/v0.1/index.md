# Pir 开发计划 v0.1 — 任务索引

> 本计划基于 [`../../docs/01-requirements.md`](../../01-requirements.md)、
> [`../../docs/02-design.md`](../../02-design.md)、[ADR-0001](../../adr/0001-extension-and-config-dir.md) /
> [ADR-0002](../../adr/0002-baseline-decisions.md)、[`coding-standards.md`](../../coding-standards.md) 制定。
> 上游对照：`external/pi` @ `2efa728` / 0.82.1（见 [`UPSTREAM.md`](../../../UPSTREAM.md)）。
>
> 版本：v0.1　创建：2026-07-28

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
| T01 | [工程骨架与类型契约锁定](./T01-workspace-skeleton.md) | M0 | — | 未开始 | — |
| T02 | [对拍基建与关键技术验证](./T02-parity-harness.md) | M0 | T01 | 未开始 | — |
| T03 | [pir-ai 核心协议（Anthropic + OpenAI 系）](./T03-pir-ai-core-protocols.md) | M1 | T01 | 未开始 | — |
| T04 | [pir-ai Auth 基础](./T04-pir-ai-auth.md) | M1 | T03 | 未开始 | — |
| T05 | [pir-agent：agent_loop 与 Agent](./T05-pir-agent-loop.md) | M2 | T01、T02 | 未开始 | — |
| T06 | [内置四工具与 ToolContext](./T06-builtin-tools.md) | M2 | T05 | 未开始 | — |
| T07 | [SessionManager（JSONL 树）](./T07-session-manager.md) | M3 | T01、T05 | 未开始 | — |
| T08 | [Compaction](./T08-compaction.md) | M3 | T07 | 未开始 | — |
| T09 | [Settings 与资源加载](./T09-settings-resources.md) | M3 | T01 | 未开始 | — |
| T10 | [Headless 模式：print / json / rpc](./T10-headless-modes.md) | M4 | T03、T04、T05、T06、T07、T08、T09 | 未开始 | — |
| T11 | [pir-tui 核心引擎](./T11-pir-tui-core.md) | M5 | T01 | 未开始 | — |
| T12 | [pir-tui 组件与 Interactive 模式](./T12-interactive-mode.md) | M5 | T10、T11 | 未开始 | — |
| T13 | [全量 Provider 与 OAuth](./T13-providers-oauth.md) | M6 | T03、T04 | 未开始 | — |
| T14 | [Packages / Trust / Export / llama / 更新](./T14-packages-trust-export.md) | M7 | T09、T10 | 未开始 | — |
| T15 | [扩展宿主 L0+L1 与 Parity Freeze](./T15-extension-host.md) | M8 | T02（spike）、T10、T12 | 未开始 | — |

## 4. 里程碑映射与并行建议

```
M0: T01 → T02
M1: T03 → T04          ┐ 并行
M2: T05 → T06          ┘
M3: T07 → T08,  T09    ┐ 与 M5（T11→T12）尽早重叠
M4: T10                ┘
M5: T11 → T12          （TUI 为硬性交付，ADR-0002 §3，不可压后）
M6: T13                （与 M3–M5 并行）
M7: T14
M8: T15                （Parity Freeze）
```

并行口径沿用设计文档 §11：T03∥T05；T07–T10 与 T11–T12 尽早重叠；T13∥T07–T12。

## 5. 目录结构

```
docs/plan/v0.1/
├── index.md            # 本文件：任务索引与进度跟踪
├── gates.md            # 门禁验收标准与流程（所有任务共用）
├── deviations/         # 偏离登记目录（一事一记 + 登记表）
│   ├── README.md       # 偏离管理流程
│   └── TEMPLATE.md     # 偏离记录模板
└── TNN-*.md            # 任务文件（T01–T15）
```

## 6. 变更记录

| 日期 | 变更 | 说明 |
|------|------|------|
| 2026-07-28 | v0.1 创建 | 初始 15 任务划分（M0–M8） |
| 2026-07-28 | 选型收口 | Bedrock 接入 / OAuth 回调 / 动态库 ABI / 事件通道 / 工具并行原语钉死（设计文档 §14），同步 T04/T05/T13/T15 与编码规范 |
