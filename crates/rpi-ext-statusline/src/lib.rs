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
//! Runtime model (subagents precedent): the host dispatch
//! thread only does in-memory state updates plus a channel send (µs
//! scale — the TE-D6 dispatch-blocking discipline); the refresh loop,
//! host calls and script child processes live on the plugin's private
//! tokio runtime. Host-call surface: `on`, `ctx.*` (including the
//! additive `ctx.sessionFile`, ADR-0022), `ui.setFooter`, `ui.setStatus`,
//! `ui.setWidget`.

pub mod config;
pub mod editor;
pub mod fields;
pub mod git;
pub mod items;
pub mod paths;
pub mod payload;
pub mod refresh;
pub mod render;
pub mod runner;
pub mod runtime;
pub mod session_stats;
pub mod settings_write;
pub mod state;
pub mod subscriptions;
pub mod tokens;

use std::sync::{Arc, Mutex, OnceLock, RwLock};

use abi_stable::prefix_type::PrefixTypeTrait;
use abi_stable::std_types::RVec;
use rpi_ext_host::native::{PluginCookie, RpiHostCalls, RpiNativeModule, RpiNativeModule_Ref};
use serde_json::{Value, json};

use crate::refresh::Trigger;
use crate::runtime::PluginRuntime;
use crate::state::EngineState;

/// Live-token dispatch events (03-realtime-token-count §1.4/§1.5): the
/// event SET is reconciled from the configuration (`subscriptions`), while
/// these two bookkeeping events map to `Trigger::Live` (never the 300ms
/// debounce path).
#[allow(dead_code)] // documented dispatch table; the match below is the live path
const LIVE_DISPATCH_EVENTS: &[&str] = &["message_start", "message_update"];

/// Host-call handle + plugin-owned runtime, established once by
/// `rpi_extension_init`. The cookie is stored as `usize` to keep the state
/// `Send + Sync`. The fields are ownership keeps:
/// dropping the runtime would park the refresh loop.
#[allow(dead_code)]
struct PluginState {
    runtime: PluginRuntime,
}

static STATE: OnceLock<PluginState> = OnceLock::new();

/// The refresh loop's host channel, REBINDABLE across hosts (subagents
/// session-switch discipline): a session replacement (`/resume`
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

/// Dynamic `on`/`off` registrations keyed by event name (items mode) plus the
/// command-mode table (TE52 R20 §4.4).
static SUBSCRIPTIONS: OnceLock<Mutex<subscriptions::SubscriptionState>> = OnceLock::new();

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
    // `/statusline` editor/config command (manifest capability `commands`).
    let response = host_call_static(
        &channel,
        "registerCommand",
        json!({
            "name": "statusline",
            "description": "Configure the status line: /statusline | help | reset",
        }),
    );
    if response.get("error").is_some() {
        return response;
    }

    let first_install = STATE.get().is_none();
    // Rebind: newest host wins (see CHANNEL). The new host's registry is
    // empty, so drop the old subscription ids and re-register below; the
    // loop restart shares the engine state.
    if !first_install {
        *CHANNEL
            .get()
            .expect("channel cell installed with the first state")
            .write()
            .unwrap_or_else(|error| error.into_inner()) = channel;
        if let Some(subscriptions) = SUBSCRIPTIONS.get() {
            *subscriptions
                .lock()
                .unwrap_or_else(|error| error.into_inner()) =
                subscriptions::SubscriptionState::default();
        }
        reconcile_subscriptions(&channel, &config::load_settings_snapshot());
        let state = STATE.get().expect("state installed");
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
            }});
        }
    };
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let initial = config::load_settings_snapshot();
    let _ = TRIGGERS.set(Mutex::new(tx));
    let _ = ENGINE.set(Mutex::new(EngineState::default()));
    let _ = SUBSCRIPTIONS.set(Mutex::new(subscriptions::SubscriptionState::default()));
    let _ = CHANNEL.set(Arc::new(RwLock::new(channel)));
    reconcile_subscriptions(&channel, &initial);
    runtime.spawn(refresh::refresh_loop(
        Arc::clone(CHANNEL.get().expect("channel cell just set")),
        rx,
    ));
    let _ = STATE.set(PluginState { runtime });
    json!({"ok": true})
}

