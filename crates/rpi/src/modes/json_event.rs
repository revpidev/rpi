//! JSON/RPC wire-event conversion — port of
//! `packages/coding-agent/src/modes/json-event.ts` @ pi 0.85.0+ (9841914).
//!
//! `toJsonEvent` (json-event.ts:46-60) rewrites `message_update` streaming
//! events for the wire: the top-level cumulative `message` snapshot is
//! replaced by its constant-sized `usage` (c93ea6ccf, #7982 — the latest
//! cumulative provider-reported counts, zero when a provider only reports
//! usage at completion), and a `toolcall_start` delta gains the
//! constant-sized `id`/`toolName` of the toolCall block its `contentIndex`
//! points at (830a0a59e, #7953); every other `assistantMessageEvent` only
//! loses its `partial`. All other events pass through unchanged.
//! `message_start` provides the initial message, deltas build it, and
//! `message_end.message` is the authoritative final state
//! (docs/json.md:87-93, docs/rpc.md:952-956).
//!
//! Implementation choice: serialize-then-rewrite on the `serde_json::Value`,
//! which is the exact equivalent of upstream's rest-destructure
//! (`const { partial: _partial, ...deltaEvent } = assistantMessageEvent`) —
//! both operate on the already-serialized plain object, so the Rust internal
//! event types can keep their cumulative fields (the 7290 regression test
//! asserts they do) without a parallel `Serialize` wrapper that would
//! duplicate — and could drift from — the pinned serde shapes
//! (coding-standards §4.4). The stripped `Value` is dropped before the line
//! is written, so wire memory stays bounded; internal events still carry the
//! cumulative snapshot upstream parity requires.
//!
//! Failure semantics: upstream `toJsonEvent` throws on two invariant
//! violations — a non-assistant `message_update` message (json-event.ts:50)
//! and a `toolcall_start` whose `partial.content[contentIndex]` is not a
//! toolCall block (json-event.ts:24-26) — and both mode pumps call it
//! unguarded, so a violation crashes the process with an unhandled error.
//! rpi returns [`Err`] with the upstream message instead: the sync
//! subscriber closures run inside the agent-loop task, where a panic would
//! be swallowed at the tokio task boundary (process alive, wire dead), so
//! the call sites reject the event — stderr diagnostic, no wire line
//! (V14-03 §5 实现取舍, G2 登记).
//!
//! Intentional differences: none beyond the error channel (the TS overloads
//! collapse into one fn).

use serde_json::{Map, Value};

use crate::core::agent_session::AgentSessionEvent;

/// `toJsonEvent` (json-event.ts:46-60): the single conversion point shared
/// by print (`--mode json`) and RPC mode. Returns the wire shape of `event`,
/// or the upstream error string when a `message_update` invariant is
/// violated (see module docs for the failure-semantics rationale).
pub fn to_json_event(event: &AgentSessionEvent) -> Result<Value, String> {
    // AgentSessionEvent is a plain-data serde type; serialization cannot
    // fail. Fall back to `null` (never reached) instead of panicking.
    let mut value = serde_json::to_value(event).unwrap_or(Value::Null);
    if value.get("type").and_then(Value::as_str) != Some("message_update") {
        return Ok(value);
    }
    let Some(object) = value.as_object_mut() else {
        return Ok(value);
    };
    // `message_update` is only emitted for assistant messages during
    // streaming; upstream throws otherwise (json-event.ts:49-52). rpi's
    // `AgentMessage` union cannot carry that guarantee at the type level,
    // so the check is runtime, like upstream's.
    if object
        .get("message")
        .and_then(|message| message.get("role"))
        .and_then(Value::as_str)
        != Some("assistant")
    {
        return Err("message_update message is not an assistant message".to_owned());
    }
    // `{ type, usage, assistantMessageEvent }` — the upstream return
    // literal (json-event.ts:55-59). `usage` replaces the cumulative
    // `message`: it is the latest cumulative provider-reported usage and
    // may remain zero when a provider only reports usage at completion
    // (docs/json.md:88-90). The role check above guarantees the snapshot
    // is an assistant message, whose `usage` always serializes.
    let usage = object
        .get("message")
        .and_then(|message| message.get("usage"))
        .cloned()
        .unwrap_or(Value::Null);
    let assistant_message_event = match object.get_mut("assistantMessageEvent") {
        Some(event) => to_json_assistant_message_event(event.take())?,
        None => Value::Null,
    };
    let type_tag = object
        .remove("type")
        .unwrap_or_else(|| Value::String("message_update".to_owned()));
    let mut wire = Map::new();
    wire.insert("type".to_owned(), type_tag);
    wire.insert("usage".to_owned(), usage);
    wire.insert("assistantMessageEvent".to_owned(), assistant_message_event);
    // Rebuilding the map (instead of `remove` + append) pins the wire key
    // order to the upstream literal; `preserve_order` keeps insertion order
    // on the wire.
    *object = wire;
    Ok(value)
}

