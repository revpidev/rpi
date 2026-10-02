//! V16-06 FR-F wire-shape parity: the native (L0) types in
//! `rpi-ext-host` and the wasm (L1) guest view in `rpi-ext-sdk` serialize
//! identically for the new event/tool-result payloads.

use rpi_ext_host::types as host;
use rpi_ext_sdk::events as guest;
use serde_json::json;

#[test]
fn provider_stream_event_wire_shape_matches() {
    let host_event = host::ProviderStreamEvent {
        provider: "openai".to_owned(),
        api: "openai-responses".to_owned(),
        model: "gpt-6.1-sol".to_owned(),
        data: json!({"type": "response.output_text.delta", "delta": "hi"}),
    };
    let guest_event = guest::ProviderStreamEvent {
        provider: "openai".to_owned(),
        api: "openai-responses".to_owned(),
        model: "gpt-6.1-sol".to_owned(),
        data: json!({"type": "response.output_text.delta", "delta": "hi"}),
    };
    assert_eq!(
        serde_json::to_value(host_event).unwrap(),
        serde_json::to_value(guest_event).unwrap()
    );
}

#[test]
fn mcp_servers_change_wire_shape_matches() {
    let servers = vec![json!({"name": "docs", "enabled": true})];
    assert_eq!(
        serde_json::to_value(host::McpServersChangeEvent {
            servers: servers.clone()
        })
        .unwrap(),
        serde_json::to_value(guest::McpServersChangeEvent { servers }).unwrap()
    );
}

#[test]
fn tool_execution_events_wire_shape_matches() {
    let host_start = host::ToolExecutionStartEvent {
        tool_call_id: "call/1".to_owned(),
        tool_name: "echo".to_owned(),
        args: json!({"a": 1}),
        parent_tool_call_id: Some("call".to_owned()),
    };
    let guest_start = guest::ToolExecutionStartEvent {
        tool_call_id: "call/1".to_owned(),
        tool_name: "echo".to_owned(),
        args: json!({"a": 1}),
        parent_tool_call_id: Some("call".to_owned()),
    };
    assert_eq!(
        serde_json::to_value(host_start).unwrap(),
        serde_json::to_value(guest_start).unwrap()
    );

    let host_end = host::ToolExecutionEndEvent {
        tool_call_id: "call/1".to_owned(),
        tool_name: "echo".to_owned(),
        result: json!({"content": []}),
        is_error: true,
        parent_tool_call_id: None,
    };
    let guest_end = guest::ToolExecutionEndEvent {
        tool_call_id: "call/1".to_owned(),
        tool_name: "echo".to_owned(),
        result: json!({"content": []}),
        is_error: true,
        parent_tool_call_id: None,
    };
    assert_eq!(
        serde_json::to_value(host_end).unwrap(),
        serde_json::to_value(guest_end).unwrap()
    );
}

#[test]
fn tool_result_event_and_patch_wire_shape_matches() {
    let host_event = host::ToolResultEvent {
        tool_call_id: "call-1".to_owned(),
        tool_name: "bash".to_owned(),
        input: json!({}),
        content: vec![json!({"type": "text", "text": "out"})],
        is_error: false,
        details: Some(json!({})),
        structured_content: Some(json!({"ok": true})),
        usage: None,
        parent_tool_call_id: Some("call".to_owned()),
    };
    let guest_event = guest::ToolResultEvent {
        tool_call_id: "call-1".to_owned(),
        tool_name: "bash".to_owned(),
        input: json!({}),
        content: vec![json!({"type": "text", "text": "out"})],
        is_error: false,
        details: Some(json!({})),
        structured_content: Some(json!({"ok": true})),
        usage: None,
        parent_tool_call_id: Some("call".to_owned()),
    };
    assert_eq!(
        serde_json::to_value(host_event).unwrap(),
        serde_json::to_value(guest_event).unwrap()
    );

    let host_patch = host::ToolResultEventResult {
        content: Some(vec![json!({"type": "text", "text": "redacted"})]),
        details: None,
        structured_content: None,
        is_error: Some(true),
        usage: None,
    };
    let guest_patch = guest::ToolResultEventResult {
        content: Some(vec![json!({"type": "text", "text": "redacted"})]),
        details: None,
        structured_content: None,
        is_error: Some(true),
        usage: None,
    };
    assert_eq!(
        serde_json::to_value(host_patch).unwrap(),
        serde_json::to_value(guest_patch).unwrap()
    );
}