/// Reconcile the dynamic subscription set (and the items live measure) with
/// the current configuration. Called at install, after a rebind and on every
/// refresh poll.
pub(crate) fn reconcile_subscriptions(
    channel: &AsyncHostCalls,
    settings: &config::SettingsSnapshot,
) {
    let command_live = matches!(
        settings.status_line.as_ref(),
        Some(config::StatusLine::Command(config)) if config.live_tokens.enabled
    );
    let desired = subscriptions::desired_events(settings.status_line.as_ref(), command_live);
    if let Some(state) = SUBSCRIPTIONS.get() {
        state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .reconcile(channel, &desired);
    }
    // Live membership is dynamic in both modes: disabled → off + clear,
    // enabled → arm the measurement. Command mode used to require a restart;
    // the poll-time reconciliation removes that trap (the event set still
    // stays empty while the switch is off, so the zero-overhead property is
    // unchanged).
    match settings.status_line.as_ref() {
        Some(config::StatusLine::Items(items)) => {
            with_engine(|engine| engine.set_live_measure(items.live_enabled()));
        }
        Some(config::StatusLine::Command(config)) => {
            with_engine(|engine| engine.set_live_measure(config.live_tokens.enabled));
        }
        None => with_engine(|engine| engine.set_live_measure(false)),
    }
}

/// Event dispatch: light state updates + a channel send, nothing more.
fn handle_dispatch(message: &Value) -> Value {
    match message.get("kind").and_then(Value::as_str) {
        Some("event") => {}
        Some("command") => {
            if message.get("name").and_then(Value::as_str) != Some("statusline") {
                return Value::Null;
            }
            let args = message.get("args").and_then(Value::as_str).unwrap_or("");
            let calls = *CHANNEL
                .get()
                .expect("channel cell installed before commands")
                .read()
                .unwrap_or_else(|error| error.into_inner());
            return editor::handle_command(&calls, args);
        }
        _ => return Value::Null,
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
            // Arm on the event itself: a stream may start immediately after
            // a config save, before the next poll-time reconcile runs. Live
            // events only arrive while the streaming subscription is active,
            // so arming here is always correct (and avoids dropping the
            // first deltas of that window).
            with_engine(|engine| {
                engine.arm_live_measure();
                if let Some(live) = engine.live.as_mut() {
                    live.on_message_start(payload.pointer("/message/role").and_then(Value::as_str));
                }
            });
            Trigger::Live
        }
        "message_update" => {
            // FR-A + #45: O(1) amortized per delta — count the chars,
            // retain the raw delta text and read the cumulative partial's
            // provider usage; all inside LiveMeasure. Same arm-on-event
            // rule as message_start (also covers enabling liveTokens
            // mid-stream).
            with_engine(|engine| {
                engine.arm_live_measure();
                if let Some(live) = engine.live.as_mut() {
                    live.on_message_update(&payload);
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
        // TE52 items triggers (subscribed only when a selected field needs
        // them; a stray event is still cheap bookkeeping).
        "turn_end" => Trigger::Event("turn_end"),
        "agent_start" => Trigger::Event("agent_start"),
        "agent_settled" => Trigger::Event("agent_settled"),
        "mode_change" => Trigger::Event("mode_change"),
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

pub(crate) fn with_engine<T>(update: impl FnOnce(&mut EngineState) -> T) -> T {
    let engine = ENGINE
        .get()
        .expect("engine state installed before dispatch");
    let mut engine = engine.lock().unwrap_or_else(|error| error.into_inner());
    update(&mut engine)
}

/// Wake the refresh loop after a configuration save (dynamic subscriptions
/// and the first re-render happen on the next tick).
pub(crate) fn request_refresh(reason: &'static str) {
    request_trigger(Trigger::Event(reason));
}

/// Push an arbitrary trigger (background Git probes).
pub(crate) fn request_trigger(trigger: Trigger) {
    if let Some(cell) = TRIGGERS.get() {
        let _ = cell
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .send(trigger);
    }
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

/// Test seam (`install_for_test` precedent): install directly
/// from Rust, bypassing the cdylib/ABI boundary.
pub fn install_for_test(calls: RpiHostCalls, cookie: PluginCookie) -> Value {
    install(calls, cookie)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_dispatch_events_are_unique() {
        let mut live = LIVE_DISPATCH_EVENTS.to_vec();
        live.sort_unstable();
        let count = live.len();
        live.dedup();
        assert_eq!(live.len(), count);
        assert_eq!(count, 2, "message_start + message_update only");
    }

    #[test]
    fn subscription_tables_are_disjoint_and_unique() {
        let mut base = subscriptions::BASE_EVENTS.to_vec();
        base.sort_unstable();
        let count = base.len();
        base.dedup();
        assert_eq!(base.len(), count);
        let mut all = subscriptions::BASE_EVENTS
            .iter()
            .chain(subscriptions::COMMAND_EVENTS.iter())
            .chain(subscriptions::LIVE_EVENTS.iter())
            .copied()
            .collect::<Vec<_>>();
        all.sort_unstable();
        all.dedup();
        for event in subscriptions::COMMAND_EVENTS
            .iter()
            .chain(subscriptions::LIVE_EVENTS.iter())
        {
            assert!(
                !subscriptions::BASE_EVENTS.contains(event),
                "{event} must not be duplicated in the base set"
            );
        }
    }
}
