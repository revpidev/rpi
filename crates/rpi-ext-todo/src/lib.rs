//! `rpi-ext-todo` — L0 native plugin, port of `@juicesharp/rpiv-todo`
//! v2.10.1+ (`juicesharp/rpiv-mono` `packages/rpiv-todo/` @ `0fdf4f8`;
//! `338b264..0fdf4f8` is comment-level for this package — behavioral
//! surface = v2.9.0 semantics).
//!
//! P0 (TE34): the `todo` tool with its full six-action state machine and
//! `blockedBy` dependency validation, the session-isolated state store,
//! branch replay through `ctx.sessionToolResults` (ADR-0030, host side
//! V15-14), and the prompt surface. P1 (TE35): the persistent overlay
//! widget above the editor (`setWidget` re-send shape mapping, design
//! §4.1), the `/todos` command, the collapse shortcut, the XDG config,
//! the embedded nine-locale i18n tables (TE-D43), and the
//! renderCall/renderResult transcript renderers. Event wiring follows
//! upstream `index.ts` @ `0fdf4f8` on both layers.
//!
//! Docs: `rpi-docs/extensions/rpiv-todo/{01,02}.md` and the task files
//! `rpi-docs/plan/extensions/TE34-rpiv-todo-p0.md` / `TE35-rpiv-todo-p1.md`.
//!
//! Native plugin runtime model (ask-user-question precedent):
//! `rpi_extension_init` registers through the host-call handle and
//! records the channel under the init cookie; `rpi_dispatch` answers with
//! THE SAME cookie's channel — each host (each session's extension
//! runner) loads the plugin with its own cookie, so `ctx.*` calls made
//! while serving one host's dispatch resolve against that host's bound
//! session (per-session isolation, design §5). This replaces the
//! ask-user-question "newest channel wins" simplification, which would
//! cross-wire child sessions onto the interactive session's context.

pub mod config;
pub mod i18n;
pub mod overlay;
pub mod state;
pub mod tool;
pub mod view;

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use abi_stable::prefix_type::PrefixTypeTrait;
use abi_stable::std_types::RVec;
use rpi_ext_host::native::{PluginCookie, RpiHostCalls, RpiNativeModule, RpiNativeModule_Ref};
use serde_json::{json, Value};

use crate::state::replay::replay_via_host;
use crate::state::store::{reset_store, store};
use crate::tool::types::TOOL_NAME;

/// Structured host-call failure (`{"error": {"kind", "message"}}`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostError {
    /// ABI error kind (`invalidRequest`, `stale`, `capabilityDenied`, …).
    pub kind: String,
    /// Human-readable detail.
    pub message: String,
}

impl std::fmt::Display for HostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.kind, self.message)
    }
}

impl HostError {
    /// The stale-ctx failure of an invalidated context proxy (upstream
    /// `isStaleCtxError` matches the stable "stale after session
    /// replacement" substring of pi-core's runner error; rpi surfaces the
    /// same condition as the `stale` error kind).
    pub fn is_stale(&self) -> bool {
        self.kind == "stale"
    }
}

/// Guest → host JSON call surface (`{"call": method, "args": args}` →
/// `{"ok": ...} | {"error": {"kind", "message"}}`). Abstracted so pure
/// logic is testable without the abi_stable boundary.
pub trait HostCall {
    /// Send one host call; `Ok` carries the unwrapped `ok` payload.
    fn call(&self, method: &str, args: Value) -> Result<Value, HostError>;
}

/// The native (L0) transport over the abi_stable trampoline.
#[derive(Clone, Copy)]
pub struct NativeHostCall {
    /// `RpiHostCalls::call` handed to `rpi_extension_init`.
    call: extern "C" fn(PluginCookie, RVec<u8>) -> RVec<u8>,
    /// Opaque context pointer (stored as `usize` to stay `Send + Sync`).
    cookie: usize,
}

impl NativeHostCall {
    /// Build a transport from the init receipt.
    pub fn new(calls: RpiHostCalls, cookie: PluginCookie) -> Self {
        Self {
            call: calls.call,
            cookie: cookie as usize,
        }
    }
}

impl HostCall for NativeHostCall {
    fn call(&self, method: &str, args: Value) -> Result<Value, HostError> {
        let request = serde_json::to_vec(&json!({
            "call": method,
            "args": args,
            "seq": 0,
        }))
        .map_err(|error| HostError {
            kind: "protocolError".to_owned(),
            message: format!("request JSON: {error}"),
        })?;
        let response = (self.call)(self.cookie as PluginCookie, RVec::from(request));
        let response: Value = serde_json::from_slice(&response[..]).map_err(|error| HostError {
            kind: "protocolError".to_owned(),
            message: format!("response JSON: {error}"),
        })?;
        if let Some(error) = response.get("error") {
            return Err(HostError {
                kind: error
                    .get("kind")
                    .and_then(Value::as_str)
                    .unwrap_or("internal")
                    .to_owned(),
                message: error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("host error")
                    .to_owned(),
            });
        }
        Ok(response.get("ok").cloned().unwrap_or(Value::Null))
    }
}

// ---------------------------------------------------------------------------
// Session-id extraction (FR-G; verified against the current ABI surface)
// ---------------------------------------------------------------------------

