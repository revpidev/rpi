//! rpi-statusline (TE12): Claude Code-compatible script statusline, L0
//! native plugin.
//!
//! Behavior: the user configures a command under the global settings.json
//! `statusLine` key; this plugin feeds the session facts to that command
//! on stdin (the CC statusline JSON contract) and renders its stdout as
//! the footer — replacing it entirely (`placement: "replace"`, default)
//! or as a status entry below it (`placement: "status"`). See
//! `rpi-docs/plan/extensions/TE12-statusline.md` and
//! `rpi-docs/extensions/rpi-statusline/` for the full spec.
//!
//! Runtime model (mcp-adapter / subagents precedents): the host dispatch
//! thread only does in-memory state updates plus a channel send (µs
//! scale — the TE-D6 dispatch-blocking discipline); the refresh loop,
//! host calls and script child processes live on the plugin's private
//! tokio runtime. Zero ABI additions: every host call used here
//! (`on`, `ctx.*`, `ui.setFooter`, `ui.setStatus`) already exists.

pub mod config;
pub mod paths;
pub mod payload;
pub mod refresh;
pub mod render;
pub mod runner;
pub mod runtime;
pub mod state;

use std::sync::{Mutex, OnceLock};

use abi_stable::prefix_type::PrefixTypeTrait;
use abi_stable::std_types::RVec;
use rpi_ext_host::native::{PluginCookie, RpiHostCalls, RpiNativeModule, RpiNativeModule_Ref};
use serde_json::{json, Value};

use crate::refresh::Trigger;
use crate::runtime::PluginRuntime;
use crate::state::EngineState;

/// Events subscribed at install (TE12 事件映射表; everything else is
/// deliberately not subscribed — `message_update` & friends are high
/// frequency with no new payload).
const SUBSCRIBED_EVENTS: &[&str] = &[
    "message_end",
    "session_start",
    "session_compact",
    "session_info_changed",
    "model_select",
    "thinking_level_select",
    "tool_execution_end",
    "session_shutdown",
];

/// Host-call handle + plugin-owned runtime, established once by
/// `rpi_extension_init`. The cookie is stored as `usize` to keep the state
/// `Send + Sync` (mcp-adapter precedent). The fields are ownership keeps:
/// dropping the runtime would park the refresh loop.
#[allow(dead_code)]
struct PluginState {
    calls: RpiHostCalls,
    cookie: usize,
    runtime: PluginRuntime,
}

static STATE: OnceLock<PluginState> = OnceLock::new();

/// Shared engine state: dispatch thread writes (usage accumulation,
/// session lifecycle), refresh loop reads (snapshot) / writes (mounted
/// channel).
pub(crate) static ENGINE: OnceLock<Mutex<EngineState>> = OnceLock::new();

/// Trigger channel sender stashed at install so the sync dispatch path can
/// wake the async refresh loop.
static TRIGGERS: OnceLock<tokio::sync::mpsc::UnboundedSender<Trigger>> = OnceLock::new();

/// Clonable host-call channel for background tasks (subagents lib.rs:70-95
/// precedent, verbatim): the refresh loop outlives the install dispatch
/// that started it and needs `ui.*` / `ctx.*` from the plugin runtime.
#[derive(Clone, Copy)]
pub struct AsyncHostCalls {
    pub call: extern "C" fn(PluginCookie, RVec<u8>) -> RVec<u8>,
    pub cookie: usize,
}

/// Fire one host call from a background context; returns the raw envelope.
pub fn host_call_static(channel: &AsyncHostCalls, method: &str, args: Value) -> Value {
    let request = serde_json::to_vec(&json!({
        "call": method,
        "args": args,
        "seq": 0,
    }))
    .unwrap_or_default();
    let response = (channel.call)(channel.cookie as PluginCookie, RVec::from(request));
    serde_json::from_slice(&response[..]).unwrap_or(Value::Null)
}

/// Host call with `{"ok": ...}` unwrapping; `None` on an error envelope
/// (logged, never fatal — FR-F discipline).
pub(crate) fn host_ok(channel: &AsyncHostCalls, method: &str, args: Value) -> Option<Value> {
    let response = host_call_static(channel, method, args);
    if let Some(error) = response.get("error") {
        tracing::warn!(method, ?error, "rpi-statusline host call failed");
        return None;
    }
    Some(response.get("ok").cloned().unwrap_or(Value::Null))
}

