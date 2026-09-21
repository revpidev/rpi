//! V15-14 (ADR-0030): the `ctx.sessionToolResults` wire shape must be
//! identical on both carriers. The native dispatch (`rpi-ext-host`) and
//! the wasm SDK (`rpi-ext-sdk`) each own a typed view of the frozen
//! result shape — this suite asserts byte-level JSON parity over a
//! shared sample corpus (the V14-25 `session_entries_parity.rs`
//! precedent, scoped to the single host-call added by ADR-0030), plus
//! identical request shapes and probe outcomes against one transport
//! script.

use std::collections::VecDeque;
use std::sync::Mutex;

use rpi_ext_host::types::SessionToolResultInfo as NativeResult;
use rpi_ext_sdk::interactive_ui::{HostCall, InteractiveUiError, InteractiveUiErrorKind};
use rpi_ext_sdk::session_entries as wasm;
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Scripted transport
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
enum Reply {
    Ok(Value),
    Err {
        kind: &'static str,
        message: &'static str,
    },
}

struct FakeHost {
    calls: Mutex<Vec<(String, Value)>>,
    replies: Mutex<VecDeque<Reply>>,
}

impl FakeHost {
    fn new(replies: Vec<Reply>) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            replies: Mutex::new(replies.into()),
        }
    }

    fn calls(&self) -> Vec<(String, Value)> {
        self.calls.lock().unwrap().clone()
    }
}

impl HostCall for FakeHost {
    fn call(&self, method: &str, args: Value) -> Result<Value, InteractiveUiError> {
        self.calls
            .lock()
            .unwrap()
            .push((method.to_owned(), args.clone()));
        match self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Reply::Err {
                kind: "internal",
                message: "no scripted reply",
            }) {
            Reply::Ok(value) => Ok(value),
            Reply::Err { kind, message } => Err(InteractiveUiError::from_host_error(kind, message)),
        }
    }
}

// ---------------------------------------------------------------------------
// Sample corpus (both directions: serialize native → parse wasm and vice
// versa, re-serialize equal — byte-level parity)
// ---------------------------------------------------------------------------

fn corpus() -> Vec<NativeResult> {
    vec![
        // Root entry, structured details snapshot (rpiv-todo envelope
        // shape — the ADR-0030 consumer's payload).
        NativeResult {
            id: "e1".to_owned(),
            parent_id: None,
            timestamp: "2026-09-20T00:00:00.000Z".to_owned(),
            tool_name: "todo".to_owned(),
            is_error: false,
            details: json!({
                "tasks": [
                    {"id": 1, "content": "write replay tests", "status": "pending",
                     "priority": "high"},
                    {"id": 2, "content": "清空已完成", "status": "completed",
                     "priority": "medium"}
                ],
                "nextId": 3
            }),
        },
        // Mid-branch error result, details absent (null).
        NativeResult {
            id: "e2".to_owned(),
            parent_id: Some("e1".to_owned()),
            timestamp: "2026-09-20T00:00:01.000Z".to_owned(),
            tool_name: "todo".to_owned(),
            is_error: true,
            details: Value::Null,
        },
        // Another extension's tool (mcp approval-collection family) —
        // the channel is generic over toolName, not todo-specific.
        NativeResult {
            id: "e3".to_owned(),
            parent_id: Some("e2".to_owned()),
            timestamp: "2026-09-20T00:00:02.000Z".to_owned(),
            tool_name: "mcp_approval".to_owned(),
            is_error: false,
            details: json!({"granted": ["tavily__search"], "revoked": []}),
        },
    ]
}

/// Byte-level parity: native serialization round-trips through the wasm
/// type and back, and the two serializations are identical strings.
#[test]
fn result_shape_parity_native_wasm() {
    for native in corpus() {
        let native_json = serde_json::to_string(&native).expect("native serialize");

        let wasm_result: wasm::SessionToolResult =
            serde_json::from_str(&native_json).expect("wasm parses native JSON");
        let wasm_json = serde_json::to_string(&wasm_result).expect("wasm serialize");
        assert_eq!(native_json, wasm_json, "byte-identical for {}", native.id);

        let round_trip: NativeResult =
            serde_json::from_str(&wasm_json).expect("native parses wasm JSON");
        assert_eq!(round_trip, native);
    }
}

/// The frozen field set/order on the wire (ADR-0030 decision 2/3): `id,
/// parentId, timestamp, toolName, isError, details` — camelCase, exactly
/// six fields, `details` always present, NO `content` key ever.
#[test]
fn result_wire_fields_are_frozen_to_six() {
    let native = &corpus()[1];
    let json = serde_json::to_value(native).expect("serialize");
    let object = json.as_object().expect("object");
    assert_eq!(
        object.keys().collect::<Vec<_>>(),
        vec![
            "id",
            "parentId",
            "timestamp",
            "toolName",
            "isError",
            "details"
        ],
        "frozen six-field key set (FR-B red line)"
    );
    assert_eq!(json["parentId"], json!("e1"));
    assert_eq!(json["isError"], json!(true));
    assert_eq!(json["details"], Value::Null, "absent details → null");
    assert!(
        !object.contains_key("content"),
        "content blocks must never be projected"
    );
}

/// Request-shape parity: the SDK wrapper emits exactly the frozen request
/// (`{toolName, limit?}` — toolName required, limit optional) for
/// `ctx.sessionToolResults`.
#[test]
fn request_shape_and_probe_parity() {
    let host = FakeHost::new(vec![
        Reply::Ok(json!([])),
        Reply::Ok(json!([])),
        Reply::Ok(json!([])),
    ]);
    wasm::session_tool_results(&host, "todo", Some(100)).expect("call 1");
    wasm::session_tool_results(&host, "todo", None).expect("call 2");
    wasm::supports_session_tool_results(&host).expect("probe");
    assert_eq!(
        host.calls(),
        vec![
            (
                "ctx.sessionToolResults".to_owned(),
                json!({"toolName": "todo", "limit": 100})
            ),
            (
                "ctx.sessionToolResults".to_owned(),
                json!({"toolName": "todo"})
            ),
            (
                "ctx.sessionToolResults".to_owned(),
                json!({"toolName": "rpi-abi-probe"})
            ),
        ]
    );
}

/// Probe outcome parity: `unknownMethod` (pre-V15-14 host) → `Ok(false)`;
/// any successful answer (including `[]`) → `Ok(true)`; other kinds
/// propagate.
#[test]
fn probe_outcomes() {
    let old_host = FakeHost::new(vec![Reply::Err {
        kind: "unknownMethod",
        message: "unknown host call: ctx.sessionToolResults",
    }]);
    assert!(!wasm::supports_session_tool_results(&old_host).expect("old host probe"));

    let new_host = FakeHost::new(vec![Reply::Ok(json!([]))]);
    assert!(wasm::supports_session_tool_results(&new_host).expect("new host probe"));

    let denied_host = FakeHost::new(vec![Reply::Err {
        kind: "capabilityDenied",
        message: "requires session",
    }]);
    assert_eq!(
        wasm::supports_session_tool_results(&denied_host)
            .expect_err("denied")
            .kind,
        InteractiveUiErrorKind::CapabilityDenied
    );
}
