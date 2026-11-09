//! rpi#78 regression: the extension `message_update` path is delta-sized.
//!
//! The host used to materialize the whole accumulated `AssistantMessage` on
//! every streaming delta whenever an extension subscribed (deep clone +
//! `serde_json::to_value` + a full JSON round trip across the ext ABI), i.e.
//! O(message) per delta and O(n²) per response. `message_update_delta` is the
//! additive delta-only event (rpi#78 A1): the payload is the same
//! `{type, usage, assistantMessageEvent}` shape the JSON/RPC wire already uses,
//! built from the typed event without ever touching the cumulative snapshot.
//!
//! This gate pins the size contract: the payload depends on the DELTA, not on
//! the accumulated message. A 100× larger message with the same one-word delta
//! must produce a payload of exactly the same size (and well under 1 KB), while
//! the cumulative event that `message_update` carries keeps growing.

use std::sync::Arc;

use rpi::core::agent_session::AgentSessionEvent;
use rpi::modes::json_event::{message_update_payload, to_json_event};
use rpi_agent::types::AgentEvent;
use rpi_ai::types::{
    ApiKind, AssistantContent, AssistantMessage, AssistantRole, SharedAssistantMessage, StopReason,
    StreamEvent, TextContent, ToolCall, Usage,
};

/// A message whose accumulated content is `filler` bytes long, plus a small
/// `text_delta` event that references it as its `partial` snapshot.
fn sample(filler: usize, with_tool_call: bool) -> (SharedAssistantMessage, StreamEvent) {
    let body = "x".repeat(filler);
    let mut content = vec![AssistantContent::Text(TextContent {
        text: body,
        text_signature: None,
    })];
    if with_tool_call {
        content.push(AssistantContent::ToolCall(ToolCall {
            id: "toolu_01".to_owned(),
            name: "write".to_owned(),
            arguments: serde_json::Map::new(),
            thought_signature: None,
            namespace: None,
            arguments_error: None,
        }));
    }
    let message = AssistantMessage {
        role: AssistantRole::Assistant,
        content,
        api: ApiKind("openai-completions".into()),
        provider: "faux".to_owned(),
        model: "faux-1".to_owned(),
        response_model: None,
        response_id: None,
        provider_thinking_level: None,
        thinking_level: None,
        diagnostics: None,
        usage: Usage::default(),
        stop_reason: StopReason::Stop,
        error_message: None,
        timestamp: 1_700_000_000_000,
        deferred: None,
        end_turn: None,
        raw_stop_reason: None,
        duration_ms: None,
    };
    let message = SharedAssistantMessage::new(message);
    let event = if with_tool_call {
        StreamEvent::ToolCallStart {
            content_index: 1,
            partial: message.clone(),
        }
    } else {
        StreamEvent::TextDelta {
            content_index: 0,
            delta: "chunk".to_owned(),
            partial: message.clone(),
        }
    };
    (message, event)
}

fn payload_bytes(message: &SharedAssistantMessage, event: &StreamEvent) -> usize {
    let payload = message_update_payload(message, event).expect("delta payload");
    serde_json::to_vec(&payload).expect("serialize").len()
}

/// The delta payload is O(delta): a 100× larger message must not make it bigger.
#[test]
fn delta_payload_is_independent_of_the_accumulated_message() {
    let (small_message, small_event) = sample(2_000, false);
    let (large_message, large_event) = sample(200_000, false);
    let small = payload_bytes(&small_message, &small_event);
    let large = payload_bytes(&large_message, &large_event);
    assert_eq!(
        small, large,
        "the delta payload must not include the accumulated message ({small} -> {large})"
    );
    assert!(
        large < 1_024,
        "a one-word delta must stay tiny (got {large} bytes)"
    );

    // `toolcall_start` labels must be O(1) too (id/toolName from the snapshot,
    // read by index rather than serialized).
    let (tool_small, tool_small_event) = sample(2_000, true);
    let (tool_large, tool_large_event) = sample(200_000, true);
    let tool_small_bytes = payload_bytes(&tool_small, &tool_small_event);
    let tool_large_bytes = payload_bytes(&tool_large, &tool_large_event);
    assert_eq!(tool_small_bytes, tool_large_bytes);
    assert!(tool_large_bytes < 1_024, "got {tool_large_bytes} bytes");
}

