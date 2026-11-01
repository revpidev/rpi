//! Host-call helpers shared by the plugin modules (thin wrappers over the
//! [`crate::HostCall`] JSON surface with fail-soft defaults) plus the
//! `ctx.usage.*` consumption helpers (V16-05 FR-A R5; V18-10 / TE53).

use rpi_ext_host::types::SessionStatsSnapshot;
use rpi_ext_host::usage::{UsageError, UsageErrorKind, UsageFetchResult, UsageReport};
use serde_json::{Value, json};

use crate::HostCall;

/// One command output payload for `ui.emitStructured` (TE53 §7.4).
#[derive(Debug)]
pub enum Output {
    /// Multi-line human report (`usageText`).
    Text(String),
    /// Object or array report (`usageReport`).
    Json(Value),
}

/// `ctx.hasUI` → transport failures degrade to `false`.
pub fn has_ui(host: &dyn HostCall) -> bool {
    host.call("ctx.hasUI", json!({}))
        .ok()
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
}

/// `ctx.model` → the current model JSON (`{id, provider, base_url, ...}`);
/// `None` when unbound or when the host predates the accessor.
pub fn current_model(host: &dyn HostCall) -> Option<Value> {
    host.call("ctx.model", json!({}))
        .ok()
        .filter(|value| !value.is_null())
}

/// The `provider` field of a `ctx.model` object.
pub fn model_provider(model: &Value) -> Option<&str> {
    model.get("provider").and_then(Value::as_str)
}

/// `ctx.usage.listProviders()` → the reachable provider ids (empty on
/// failure).
pub fn usage_list_providers(host: &dyn HostCall) -> Vec<String> {
    host.call("ctx.usage.listProviders", json!({}))
        .ok()
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or_default()
}

/// `ctx.usage.fetch(provider, force?)` → the latest successful envelope JSON
/// (`None` when nothing succeeded yet). Legacy surface; the command/footer
/// paths use [`usage_fetch_detailed`] so errors and staleness are visible.
pub fn usage_fetch(host: &dyn HostCall, provider: &str, force: bool) -> Option<Value> {
    host.call(
        "ctx.usage.fetch",
        json!({ "provider": provider, "force": force }),
    )
    .ok()
    .filter(|value| !value.is_null())
}

/// `ctx.usage.fetchDetailed(provider, force?)` → the typed result. A
/// transport/protocol failure of the host-call itself is classified instead
/// of being mistaken for a successful empty result.
pub fn usage_fetch_detailed(host: &dyn HostCall, provider: &str, force: bool) -> UsageFetchResult {
    match host.call(
        "ctx.usage.fetchDetailed",
        json!({ "provider": provider, "force": force }),
    ) {
        Ok(value) => serde_json::from_value(value).unwrap_or_else(|_| {
            UsageFetchResult::failure(UsageError::new(UsageErrorKind::ShapeChanged))
        }),
        Err(error) => UsageFetchResult::failure(classify_host_error(&error)),
    }
}

/// `ctx.usage.cached(provider)` → the same-identity cache result. Zero
/// scripts and zero network on the host side (TE53 §5.7 offline/xAI paths).
pub fn usage_cached(host: &dyn HostCall, provider: &str) -> UsageFetchResult {
    match host.call("ctx.usage.cached", json!({ "provider": provider })) {
        Ok(value) => serde_json::from_value(value).unwrap_or_else(|_| {
            UsageFetchResult::failure(UsageError::new(UsageErrorKind::ShapeChanged))
        }),
        Err(error) => UsageFetchResult::failure(classify_host_error(&error)),
    }
}

/// `ctx.sessionStats()` (V18-02): the host's absolute current-session
/// snapshot, or `None` on an unbound/legacy host.
pub fn session_stats(host: &dyn HostCall) -> Option<SessionStatsSnapshot> {
    host.call("ctx.sessionStats", json!({}))
        .ok()
        .filter(|value| !value.is_null())
        .and_then(|value| serde_json::from_value(value).ok())
}