/// `ctx.sessionFile()` (rpi additive, ADR-0022) — the authoritative
/// `{path, id}` accessor; `id` is the bound session's id. Unbound hosts
/// answer `{path: null, id: ""}` (never an error), and transport failures
/// resolve to `""` too: an unknown/empty session id is the upstream ""
/// foreground sentinel (upstream `sid(ctx)` null-coercion).
pub fn sid_of(host: &dyn HostCall) -> String {
    host.call("ctx.sessionFile", json!({}))
        .ok()
        .and_then(|value| value.get("id").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_default()
}

/// `ctx.hasUI` — transport failures degrade to `false` (the reconciler
/// precedent in rpi-ext-ask-user-question).
fn has_ui(host: &dyn HostCall) -> bool {
    host.call("ctx.hasUI", json!({}))
        .ok()
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Channel registry (cookie → host channel)
// ---------------------------------------------------------------------------

/// Channels by init cookie. Each `load_native_plugin` run gets its own
/// `NativeCallContext` (cookie), one per host instance; reloads allocate
/// fresh cookies, so stale entries are inert (no dispatch arrives with
/// them) and bounded by the reload count.
static CHANNELS: OnceLock<Mutex<HashMap<usize, NativeHostCall>>> = OnceLock::new();

fn channels() -> &'static Mutex<HashMap<usize, NativeHostCall>> {
    CHANNELS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn store_channel(cookie: PluginCookie, host: NativeHostCall) {
    channels()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(cookie as usize, host);
}

fn channel_for(cookie: PluginCookie) -> Option<NativeHostCall> {
    channels()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(&(cookie as usize))
        .copied()
}

// ---------------------------------------------------------------------------
// P1 globals: locale table, overlay controller, bound collapse key
// (upstream index.ts closure state: the i18n registration at module
// init, `todoOverlay`/`uiCtx`, and the factory-scope `collapseKey`)
// ---------------------------------------------------------------------------

/// The process locale table (detected once at install; upstream
/// registers the rpiv-i18n strings once at module init — TE-D43 embeds
/// the tables instead).
static I18N: OnceLock<i18n::I18n> = OnceLock::new();

fn current_i18n() -> &'static i18n::I18n {
    I18N.get_or_init(i18n::I18n::detect)
}

/// The foreground overlay controller (upstream `todoOverlay`/`uiCtx`
/// closure variables; foreground-unique like the store's render pointer).
static OVERLAY: OnceLock<Mutex<overlay::OverlayController>> = OnceLock::new();

fn overlay_controller() -> &'static Mutex<overlay::OverlayController> {
    OVERLAY.get_or_init(|| Mutex::new(overlay::OverlayController::default()))
}

/// The registration-time collapse key (upstream factory-scope
/// `resolveCollapseKey()` — register-once; a config change needs a
/// restart to re-bind, requirements §7). Empty = the "off" sentinel
/// (no shortcut registered).
static COLLAPSE_KEY: OnceLock<String> = OnceLock::new();

fn registered_collapse_key() -> &'static String {
    COLLAPSE_KEY.get_or_init(config::resolve_collapse_key)
}

// ---------------------------------------------------------------------------
// Install + dispatch
// ---------------------------------------------------------------------------

fn error_envelope(kind: &str, message: impl std::fmt::Display) -> Value {
    json!({"error": {"kind": kind, "message": message.to_string()}})
}

/// Install: register the tool (+ render flags), the `/todos` command,
/// the collapse shortcut (unless `"off"`), subscribe the six lifecycle
/// events, resolve the locale, and record the channel. Idempotent across
/// reloads (fresh cookie per load).
fn install(calls: RpiHostCalls, cookie: PluginCookie) -> Value {
    install_with_key(calls, cookie, registered_collapse_key().clone())
}

/// Test/production seam over an explicit registration-time collapse key.
fn install_with_key(calls: RpiHostCalls, cookie: PluginCookie, collapse_key: String) -> Value {
    let host = NativeHostCall::new(calls, cookie);

    // Locale detection happens before any registration that could
    // surface localized chrome (upstream registers strings at module
    // init, before the factory export runs).
    let _ = current_i18n();

    if let Err(error) = host.call("registerTool", tool::tool_definition()) {
        return error_envelope("init", error);
    }
    if let Err(error) = host.call("registerCommand", tool::todos_command_definition()) {
        return error_envelope("init", error);
    }
    // Factory-scope key resolution (register-once contract): the binding
    // is skipped entirely when collapseKey is "off" (upstream
    // index.ts:156-168). The per-render hint still re-resolves the key
    // from config so a config edit shows up in the collapsed hint
    // without a re-bind (the "off" sentinel's static `collapsed` label
    // covers exactly that window).
    if collapse_key != config::COLLAPSE_KEY_OFF {
        if let Err(error) = host.call(
            "registerShortcut",
            json!({
                "shortcut": collapse_key,
                "description": "Collapse or expand the todo overlay",
            }),
        ) {
            return error_envelope("init", error);
        }
    }
    for event in [
        "session_start",
        "session_compact",
        "session_tree",
        "session_shutdown",
        "tool_execution_end",
        "agent_start",
    ] {
        if let Err(error) = host.call("on", json!({ "event": event })) {
            return error_envelope("init", error);
        }
    }
    store_channel(cookie, host);
    tracing::info!("rpiv-todo installed");
    json!({"ok": true})
}

/// Dispatch one host → plugin message. Events and tool executions are
/// answered through the dispatching host's own channel so `ctx.*` calls
/// resolve against that host's bound session.
fn dispatch_message(cookie: PluginCookie, message: &Value) -> Value {
    let Some(host) = channel_for(cookie) else {
        return Value::Null;
    };
    match message.get("kind").and_then(Value::as_str) {
        Some("toolExecute")
            if message.get("toolName").and_then(Value::as_str) == Some(TOOL_NAME) =>
        {
            let params = message.get("params").cloned().unwrap_or(Value::Null);
            tool::execute(&host, &params)
        }
        Some("command")
            if message.get("name").and_then(Value::as_str)
                == Some(crate::tool::types::COMMAND_NAME) =>
        {
            tool::handle_todos_command(&host, current_i18n());
            Value::Null
        }
        Some("shortcut")
            if message.get("shortcut").and_then(Value::as_str)
                == Some(registered_collapse_key().as_str()) =>
        {
            let mut controller = overlay_controller()
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            controller.handle_shortcut(&host, current_i18n());
            Value::Null
        }
        Some("render") if message.get("toolName").and_then(Value::as_str) == Some(TOOL_NAME) => {
            render_dispatch(&host, message)
        }
        Some("event") => {
            let event = message.get("event").and_then(Value::as_str).unwrap_or("");
            let payload = message.get("payload").cloned().unwrap_or(Value::Null);
            handle_event(&host, event, &payload);
            Value::Null
        }
        _ => Value::Null,
    }
}

