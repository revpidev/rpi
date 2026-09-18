# rpi JSON / RPC wire protocol contract

> This document is the user-facing contract for rpi's own stdout wire protocol in `--mode json` (print mode) and `--mode rpc` (RPC mode). rpi is byte-compared against upstream Pi v0.85.0+ (`9841914`, ADR-0023); the full 33-command RPC table and per-field documentation live in the upstream docs `external/pi/packages/coding-agent/docs/rpc.md` and `docs/json.md` — this document only pins the contract points verified on the rpi side (each has a corresponding test anchor).

## Both modes share one conversion point

The event streams of print mode and RPC mode share the same conversion function (`crates/rpi/src/modes/json_event.rs::to_json_event`, mirroring upstream `json-event.ts`). The `message_update` wire format is therefore identical in both modes.

## First line: session header

The first line of both modes is the session header:

```json
{"type":"session","version":3,...}
```

## Event sequence

One prompt round emits `agent_start → message_start → message_update* → message_end → turn_end → … → agent_end` (retries/compaction/queued continuations may insert further rounds; `agent_settled` marks full quiescence).

## `message_update`: delta-only (v0.11 breaking change)

Since v0.11, `message_update` carries **only incremental deltas and constant-size metadata** — no cumulative fields:

- the top-level cumulative `message` field is gone, replaced by a constant-size `usage` (v0.1.4 / c93ea6ccf): the **cumulative** usage from the most recent provider report; when the provider only reports usage at completion, it stays zero during streaming;
- `assistantMessageEvent.partial` is gone;
- `toolcall_start` deltas carry a constant-size `id` and `toolName` (v0.1.4 / 830a0a59e), taken from the toolCall block of the stripped `partial.content[contentIndex]` — clients can label the tool call before the first argument delta arrives.

```json
{"type":"message_update","usage":{...},"assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"Hello "}}
```

Key order is pinned: top level `type → usage → assistantMessageEvent`; `toolcall_start` deltas `type → contentIndex → id → toolName` (both match upstream's returned literals; `json_event.rs` unit tests assert them byte-for-byte).

Delta type table (`assistantMessageEvent.type`):

| Type | Meaning |
|------|---------|
| `text_start` / `text_delta` / `text_end` | text block start / increment / end |
| `thinking_start` / `thinking_delta` / `thinking_end` | thinking block start / increment / end |
| `toolcall_start` / `toolcall_delta` / `toolcall_end` | tool call start (with `id`/`toolName`) / argument increment / end |

Note: **`start` / `done` / `error` are no longer delta-type table entries** (removed in v0.11). `toolcall_end` carries the complete `toolCall` object (including optional `namespace`); `toolcall_delta` must be buffered and joined by the client per `contentIndex`.

### Client assembly rules

Clients that need live partial messages must assemble them: `message_start` provides the initial message, subsequent deltas apply by `contentIndex`; **`message_end.message` is the authoritative terminal state**. Do not rely on cumulative snapshots in intermediate events (they no longer exist on the wire).

## Queue commands and the Esc combination (`abort` / `clear_queue`)

### `abort`: responds only once idle

`abort` cancels the current run and **waits for the session to be fully idle (including compaction) before responding** (v0.1.4 / bea67d90d):

```json
{"type": "abort"}
```

Response:

```json
{"type": "response", "command": "abort", "success": true}
```

Note: `abort` itself does **not** drain the queue — steering/followUp items left in the queue keep driving the session after the abort. Send `clear_queue` first when a clean slate is needed.

### `clear_queue`: retrieve and drain the queues (v0.1.4 / a79b37334)

Takes out and clears the steering and follow-up queues, returning their texts:

```json
{"type": "clear_queue"}
```

Response:

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

**Interactive Esc semantics**: a client implementing Esc should send `clear_queue` then `abort`, and restore the returned texts into its editor (the recommended combination per upstream docs/rpc.md:155-158). `clear_queue` only reads and drains the queues — it triggers no turn and is not coupled to abort; response ordering and queue-consumption semantics are the client's to compose.

## Backpressure and write errors

- Event writes in print/rpc mode go through a unified backpressure write path (`crates/rpi/src/core/output_guard.rs::RawStdout`): when the pipe peer consumes slowly, event sources are throttled naturally — events are never dropped, merged, or buffered unboundedly.
- On write failure (e.g. the peer closes the pipe) the process exits with **exit code 1**: RPC mode exits immediately on the first write error; print mode maps it to exit code 1 when the run ends naturally.

## Test anchors

| Contract | Anchor |
|------|------|
| delta-only conversion + `usage`/`id`/`toolName` increments (key order pinned) | `json_event.rs` unit tests (7) |
| quadratic-output regression (#7290) | `crates/rpi/tests/regression_7290_json_stream_linear.rs` |
| backpressure against a slow consumer | `crates/rpi/tests/json_rpc_backpressure_test.rs` |
| 33-command contract | `crates/rpi` `rpc_mode_test.rs` (20 contract tests) |
| `clear_queue` retrieve/drain + Esc combination (idle after clear_queue + abort; drained steering not consumed) | `rpc_mode_test.rs` `clear_queue_returns_and_purges_queues` |
