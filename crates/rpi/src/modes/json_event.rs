//! JSON/RPC wire-event conversion — port of
//! `packages/coding-agent/src/modes/json-event.ts` @ pi 0.84.1+ (4181f66).
//!
//! `toJsonEvent` (json-event.ts:28-40) strips cumulative assistant snapshots
//! from streaming wire events: `message_update` loses its top-level `message`
//! field and its `assistantMessageEvent.partial`; every other event passes
//! through unchanged. `message_start` provides the initial message, deltas
//! build it, and `message_end.message` is the authoritative final state
//! (docs/json.md:82-85, docs/rpc.md:952-956).
//!
//! Implementation choice: serialize-then-strip on the `serde_json::Value`,
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
//! Intentional differences: none (the TS overloads collapse into one fn).

use serde_json::Value;

use crate::core::agent_session::AgentSessionEvent;

/// `toJsonEvent` (json-event.ts:28-40): the single conversion point shared
/// by print (`--mode json`) and RPC mode. Returns the wire shape of `event`.
pub fn to_json_event(event: &AgentSessionEvent) -> Value {
    // AgentSessionEvent is a plain-data serde type; serialization cannot
    // fail. Fall back to `null` (never reached) instead of panicking.
    let mut value = serde_json::to_value(event).unwrap_or(Value::Null);
    if value.get("type").and_then(Value::as_str) != Some("message_update") {
        return value;
    }
    if let Some(object) = value.as_object_mut() {
        // `{ type: "message_update", assistantMessageEvent }` — the upstream
        // return literal keeps only these two keys, in this order.
        object.remove("message");
        if let Some(assistant_message_event) = object
            .get_mut("assistantMessageEvent")
            .and_then(Value::as_object_mut)
        {
            // `done`/`error` assistant events carry no `partial`
            // (json-event.ts:34-36 keeps them untouched apart from the
            // top-level `message` drop).
            assistant_message_event.remove("partial");
        }
    }
    value
}

#[cfg(test)]
mod tests {
    use rpi_agent::messages::AgentMessage;
    use rpi_agent::types::AgentEvent;
    use rpi_ai::types::{
        AssistantContent, AssistantMessage, AssistantRole, DoneReason, ErrorReason, StopReason,
        StreamEvent, TextContent, Usage,
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

    /// Delta variants: top-level `message` and `assistantMessageEvent.partial`
    /// gone; `contentIndex`/`delta` retained, camelCase, no null padding.
    #[test]
    fn message_update_delta_strips_cumulative_fields() {
        let event = message_update(StreamEvent::TextDelta {
            content_index: 0,
            delta: "chunk".to_owned(),
            partial: assistant_message("cum"),
        });
        let wire = to_json_event(&event);
        assert_eq!(
            wire,
            json!({
                "type": "message_update",
                "assistantMessageEvent": {
                    "type": "text_delta",
                    "contentIndex": 0,
                    "delta": "chunk",
                }
            })
        );
        // Key order matches the upstream return literal (type first).
        let line = serde_json::to_string(&wire).expect("serialize");
        assert_eq!(
            line,
            r#"{"type":"message_update","assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"chunk"}}"#
        );
    }

    /// `start`/`done`/`error` assistant events keep their `message`/`error`
    /// payloads; only `partial` (when present) and the top-level cumulative
    /// `message` are stripped (json-event.ts:33-39).
    #[test]
    fn message_update_done_and_error_keep_terminal_payload() {
        let done = message_update(StreamEvent::Done {
            reason: DoneReason::Stop,
            message: assistant_message("final"),
        });
        let wire = to_json_event(&done);
        assert!(wire.get("message").is_none());
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
        let wire = to_json_event(&error);
        assert!(wire.get("message").is_none());
        let ame = &wire["assistantMessageEvent"];
        assert_eq!(ame["type"], json!("error"));
        assert!(ame.get("partial").is_none());
        assert_eq!(ame["error"]["errorMessage"], json!("provider exploded"));
    }

    /// Every non-`message_update` event passes through byte-identical
    /// (json-event.ts:29-31 early return).
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
            assert_eq!(to_json_event(event), direct);
        }
    }
}