/// `toJsonAssistantMessageEvent` (json-event.ts:18-33): strip the cumulative
/// `partial` snapshot. A `toolcall_start` additionally carries the
/// constant-sized `id`/`toolName` taken from the toolCall block the event's
/// `contentIndex` points at, so clients can label tool calls before the
/// first argument delta arrives.
fn to_json_assistant_message_event(mut event: Value) -> Result<Value, String> {
    let Some(object) = event.as_object_mut() else {
        return Ok(event);
    };
    if object.get("type").and_then(Value::as_str) == Some("toolcall_start") {
        let content_index = object.get("contentIndex").and_then(Value::as_u64);
        let tool_call = object
            .get("partial")
            .and_then(|partial| partial.get("content"))
            .and_then(Value::as_array)
            .and_then(|content| content_index.and_then(|index| content.get(index as usize)));
        // `toolCall?.type !== "toolCall"` (json-event.ts:24-26) — one check
        // covers a missing index, an out-of-range index, and a non-toolCall
        // block at the index.
        if tool_call
            .and_then(|call| call.get("type"))
            .and_then(Value::as_str)
            != Some("toolCall")
        {
            return Err(format!(
                "toolcall_start content at index {} is not a tool call",
                content_index.map_or_else(|| "undefined".to_owned(), |index| index.to_string())
            ));
        }
        let id = tool_call
            .and_then(|call| call.get("id"))
            .cloned()
            .unwrap_or(Value::Null);
        let tool_name = tool_call
            .and_then(|call| call.get("name"))
            .cloned()
            .unwrap_or(Value::Null);
        // `{ ...deltaEvent, id, toolName }` (json-event.ts:28): the
        // remaining delta keys keep their order; the two constants append.
        object.remove("partial");
        object.insert("id".to_owned(), id);
        object.insert("toolName".to_owned(), tool_name);
        return Ok(event);
    }
    // `done`/`error` assistant events carry no `partial`
    // (json-event.ts:30-33 keeps them untouched apart from the top-level
    // `message` drop); delta variants lose only their `partial`.
    object.remove("partial");
    Ok(event)
}

#[cfg(test)]
mod tests {
    use rpi_agent::messages::AgentMessage;
    use rpi_agent::types::AgentEvent;
    use rpi_ai::types::{
        AssistantContent, AssistantMessage, AssistantRole, DoneReason, ErrorReason, StopReason,
        StreamEvent, TextContent, ToolCall, Usage,
    };
    use serde_json::json;

    use super::*;

    fn assistant_message(text: &str) -> AssistantMessage {
        AssistantMessage {
            role: AssistantRole::Assistant,
            content: vec![AssistantContent::Text(TextContent {
                text: text.to_owned(),
                text_signature: None,
            })],
            api: "anthropic-messages".into(),
            provider: "anthropic".to_owned(),
            model: "m".to_owned(),
            response_model: None,
            response_id: None,
            provider_thinking_level: None,
            diagnostics: None,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: 2,
            deferred: None,
            end_turn: None,
            raw_stop_reason: None,
        }
    }

    fn message_update(assistant_message_event: StreamEvent) -> AgentSessionEvent {
        AgentSessionEvent::Agent(Box::new(AgentEvent::MessageUpdate {
            message: AgentMessage::Assistant(assistant_message("cumulative")),
            assistant_message_event: Box::new(assistant_message_event),
        }))
    }

    /// Delta variants: top-level `message` replaced by its `usage`
    /// (c93ea6ccf); `assistantMessageEvent.partial` gone;
    /// `contentIndex`/`delta` retained, camelCase, no null padding.
    #[test]
    fn message_update_delta_strips_cumulative_fields() {
        let event = message_update(StreamEvent::TextDelta {
            content_index: 0,
            delta: "chunk".to_owned(),
            partial: assistant_message("cum"),
        });
        let wire = to_json_event(&event).expect("convert");
        assert_eq!(
            wire,
            json!({
                "type": "message_update",
                "usage": Usage::default(),
                "assistantMessageEvent": {
                    "type": "text_delta",
                    "contentIndex": 0,
                    "delta": "chunk",
                }
            })
        );
        // Key order matches the upstream return literal
        // (`{type, usage, assistantMessageEvent}`; zero usage still carried).
        let line = serde_json::to_string(&wire).expect("serialize");
        assert_eq!(
            line,
            r#"{"type":"message_update","usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0,"cost":{"input":0.0,"output":0.0,"cacheRead":0.0,"cacheWrite":0.0,"total":0.0}},"assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"chunk"}}"#
        );
    }