/// The transcript render dispatch (`{"kind":"render","what":…}`):
/// `renderCall` reads the FOREGROUND slot (the render context carries no
/// session identity — upstream `renderTodoCall(args, theme, _context)`
/// resolves through `getRenderState()`; a detached call falls back to
/// `#<id>`, intentionally); `renderResult` inspects the result's
/// `details`. The theme re-reads `ctx.ui.theme` per dispatch.
fn render_dispatch(host: &NativeHostCall, message: &Value) -> Value {
    let theme = match host.call("ui.theme", json!({})) {
        Ok(theme) => view::AnsiTheme::from_theme_json(&theme),
        Err(_) => view::AnsiTheme::from_theme_json(&Value::Null),
    };
    match message.get("what").and_then(Value::as_str) {
        Some("toolCall") => {
            let context = message.get("context").cloned().unwrap_or(Value::Null);
            let args = context.get("args").cloned().unwrap_or(Value::Null);
            let state = store().render_state();
            view::render_todo_call(&args, &theme, &state, current_i18n())
        }
        Some("toolResult") => {
            let result = message.get("result").cloned().unwrap_or(Value::Null);
            view::render_todo_result(&result, &theme, current_i18n())
        }
        _ => Value::Null,
    }
}

// ---------------------------------------------------------------------------
// Event wiring (upstream index.ts:194-290 @ `0fdf4f8`)
// ---------------------------------------------------------------------------

/// Replay the branch into `session_id`'s slot (upstream `replayAndRefresh`
/// body minus the overlay refresh). A stale context keeps the current
/// state — the replacement session's `session_start` replays it; other
/// errors are real replay bugs and are surfaced to the log (upstream
/// rethrows to Pi's `emitError`; the native dispatch envelope has no
/// error channel, so `tracing::warn` is the P0 equivalent — task-file §7
/// note).
fn replay_into_slot(host: &dyn HostCall, session_id: &str) {
    match replay_via_host(host) {
        Ok(state) => store().replace_state(session_id, state),
        Err(error) if error.is_stale() => {
            tracing::debug!("rpiv-todo: replay skipped on stale ctx (state kept)");
        }
        Err(error) => {
            tracing::warn!(%error, "rpiv-todo: replay failed");
        }
    }
}

fn handle_event(host: &dyn HostCall, event: &str, payload: &Value) {
    match event {
        // Every session replays into its OWN data slot (Phase 1 isolation).
        // First UI-bearing session_start claims the foreground
        // (creator-ownership); only the foreground re-binds and refreshes
        // the shared overlay (a child with a distinct sid is skipped —
        // it must not rebind to a relay/stale ui).
        "session_start" => {
            let sid = sid_of(host);
            replay_into_slot(host, &sid);
            if !has_ui(host) {
                return;
            }
            if store().active_render_session().is_empty() {
                store().set_active_render_session(&sid);
            }
            if sid != store().active_render_session() {
                return;
            }
            // Foreground: bind the UI ctx to the current generation and
            // refresh with a completed-display reset (upstream
            // `uiCtx = ctx.ui; await updateTodoOverlay(true, generation)`).
            let generation = store().lifecycle_generation();
            let mut controller = overlay_controller()
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            controller.bind_ui(generation);
            controller.update_todo_overlay(host, current_i18n(), true);
            tracing::debug!(sid = %sid, "rpiv-todo: foreground claimed");
        }
        // Shared by session_compact and session_tree (verbatim-identical
        // pre-extraction upstream): re-key the session's slot, refresh the
        // overlay only when the refreshed session IS the foreground.
        "session_compact" | "session_tree" => {
            let sid = sid_of(host);
            replay_into_slot(host, &sid);
            if sid == store().active_render_session() {
                let mut controller = overlay_controller()
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                controller.update_todo_overlay(host, current_i18n(), true);
                tracing::debug!(event, "rpiv-todo: foreground replayed after {event}");
            }
        }
        // The shutting-down session's own slot is always evicted. Overlay
        // teardown is sid-gated: a child shutdown must not dispose the
        // foreground; only the foreground's own shutdown (or an
        // unknown/stale sid, resolved to "") tears it down and clears the
        // pointer + generation (try/finally semantics: the pointer clears
        // even when the dispose push fails).
        "session_shutdown" => {
            let sid = sid_of(host);
            store().evict_session(&sid);
            if sid.is_empty() || sid == store().active_render_session() {
                store().clear_active_render_session();
                overlay_controller()
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .teardown(host);
                tracing::debug!("rpiv-todo: foreground torn down");
            }
        }
        // Reads the store at render time; do NOT replay here (the branch
        // is stale — message_end runs after tool_execution_end). A
        // transient widget failure costs this one refresh and warns; it
        // does not surface as an extension error (upstream catch).
        "tool_execution_end" => {
            let tool_name = payload.get("toolName").and_then(Value::as_str);
            let is_error = payload.get("isError").and_then(Value::as_bool);
            if tool_name == Some(TOOL_NAME) && is_error != Some(true) {
                let mut controller = overlay_controller()
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                controller.update_todo_overlay(host, current_i18n(), false);
            }
        }
        // Completed-task fade-out migration (upstream
        // `todoOverlay?.hideCompletedTasksFromPreviousTurn()`).
        "agent_start" => {
            let mut controller = overlay_controller()
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            controller.on_agent_start(host, current_i18n());
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// abi_stable entry points
// ---------------------------------------------------------------------------

fn pack(value: &Value) -> RVec<u8> {
    RVec::from(serde_json::to_vec(value).unwrap_or_else(|_| b"null".to_vec()))
}

/// Load entry (abi_stable). A panic must cross the ABI as an error
/// envelope, not unwind into the host (ask-user-question precedent).
#[allow(clippy::missing_safety_doc)]
pub extern "C" fn init(calls: RpiHostCalls, cookie: PluginCookie) -> RVec<u8> {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| install(calls, cookie)))
        .unwrap_or_else(|panic| {
            error_envelope("internal", format!("rpiv-todo init panicked: {panic:?}"))
        });
    pack(&result)
}

/// Dispatch entry (abi_stable).
#[allow(clippy::missing_safety_doc)]
pub extern "C" fn dispatch(cookie: PluginCookie, message: RVec<u8>) -> RVec<u8> {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let message: Value = serde_json::from_slice(&message[..]).unwrap_or(Value::Null);
        dispatch_message(cookie, &message)
    }))
    .unwrap_or_else(|_panic| {
        json!({
            "content": [{
                "type": "text",
                "text": "rpiv-todo panicked while handling a dispatch",
            }],
            "isError": true,
        })
    });
    pack(&result)
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

