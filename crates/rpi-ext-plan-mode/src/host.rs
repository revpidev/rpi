//! Host-call helpers shared by the plugin modules (thin wrappers over the
//! [`crate::HostCall`] JSON surface with fail-soft defaults).

use serde_json::{Value, json};

use crate::HostCall;

/// `ctx.sessionFile` → the bound session id (`""` on an unbound host and
/// on transport failures — the todo-plugin sentinel precedent).
pub fn sid_of(host: &dyn HostCall) -> String {
    host.call("ctx.sessionFile", json!({}))
        .ok()
        .and_then(|value| value.get("id").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_default()
}

/// `ctx.getMode()` → the host permission mode wire value; `None` on a
/// transport failure. Callers must not degrade an unreadable mode to
/// `default`: that looks like leaving Plan mode and would drop the boundary
/// (v0.1.6 review round 2, O4).
pub fn get_mode(host: &dyn HostCall) -> Option<String> {
    host.call("getMode", json!({}))
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
}

/// `ctx.hasUI` → transport failures degrade to `false`.
pub fn has_ui(host: &dyn HostCall) -> bool {
    host.call("ctx.hasUI", json!({}))
        .ok()
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
}

/// `ctx.mode` → `"tui"` / `"rpc"` / `"json"` / `"print"` (hosts that
/// predate `ctx.mode` answer `None`).
pub fn mode_of(host: &dyn HostCall) -> Option<String> {
    host.call("ctx.mode", json!({}))
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
}

/// `ctx.cwd` → the session working directory.
pub fn cwd_of(host: &dyn HostCall) -> Option<String> {
    host.call("ctx.cwd", json!({}))
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
}

/// `ctx.getAllTools` → the raw `ToolInfo[]` JSON; `None` on failure. An
/// empty `Some([])` is a real empty surface; `None` must not be treated as
/// one — the boundary release would fail open (O4).
pub fn get_all_tools(host: &dyn HostCall) -> Option<Vec<Value>> {
    host.call("getAllTools", json!({}))
        .ok()
        .and_then(|value| value.as_array().cloned())
}

/// `ctx.getActiveTools` → the active tool names; `None` on failure (O4).
pub fn get_active_tools(host: &dyn HostCall) -> Option<Vec<String>> {
    host.call("getActiveTools", json!({}))
        .ok()
        .and_then(|value| serde_json::from_value(value).ok())
}
