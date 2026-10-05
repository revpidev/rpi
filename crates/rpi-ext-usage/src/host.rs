//! Host-call helpers shared by the plugin modules (thin wrappers over the
//! [`crate::HostCall`] JSON surface with fail-soft defaults) plus the
//! `ctx.usage.*` consumption helpers (V16-05 FR-A R5).

use serde_json::{Value, json};

use crate::HostCall;

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
/// (`None` when nothing succeeded yet).
pub fn usage_fetch(host: &dyn HostCall, provider: &str, force: bool) -> Option<Value> {
    host.call(
        "ctx.usage.fetch",
        json!({ "provider": provider, "force": force }),
    )
    .ok()
    .filter(|value| !value.is_null())
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