/// Test seam: install directly from Rust, bypassing the cdylib/ABI
/// boundary (ask-user-question `install_for_test` precedent).
#[doc(hidden)]
pub fn install_for_test(calls: RpiHostCalls, cookie: PluginCookie) -> Value {
    install(calls, cookie)
}

/// Test seam: dispatch a message against the cookie's channel.
#[doc(hidden)]
pub fn dispatch_for_test(cookie: PluginCookie, message: &Value) -> Value {
    dispatch_message(cookie, message)
}

/// Test seam: store reset (upstream `__resetState` import path — the
/// upstream suite also gets a FRESH index.ts closure per `registerTodo`
/// call, so the overlay controller resets alongside the store).
#[doc(hidden)]
pub fn __reset_state() {
    reset_store();
    *overlay_controller()
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = overlay::OverlayController::default();
}

/// Test seam: pin the process locale table to English before the first
/// real detection (the upstream suite is always English — the rpiv-i18n
/// SDK is absent under vitest, so the shim's inline fallbacks win; the
/// rpi counterpart of that absence is a pinned test locale). Best-effort:
/// returns false once a real detection has latched.
#[cfg(test)]
#[doc(hidden)]
pub fn set_test_locale(locale: &str) -> bool {
    I18N.set(i18n::I18n::for_locale(locale)).is_ok()
}