/// Install: start the runtime, subscribe to the session events, spawn the
/// refresh loop. Idempotent (a second init reports success without
/// re-arming).
fn install(calls: RpiHostCalls, cookie: PluginCookie) -> Value {
    if STATE.get().is_some() {
        return json!({"ok": true});
    }
    let runtime = match PluginRuntime::start() {
        Ok(runtime) => runtime,
        Err(error) => {
            return json!({"error": {
                "kind": "internal",
                "message": format!("statusline runtime start failed: {error}"),
            }})
        }
    };
    let channel = AsyncHostCalls {
        call: calls.call,
        cookie: cookie as usize,
    };
    for event in SUBSCRIBED_EVENTS {
        let response = host_call_static(&channel, "on", json!({"event": event}));
        if response.get("error").is_some() {
            return response;
        }
    }
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let _ = TRIGGERS.set(tx);
    let _ = ENGINE.set(Mutex::new(EngineState::default()));
    runtime.spawn(refresh::refresh_loop(channel, rx));
    let _ = STATE.set(PluginState {
        calls,
        cookie: cookie as usize,
        runtime,
    });
    json!({"ok": true})
}

/// Event dispatch: light state updates + a channel send, nothing more.
fn handle_dispatch(message: &Value) -> Value {
    if message.get("kind").and_then(Value::as_str) != Some("event") {
        return Value::Null;
    }
    let event = message.get("event").and_then(Value::as_str).unwrap_or("");
    let payload = message.get("payload").cloned().unwrap_or(Value::Null);
    let trigger = match event {
        "message_end" => {
            let message = payload.get("message").cloned().unwrap_or(Value::Null);
            with_engine(|engine| engine.accumulate_usage(&message));
            Trigger::Event("message_end")
        }
        "session_start" => {
            let reason = payload.get("reason").and_then(Value::as_str);
            with_engine(|engine| engine.on_session_start(reason));
            Trigger::Event("session_start")
        }
        "session_shutdown" => Trigger::Shutdown,
        "session_compact" => Trigger::Event("session_compact"),
        "session_info_changed" => Trigger::Event("session_info_changed"),
        "model_select" => Trigger::Event("model_select"),
        "thinking_level_select" => Trigger::Event("thinking_level_select"),
        "tool_execution_end" => Trigger::Event("tool_execution_end"),
        _ => return Value::Null,
    };
    if let Some(tx) = TRIGGERS.get() {
        let _ = tx.send(trigger);
    }
    Value::Null
}

fn with_engine<T>(update: impl FnOnce(&mut EngineState) -> T) -> T {
    let engine = ENGINE
        .get()
        .expect("engine state installed before dispatch");
    let mut engine = engine.lock().unwrap_or_else(|error| error.into_inner());
    update(&mut engine)
}

fn pack(value: Value) -> RVec<u8> {
    RVec::from(serde_json::to_vec(&value).unwrap_or_else(|_| b"null".to_vec()))
}

#[allow(clippy::missing_safety_doc)]
pub extern "C" fn init(calls: RpiHostCalls, cookie: PluginCookie) -> RVec<u8> {
    // Panic guard (subagents lib.rs:1125-1143 precedent): a plugin panic
    // must cross the ABI as an error envelope, not unwind into the host.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        install(calls, cookie)
    }))
    .unwrap_or_else(|panic| {
        json!({"error": {"kind": "internal", "message": format!("statusline init panicked: {panic:?}")}})
    });
    pack(result)
}

#[allow(clippy::missing_safety_doc)]
pub extern "C" fn dispatch(cookie: PluginCookie, message: RVec<u8>) -> RVec<u8> {
    let _ = cookie;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let message: Value = serde_json::from_slice(&message[..]).unwrap_or(Value::Null);
        handle_dispatch(&message)
    }))
    .unwrap_or_else(|panic| {
        json!({"error": {"kind": "internal", "message": format!("statusline dispatch panicked: {panic:?}")}})
    });
    pack(result)
}

/// The root module export (abi_stable).
#[abi_stable::export_root_module]
pub fn module() -> RpiNativeModule_Ref {
    RpiNativeModule {
        rpi_extension_init: init,
        rpi_dispatch: dispatch,
    }
    .leak_into_prefix()
}

/// Test seam (mcp-adapter `install_for_test` precedent): install directly
/// from Rust, bypassing the cdylib/ABI boundary.
pub fn install_for_test(calls: RpiHostCalls, cookie: PluginCookie) -> Value {
    install(calls, cookie)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscribed_events_are_unique_and_sorted_for_documentation() {
        let mut events = SUBSCRIBED_EVENTS.to_vec();
        events.sort_unstable();
        let count = events.len();
        events.dedup();
        assert_eq!(events.len(), count, "no duplicate subscriptions");
        assert_eq!(count, 8, "the TE12 event table");
    }
}
