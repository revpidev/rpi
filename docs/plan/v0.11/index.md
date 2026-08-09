# Pir 开发计划 v0.11 — 任务索引

> 本计划基于 [`../../v0.11/01-requirements.md`](../../v0.11/01-requirements.md)、
> [`../../v0.11/02-design.md`](../../v0.11/02-design.md) 制定，是 v0.1 计划（[`../v0.1/`](../v0.1/)）的增量升级计划。
> 上游对照：`external/pi` @ `4181f66` / v0.84.1+（见 [`UPSTREAM.md`](../../../UPSTREAM.md)）。
> 沿用 v0.1 的 ADR 体系与 [`coding-standards.md`](../../coding-standards.md)；v0.1 全部任务（T01–T16）已完成，本计划任务号从 T17 起。
>
> 版本：v0.11　创建：2026-08-09

---

## 1. 使用说明

- 每个任务一个文件（`TNN-<slug>.md`），可**独立开发、独立自测、独立验收**。约定与流程同 v0.1（[`../v0.1/index.md`](../v0.1/index.md) §1–§2）。
- 任务完成后必须通过门禁验收：通用门禁见 [`gates.md`](./gates.md)，任务特有验收标准见各任务文件「门禁验收」一节。**未通过门禁的任务不得标记为已完成。**
- 偏离管理沿用 v0.1 流程，登记目录为 [`deviations/`](./deviations/)，**编号从 D-051 起**（v0.1 用到 D-050）。
- 本版本是**升级变更**而非新建：每个任务首先要识别 v0.1 已有实现中的受影响面，改动以「增量对齐」为原则，禁止顺手重写既有正确实现。

## 2. 升级总策略（对应设计 §1.1）

1. 基准唯一：`4181f66`，不参考中间版本。
2. 先协议后表现：M1（线格式/消息类型）先行恢复对拍绿，再做行为修正（M2/M3），最后 TUI 大工程（M4/M5）。
3. 不追过渡态：harness v2 运行时、session v4、deferred 生命周期等 [DEFER] 项一律不实现（需求 §1.2），门禁 G4 红线拦截。
4. 渲染基线重录前置：T28 第一天统一重录 TUI 逐帧黄金文件（输入即时渲染使旧基线失效）。

## 3. 任务索引

| ID | 任务 | 里程碑 | 依赖 | 状态 | 验收日期 |
|----|------|--------|------|------|----------|
| T17 | [pir-ai 消息类型与请求选项扩展](./T17-pir-ai-types.md) | M1 | — | 未开始 | |
| T18 | [JSON/RPC delta 线格式与 stdout 背压](./T18-json-rpc-delta.md) | M1 | T17 | 未开始 | |
| T19 | [流终止语义与通用流修复](./T19-stream-termination.md) | M2 | T17 | 未开始 | |
| T20 | [Provider 适配器修复与 compat 扩展](./T20-provider-fixes.md) | M2 | T17 | 未开始 | |
| T21 | [Models refresh 事务化与 OAuth 行为](./T21-models-refresh-oauth.md) | M2 | T17 | 未开始 | |
| T22 | [pir-agent 循环微行为与 compaction 契约](./T22-agent-loop-compaction.md) | M2 | T17 | 未开始 | |
| T23 | [主路径会话行为簇](./T23-session-behaviors.md) | M2 | T18、T19、T22 | 未开始 | |
| T24 | [资源加载与包管理](./T24-resources-packages.md) | M2 | — | 未开始 | |
| T25 | [auth 命令族](./T25-auth-commands.md) | M3 | T21 | 未开始 | |
| T26 | [新 provider 与模型目录更新](./T26-new-providers.md) | M3 | T20、T21 | 未开始 | |
| T27 | [扩展 API 面同步与 Wasm ABI 版本化](./T27-extension-api.md) | M3 | T21、T23 | 未开始 | |
| T28 | [pir-tui 渲染器 trait 化与行为修正](./T28-tui-refactor.md) | M4 | T17 | 未开始 | |
| T29 | [LaTeX 与 Mermaid 渲染](./T29-latex-mermaid.md) | M4 | T28、T27 | 未开始 | |
| T30 | [布局引擎](./T30-layout-engine.md) | M5 | T28 | 未开始 | |
| T31 | [全屏渲染器](./T31-alt-screen.md) | M5 | T30 | 未开始 | |
| T32 | [UI 模式接线与 v0.11 Parity Freeze](./T32-ui-mode-freeze.md) | M5 | T18、T23、T27、T29、T31 | 未开始 | |

## 4. 里程碑映射与并行建议

```
M1: T17 → T18                     （协议对齐，先行解锁对拍）
M2: T19 ∥ T20 ∥ T21 ∥ T22 ∥ T24   （五路并行）
    T23 在 T18/T19/T22 就绪后启动
M3: T25（依赖 T21）∥ T26（依赖 T20/T21）∥ T27（依赖 T21/T23）
M4: T28 → T29                     （T28 与 M2/M3 可全程并行；T29 需 T27 的扩展面）
M5: T30 → T31 → T32               （T32 为 Parity Freeze，收口全版本）
```

- T28（TUI trait 化）与 M2/M3 完全无依赖交叉，建议尽早启动以摊薄 M5 风险。
- T23 的 length-stop 恢复链对应上游四个相互依赖的 commit，必须整体实现（设计 §8 风险 3），不拆分到其他任务。

## 5. 目录结构

```
docs/plan/v0.11/
├── index.md            # 本文件：任务索引与进度跟踪
├── gates.md            # 门禁验收标准（v0.11 版，红线已更新）
├── deviations/         # 偏离登记（编号 D-051 起，流程同 v0.1）
│   ├── README.md
│   └── TEMPLATE.md
└── TNN-*.md            # 任务文件（T17–T32）
```

## 6. 变更记录

| 日期 | 变更 | 说明 |
|------|------|------|
| 2026-08-09 | v0.11 创建 | 基于 v0.11 需求/设计文档划分 16 任务（T17–T32，M1–M5）；上游基线 `2efa728`(v0.82.1) → `4181f66`(v0.84.1+)，461 commits / 655 文件 |
