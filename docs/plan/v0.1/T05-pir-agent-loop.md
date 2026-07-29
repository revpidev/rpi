# T05：pir-agent — agent_loop 与 Agent

- **状态**：未开始
- **里程碑**：M2
- **依赖**：T01、T02（faux provider 驱动事件序测试）
- **上游对照**：`packages/agent/src/agent-loop.ts`、`agent.ts`、`types.ts`、`harness/messages.ts`（扩展消息格式）
- **需求章节**：§4.1、§4.2、§4.3
- **预估**：1.3–1.6 人月（M2 共 2–2.4，与 T06 合计）

---

## 目标

移植 Pi 的核心资产——事件序确定的 tool-use 状态机，用事件序黄金测试锁死，
作为所有运行模式（print/json/rpc/interactive）共用的引擎。

## 范围

### In

- `agent_loop.rs`：无状态循环（伪代码见设计文档 §4.2），observational EventStream（**无屏障**）
- `agent.rs`：状态、**全事件订阅屏障**、steering / follow-up 队列、互斥 run
- 循环语义（需求 §4.3 共 19 条，逐条锚定）：
  - `transform_context`（可选，不得抛异常）→ `convert_to_llm`（必选；Agent 默认 filter）→ **每次 LLM 前动态 `get_api_key`** → `stream_fn`；partial 实时写 context 尾部，done/error 替换
  - 工具执行 parallel（默认）/ sequential；**batch 内任一工具声明 sequential 则整批顺序**
  - parallel 精确语义：preflight **始终顺序**（find → `prepare_arguments` shim → 校验 → `before_tool_call` → abort 检查）；immediate 结果按源序即时 emit end；并发执行 end 按完成序；**toolResult 持久化按 assistant 源序**（编码规范 §6.3）
  - `before_tool_call` block / `after_tool_call` 五字段独立整体替换（无深合并，钩子抛错降级 error result）/ `terminate`（runtime-only，全员 true 才生效）/ `should_stop_after_turn` / `prepare_next_turn`
  - **`stopReason === "length"` 整批失败保护**：所有 tool call 不执行，各产固定文案错误 toolResult
  - 参数校验失败/工具未找到 → 错误 toolResult 不执行
  - steering 双轮询点（**run 启动时** + 内层迭代末尾）与 follow-up（空闲后）；`one-at-a-time`（**默认**，drain 取最老一条）| `all`
  - `error`/`aborted` 提前返回：直接 `turn_end([])` + `agent_end`，不查工具不轮询
  - turn 边界：首个 turn 不重复 `turn_start`；prompt 消息 message_start/end 在 `turn_start` 后
  - abort 多检查点（错误文案 `"Operation aborted"`）；sequential 每个工具后检查 break
  - `continue()` 降级链：assistant 结尾时先 drain steering（跳过首次轮询）再 drain followUp，否则抛错
  - `Agent` 屏障：每事件先 reduce 状态再按注册顺序 await 全部 listener；`agent_end` settle 前 `isStreaming` 保持 true、`waitForIdle()` 不 resolve
  - `handleRunFailure`：loop 抛错合成 failure assistant 消息 + 补发完整事件序列
  - `tool_execution_update`：settle 后 onUpdate 忽略，已排队 update 返回前 await
  - 互斥：`activeRun` 存在时 `prompt()`/`continue()` 抛错
- 事件载荷（需求 §4.2）：`turn_end{message,toolResults}` / `agent_end{messages}` / `message_update{assistantMessageEvent}`（10 种流式子事件）/ toolResult 消息的 message_start/end 对
- 扩展消息 → LLM 逐字文本格式（`bashExecutionToText`、PREFIX/SUFFIX 常量；`addedToolNames` 条件挂载）
- 取消：`CancellationToken` 贯穿 stream 与工具执行
- 并发：每 listener 独立 tokio mpsc（不用 broadcast）；`JoinSet` 并行工具执行

### Out

- 具体工具实现（T06）
- session 持久化（T07；`Agent` 不感知 JSONL）
- harness 层（T16）

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
- [ ] parallel 执行：乱序完成下 toolResult 仍按源序组装；batch 内任一 sequential → 整批顺序
- [ ] preflight 顺序性与 immediate 结果按源序即时完成
- [ ] subscribe 屏障：慢 listener 阻塞时 preflight 等待语义正确（全事件，非仅 message_end）
- [ ] steering 双轮询点 / follow-up 两种 mode 的注入时机与上游一致
- [ ] length 截断整批失败保护：事件序与错误文案一致
- [ ] error/aborted 提前返回路径事件序正确
- [ ] abort：stream 中途取消 → `stopReason: "aborted"` 收尾；continue 降级链各分支
- [ ] `terminate: true`（全员）跳过后续 LLM；`should_stop_after_turn` / `prepare_next_turn` 正确收尾与替换
- [ ] `handleRunFailure` 合成事件序列正确
- [ ] 扩展消息文本格式（bashExecution/compaction/branchSummary）与上游逐字节一致

## 门禁验收

通用门禁 G1–G7 全过（G3 以事件序黄金测试 + faux 场景对拍执行）。

任务特有标准：

- [ ] 需求 §4.3 十九条语义逐条有测试锚点（验收记录中列映射表）
- [ ] faux 场景（单轮问答、工具调用、steering、follow-up、abort、length 截断）事件序列与 fixtures 归一化 diff 一致

## 偏离记录

| 偏离 ID | 摘要 | 状态 |
|---------|------|------|
| — | （暂无） | — |

## 验收记录

（待填写，模板见 `gates.md` §3）
