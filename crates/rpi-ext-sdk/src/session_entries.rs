//! Session-entries read (ADR-0027) — guest-side helper for the additive
//! `ctx.sessionEntries` host-call.
//!
//! Returns the **active branch** (root→leaf) `type:"custom"` entries of the
//! current session — the cross-JSON-ABI equivalent of upstream
//! `ctx.sessionManager.getBranch()` filtered to custom entries
//! (`index.ts:217-229` @ 928c30c). Read-only: message/tool_result
//! conversation entries are never returned. Unbound hosts / sessions
//! without an active branch answer `[]`; hosts that predate the method
//! answer `unknownMethod` — probe with [`supports_session_entries`] (or
//! branch on the error kind) and fall back.
//!
//! rpi-docs: `adr/0027-session-entries-abi.md`, `extension-abi.md` §3
//! (method table) / §8.4 (landed with V14-25).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::interactive_ui::{HostCall, InteractiveUiError};

/// `ctx.sessionEntries` — the additive host-call of ADR-0027.
pub const METHOD_SESSION_ENTRIES: &str = "ctx.sessionEntries";

/// One custom entry of the active branch (ADR-0027 frozen shape):
/// `{id, parentId, timestamp, customType, data}`. `data` is the entry
/// payload verbatim (`null` when the entry carries none).
///
/// The native mirror is `rpi_ext_host::types::SessionEntryInfo`; the two
/// serialize byte-identically (V14-20 parity precedent, asserted by
/// `rpi-ext-host/tests/session_entries_parity.rs`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionEntry {
    pub id: String,
    pub parent_id: Option<String>,
    pub timestamp: String,
    pub custom_type: String,
    pub data: Value,
}

/// Read the active branch's custom entries (ADR-0027).
///
/// - `custom_type`: exact-match filter; `None` = all custom entries.
/// - `limit`: keep the filtered **tail** N entries (path order kept);
///   `None` = unlimited. The host caps explicit limits at
///   `SESSION_ENTRIES_MAX_LIMIT` (10_000); invalid values are treated as
///   absent by the dispatch layer.
///
/// Errors: `unknownMethod` on hosts without the method (pre-V14-25);
/// `capabilityDenied` without capability `session`.
pub fn session_entries<H: HostCall + ?Sized>(
    host: &H,
    custom_type: Option<&str>,
    limit: Option<u64>,
) -> Result<Vec<SessionEntry>, InteractiveUiError> {
    // The structured host-call error envelope is shared across the typed
    // boundary (V14-20 introduced it; the kind table in
    // `extension-abi.md` §4 is method-agnostic), so the interactive-UI
    // error type doubles as the transport error here.
    let mut args = serde_json::Map::new();
    if let Some(custom_type) = custom_type {
        args.insert(
            "customType".to_owned(),
            Value::String(custom_type.to_owned()),
        );
    }
    if let Some(limit) = limit {
        args.insert("limit".to_owned(), Value::from(limit));
    }
    let response = host.call(METHOD_SESSION_ENTRIES, Value::Object(args))?;
    serde_json::from_value(response)
        .map_err(|error| InteractiveUiError::protocol(format!("sessionEntries reply: {error}")))
}