#[test]
fn toolcall_start_label_extraction_releases_the_live_message_lock() {
    let (message, event) = sample(2_000, true);
    let payload = message_update_payload(&message, &event).expect("toolcall_start payload");
    assert_eq!(payload["assistantMessageEvent"]["id"], "toolu_01");
    assert_eq!(payload["assistantMessageEvent"]["toolName"], "write");

    // A nested serializer or a retained read guard would deadlock this update.
    message.with_mut(|message| message.usage.output = 7);
    let payload = message_update_payload(&message, &event).expect("payload after mutation");
    assert_eq!(payload["usage"]["output"], 7);
}

/// The legacy cumulative payload that `message_update` carries keeps growing
/// with the message — the cost the delta event exists to avoid.
#[test]
fn legacy_cumulative_payload_still_scales_with_the_message() {
    let (small_message, small_event) = sample(2_000, false);
    let (large_message, large_event) = sample(200_000, false);
    let cumulative = |message: &SharedAssistantMessage, event: &StreamEvent| {
        let value = serde_json::json!({
            "message": message.snapshot(),
            "assistantMessageEvent": event,
        });
        serde_json::to_vec(&value).expect("serialize").len()
    };
    let small = cumulative(&small_message, &small_event);
    let large = cumulative(&large_message, &large_event);
    assert!(
        large > small * 50,
        "the cumulative payload scales with the message: {small} -> {large}"
    );
}

/// The JSON/RPC wire stays delta-only and now shares the same converter.
#[test]
fn wire_and_ext_payload_share_the_delta_shape() {
    let (message, event) = sample(200_000, false);
    let session_event = AgentSessionEvent::Agent(Box::new(AgentEvent::MessageUpdate {
        message: message.clone(),
        assistant_message_event: Arc::new(event.clone()),
    }));
    let wire = to_json_event(&session_event).expect("wire");
    assert!(wire.get("message").is_none(), "wire drops the message");
    assert!(wire["assistantMessageEvent"].get("partial").is_none());
    let ext = message_update_payload(&message, &event).expect("ext payload");
    assert_eq!(
        wire["assistantMessageEvent"], ext["assistantMessageEvent"],
        "the wire and the delta event carry one shape"
    );
    assert_eq!(wire["usage"], ext["usage"]);
}

/// Tool-call completion carries the completed call, but must not serialize or
/// expose the unrelated accumulated snapshot just to strip it afterward.
#[test]
fn toolcall_end_payload_excludes_the_accumulated_partial() {
    let (message, _) = sample(200_000, true);
    let tool_call = message.with(|message| {
        let Some(AssistantContent::ToolCall(tool_call)) = message.content.get(1) else {
            panic!("tool-call fixture");
        };
        tool_call.clone()
    });
    let event = StreamEvent::ToolCallEnd {
        content_index: 1,
        tool_call: tool_call.clone(),
        partial: message.clone(),
    };
    let payload = message_update_payload(&message, &event).expect("toolcall_end payload");
    assert_eq!(payload["type"], "message_update");
    assert_eq!(payload["assistantMessageEvent"]["type"], "toolcall_end");
    assert_eq!(payload["assistantMessageEvent"]["contentIndex"], 1);
    assert_eq!(
        payload["assistantMessageEvent"]["toolCall"]["type"],
        "toolCall"
    );
    assert_eq!(
        payload["assistantMessageEvent"]["toolCall"]["id"],
        "toolu_01"
    );
    assert!(payload["assistantMessageEvent"].get("partial").is_none());
    assert!(serde_json::to_vec(&payload).expect("serialize").len() < 1_024);
}
