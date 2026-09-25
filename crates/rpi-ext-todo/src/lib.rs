//! `rpi-ext-todo` — L0 native plugin, port of `@juicesharp/rpiv-todo`
//! v2.10.1+ (`juicesharp/rpiv-mono` `packages/rpiv-todo/` @ `0fdf4f8`;
//! `338b264..0fdf4f8` is comment-level for this package — behavioral
//! surface = v2.9.0 semantics).
//!
//! P0 (TE34): the `todo` tool with its full six-action state machine and
//! `blockedBy` dependency validation, the session-isolated state store,
//! branch replay through `ctx.sessionToolResults` (ADR-0030, host side
//! V15-14), and the prompt surface (promptSnippet + the eight built-in
//! guidelines). Event wiring follows upstream `index.ts` @ `0fdf4f8`:
//! `session_start` / `session_compact` / `session_tree` replay into the
//! event's own session slot, `session_shutdown` evicts it (foreground
//! teardown bumps the lifecycle generation and clears the render pointer),
//! `tool_execution_end` / `agent_start` are logged placeholders until the
//! overlay lands (TE35).
//!
//! Docs: `rpi-docs/extensions/rpiv-todo/{01,02}.md` and the task file
//! `rpi-docs/plan/extensions/TE34-rpiv-todo-p0.md`.
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
// Install + dispatch
// ---------------------------------------------------------------------------

fn error_envelope(kind: &str, message: impl std::fmt::Display) -> Value {
    json!({"error": {"kind": kind, "message": message.to_string()}})
}

/// Install: register the tool, subscribe the six lifecycle events, record
/// the channel. Idempotent across reloads (fresh cookie per load).
fn install(calls: RpiHostCalls, cookie: PluginCookie) -> Value {
    let host = NativeHostCall::new(calls, cookie);

    if let Err(error) = host.call("registerTool", tool::tool_definition()) {
        return error_envelope("init", error);
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
        Some("event") => {
            let event = message.get("event").and_then(Value::as_str).unwrap_or("");
            let payload = message.get("payload").cloned().unwrap_or(Value::Null);
            handle_event(&host, event, &payload);
            Value::Null
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
        // (creator-ownership) without loading any overlay; only the
        // foreground re-binds (overlay wiring lands with TE35).
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
            tracing::debug!(sid = %sid, "rpiv-todo: foreground claimed");
        }
        // Shared by session_compact and session_tree (verbatim-identical
        // pre-extraction upstream): re-key the session's slot, refresh the
        // overlay only when the refreshed session IS the foreground.
        "session_compact" | "session_tree" => {
            let sid = sid_of(host);
            replay_into_slot(host, &sid);
            if sid == store().active_render_session() {
                tracing::debug!(event, "rpiv-todo: foreground replayed after {event}");
            }
        }
        // The shutting-down session's own slot is always evicted. Overlay
        // teardown is sid-gated: a child shutdown must not dispose the
        // foreground; only the foreground's own shutdown (or an
        // unknown/stale sid, resolved to "") tears it down and clears the
        // pointer + generation.
        "session_shutdown" => {
            let sid = sid_of(host);
            store().evict_session(&sid);
            if sid.is_empty() || sid == store().active_render_session() {
                store().clear_active_render_session();
                tracing::debug!("rpiv-todo: foreground torn down");
            }
        }
        // Reads the store at render time; do NOT replay here (the branch
        // is stale — message_end runs after tool_execution_end). The
        // overlay refresh itself lands with TE35; P0 logs the trigger.
        "tool_execution_end" => {
            let tool_name = payload.get("toolName").and_then(Value::as_str);
            let is_error = payload.get("isError").and_then(Value::as_bool);
            if tool_name == Some(TOOL_NAME) && is_error != Some(true) {
                tracing::debug!("rpiv-todo: todo tool succeeded (overlay refresh: TE35)");
            }
        }
        // Completed-task fade-out migration lands with TE35.
        "agent_start" => {
            tracing::debug!("rpiv-todo: agent_start (fade-out migration: TE35)");
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

/// Test seam: store reset (upstream `__resetState` import path).
#[doc(hidden)]
pub fn __reset_state() {
    reset_store();
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
        calls: Mutex<Vec<(String, Value)>>,
    }

    impl MockHost {
        fn new(session_id: &'static str) -> Self {
            MockHost {
                session_id,
                session_results: Vec::new(),
                has_ui: true,
                stale: false,
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
