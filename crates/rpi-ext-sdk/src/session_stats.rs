//! Session statistics snapshot (ADR-0040 / V18-02) — guest-side helper for
//! the additive `ctx.sessionStats` host-call.
//!
//! The host returns one **absolute** snapshot taken under a single
//! `SessionManager` lock: session identity, the header timestamp, the entry
//! count, the four token components and cost. Consumers must not rebuild
//! session totals from the transcript or from `message_end` snapshots; they
//! only derive their own display values (for example `used = input +
//! cacheWrite + output`) from this object.
//!
//! Unbound hosts answer `null`; hosts that predate the method answer
//! `unknownMethod` — probe with [`supports_session_stats`] and fall back.
//!
//! rpi-docs: `adr/0040-statusline-items-settings-rendering.md`,
//! `extension-abi.md` §3 (method table) / §8.4.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::interactive_ui::{HostCall, InteractiveUiError};

/// `ctx.sessionStats` — the additive host-call of ADR-0040.
pub const METHOD_SESSION_STATS: &str = "ctx.sessionStats";

/// One absolute session snapshot (ADR-0040 frozen shape; the
/// `user_messages`/`assistant_messages` counts are the 2026-11 additive
/// extension).
///
/// The native mirror is `rpi_ext_host::types::SessionStatsSnapshot`; the two
/// serialize byte-identically (same camelCase field names, `sessionFile`
/// omitted for in-memory sessions).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStats {
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_file: Option<String>,
    /// Session header timestamp (RFC3339); not the process start.
    pub started_at: String,
    /// `get_entries().len()` at snapshot time, header excluded and not
    /// branch-filtered.
    pub entry_count: u64,
    /// Persisted user messages (conversation turns).
    #[serde(default)]
    pub user_messages: u64,
    /// Persisted assistant messages (model outputs/steps).
    #[serde(default)]
    pub assistant_messages: u64,
    pub tokens: SessionTokenStats,
    pub cost: f64,
}

/// The counts-only shape of `ctx.sessionStats({countsOnly:true})` (ADR-0049
/// decision 8 / V19-07): identity plus the three counts, answered in O(1)
/// by the host (no entry clone/iteration, no usage aggregation). Use it as a
/// cheap watermark probe between full snapshots. The native mirror is
/// `rpi_ext_host::types::SessionCountsSnapshot`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCounts {
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_file: Option<String>,
    /// Session header timestamp (RFC3339).
    pub started_at: String,
    /// `get_entries().len()` at probe time, header excluded.
    pub entry_count: u64,
    #[serde(default)]
    pub user_messages: u64,
    #[serde(default)]
    pub assistant_messages: u64,
}

/// One or the other shape of `ctx.sessionStats`: a host that predates the
/// counts-only argument (or does not implement the probe) answers the
/// `{countsOnly:true}` request with its FULL snapshot — callers must accept
/// both and read `assistant_messages` from either.
#[derive(Debug, Clone, PartialEq)]
pub enum SessionStatsReply {
    Counts(SessionCounts),
    Full(SessionStats),
}

impl SessionStatsReply {
    /// The persisted assistant message count — the watermark consumers of
    /// the probe are after — from either shape.
    pub fn assistant_messages(&self) -> u64 {
        match self {
            Self::Counts(counts) => counts.assistant_messages,
            Self::Full(stats) => stats.assistant_messages,
        }
    }

    pub fn entry_count(&self) -> u64 {
        match self {
            Self::Counts(counts) => counts.entry_count,
            Self::Full(stats) => stats.entry_count,
        }
    }
}

/// The four token components (camelCase on the wire: `cacheRead`,
/// `cacheWrite`). The raw provider `totalTokens` is deliberately absent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionTokenStats {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

/// Read the current session snapshot.
///
/// Returns `Ok(None)` for an unbound host (`null` on the wire) and an
/// `unknownMethod` error on hosts without the method.
pub fn session_stats<H: HostCall + ?Sized>(
    host: &H,
) -> Result<Option<SessionStats>, InteractiveUiError> {
    let response = host.call(METHOD_SESSION_STATS, Value::Object(Default::default()))?;
    serde_json::from_value(response)
        .map_err(|error| InteractiveUiError::protocol(format!("sessionStats reply: {error}")))
}

