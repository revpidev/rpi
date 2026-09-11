//! V14-25 (ADR-0027): the `ctx.sessionEntries` wire shape must be identical
//! on both carriers. The native dispatch (`rpi-ext-host`) and the wasm SDK
//! (`rpi-ext-sdk`) each own a typed view of the frozen entry shape — this
//! suite asserts byte-level JSON parity over a shared sample corpus (the
//! V14-20 `interactive_ui_parity.rs` precedent, scoped to the single
//! host-call added by ADR-0027), plus identical request shapes and probe
//! outcomes against one transport script.

use std::collections::VecDeque;
use std::sync::Mutex;

use rpi_ext_host::types::SessionEntryInfo as NativeEntry;
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

fn corpus() -> Vec<NativeEntry> {
    vec![
        // Root entry without parent, structured data.
        NativeEntry {
            id: "e1".to_owned(),
            parent_id: None,
            timestamp: "2026-09-09T00:00:00.000Z".to_owned(),
            custom_type: "mcp-approval-v1".to_owned(),
            data: json!({
                "version": 1,
                "kind": "tool",
                "decision": "allow_for_session",
                "serverName": "tavily",
                "originalToolName": "tavily_search",
                "definitionHash": "6f9aa8c2",
                "argsHash": "0d41c6e2",
            }),
        },
        // Mid-branch entry, data absent (null).
        NativeEntry {
            id: "e2".to_owned(),
            parent_id: Some("e1".to_owned()),
            timestamp: "2026-09-09T00:00:01.000Z".to_owned(),
            custom_type: "other-type".to_owned(),
            data: Value::Null,
        },
        // Arbitrary nested data payload (原样透传).
        NativeEntry {
            id: "e3".to_owned(),
            parent_id: Some("e2".to_owned()),
            timestamp: "2026-09-09T00:00:02.000Z".to_owned(),
            custom_type: "rich".to_owned(),
            data: json!({"nested": {"list": [1, 2, 3], "flag": true}, "s": "文/字"}),
        },
    ]
}

/// Byte-level parity: native serialization round-trips through the wasm
/// type and back, and the two serializations are identical strings.
#[test]
fn entry_shape_parity_native_wasm() {
    for native in corpus() {
        let native_json = serde_json::to_string(&native).expect("native serialize");

        let wasm_entry: wasm::SessionEntry =
            serde_json::from_str(&native_json).expect("wasm parses native JSON");
        let wasm_json = serde_json::to_string(&wasm_entry).expect("wasm serialize");
        assert_eq!(native_json, wasm_json, "byte-identical for {}", native.id);

        let round_trip: NativeEntry =
            serde_json::from_str(&wasm_json).expect("native parses wasm JSON");
        assert_eq!(round_trip, native);
    }
}

/// The frozen field set/order on the wire: `id, parentId, timestamp,
/// customType, data` (camelCase, `data` always present).
#[test]
fn entry_wire_fields_are_frozen() {
    let native = &corpus()[1];
    let json = serde_json::to_value(native).expect("serialize");
    let object = json.as_object().expect("object");
    assert_eq!(
        object.keys().collect::<Vec<_>>(),
        vec!["id", "parentId", "timestamp", "customType", "data"],
        "frozen key set"
    );
    assert_eq!(json["parentId"], json!("e1"));
    assert_eq!(json["data"], Value::Null, "absent data → null");
}

/// Request-shape parity: the SDK wrapper emits exactly the frozen request
/// (`{customType?, limit?}`, both optional) for `ctx.sessionEntries`.
#[test]
fn request_shape_and_probe_parity() {
    let host = FakeHost::new(vec![
        Reply::Ok(json!([])),
        Reply::Ok(json!([])),
        Reply::Ok(json!([])),
    ]);
    wasm::session_entries(&host, Some("mcp-approval-v1"), Some(100)).expect("call 1");
    wasm::session_entries(&host, None, None).expect("call 2");
    wasm::supports_session_entries(&host).expect("probe");
    assert_eq!(
        host.calls(),
        vec![
            (
                "ctx.sessionEntries".to_owned(),
                json!({"customType": "mcp-approval-v1", "limit": 100})
            ),
            ("ctx.sessionEntries".to_owned(), json!({})),
            ("ctx.sessionEntries".to_owned(), json!({})),
        ]
    );
}

/// Probe outcome parity: `unknownMethod` (pre-V14-25 host) → `Ok(false)`;
/// any successful answer (including `[]`) → `Ok(true)`; other kinds
/// propagate.
#[test]
fn probe_outcomes() {
    let old_host = FakeHost::new(vec![Reply::Err {
        kind: "unknownMethod",
        message: "unknown host call: ctx.sessionEntries",
    }]);
    assert!(!wasm::supports_session_entries(&old_host).expect("old host probe"));

    let new_host = FakeHost::new(vec![Reply::Ok(json!([]))]);
    assert!(wasm::supports_session_entries(&new_host).expect("new host probe"));

    let denied_host = FakeHost::new(vec![Reply::Err {
        kind: "capabilityDenied",
        message: "requires session",
    }]);
    assert_eq!(
        wasm::supports_session_entries(&denied_host)
            .expect_err("denied")
            .kind,
        InteractiveUiErrorKind::CapabilityDenied
    );
}
