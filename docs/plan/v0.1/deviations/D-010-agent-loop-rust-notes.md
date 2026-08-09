# D-010：agent_loop 与 Agent 的 Rust 落地差异

- **状态**：已回写
- **关联任务**：T05
- **级别**：实现细节偏离
- **发现日期**：2026-08-01

## 原文档约定

- 文档与章节：`docs/02-design.md` §4.2–§4.4；`docs/01-requirements.md` §4.2、§4.3
- 原文约定：移植上游 `agent-loop.ts` / `agent.ts` 全部循环语义与事件序；Agent 层全事件订阅屏障；StreamFn 注入签名 `Fn(Model, Context, StreamOptions) -> BoxStream<'static, StreamEvent>`。

## 实际实现与偏离原因

按上游语义落地，以下实现细节因 Rust 语言/类型系统约束与上游存在差异（行为契约不变，均有同名移植测试锚定）：

1. **before_tool_call args 回传通道**：上游钩子原地 mutate validated args 直达 execute（不再校验）；Rust 钩子按值接收 context 无法回传，改为 `BeforeToolCallResult.args: Option<Value>` 显式替换，同样不做 revalidation（`executes_mutated_before_tool_call_args_without_revalidation` 锚定）。
2. **after_tool_call 错误通道**：上游 throw → catch 降级 error result；Rust 侧 `AfterToolCallFn` 返回 `Result<Option<AfterToolCallResult>, AgentError>`，`Err` 时整体替换为 error result（`after_tool_call_hook_error_degrades_to_error_result` 锚定）。
3. **流无终止事件**：上游挂在 `response.result()` 上取最终消息；Rust `BoxStream` 无 result 通道，for-await 自然结束且无 done/error 时合成 `stopReason=error` 的 assistant 消息（文案 "Stream ended without a terminal done/error event"）并 `tracing::warn`，其余收尾流程与上游一致。
4. **已排队 update 的 await 顺序**：上游 `Promise.all(updateEvents)` 并发 await；Rust 按入队顺序逐个 await（满足「返回前全部 await」，事件序更确定）。
5. **parallel JoinError**：仅任务 panic/abort 可达（工具本身错误已编码为 error result）；缺位槽位合成 `"Tool task failed: ..."` error result 并 `tracing::error`；上游会传播异常。
6. **`AgentToolResult.details: Value::null` → ToolResultMessage.details 省略**：对齐上游 `undefined` 省略语义；`create_error_tool_result` 显式 `details: {}` 与上游一致。
7. **逐字文案错误变体**：新增 `AgentError::Message(String)`（Display 无前缀），供工具/hook 返回需与上游逐字对齐的错误文案；其余变体 Display 带前缀，不适用该场景。
8. **`pending_tool_calls` 用 `HashSet<String>`**：JS Set 的插入序迭代在当前无可观察消费者。
9. **`continue()` → `continue_run`**：Rust 关键字避让，语义不变。
10. **reasoning/thinking_budgets 不经 StreamFn 转发**：钉死的 `StreamOptions` 原无这两个字段（D-013 后 `reasoning` 已落 `StreamOptions`）；它们保留在 `AgentLoopConfig` 上维持 prepareNextTurn 语义（"off"→None）。reasoning 由 `stream_assistant_response` 每轮绑定进 `StreamOptions.reasoning`（镜像上游 `{...config, apiKey, signal}` 展开；2026-08-06 修复前未绑定，thinking 级别从未到达请求，见 `agent_loop.rs` 注释与 `forwards_thinking_level_to_stream_fn_options` 回归测试）。**2026-08-09 订正（T17）**：`StreamOptions.reasoning` 只到 StreamFn 边界还不够——适配器仅在 `stream_simple` 层映射 reasoning（设计 §3.3），plain `stream` 一律丢弃，agent 路径此前走 plain `stream` 导致 thinking 从未进入 provider 请求（会话实测无 thinking 块）。sdk.rs 的 agent stream_fn 现改在 `stream_simple` 边界做转换：`StreamOptions.reasoning` → `SimpleStreamOptions.reasoning`（`ThinkingLevel::from_model_level`），`thinking_budgets` 同源（settings）一并带入，镜像上游 sdk.ts:312 `modelRuntime.streamSimple(...)`；锚定测试 `thinking_level_reaches_provider_stream_and_lands_in_transcript`（默认 Medium 与 set High 均到达 provider，thinking 块落入会话消息）。
11. **listener 屏障为 in-process 串行 await**：设计 §4.3「每 listener 独立 mpsc」适用于事件外发消费者；`Agent::subscribe` 的屏障本身按注册顺序逐个 await listener（与上游 `for (listener) await listener(event, signal)` 同构），屏障语义不变。

## 影响面

无（纯内部）。对外行为（事件序、错误文案、hook 调用顺序、abort/length 保护语义、屏障与互斥语义）逐字对齐上游，有 27 个 agent_loop 移植测试 + 19 个 agent 移植测试 + 5 场景 fixtures 归一化 diff 锚定。

## 处置

- **回写位置**：`docs/02-design.md` §4.4（Rust 落地注记）
- **回写日期**：2026-08-01
- **ADR**：不需要