/// Host-call failure → the fixed usage error vocabulary. The message never
/// reaches a report; only its fixed `kind`/`action` pair does.
fn classify_host_error(error: &crate::HostError) -> UsageError {
    let kind = if error.kind == "unknownMethod" {
        // A host without the V18-10 method cannot answer the typed contract.
        UsageErrorKind::ShapeChanged
    } else if error.message.contains("offline") {
        UsageErrorKind::Offline
    } else {
        UsageErrorKind::Transport
    };
    UsageError::new(kind)
}

#[cfg(test)]
thread_local! {
    /// Test-local offline override (thread-local so parallel tests in one
    /// binary cannot observe each other's state).
    static TEST_OFFLINE: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// `RPI_OFFLINE` semantics shared with the host (`1` / `true` / `yes`).
///
/// Unit tests use [`set_test_offline`] instead of mutating the process
/// environment, so a parallel footer test cannot observe another test's
/// offline state.
pub fn offline() -> bool {
    #[cfg(test)]
    if let Some(value) = TEST_OFFLINE.with(std::cell::Cell::get) {
        return value;
    }
    match std::env::var("RPI_OFFLINE") {
        Ok(value) => {
            value == "1" || value.eq_ignore_ascii_case("true") || value.eq_ignore_ascii_case("yes")
        }
        Err(_) => false,
    }
}

/// Test seam: force [`offline`] on this test thread without touching the
/// process environment.
#[cfg(test)]
pub fn set_test_offline(value: Option<bool>) {
    TEST_OFFLINE.with(|slot| slot.set(value));
}

/// Validate the structured-output channel before any fetch/script runs
/// (TE53 §5.6 / §7.4). NullUi and subagents answer `unsupported-output`.
pub fn validate_structured(host: &dyn HostCall) -> Result<(), crate::HostError> {
    host.call(
        "ui.emitStructured",
        json!({"method": "usageText", "data": "", "validateOnly": true}),
    )
    .map(|_| ())
}

/// Emit one command report through `ui.emitStructured` (never `ui.notify`
/// directly and never a JSON string inside a text notification).
pub fn emit_output(host: &dyn HostCall, output: &Output) -> Result<(), crate::HostError> {
    let args = match output {
        Output::Text(text) => json!({ "method": "usageText", "data": text }),
        Output::Json(value) => json!({"method": "usageReport", "data": value}),
    };
    host.call("ui.emitStructured", args).map(|_| ())
}

/// Emit a typed report through the same structured channel.
pub fn emit_report(host: &dyn HostCall, report: &UsageReport) -> Result<(), crate::HostError> {
    emit_output(host, &Output::Json(report.to_json()))
}

/// `ctx.usage.register(provider, scriptPath)` → registration receipt.
pub fn usage_register(
    host: &dyn HostCall,
    provider: &str,
    script_path: &str,
) -> Result<(), crate::HostError> {
    host.call(
        "ctx.usage.register",
        json!({ "provider": provider, "scriptPath": script_path }),
    )
    .map(|_| ())
}

/// `ui.setStatus(key, text)`; `None` clears the entry.
pub fn set_status(host: &dyn HostCall, key: &str, text: Option<&str>) {
    let args = match text {
        Some(text) => json!({ "key": key, "text": text }),
        None => json!({ "key": key, "text": null }),
    };
    if let Err(error) = host.call("ui.setStatus", args) {
        tracing::debug!(%error, "rpi-usage: ui.setStatus rejected");
    }
}

/// `ui.notify` (info level); failures are debug-only (no user-visible
/// channel left to report them).
pub fn notify(host: &dyn HostCall, message: &str) {
    if let Err(error) = host.call(
        "ui.notify",
        json!({ "message": message, "notifyType": "info" }),
    ) {
        tracing::debug!(%error, "rpi-usage: ui.notify rejected");
    }
}
