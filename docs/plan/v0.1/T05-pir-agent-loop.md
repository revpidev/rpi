# T05：pir-agent — agent_loop 与 Agent

- **状态**：已完成
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

- [x] 设计细化
- [x] 实现
- [x] 自测
- [x] 门禁验收
- [x] 文档回写

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
| D-010 | agent_loop 与 Agent 的 Rust 落地差异（11 项实现细节，见 deviations/D-010） | 已回写 |

## 验收记录

- 验收日期：2026-08-01
- 验收人：kimi-code（T05 执行代理）
- G1 构建/静态检查：通过（`cargo build --workspace` Finished；`cargo clippy --workspace --all-targets -- -D warnings` exit=0；`cargo fmt --all -- --check` FMT-OK）
- G2 测试：通过（`cargo test --workspace` 450 passed, 0 failed；其中 pir-agent 83：既有 17 + agent_loop 27 + agent 19 + messages 15 + parity 5；无 live 测试；测试不触网，faux provider 驱动）
- G3 对拍：通过（parity_events_test.rs 5 场景——single-turn / tool-calls / steering-followup / abort / length-truncation——Agent 事件序列与 `fixtures/generated/<scenario>/events.jsonl` 归一化内容级 diff（`diff_jsonl`，行序敏感）一致；预处理剔除 message_update（delta 边界不入契约，fixtures/README §2）与 AgentSession 层事件（queue_update/agent_settled/willRetry，T16 产物），剥离 usage/details（分别依赖 T07/T16 与 T13 真实工具实现），测试文件头有注释说明）
- G4 红线：通过（`external/pi` HEAD=2efa728d2 且 porcelain 为空；pir-agent 无 `use pir_ai::providers`、无 broadcast、无 ~/.pi 访问；非测试代码无 unwrap/expect（src 内残留 expect 均在 #[cfg(test)] 或带不变式注释的 serde 测试辅助）；新增依赖仅 tokio/tracing（workspace 已有版本））
- G5 线格式：通过（AgentEvent/AgentMessage/扩展消息 serde 形状 T01 已锁并有 roundtrip 测试；toolResult 消息 addedToolNames 仅 len>0 挂载、details null 省略对齐上游；扩展消息文本格式 15 个字节级测试锚定）
- G6 文档同步：通过（agent_loop.rs/agent.rs/messages.rs 头部溯源注释含上游路径+commit 2efa728；D-010 回写 `02-design.md` §4.4）
- G7 偏离闭环：通过（D-010 已登记 + 已回写）

需求 §4.3 十九条语义 → 测试锚点映射表：

| # | 语义 | 测试锚点 |
|---|------|----------|
| 1 | transform→convert→动态 get_api_key→stream_fn；partial 写 context 尾部 | `applies_transform_context_before_convert_to_llm`、`resolves_api_key_dynamically_before_each_llm_call`、`parity_single_turn` |
| 2 | parallel 默认 / 批内任一 sequential → 整批顺序 | `forces_sequential_when_tool_has_execution_mode_sequential`、`forces_sequential_when_one_of_multiple_tools_is_sequential`、`allows_parallel_when_all_tools_have_execution_mode_parallel` |
| 3 | parallel 精确语义（preflight 顺序 / immediate 即时 end / end 完成序 / toolResult 源序） | `emits_tool_execution_end_in_completion_order_results_in_source_order`、`parity_tool_calls` |
| 4 | before_tool_call block / after_tool_call 五字段独立替换 / 钩子抛错降级 | `before_tool_call_block_yields_error_result_without_executing`、`handles_tool_calls_and_results`、`after_tool_call_hook_error_degrades_to_error_result` |
| 5 | length 整批失败保护 | `does_not_execute_tool_calls_from_length_truncated_message`、`parity_length_truncation` |
| 6 | 校验失败/工具未找到 → 错误 toolResult；prepareArguments shim | `validation_failure_yields_error_result_without_executing`、`tool_not_found_yields_error_result_without_executing`、`prepares_tool_arguments_for_validation` |
| 7 | terminate 全员 true 才跳过 LLM | `stops_after_tool_batch_when_every_tool_result_terminates`、`continues_after_parallel_tool_calls_when_not_all_terminate`、`allows_after_tool_call_to_mark_batch_as_terminating` |
| 8 | steering 双轮询点；注入先行 message_start/end | `injects_queued_messages_after_all_tool_calls_complete`、`parity_steering_followup` |
| 9 | follow-up 空闲后注入 | `continue_processes_queued_follow_up_after_assistant_turn`、`parity_steering_followup` |
| 10 | one-at-a-time 默认 / all | `continue_keeps_one_at_a_time_steering_from_assistant_tail` |
| 11 | error/aborted 提前返回（turn_end([]) + agent_end） | `parity_abort`、`emits_full_lifecycle_events_for_run_failures` |
| 12 | 首个 turn 不重复 turn_start；prompt 消息事件在 turn_start 后 | `emits_events_with_agent_message_types`、`agent_loop_continue_from_existing_context_without_user_message_events` |
| 13 | abort 检查点与 "Operation aborted"；sequential 每工具后 break | `abort_in_before_tool_call_yields_operation_aborted_error_result`、`parity_abort` |
| 14 | continue() 降级链 | `continue_keeps_one_at_a_time_steering_from_assistant_tail`、`continue_processes_queued_follow_up_after_assistant_turn`、`agent_loop_continue_throws_when_last_message_is_assistant` |
| 15 | prepareNextTurn / shouldStopAfterTurn | `uses_prepare_next_turn_snapshot_before_continuing`、`stops_after_turn_when_should_stop_after_turn_returns_true` |
| 16 | Agent 全事件屏障；agent_end settle 前 isStreaming/waitForIdle | `awaits_async_subscribers_before_prompt_resolves`、`wait_for_idle_waits_for_async_subscribers` |
| 17 | handleRunFailure 合成失败消息 + 完整事件序列 | `emits_full_lifecycle_events_for_run_failures` |
| 18 | tool_execution_update settle 语义 | `ignores_tool_updates_after_execution_settles`、`ignores_settled_parallel_tool_update_while_another_tool_is_running` |
| 19 | 互斥 activeRun | `throws_when_prompt_called_while_streaming`、`throws_when_continue_called_while_streaming` |

- 结论：通过