/// Shared serializing lock for every test that touches the process-global
/// store or channel registry (the upstream vitest suite runs serially).
#[cfg(test)]
pub(crate) static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    //! Wiring-level tests: sid extraction, install registrations, the six
    //! event handlers (FR-A/FR-D/FR-E anchors), and the `execute`
    //! orchestration against a scripted host (upstream
    //! `todo.session-isolation.test.ts` Phase-1 cases at the wiring level;
    //! the full harness e2e lands with TE36).

    use super::*;
    use serde_json::json;

    fn serialized() -> std::sync::MutexGuard<'static, ()> {
        crate::TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    /// Scripted host: canned replies per method, recorded calls.
    struct MockHost {
        session_id: &'static str,
        session_results: Vec<Value>,
        has_ui: bool,
        stale: bool,
        /// ctx.sessionToolResults fails with a NON-stale error (a real
        /// replay bug — distinct from `stale`, which models the dead ctx
        /// proxy).
        replay_error: bool,
        calls: Mutex<Vec<(String, Value)>>,
    }

    impl MockHost {
        fn new(session_id: &'static str) -> Self {
            MockHost {
                session_id,
                session_results: Vec::new(),
                has_ui: true,
                stale: false,
                replay_error: false,
                calls: Mutex::new(Vec::new()),
            }
        }

        fn with_results(session_id: &'static str, results: Vec<Value>) -> Self {
            MockHost {
                session_results: results,
                ..MockHost::new(session_id)
            }
        }

        fn recorded(&self, method: &str) -> Vec<Value> {
            self.calls
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .iter()
                .filter(|(recorded, _)| recorded == method)
                .map(|(_, args)| args.clone())
                .collect()
        }
    }

    impl HostCall for MockHost {
        fn call(&self, method: &str, args: Value) -> Result<Value, HostError> {
            self.calls
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push((method.to_owned(), args));
            match method {
                "ctx.sessionFile" => {
                    if self.stale {
                        return Err(HostError {
                            kind: "stale".to_owned(),
                            message: "stale ctx".to_owned(),
                        });
                    }
                    Ok(json!({"path": null, "id": self.session_id}))
                }
                "ctx.hasUI" => Ok(json!(self.has_ui)),
                "ctx.sessionToolResults" => {
                    if self.stale {
                        return Err(HostError {
                            kind: "stale".to_owned(),
                            message: "stale ctx".to_owned(),
                        });
                    }
                    if self.replay_error {
                        return Err(HostError {
                            kind: "internal".to_owned(),
                            message: "boom: real replay bug".to_owned(),
                        });
                    }
                    Ok(json!(self.session_results))
                }
                "registerTool" | "on" => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
    }

    fn todo_result(details: Value) -> Value {
        json!({
            "id": "e1", "parentId": null, "timestamp": "t",
            "toolName": "todo", "isError": false, "details": details,
        })
    }

    // ------------------------------------------------------------------
    // sid_of (FR-G)
    // ------------------------------------------------------------------

    #[test]
    fn sid_of_returns_the_session_file_id() {
        let host = MockHost::new("abc");
        assert_eq!(sid_of(&host), "abc");
        assert_eq!(host.recorded("ctx.sessionFile").len(), 1);
    }

    #[test]
    fn sid_of_coerces_transport_failure_to_empty_string() {
        let mut host = MockHost::new("abc");
        host.stale = true;
        assert_eq!(sid_of(&host), "");
    }

    #[test]
    fn sid_of_maps_an_empty_id_to_the_sentinel() {
        let host = MockHost::new("");
        assert_eq!(sid_of(&host), "");
    }

    // ------------------------------------------------------------------
    // Install (FR-A)
    // ------------------------------------------------------------------

    extern "C" fn fake_call(_cookie: PluginCookie, _request: RVec<u8>) -> RVec<u8> {
        RVec::from(br#"{"ok": null}"#.to_vec())
    }

    #[test]
    fn install_registers_the_tool_and_six_events() {
        let _guard = serialized();
        __reset_state();
        let calls = RpiHostCalls { call: fake_call };
        let cookie = 0xdead as PluginCookie;
        let receipt = install_for_test(calls, cookie);
        assert_eq!(receipt, json!({"ok": true}));
        // The channel registry serves dispatches for this cookie.
        let reply = dispatch_for_test(
            cookie,
            &json!({"kind": "event", "event": "agent_start", "payload": {}}),
        );
        assert_eq!(reply, Value::Null);
        // Unknown cookies answer Null.
        let reply = dispatch_for_test(
            0xbeef as PluginCookie,
            &json!({"kind": "event", "event": "agent_start", "payload": {}}),
        );
        assert_eq!(reply, Value::Null);
    }

    // ------------------------------------------------------------------
    // session_start: replay into the session's own slot + foreground claim
    // (FR-E / FR-D; upstream index.ts:194-226)
    // ------------------------------------------------------------------

    #[test]
    fn session_start_replays_into_the_sessions_own_slot() {
        let _guard = serialized();
        __reset_state();
        let host = MockHost::with_results(
            "parent",
            vec![todo_result(json!({
                "action": "create", "params": {},
                "tasks": [{ "id": 1, "subject": "from-branch", "status": "pending" }],
                "nextId": 2
            }))],
        );
        handle_event(&host, "session_start", &json!({"type": "session_start"}));
        let state = store().state_for("parent");
        assert_eq!(state.tasks.len(), 1);
        assert_eq!(state.tasks[0].subject, "from-branch");
        // First UI-bearing start claims the foreground.
        assert_eq!(store().active_render_session(), "parent");
    }

    #[test]
    fn headless_session_start_never_claims_the_foreground() {
        let _guard = serialized();
        __reset_state();
        let mut host = MockHost::new("headless");
        host.has_ui = false;
        handle_event(&host, "session_start", &json!({}));
        assert_eq!(store().active_render_session(), "");
        // The replay still landed in the session's own slot.
        assert!(store().state_for("headless").tasks.is_empty());
    }

    #[test]
    fn child_session_start_replays_own_slot_without_claiming_foreground() {
        let _guard = serialized();
        __reset_state();
        let parent = MockHost::with_results(
            "parent",
            vec![todo_result(json!({
                "action": "create", "params": {},
                "tasks": [{ "id": 1, "subject": "parent-task", "status": "pending" }],
                "nextId": 2
            }))],
        );
        handle_event(&parent, "session_start", &json!({}));
        let child = MockHost::with_results("child", Vec::new());
        handle_event(&child, "session_start", &json!({}));
        // Parent slot untouched by the child replay; child slot empty.
        assert_eq!(store().state_for("parent").tasks[0].subject, "parent-task");
        assert!(store().state_for("child").tasks.is_empty());
        // Creator-ownership: the pointer stays on the parent.
        assert_eq!(store().active_render_session(), "parent");
        assert_eq!(store().render_state().tasks[0].subject, "parent-task");
    }

    #[test]
    fn session_start_with_stale_ctx_keeps_current_state() {
        let _guard = serialized();
        __reset_state();
        store().replace_state(
            "parent",
            crate::state::TaskState {
                tasks: vec![crate::tool::types::Task {
                    id: 1,
                    subject: "committed".to_owned(),
                    status: crate::tool::types::TaskStatus::Pending,
                    description: None,
                    active_form: None,
                    blocked_by: None,
                    owner: None,
                    metadata: None,
                }],
                next_id: 2,
            },
        );
        let mut host = MockHost::new("parent");
        host.stale = true;
        handle_event(&host, "session_start", &json!({}));
        // Stale ctx: keep the current state (no clobber with empty).
        assert_eq!(store().state_for("parent").tasks[0].subject, "committed");
    }

    // ------------------------------------------------------------------
    // session_compact / session_tree (FR-E)
    // ------------------------------------------------------------------

    #[test]
    fn compact_and_tree_replay_into_the_slot() {
        let _guard = serialized();
        __reset_state();
        let host = MockHost::with_results(
            "s1",
            vec![todo_result(json!({
                "action": "update", "params": {},
                "tasks": [{ "id": 1, "subject": "after-compaction", "status": "in_progress" }],
                "nextId": 2
            }))],
        );
        handle_event(
            &host,
            "session_compact",
            &json!({"type": "session_compact"}),
        );
        assert_eq!(store().state_for("s1").tasks[0].subject, "after-compaction");
        let host = MockHost::with_results(
            "s1",
            vec![todo_result(json!({
                "action": "update", "params": {},
                "tasks": [{ "id": 1, "subject": "after-tree-nav", "status": "completed" }],
                "nextId": 2
            }))],
        );
        handle_event(&host, "session_tree", &json!({"type": "session_tree"}));
        assert_eq!(store().state_for("s1").tasks[0].subject, "after-tree-nav");
    }

    // ------------------------------------------------------------------
    // session_shutdown (FR-D eviction + foreground teardown)
    // ------------------------------------------------------------------

    #[test]
    fn shutdown_evicts_the_session_slot() {
        let _guard = serialized();
        __reset_state();
        store().commit_state(
            "parent",
            crate::state::TaskState {
                tasks: vec![crate::tool::types::Task {
                    id: 1,
                    subject: "t".to_owned(),
                    status: crate::tool::types::TaskStatus::Pending,
                    description: None,
                    active_form: None,
                    blocked_by: None,
                    owner: None,
                    metadata: None,
                }],
                next_id: 2,
            },
        );
        let host = MockHost::new("parent");
        handle_event(
            &host,
            "session_shutdown",
            &json!({"type": "session_shutdown"}),
        );
        assert!(store().state_for("parent").tasks.is_empty());
        assert_eq!(store().state_for("parent").next_id, 1);
    }

    #[test]
    fn child_shutdown_does_not_tear_down_the_foreground() {
        let _guard = serialized();
        __reset_state();
        let parent = MockHost::new("parent");
        handle_event(&parent, "session_start", &json!({}));
        let child = MockHost::new("child");
        handle_event(&child, "session_shutdown", &json!({}));
        assert_eq!(store().active_render_session(), "parent");
    }

    #[test]
    fn foreground_shutdown_clears_pointer_and_bumps_generation() {
        let _guard = serialized();
        __reset_state();
        let parent = MockHost::new("parent");
        handle_event(&parent, "session_start", &json!({}));
        let generation = store().lifecycle_generation();
        handle_event(&parent, "session_shutdown", &json!({}));
        assert_eq!(store().active_render_session(), "");
        assert!(store().lifecycle_generation() > generation);
    }

    #[test]
    fn stale_shutdown_sid_resolves_to_foreground_teardown() {
        // Disposal racing a stale ctx: sid "" is treated as foreground
        // (upstream pre-isolation safe default).
        let _guard = serialized();
        __reset_state();
        let parent = MockHost::new("parent");
        handle_event(&parent, "session_start", &json!({}));
        let mut gone = MockHost::new("whatever");
        gone.stale = true;
        handle_event(&gone, "session_shutdown", &json!({}));
        assert_eq!(store().active_render_session(), "");
    }

    // ------------------------------------------------------------------
    // compact/tree stale-ctx handling (upstream todo.invalidation.test.ts)
    // ------------------------------------------------------------------

    #[test]
    fn compact_and_tree_keep_state_on_a_stale_ctx() {
        let _guard = serialized();
        __reset_state();
        store().replace_state(
            "s1",
            crate::state::TaskState {
                tasks: vec![crate::tool::types::Task {
                    id: 1,
                    subject: "keep me".to_owned(),
                    status: crate::tool::types::TaskStatus::Pending,
                    description: None,
                    active_form: None,
                    blocked_by: None,
                    owner: None,
                    metadata: None,
                }],
                next_id: 2,
            },
        );
        let mut host = MockHost::new("s1");
        host.stale = true;
        handle_event(&host, "session_compact", &json!({}));
        // State untouched — no replay ran, the prior seed survives.
        assert_eq!(store().state_for("s1").tasks[0].subject, "keep me");
        handle_event(&host, "session_tree", &json!({}));
        assert_eq!(store().state_for("s1").tasks[0].subject, "keep me");
    }

    #[test]
    fn compact_survives_a_real_replay_error_without_panicking() {
        // A non-stale replay failure (real bug) surfaces as a warn and
        // keeps the current state — the native dispatch envelope has no
        // error channel (upstream rethrows; task-file §7.3 ruling 6).
        let _guard = serialized();
        __reset_state();
        store().commit_state(
            "s1",
            crate::state::TaskState {
                tasks: vec![crate::tool::types::Task {
                    id: 1,
                    subject: "committed".to_owned(),
                    status: crate::tool::types::TaskStatus::Pending,
                    description: None,
                    active_form: None,
                    blocked_by: None,
                    owner: None,
                    metadata: None,
                }],
                next_id: 2,
            },
        );
        let mut host = MockHost::new("s1");
        host.replay_error = true;
        handle_event(&host, "session_compact", &json!({}));
        assert_eq!(store().state_for("s1").tasks[0].subject, "committed");
    }

    // ------------------------------------------------------------------
    // tool_execution_end / agent_start placeholders (FR-A: sid/log only)
    // ------------------------------------------------------------------

    #[test]
    fn tool_execution_end_ignores_foreign_and_errored_calls() {
        let _guard = serialized();
        __reset_state();
        let host = MockHost::new("s1");
        handle_event(
            &host,
            "tool_execution_end",
            &json!({"toolName": "bash", "isError": false}),
        );
        handle_event(
            &host,
            "tool_execution_end",
            &json!({"toolName": "todo", "isError": true}),
        );
        // No state interactions; the todo success path logs only (TE35).
        handle_event(
            &host,
            "tool_execution_end",
            &json!({"toolName": "todo", "isError": false}),
        );
        assert!(host.recorded("ctx.sessionFile").is_empty());
    }

    // ------------------------------------------------------------------
    // execute orchestration (upstream tool body)
    // ------------------------------------------------------------------

    #[test]
    fn execute_creates_and_commits_into_the_calling_session_slot() {
        let _guard = serialized();
        __reset_state();
        let host = MockHost::new("s1");
        let result = crate::tool::execute(&host, &json!({"action": "create", "subject": "first"}));
        assert_eq!(
            result["content"][0]["text"],
            json!("Created #1: first (pending)")
        );
        assert_eq!(result["details"]["nextId"], json!(2));
        let state = store().state_for("s1");
        assert_eq!(state.tasks.len(), 1);
        assert_eq!(state.next_id, 2);
    }

    #[test]
    fn execute_error_envelope_still_carries_details_snapshot() {
        let _guard = serialized();
        __reset_state();
        let host = MockHost::new("s1");
        let result = crate::tool::execute(&host, &json!({"action": "create", "subject": ""}));
        assert_eq!(
            result["content"][0]["text"],
            json!("Error: subject required for create")
        );
        assert_eq!(
            result["details"]["error"],
            json!("subject required for create")
        );
        assert_eq!(result["details"]["nextId"], json!(1));
    }

    #[test]
    fn execute_invalid_action_is_a_content_only_envelope() {
        let _guard = serialized();
        __reset_state();
        let host = MockHost::new("s1");
        let result = crate::tool::execute(&host, &json!({"action": "explode"}));
        assert_eq!(
            result["content"][0]["text"],
            json!("Error: unknown action explode")
        );
        assert!(result.get("details").is_none());
        let result = crate::tool::execute(&host, &json!({}));
        assert_eq!(
            result["content"][0]["text"],
            json!("Error: action is required")
        );
    }

    // ------------------------------------------------------------------
    // Install registration surface (P1: command + shortcut + render flags)
    // — upstream todo-overlay.shortcut.test.ts registration cases
    // ------------------------------------------------------------------

    /// Scripted transport: canned `ok` replies per method + a recording
    /// of every call (the extern "C" boundary can capture nothing, so the
    /// state lives in statics under TEST_LOCK).
    struct Transport {
        replies: Mutex<std::collections::HashMap<String, Value>>,
        calls: Mutex<Vec<(String, Value)>>,
    }

    impl Transport {
        fn set_reply(&self, method: &str, reply: Value) {
            self.replies
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .insert(method.to_owned(), reply);
        }
    }

    static TRANSPORT: OnceLock<Transport> = OnceLock::new();

    fn transport() -> &'static Transport {
        TRANSPORT.get_or_init(|| Transport {
            replies: Mutex::new(std::collections::HashMap::new()),
            calls: Mutex::new(Vec::new()),
        })
    }

    extern "C" fn scripted_call(_cookie: PluginCookie, request: RVec<u8>) -> RVec<u8> {
        let parsed: Value = serde_json::from_slice(&request[..]).unwrap_or(Value::Null);
        let method = parsed
            .get("call")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let args = parsed.get("args").cloned().unwrap_or(Value::Null);
        transport()
            .calls
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push((method.clone(), args));
        let reply = transport()
            .replies
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&method)
            .cloned()
            .unwrap_or(Value::Null);
        RVec::from(serde_json::to_vec(&json!({"ok": reply})).unwrap_or_default())
    }

    fn recorded(method: &str) -> Vec<Value> {
        transport()
            .calls
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .filter(|(recorded, _)| recorded == method)
            .map(|(_, args)| args.clone())
            .collect()
    }

    fn install_with(collapse_key: &str) -> Value {
        transport()
            .calls
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
        let calls = RpiHostCalls {
            call: scripted_call,
        };
        install_with_key(calls, 0xfeed as PluginCookie, collapse_key.to_owned())
    }

    #[test]
    fn install_registers_the_command_and_default_shortcut() {
        let _guard = serialized();
        __reset_state();
        let receipt = install_with("ctrl+shift+t");
        assert_eq!(receipt, json!({"ok": true}));
        let commands = recorded("registerCommand");
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0]["name"], json!("todos"));
        assert!(commands[0]["description"]
            .as_str()
            .is_some_and(|d| d.contains("todos")));
        let shortcuts = recorded("registerShortcut");
        assert_eq!(shortcuts.len(), 1);
        assert_eq!(shortcuts[0]["shortcut"], json!("ctrl+shift+t"));
        assert!(shortcuts[0]["description"]
            .as_str()
            .is_some_and(|d| d.contains("Collapse")));
    }

    #[test]
    fn install_registers_a_configured_key_instead_of_the_default() {
        let _guard = serialized();
        __reset_state();
        install_with("alt+o");
        let shortcuts = recorded("registerShortcut");
        assert_eq!(shortcuts.len(), 1);
        assert_eq!(shortcuts[0]["shortcut"], json!("alt+o"));
    }

    #[test]
    fn install_skips_the_shortcut_entirely_for_the_off_sentinel() {
        let _guard = serialized();
        __reset_state();
        install_with("off");
        assert!(recorded("registerShortcut").is_empty());
    }

    #[test]
    fn install_falls_back_to_the_default_key_for_an_invalid_spec() {
        let _guard = serialized();
        __reset_state();
        // The resolver runs at factory scope in production; the seam takes
        // the RESOLVED key, so an invalid spec reaches install as the
        // default (config.test.rs pins the resolver matrix).
        install_with(crate::config::DEFAULT_COLLAPSE_KEY);
        let shortcuts = recorded("registerShortcut");
        assert_eq!(shortcuts[0]["shortcut"], json!("ctrl+shift+t"));
    }

    #[test]
    fn install_registers_the_tool_with_render_flags() {
        let _guard = serialized();
        __reset_state();
        install_with("ctrl+shift+t");
        let tools = recorded("registerTool");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["renderCall"], json!(true));
        assert_eq!(tools[0]["renderResult"], json!(true));
    }

    // ------------------------------------------------------------------
    // Dispatch routing (P1: command / shortcut / render)
    // ------------------------------------------------------------------

    #[test]
    fn dispatch_routes_the_todos_command() {
        let _guard = serialized();
        __reset_state();
        set_test_locale("en");
        install_with("ctrl+shift+t");
        transport().set_reply(
            "ctx.sessionFile",
            json!({"path": null, "id": "dispatch-sid"}),
        );
        transport().set_reply("ctx.hasUI", json!(true));
        // Seed through the tool path against the scripted session.
        let reply = dispatch_for_test(
            0xfeed as PluginCookie,
            &json!({"kind": "toolExecute", "toolName": "todo", "params": {"action": "create", "subject": "routed"}}),
        );
        assert_eq!(
            reply["content"][0]["text"],
            json!("Created #1: routed (pending)")
        );
        dispatch_for_test(
            0xfeed as PluginCookie,
            &json!({"kind": "command", "name": "todos", "args": ""}),
        );
        let notifies = recorded("ui.notify");
        assert_eq!(notifies.len(), 1);
        assert_eq!(notifies[0]["notifyType"], json!("info"));
        let message = notifies[0]["message"].as_str().unwrap_or_default();
        assert!(message.contains("── Pending ──"), "{message}");
        assert!(message.contains("○ #1 routed"), "{message}");
    }

    #[test]
    fn dispatch_routes_the_collapse_shortcut() {
        let _guard = serialized();
        __reset_state();
        set_test_locale("en");
        install_with("ctrl+shift+t");
        transport().set_reply(
            "ctx.sessionFile",
            json!({"path": null, "id": "dispatch-sid"}),
        );
        transport().set_reply("ctx.hasUI", json!(true));
        transport().set_reply("ui.getToolsExpanded", json!(false));
        // session_start claims the foreground, a tool call registers the
        // widget, then the shortcut toggles collapse (forced re-send).
        dispatch_for_test(
            0xfeed as PluginCookie,
            &json!({"kind": "event", "event": "session_start", "payload": {}}),
        );
        dispatch_for_test(
            0xfeed as PluginCookie,
            &json!({"kind": "toolExecute", "toolName": "todo", "params": {"action": "create", "subject": "a"}}),
        );
        dispatch_for_test(
            0xfeed as PluginCookie,
            &json!({"kind": "event", "event": "tool_execution_end", "payload": {"toolName": "todo", "isError": false}}),
        );
        let widgets_before = recorded("ui.setWidget").len();
        assert!(widgets_before >= 1, "widget registered");
        dispatch_for_test(
            0xfeed as PluginCookie,
            &json!({"kind": "shortcut", "shortcut": "ctrl+shift+t"}),
        );
        let widgets = recorded("ui.setWidget");
        assert!(widgets.len() > widgets_before, "toggle forces a re-send");
        let last = widgets.last().cloned().unwrap_or_default();
        let lines: Vec<&str> = last["content"]
            .as_array()
            .map(|items| items.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        assert!(
            lines.len() == 3 && lines[1].contains("to expand"),
            "{lines:?}"
        );
        // A foreign shortcut key is ignored (registered key only).
        let before = recorded("ui.setWidget").len();
        dispatch_for_test(
            0xfeed as PluginCookie,
            &json!({"kind": "shortcut", "shortcut": "ctrl+x"}),
        );
        assert_eq!(recorded("ui.setWidget").len(), before);
    }

    #[test]
    fn dispatch_routes_the_render_calls() {
        let _guard = serialized();
        __reset_state();
        set_test_locale("en");
        install_with("ctrl+shift+t");
        transport().set_reply("ctx.sessionFile", json!({"path": null, "id": "s"}));
        transport().set_reply("ctx.hasUI", json!(true));
        // Claim the foreground FIRST (session_start replays the branch —
        // an empty branch would wipe a pre-seeded slot), then seed the
        // task through the tool path.
        dispatch_for_test(
            0xfeed as PluginCookie,
            &json!({"kind": "event", "event": "session_start", "payload": {}}),
        );
        dispatch_for_test(
            0xfeed as PluginCookie,
            &json!({"kind": "toolExecute", "toolName": "todo", "params": {"action": "create", "subject": "render-me"}}),
        );
        let tree = dispatch_for_test(
            0xfeed as PluginCookie,
            &json!({
                "kind": "render", "what": "toolCall", "toolName": "todo",
                "context": {"args": {"action": "update", "id": 1}, "toolCallId": "tc", "cwd": "/", "executionStarted": true, "argsComplete": true, "isPartial": false, "expanded": false, "showImages": false, "isError": false}
            }),
        );
        let text = tree["props"]["text"].as_str().unwrap_or_default();
        assert!(text.contains("todo "), "{text}");
        assert!(text.contains("→"), "{text}");
        assert!(text.contains("render-me"), "{text}");

        let tree = dispatch_for_test(
            0xfeed as PluginCookie,
            &json!({
                "kind": "render", "what": "toolResult", "toolName": "todo",
                "result": {"content": [], "details": {"action": "create", "params": {}, "tasks": [{"id": 1, "subject": "render-me", "status": "pending"}], "nextId": 2}},
                "options": {"expanded": false, "isPartial": false},
                "context": {"args": {}, "toolCallId": "tc", "cwd": "/", "executionStarted": true, "argsComplete": true, "isPartial": false, "expanded": false, "showImages": false, "isError": false}
            }),
        );
        let text = tree["props"]["text"].as_str().unwrap_or_default();
        assert!(text.contains("○"), "{text}");
        assert!(text.contains("pending"), "{text}");
    }

    #[test]
    fn dispatch_routes_tool_execute_to_the_tool() {
        let _guard = serialized();
        __reset_state();
        let calls = RpiHostCalls { call: fake_call };
        let cookie = 0x1234 as PluginCookie;
        let receipt = install_for_test(calls, cookie);
        assert_eq!(receipt, json!({"ok": true}));
        // install used the real NativeHostCall for registrations; swap in
        // the mock by dispatching through handle-level execute directly
        // (the fake trampoline cannot answer ctx calls).
        let host = MockHost::new("dispatch-sid");
        let result = crate::tool::execute(&host, &json!({"action": "list"}));
        assert_eq!(result["content"][0]["text"], json!("No tasks"));
    }
}