    /// The wire `usage` is the cumulative snapshot's usage, not a zero
    /// refresh: providers that stream running totals surface them on every
    /// `message_update` (c93ea6ccf, docs/json.md:88-90).
    #[test]
    fn message_update_carries_cumulative_usage() {
        let mut partial = assistant_message("cum");
        partial.usage = Usage {
            input: 11,
            output: 22,
            cache_read: 33,
            cache_write: 44,
            cache_write1h: None,
            reasoning: Some(5),
            total_tokens: 110,
            cost: Default::default(),
        };
        let event = AgentSessionEvent::Agent(Box::new(AgentEvent::MessageUpdate {
            message: AgentMessage::Assistant(partial),
            assistant_message_event: Box::new(StreamEvent::TextDelta {
                content_index: 0,
                delta: "x".to_owned(),
                partial: assistant_message("cum"),
            }),
        }));
        let wire = to_json_event(&event).expect("convert");
        assert_eq!(wire["usage"]["input"], json!(11));
        assert_eq!(wire["usage"]["output"], json!(22));
        assert_eq!(wire["usage"]["cacheRead"], json!(33));
        assert_eq!(wire["usage"]["reasoning"], json!(5));
        assert_eq!(wire["usage"]["totalTokens"], json!(110));
    }

    /// `toolcall_start` carries the constant-sized `id`/`toolName` taken
    /// from `partial.content[contentIndex]` (830a0a59e, #7953) so clients
    /// can label tool calls before the first argument delta.
    #[test]
    fn toolcall_start_carries_id_and_tool_name() {
        let mut partial = assistant_message("cum");
        partial.content = vec![AssistantContent::ToolCall(ToolCall {
            id: "toolu_01".to_owned(),
            name: "bash".to_owned(),
            arguments: serde_json::Map::new(),
            thought_signature: None,
            namespace: None,
        })];
        let event = message_update(StreamEvent::ToolCallStart {
            content_index: 0,
            partial,
        });
        let wire = to_json_event(&event).expect("convert");
        // `{ ...deltaEvent, id, toolName }` (json-event.ts:28): remaining
        // delta keys keep their order, the two constants append.
        let line = serde_json::to_string(&wire).expect("serialize");
        assert_eq!(
            line,
            r#"{"type":"message_update","usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0,"cost":{"input":0.0,"output":0.0,"cacheRead":0.0,"cacheWrite":0.0,"total":0.0}},"assistantMessageEvent":{"type":"toolcall_start","contentIndex":0,"id":"toolu_01","toolName":"bash"}}"#
        );
    }

    /// A `toolcall_start` whose `contentIndex` does not point at a toolCall
    /// block fails conversion with the upstream error message
    /// (json-event.ts:24-26).
    #[test]
    fn toolcall_start_rejects_non_tool_call_content() {
        let mut partial = assistant_message("cum");
        partial.content = vec![AssistantContent::Text(TextContent {
            text: "not a call".to_owned(),
            text_signature: None,
        })];
        let event = message_update(StreamEvent::ToolCallStart {
            content_index: 0,
            partial,
        });
        assert_eq!(
            to_json_event(&event),
            Err("toolcall_start content at index 0 is not a tool call".to_owned())
        );

        // Out-of-range index is the same error path (`toolCall?.type` on
        // `undefined` upstream).
        let event = message_update(StreamEvent::ToolCallStart {
            content_index: 7,
            partial: assistant_message("cum"),
        });
        assert_eq!(
            to_json_event(&event),
            Err("toolcall_start content at index 7 is not a tool call".to_owned())
        );
    }

