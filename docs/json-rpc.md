# rpi JSON / RPC 线协议契约

> 本文档是 rpi 自身 `--mode json`（print 模式）与 `--mode rpc`（RPC 模式）stdout 线协议的用户向契约说明。rpi 与上游 Pi v0.84.1（`4181f66`）逐字节对拍；完整的 32 个 RPC 命令表与逐字段说明见上游文档 `external/pi/packages/coding-agent/docs/rpc.md` 与 `docs/json.md`，本文档只固化 rpi 侧已验证的契约要点（每条都有对应的测试锚点）。

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

自 v0.11 起，`message_update` **只携带增量 delta**，不再携带累积字段：

- 顶层的累积 `message` 字段已移除；
- `assistantMessageEvent.partial` 已移除。

```json
{"type":"message_update","assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"Hello "}}
```

delta 类型表（`assistantMessageEvent.type`）：

| 类型 | 含义 |
|------|------|
| `text_start` / `text_delta` / `text_end` | 文本块的开始 / 增量 / 结束 |
| `thinking_start` / `thinking_delta` / `thinking_end` | 思考块的开始 / 增量 / 结束 |
| `toolcall_start` / `toolcall_delta` / `toolcall_end` | 工具调用的开始 / 参数增量 / 结束 |

注意：**`start` / `done` / `error` 不再是 delta 类型表条目**（v0.11 移除）。`toolcall_end` 携带完整的 `toolCall` 对象（含可选 `namespace`）；`toolcall_delta` 需由客户端按 `contentIndex` 缓冲拼接。

### 客户端拼装规则

需要实时部分消息的客户端必须自行拼装：`message_start` 给出初始消息，后续 delta 按 `contentIndex` 应用；**`message_end.message` 是权威终态**。不要依赖任何中间事件的累积快照（它们已不在线上）。

## 背压与写错误

- print/rpc 模式的事件写出经过统一的背压写路径（`crates/rpi/src/core/output_guard.rs::RawStdout`）：管道对端消费缓慢时事件源会被自然地限速，事件不丢弃、不合并、无中间缓冲增长。
- 写出失败（如对端关闭管道）时进程以**退出码 1** 结束：RPC 模式在首次写错误时立即退出；print 模式在 run 自然结束时映射为退出码 1。

## 测试锚点

| 契约 | 锚点 |
|------|------|
| delta-only 转换（键序钉死） | `json_event.rs` 单测 |
| 二次方输出回归（#7290） | `crates/rpi/tests/regression_7290_json_stream_linear.rs` |
| 背压慢消费者 | `crates/rpi/tests/json_rpc_backpressure_test.rs` |
| 32 命令契约 | `crates/rpi` `rpc_mode_test.rs`（17 个契约测试） |