/// Probe whether the host implements `ctx.sessionEntries` (ADR-0027
/// consumer migration, TE33): `Ok(true)` = readable; `Ok(false)` = old
/// host answering `unknownMethod` (fall back to the JSONL read);
/// `Err` = transport failure.
pub fn supports_session_entries<H: HostCall + ?Sized>(
    host: &H,
) -> Result<bool, InteractiveUiError> {
    match host.call(METHOD_SESSION_ENTRIES, Value::Object(Default::default())) {
        // The method is read-only with no side effects, so a successful
        // call (including `[]`) already proves support.
        Ok(_) => Ok(true),
        Err(error) => match error.kind {
            crate::interactive_ui::InteractiveUiErrorKind::UnknownMethod => Ok(false),
            _ => Err(error),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interactive_ui::{HostCall, InteractiveUiError, InteractiveUiErrorKind};
    use serde_json::json;

    /// Scripted transport: canned replies, recorded calls.
    struct FakeHost {
        replies: std::cell::RefCell<std::collections::VecDeque<Result<Value, InteractiveUiError>>>,
        calls: std::cell::RefCell<Vec<(String, Value)>>,
    }

    impl FakeHost {
        fn new(replies: Vec<Result<Value, InteractiveUiError>>) -> Self {
            Self {
                replies: std::cell::RefCell::new(replies.into()),
                calls: std::cell::RefCell::new(Vec::new()),
            }
        }
    }

    impl HostCall for FakeHost {
        fn call(&self, method: &str, args: Value) -> Result<Value, InteractiveUiError> {
            self.calls
                .borrow_mut()
                .push((method.to_owned(), args.clone()));
            self.replies
                .borrow_mut()
                .pop_front()
                .unwrap_or_else(|| Err(InteractiveUiError::protocol("no scripted reply")))
        }
    }

    fn entry(id: &str, parent: Option<&str>, custom_type: &str, data: Value) -> SessionEntry {
        let timestamp = match id {
            "e2" => "2026-09-09T00:00:01.000Z",
            _ => "2026-09-09T00:00:00.000Z",
        };
        SessionEntry {
            id: id.to_owned(),
            parent_id: parent.map(str::to_owned),
            timestamp: timestamp.to_owned(),
            custom_type: custom_type.to_owned(),
            data,
        }
    }

    #[test]
    fn wraps_the_frozen_request_shape() {
        let host = FakeHost::new(vec![Ok(json!([]))]);
        session_entries(&host, Some("mcp-approval-v1"), Some(100)).expect("sessionEntries call");
        assert_eq!(
            host.calls.borrow().as_slice(),
            &[(
                METHOD_SESSION_ENTRIES.to_owned(),
                json!({"customType": "mcp-approval-v1", "limit": 100})
            )]
        );

        // Both arguments optional: empty object is the no-filter request.
        let host = FakeHost::new(vec![Ok(json!([]))]);
        session_entries(&host, None, None).expect("sessionEntries call");
        assert_eq!(
            host.calls.borrow().as_slice(),
            &[(METHOD_SESSION_ENTRIES.to_owned(), json!({}))]
        );
    }

    #[test]
    fn parses_the_entry_array() {
        let host = FakeHost::new(vec![Ok(json!([
            {
                "id": "e1",
                "parentId": null,
                "timestamp": "2026-09-09T00:00:00.000Z",
                "customType": "mcp-approval-v1",
                "data": {"version": 1}
            },
            {
                "id": "e2",
                "parentId": "e1",
                "timestamp": "2026-09-09T00:00:01.000Z",
                "customType": "other",
                "data": null
            }
        ]))]);
        let entries = session_entries(&host, None, None).expect("entries");
        assert_eq!(
            entries,
            vec![
                entry("e1", None, "mcp-approval-v1", json!({"version": 1})),
                entry("e2", Some("e1"), "other", Value::Null),
            ]
        );
    }

    #[test]
    fn surfaces_unknown_method_for_old_hosts() {
        // A6: pre-V14-25 hosts answer unknownMethod — consumers probe and
        // fall back (TE33: JSONL read via ctx.sessionFile.path).
        let host = FakeHost::new(vec![
            Err(InteractiveUiError::new(
                InteractiveUiErrorKind::UnknownMethod,
                "unknown host call: ctx.sessionEntries",
            )),
            Err(InteractiveUiError::new(
                InteractiveUiErrorKind::UnknownMethod,
                "unknown host call: ctx.sessionEntries",
            )),
        ]);
        let error = session_entries(&host, None, None).expect_err("old host");
        assert_eq!(error.kind, InteractiveUiErrorKind::UnknownMethod);
        assert!(error.is_unknown_method());
        assert!(!supports_session_entries(&host).expect("probe"));

        // New host: `[]` (or any successful call) proves support.
        let host = FakeHost::new(vec![Ok(json!([]))]);
        assert!(supports_session_entries(&host).expect("probe"));

        // Transport failures propagate (not a support question).
        let host = FakeHost::new(vec![Err(InteractiveUiError::new(
            InteractiveUiErrorKind::CapabilityDenied,
            "requires session",
        ))]);
        assert_eq!(
            supports_session_entries(&host).expect_err("denied").kind,
            InteractiveUiErrorKind::CapabilityDenied
        );
    }
}