/// Read the counts-only probe `ctx.sessionStats({countsOnly:true})`.
///
/// The argument is additive (`rpiAbi` unchanged): a host without the O(1)
/// probe answers the full snapshot, which is reported as
/// [`SessionStatsReply::Full`] so the caller never has to special-case it.
/// `Ok(None)` for an unbound host (`null` on the wire); `unknownMethod` on
/// hosts without the method at all.
pub fn session_counts<H: HostCall + ?Sized>(
    host: &H,
) -> Result<Option<SessionStatsReply>, InteractiveUiError> {
    let response = host.call(
        METHOD_SESSION_STATS,
        serde_json::json!({"countsOnly": true}),
    )?;
    if response.is_null() {
        return Ok(None);
    }
    // The two shapes are distinguishable by `tokens` (only the full snapshot
    // carries it); deserialize through `serde_json::Value` so a malformed
    // reply surfaces as a protocol error either way.
    let is_counts = response.get("tokens").is_none();
    let reply = if is_counts {
        serde_json::from_value(response)
            .map(SessionStatsReply::Counts)
            .map_err(|error| {
                InteractiveUiError::protocol(format!("sessionStats counts reply: {error}"))
            })?
    } else {
        serde_json::from_value(response)
            .map(SessionStatsReply::Full)
            .map_err(|error| InteractiveUiError::protocol(format!("sessionStats reply: {error}")))?
    };
    Ok(Some(reply))
}