    /// `message_update` for a non-assistant message fails conversion with
    /// the upstream error message (json-event.ts:50-52). rpi's
    /// `AgentEvent::MessageUpdate` is typed over the `AgentMessage` union,
    /// so this is reachable in principle, unlike upstream's TS types.
    #[test]
    fn message_update_rejects_non_assistant_message() {
        use rpi_ai::types::{UserContent, UserMessage, UserRole};
        let event = AgentSessionEvent::Agent(Box::new(AgentEvent::MessageUpdate {
            message: AgentMessage::User(UserMessage {
                role: UserRole::User,
                content: UserContent::Text("hi".to_owned()),
                timestamp: 2,
            }),
            assistant_message_event: Box::new(StreamEvent::TextDelta {
                content_index: 0,
                delta: "x".to_owned(),
                partial: assistant_message("cum"),
            }),
        }));
        assert_eq!(
            to_json_event(&event),
            Err("message_update message is not an assistant message".to_owned())
        );
    }

    /// V14-03 FR-F R1 / V14-04 FR-D linkage: the `AssistantMessage`
    /// `providerThinkingLevel` field rides message-bearing events verbatim —
    /// camelCase, absent when `None` (ai/types.ts:437, 4e69b0c28). The
    /// write/replay logic itself is V14-05; this pins the event-face shape.
    #[test]
    fn provider_thinking_level_serializes_camelcase_and_skips_none() {
        let mut message = assistant_message("hi");
        let event = AgentSessionEvent::Agent(Box::new(AgentEvent::MessageEnd {
            message: AgentMessage::Assistant(message.clone()),
        }));
        let wire = to_json_event(&event).expect("convert");
        assert!(
            !serde_json::to_string(&wire)
                .expect("serialize")
                .contains("providerThinkingLevel"),
            "None must be absent from the wire"
        );

        message.provider_thinking_level = Some("high".to_owned());
        message.response_id = Some("resp_1".to_owned());
        let event = AgentSessionEvent::Agent(Box::new(AgentEvent::MessageEnd {
            message: AgentMessage::Assistant(message),
        }));
        let wire = to_json_event(&event).expect("convert");
        assert_eq!(wire["message"]["providerThinkingLevel"], json!("high"));
        // Field order matches upstream (responseId → providerThinkingLevel,
        // types.ts:429-437).
        let line = serde_json::to_string(&wire).expect("serialize");
        assert!(line.contains("\"responseId\":\"resp_1\",\"providerThinkingLevel\":\"high\""));
    }

    /// `start`/`done`/`error` assistant events keep their `message`/`error`
    /// payloads inside `assistantMessageEvent`; only `partial` (when
    /// present) and the top-level cumulative `message` are stripped
    /// (json-event.ts:30-39) — replaced by the constant `usage`.
    #[test]
    fn message_update_done_and_error_keep_terminal_payload() {
        let done = message_update(StreamEvent::Done {
            reason: DoneReason::Stop,
            message: assistant_message("final"),
        });
        let wire = to_json_event(&done).expect("convert");
        assert!(wire.get("message").is_none());
        assert!(wire.get("usage").is_some(), "usage still carried");
        let ame = &wire["assistantMessageEvent"];
        assert_eq!(ame["type"], json!("done"));
        assert!(ame.get("partial").is_none());
        assert_eq!(ame["message"]["content"][0]["text"], json!("final"));

        let mut error_message = assistant_message("boom");
        error_message.stop_reason = StopReason::Error;
        error_message.error_message = Some("provider exploded".to_owned());
        let error = message_update(StreamEvent::Error {
            reason: ErrorReason::Error,
            error: error_message,
        });
        let wire = to_json_event(&error).expect("convert");
        assert!(wire.get("message").is_none());
        let ame = &wire["assistantMessageEvent"];
        assert_eq!(ame["type"], json!("error"));
        assert!(ame.get("partial").is_none());
        assert_eq!(ame["error"]["errorMessage"], json!("provider exploded"));
    }

    /// Every non-`message_update` event passes through byte-identical
    /// (json-event.ts:47-49 early return).
    #[test]
    fn other_events_pass_through_unchanged() {
        let cases = vec![
            AgentSessionEvent::Agent(Box::new(AgentEvent::AgentStart)),
            AgentSessionEvent::Agent(Box::new(AgentEvent::MessageStart {
                message: AgentMessage::Assistant(assistant_message("")),
            })),
            AgentSessionEvent::Agent(Box::new(AgentEvent::MessageEnd {
                message: AgentMessage::Assistant(assistant_message("final")),
            })),
            AgentSessionEvent::Agent(Box::new(AgentEvent::TurnEnd {
                message: AgentMessage::Assistant(assistant_message("final")),
                tool_results: vec![],
            })),
        ];
        for event in &cases {
            let direct = serde_json::to_value(event).expect("serialize");
            assert_eq!(to_json_event(event).expect("convert"), direct);
        }
    }
}
