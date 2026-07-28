# T05：pir-agent — agent_loop 与 Agent

- **状态**：未开始
- **里程碑**：M2
- **依赖**：T01、T02（faux provider 驱动事件序测试）
- **上游对照**：`packages/agent/src/agent-loop.ts`、`agent.ts`、`types.ts`
- **需求章节**：§4.2、§4.3
- **预估**：1–1.3 人月（M2 共 1.5–2，与 T06 合计）

---

## 目标

移植 Pi 的核心资产——事件序确定的 tool-use 状态机，用事件序黄金测试锁死，
作为所有运行模式（print/json/rpc/interactive）共用的引擎。

## 范围

### In

- `agent_loop.rs`：无状态循环（伪代码见设计文档 §4.2），emit 事件
- `agent.rs`：状态、subscribe 屏障、steering / follow-up 队列、互斥 run
- 循环语义（需求 §4.3 逐条）：
  - `transform_context`（可选）→ `convert_to_llm`（必选）→ `stream_fn`
  - 工具执行 parallel（默认）/ sequential；completion 事件按完成序，**toolResult 按 assistant 源序**（编码规范 §6.3）
  - `before_tool_call` block / `after_tool_call` 改结果 / `terminate` / `should_stop_after_turn`
  - steering（turn 内工具结束后注入）与 follow-up（空闲后注入）；`one-at-a-time` | `all` 模式
  - abort / continue（重试）语义
  - subscribe 屏障：listener 逐个 await，`message_end` 之后才开始 tool preflight
- 取消：`CancellationToken` 贯穿 stream 与工具执行
- 并发：每 listener 独立 tokio mpsc（不用 broadcast）；`JoinSet` 并行工具执行

### Out

- 具体工具实现（T06）
- session 持久化（T07；`Agent` 不感知 JSONL）

## 开发要点

- `StreamFn` 注入边界不得破坏：agent 内禁止 `use pir_ai::providers`（编码规范 §4.2）
- 事件序是对拍契约的核心：每个语义点都要有确定性 faux 驱动的黄金测试
- 移植上游 `agent-loop.test.ts` / `agent.test.ts` 用例意图，同名 Rust 测试
- 不在锁内 `.await`；后台 spawn 任务必须有取消路径（编码规范 §6.4/§6.5）

## 进度跟踪

- [ ] 设计细化
- [ ] 实现
- [ ] 自测
- [ ] 门禁验收
- [ ] 文档回写

## 自测清单

- [ ] 上游 agent loop 测试意图移植全部通过（事件类型序列逐条断言）
- [ ] parallel 执行：乱序完成下 toolResult 仍按源序组装
- [ ] subscribe 屏障：慢 listener 阻塞时 preflight 等待语义正确
- [ ] steering / follow-up 两种 mode 的注入时机与上游一致
- [ ] abort：stream 中途取消 → `stopReason: "aborted"` 收尾；continue 重试语义一致
- [ ] `terminate: true` 跳过后续 LLM；`should_stop_after_turn` 正确收尾

## 门禁验收

通用门禁 G1–G7 全过（G3 以事件序黄金测试 + faux 场景对拍执行）。

任务特有标准：

- [ ] 需求 §4.3 九条语义逐条有测试锚点（验收记录中列映射表）
- [ ] faux 场景（单轮问答、工具调用、steering、follow-up）事件序列与 fixtures 归一化 diff 一致

## 偏离记录

| 偏离 ID | 摘要 | 状态 |
|---------|------|------|
| — | （暂无） | — |

## 验收记录

（待填写，模板见 `gates.md` §3）
