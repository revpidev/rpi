# rpi JSON / RPC 线协议契约

> 本文档是 rpi 自身 `--mode json`（print 模式）与 `--mode rpc`（RPC 模式）stdout 线协议的用户向契约说明。rpi 与上游 Pi v0.85.0+（`9841914`，ADR-0023）逐字节对拍；完整的 33 个 RPC 命令表与逐字段说明见上游文档 `external/pi/packages/coding-agent/docs/rpc.md` 与 `docs/json.md`，本文档只固化 rpi 侧已验证的契约要点（每条都有对应的测试锚点）。

## 两种模式共用一个转换点

print 模式与 RPC 模式的事件流共用同一个转换函数（`crates/rpi/src/modes/json_event.rs::to_json_event`，对应上游 `json-event.ts`）。因此两种模式的 `message_update` 线格式完全一致。

## 首行：session header

两种模式的首行均为 session header：

```json
{"type":"session","version":3,...}
```

## 事件序列

一轮 prompt 的事件序为 `agent_start → message_start → message_update* →message_end → turn_end → … → agent_end`（retry/compaction/排队续体可能插入更多轮次；`agent_settled` 表示完全收敛）。

## `message_update`：delta-only（v0.11 破坏性变更）

自 v0.11 起，`message_update` **只携带增量 delta 与常量大小的元数据**，不再携带累积字段：

- 顶层的累积 `message` 字段已移除，替换为常量大小的 `usage`（v0.1.4 / c93ea6ccf）：最新一次 provider 上报的**累计** usage；provider 只在完成时上报 usage 时，流式期间保持为零；
- `assistantMessageEvent.partial` 已移除；
- `toolcall_start` 增附带常量大小的 `id` 与 `toolName`（v0.1.4 / 830a0a59e），取自被剥掉的 `partial.content[contentIndex]` 的 toolCall 块，客户端可在首个参数 delta 前标注工具调用。

```json
{"type":"message_update","usage":{...},"assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"Hello "}}
```

键序钉死：顶层 `type → usage → assistantMessageEvent`；`toolcall_start` 增为 `type → contentIndex → id → toolName`（均与上游返回字面量一致，`json_event.rs` 单测逐字节断言）。

delta 类型表（`assistantMessageEvent.type`）：

| 类型 | 含义 |
|------|------|
| `text_start` / `text_delta` / `text_end` | 文本块的开始 / 增量 / 结束 |
| `thinking_start` / `thinking_delta` / `thinking_end` | 思考块的开始 / 增量 / 结束 |
| `toolcall_start` / `toolcall_delta` / `toolcall_end` | 工具调用的开始（含 `id`/`toolName`）/ 参数增量 / 结束 |

注意：**`start` / `done` / `error` 不再是 delta 类型表条目**（v0.11 移除）。`toolcall_end` 携带完整的 `toolCall` 对象（含可选 `namespace`）；`toolcall_delta` 需由客户端按 `contentIndex` 缓冲拼接。

### 客户端拼装规则

需要实时部分消息的客户端必须自行拼装：`message_start` 给出初始消息，后续 delta 按 `contentIndex` 应用；**`message_end.message` 是权威终态**。不要依赖任何中间事件的累积快照（它们已不在线上）。

## 队列命令与 Esc 组合（`abort` / `clear_queue`）

### `abort`：等待 idle 才响应

`abort` 中止当前运行，且**响应前等待会话完全 idle**（含 compaction，v0.1.4 / bea67d90d）：

```json
{"type": "abort"}
```

响应：

```json
{"type": "response", "command": "abort", "success": true}
```

注意：`abort` 本身**不清空队列**——队列中残留的 steering/followUp 会在中止后续驱动会话。需要清空时先发 `clear_queue`。

### `clear_queue`：取回并清空队列（v0.1.4 / a79b37334）

取出并清空 steering 与 follow-up 队列，返回其文本：

```json
{"type": "clear_queue"}
```

响应：

```json
{
  "type": "response",
  "command": "clear_queue",
  "success": true,
  "data": {
    "steering": ["Change direction"],
    "followUp": ["Summarize when finished"]
  }
}
```

**交互式 Esc 语义**：客户端实现 Esc 行为应先 `clear_queue` 再 `abort`，然后把返回的文本还原进编辑器（上游 docs/rpc.md:155-158 的推荐组合）。`clear_queue` 只读清队列，不触发 turn、不与 abort 耦合；两命令的响应顺序与队列消费语义由客户端组合保证。

## 背压与写错误

- print/rpc 模式的事件写出经过统一的背压写路径（`crates/rpi/src/core/output_guard.rs::RawStdout`）：管道对端消费缓慢时事件源会被自然地限速，事件不丢弃、不合并、无中间缓冲增长。
- 写出失败（如对端关闭管道）时进程以**退出码 1** 结束：RPC 模式在首次写错误时立即退出；print 模式在 run 自然结束时映射为退出码 1。

## 测试锚点

| 契约 | 锚点 |
|------|------|
| delta-only 转换 + `usage`/`id`/`toolName` 增量（键序钉死） | `json_event.rs` 单测（7 个） |
| 二次方输出回归（#7290） | `crates/rpi/tests/regression_7290_json_stream_linear.rs` |
| 背压慢消费者 | `crates/rpi/tests/json_rpc_backpressure_test.rs` |
| 33 命令契约 | `crates/rpi` `rpc_mode_test.rs`（20 个契约测试） |
| `clear_queue` 取回/清空 + Esc 组合（clear_queue + abort 后 idle、不消费已清 steering） | `rpc_mode_test.rs` `clear_queue_returns_and_purges_queues` |
