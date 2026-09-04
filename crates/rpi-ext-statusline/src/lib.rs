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
//! tokio runtime. Host-call surface: `on`, `ctx.*` (including the
//! additive `ctx.sessionFile`, ADR-0022), `ui.setFooter`, `ui.setStatus`,
//! `ui.setWidget`.

pub mod config;
pub mod paths;
pub mod payload;
pub mod refresh;
pub mod render;
pub mod runner;
pub mod runtime;
pub mod state;

use std::sync::{Arc, Mutex, OnceLock, RwLock};

use abi_stable::prefix_type::PrefixTypeTrait;
use abi_stable::std_types::RVec;
use rpi_ext_host::native::{PluginCookie, RpiHostCalls, RpiNativeModule, RpiNativeModule_Ref};
use serde_json::{json, Value};

use crate::refresh::Trigger;
use crate::runtime::PluginRuntime;
use crate::state::EngineState;

/// Events subscribed at install (TE12 事件映射表; everything else is
/// deliberately not subscribed — other high-frequency events carry no
/// data this plugin consumes).
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

/// Live-token subscriptions (03-realtime-token-count §1.4/§1.5): only
/// armed at install when `statusLine.liveTokens` is configured — the
/// subscription set is the zero-cost off switch (unsubscribed users pay
/// no per-delta host payload serialization). Both dispatch as
/// bookkeeping-only `Trigger::Live` (never the 300ms-debounce path).
const LIVE_EVENTS: &[&str] = &["message_start", "message_update"];

/// Host-call handle + plugin-owned runtime, established once by
/// `rpi_extension_init`. The cookie is stored as `usize` to keep the state
/// `Send + Sync` (mcp-adapter precedent). The fields are ownership keeps:
/// dropping the runtime would park the refresh loop.
#[allow(dead_code)]
struct PluginState {
    runtime: PluginRuntime,
}

static STATE: OnceLock<PluginState> = OnceLock::new();

/// The refresh loop's host channel, REBINDABLE across hosts (mcp-adapter /
/// subagents session-switch discipline): a session replacement (`/resume`
/// `/new` `/fork` `/clone` `/import`) re-loads this same dlopen-memoized
/// cdylib on a fresh `NativeExtensionHost`, and the replaced host is
/// dropped once the outgoing session goes away — the loop must push
/// through the newest channel or every footer write after `/resume`
/// dangles on the freed cookie.
static CHANNEL: OnceLock<Arc<RwLock<AsyncHostCalls>>> = OnceLock::new();

/// Shared engine state: dispatch thread writes (usage accumulation,
/// session lifecycle), refresh loop reads (snapshot) / writes (mounted
/// channel).
pub(crate) static ENGINE: OnceLock<Mutex<EngineState>> = OnceLock::new();

/// Trigger channel sender stashed at install so the sync dispatch path can
/// wake the async refresh loop. Mutex-wrapped: a rebind replaces the sender
/// together with the restarted loop (the previous loop broke on
/// `session_shutdown`).
static TRIGGERS: OnceLock<Mutex<tokio::sync::mpsc::UnboundedSender<Trigger>>> = OnceLock::new();

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

/// Install: subscribe to the session events, spawn the refresh loop. A
/// second init (session replacement re-loading the same dlopen-memoized
/// cdylib on a fresh host) RE-BINDS: re-subscribes the events (the fresh
/// host registry is empty — without this every statusline event dies
/// after `/resume`), adopts the new channel and restarts the refresh loop
/// (the outgoing host's `session_shutdown` broke the previous one).
fn install(calls: RpiHostCalls, cookie: PluginCookie) -> Value {
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
    // §1.5 启用时机注记: the live subscription set is fixed at load — a
    // runtime `liveTokens` key edit changes nothing until restart.
    let live_tokens_enabled = config::live_tokens_configured_in_global_settings();
    if live_tokens_enabled {
        for event in LIVE_EVENTS {
            let response = host_call_static(&channel, "on", json!({"event": event}));
            if response.get("error").is_some() {
                return response;
            }
        }
    }

    // Rebind: newest host wins (see CHANNEL). The loop restart shares the
    // engine state — the new session's `session_start` event resets the
    // per-session fields.
    if let Some(state) = STATE.get() {
        *CHANNEL
            .get()
            .expect("channel cell installed with the first state")
            .write()
            .unwrap_or_else(|error| error.into_inner()) = channel;
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        *TRIGGERS
            .get()
            .expect("trigger cell installed with the first state")
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = tx;
        state.runtime.spawn(refresh::refresh_loop(
            Arc::clone(CHANNEL.get().expect("channel cell")),
            rx,
        ));
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
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let _ = TRIGGERS.set(Mutex::new(tx));
    let _ = ENGINE.set(Mutex::new(EngineState::default()));
    let _ = CHANNEL.set(Arc::new(RwLock::new(channel)));
    if live_tokens_enabled {
        with_engine(|engine| engine.arm_live_measure());
    }
    runtime.spawn(refresh::refresh_loop(
        Arc::clone(CHANNEL.get().expect("channel cell just set")),
        rx,
    ));
    let _ = STATE.set(PluginState { runtime });
    json!({"ok": true})
}

/// Event dispatch: light state updates + a channel send, nothing more.
fn handle_dispatch(message: &Value) -> Value {
    if message.get("kind").and_then(Value::as_str) != Some("event") {
        return Value::Null;
    }
    let event = message.get("event").and_then(Value::as_str).unwrap_or("");
    tracing::info!(event, "statusline dispatch");
    let payload = message.get("payload").cloned().unwrap_or(Value::Null);
    let trigger = match event {
        "message_end" => {
            let message = payload.get("message").cloned().unwrap_or(Value::Null);
            with_engine(|engine| {
                engine.accumulate_usage(&message);
                // FR-C/FR-D: exact output tokens + streaming=false.
                if let Some(live) = engine.live.as_mut() {
                    live.on_message_end(&message);
                }
            });
            Trigger::Event("message_end")
        }
        "message_start" => {
            // Bookkeeping only (FR-D); never the debounce must-run path.
            with_engine(|engine| {
                if let Some(live) = engine.live.as_mut() {
                    live.on_message_start(payload.pointer("/message/role").and_then(Value::as_str));
                }
            });
            Trigger::Live
        }
        "message_update" => {
            // FR-A: O(1) per delta — read {type, delta}, accumulate chars.
            with_engine(|engine| {
                if let Some(live) = engine.live.as_mut() {
                    live.on_message_update(
                        payload.get("assistantMessageEvent").unwrap_or(&Value::Null),
                    );
                }
            });
            Trigger::Live
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
    if let Some(cell) = TRIGGERS.get() {
        let _ = cell
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .send(trigger);
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
        let mut live = LIVE_EVENTS.to_vec();
        live.sort_unstable();
        live.dedup();
        assert_eq!(live.len(), 2, "message_start + message_update only");
    }
}