/// Probe whether the host implements `ctx.sessionStats`.
///
/// `Ok(true)` = method present (a `null` payload still proves support for
/// an unbound host); `Ok(false)` = old host answering `unknownMethod`;
/// `Err` = transport/capability failure.
pub fn supports_session_stats<H: HostCall + ?Sized>(host: &H) -> Result<bool, InteractiveUiError> {
    match host.call(METHOD_SESSION_STATS, Value::Object(Default::default())) {
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
    use crate::interactive_ui::{InteractiveUiError, InteractiveUiErrorKind};
    use serde_json::json;
    use std::cell::RefCell;

    struct RecordingHost {
        calls: RefCell<Vec<(String, Value)>>,
        reply: Result<Value, InteractiveUiError>,
    }

    impl HostCall for RecordingHost {
        fn call(&self, method: &str, args: Value) -> Result<Value, InteractiveUiError> {
            self.calls
                .borrow_mut()
                .push((method.to_owned(), args.clone()));
            self.reply.clone()
        }
    }

    fn host(reply: Result<Value, InteractiveUiError>) -> RecordingHost {
        RecordingHost {
            calls: RefCell::new(Vec::new()),
            reply,
        }
    }

    #[test]
    fn parses_the_snapshot_and_sends_empty_args() {
        let host = host(Ok(json!({
            "sessionId": "s1",
            "sessionFile": "/tmp/s1.jsonl",
            "startedAt": "2026-10-27T10:00:00Z",
            "entryCount": 12,
            "userMessages": 4,
            "assistantMessages": 7,
            "tokens": {
                "input": 10,
                "output": 20,
                "cacheRead": 30,
                "cacheWrite": 40
            },
            "cost": 1.25
        })));
        let stats = session_stats(&host).expect("snapshot").expect("bound");
        assert_eq!(stats.session_id, "s1");
        assert_eq!(stats.session_file.as_deref(), Some("/tmp/s1.jsonl"));
        assert_eq!(stats.started_at, "2026-10-27T10:00:00Z");
        assert_eq!(stats.entry_count, 12);
        assert_eq!(stats.user_messages, 4);
        assert_eq!(stats.assistant_messages, 7);
        assert_eq!(stats.tokens.input, 10);
        assert_eq!(stats.tokens.output, 20);
        assert_eq!(stats.tokens.cache_read, 30);
        assert_eq!(stats.tokens.cache_write, 40);
        assert_eq!(stats.cost, 1.25);
        let calls = host.calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, METHOD_SESSION_STATS);
        assert_eq!(calls[0].1, json!({}));
    }

    #[test]
    fn in_memory_session_omits_the_file() {
        let host = host(Ok(json!({
            "sessionId": "s2",
            "startedAt": "2026-10-27T10:00:00Z",
            "entryCount": 0,
            "userMessages": 0,
            "assistantMessages": 0,
            "tokens": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
            "cost": 0.0
        })));
        let stats = session_stats(&host).expect("snapshot").expect("bound");
        assert!(stats.session_file.is_none());
    }

    #[test]
    fn counts_are_additive_for_older_hosts() {
        let host = host(Ok(json!({
            "sessionId": "s3",
            "startedAt": "2026-10-27T10:00:00Z",
            "entryCount": 5,
            "tokens": {"input": 1, "output": 2, "cacheRead": 3, "cacheWrite": 4},
            "cost": 0.0
        })));
        let stats = session_stats(&host).expect("snapshot").expect("bound");
        assert_eq!(stats.user_messages, 0);
        assert_eq!(stats.assistant_messages, 0);
    }

    #[test]
    fn null_means_unbound_and_still_proves_support() {
        let host = host(Ok(Value::Null));
        assert!(session_stats(&host).expect("null").is_none());
        assert!(supports_session_stats(&host).expect("probe"));
    }

    #[test]
    fn unknown_method_is_the_old_host_signal() {
        let host = host(Err(InteractiveUiError::new(
            InteractiveUiErrorKind::UnknownMethod,
            "unknown host call: ctx.sessionStats",
        )));
        assert!(!supports_session_stats(&host).expect("probe"));
        let error = session_stats(&host).expect_err("old host");
        assert_eq!(error.kind, InteractiveUiErrorKind::UnknownMethod);
    }

    /// V19-07: the counts-only request sends the additive argument and
    /// parses the counts shape; an old host's full snapshot is reported as
    /// the `Full` variant with the same watermark accessor.
    #[test]
    fn counts_only_request_parses_both_shapes() {
        let counts_host = host(Ok(json!({
            "sessionId": "s1",
            "startedAt": "2026-10-27T10:00:00Z",
            "entryCount": 9,
            "userMessages": 4,
            "assistantMessages": 5
        })));
        let reply = session_counts(&counts_host).expect("probe").expect("bound");
        assert_eq!(
            reply,
            SessionStatsReply::Counts(SessionCounts {
                session_id: "s1".into(),
                session_file: None,
                started_at: "2026-10-27T10:00:00Z".into(),
                entry_count: 9,
                user_messages: 4,
                assistant_messages: 5,
            })
        );
        assert_eq!(reply.assistant_messages(), 5);
        assert_eq!(reply.entry_count(), 9);
        assert_eq!(
            counts_host.calls.borrow()[0],
            (METHOD_SESSION_STATS.to_owned(), json!({"countsOnly": true}))
        );
        // Old host: the full snapshot comes back for the same request.
        let old_host = host(Ok(json!({
            "sessionId": "s1",
            "startedAt": "2026-10-27T10:00:00Z",
            "entryCount": 9,
            "userMessages": 4,
            "assistantMessages": 5,
            "tokens": {"input": 1, "output": 2, "cacheRead": 3, "cacheWrite": 4},
            "cost": 0.5
        })));
        let reply = session_counts(&old_host).expect("probe").expect("bound");
        assert!(matches!(reply, SessionStatsReply::Full(_)));
        assert_eq!(reply.assistant_messages(), 5);
        assert_eq!(
            old_host.calls.borrow()[0],
            (METHOD_SESSION_STATS.to_owned(), json!({"countsOnly": true}))
        );
        // Unbound and old-host signals match the full read.
        assert!(
            session_counts(&host(Ok(Value::Null)))
                .expect("null")
                .is_none()
        );
        let error = session_counts(&host(Err(InteractiveUiError::new(
            InteractiveUiErrorKind::UnknownMethod,
            "unknown host call: ctx.sessionStats",
        ))))
        .expect_err("old host");
        assert_eq!(error.kind, InteractiveUiErrorKind::UnknownMethod);
    }

    #[test]
    fn capability_denied_is_a_real_error() {
        let host = host(Err(InteractiveUiError::new(
            InteractiveUiErrorKind::CapabilityDenied,
            "session",
        )));
        let error = supports_session_stats(&host).expect_err("denied");
        assert_eq!(error.kind, InteractiveUiErrorKind::CapabilityDenied);
    }
}
