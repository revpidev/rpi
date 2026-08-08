//! Permission-gate example extension (wasm guest, ABI v1).
//!
//! Behavior (matches the native inline variant used in
//! `crates/pir/tests/extension_host_w6_test.rs`):
//! - blocks the `read` tool with a reason (`tool_call` handler),
//! - registers a custom `gate_tool` that returns fixed output.

use pir_ext_sdk::{export, Extension};
use serde_json::{json, Value};

fn register(ext: &mut Extension) {
    ext.on("tool_call", |payload| {
        if payload["toolName"].as_str() == Some("read") {
            Ok(json!({"block": true, "reason": "gate-block"}))
        } else {
            Ok(Value::Null)
        }
    });
    ext.tool(
        json!({
            "name": "gate_tool",
            "label": "Gate Tool",
            "description": "parity tool",
            "parameters": {"type": "object"},
        }),
        |_params| {
            Ok(json!({
                "content": [{"type": "text", "text": "gate-output"}],
                "details": null,
            }))
        },
    );
}

export!(register);
