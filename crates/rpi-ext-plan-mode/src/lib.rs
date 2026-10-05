//! `rpi-ext-plan-mode` — L0 native plugin (TE43): Claude-Code-style plan
//! mode for rpi.
//!
//! Plan mode is a read-only research state. Entering it (via `/plan` or
//! the host `app.mode.cycle` keybinding, Shift+Tab by default) snapshots
//! the active tool set, hides every non-whitelist tool through the
//! V16-14 session exposure override layer, and activates the whitelist
//! (read tools + `write_plan`). The model researches, writes a structured
//! plan with `write_plan`, and the resulting review dialog approves the
//! plan (leaving Plan mode and injecting the summary for execution),
//! requests revisions (routed back through the tool result), or abandons
//! it.
//!
//! The host permission mode is the single authority (V16-05); this plugin
//! only mirrors it into the boundary. Every mode-relevant event funnels
//! into `mode::reconcile`, which re-derives the boundary from the live
//! `getAllTools` surface, so tools registered mid-plan (codemode, MCP,
//! subagents) are re-tightened before the next request.
//!
//! Native runtime model (rpi-todo precedent): `rpi_extension_init`
//! registers through the host-call handle and records the channel under
//! the init cookie; `rpi_dispatch` answers with the same cookie's
//! channel, so `ctx.*` calls resolve against the dispatching host's bound
//! session. Plan state is keyed by session id and survives an extension
//! reload (the native library stays mapped).
//!
//! Docs: `rpi-docs/extensions/rpi-plan-mode/{01,02}.md` and the TE43 task
//! record under `rpi-docs/plan/extensions/TE43-plan-mode.md`.

pub mod config;
pub mod host;
pub mod i18n;
pub mod mode;
pub mod plan_file;
pub mod prompt;
pub mod review;
pub mod tool;
pub mod view;
pub mod whitelist;

#[cfg(test)]
mod test_host;

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use abi_stable::prefix_type::PrefixTypeTrait;
use abi_stable::std_types::RVec;
use rpi_ext_host::native::{PluginCookie, RpiHostCalls, RpiNativeModule, RpiNativeModule_Ref};
use serde_json::{Value, json};

use crate::host::get_mode;

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

/// Guest → host JSON call surface (`{"call": method, "args": args}` →
/// `{"ok": ...} | {"error": {"kind", "message"}}`). Abstracted so the
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
// Channel registry (cookie → host channel)
// ---------------------------------------------------------------------------

/// Channels by init cookie. Each `load_native_plugin` run gets its own
/// cookie; reloads allocate fresh cookies, so stale entries are inert and
/// bounded by the reload count (rpi-todo precedent).
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

fn notify(host: &dyn HostCall, message: &str) {
    if let Err(error) = host.call(
        "ui.notify",
        json!({ "message": message, "notifyType": "info" }),
    ) {
        tracing::debug!(%error, "rpi-plan-mode: ui.notify rejected");
    }
}

/// Install: register the `write_plan` tool and the `/plan` command,
/// subscribe the mode/lifecycle events, and record the channel. Idempotent
/// across reloads (fresh cookie per load).
fn install(calls: RpiHostCalls, cookie: PluginCookie) -> Value {
    let host = NativeHostCall::new(calls, cookie);
    let result = install_with_host(&host);
    if result.get("ok").is_some() {
        store_channel(cookie, host);
    }
    result
}

/// The registration body, split from the ABI transport so it is testable
/// against a [`HostCall`] fake.
fn install_with_host(host: &dyn HostCall) -> Value {
    if let Err(error) = host.call("registerTool", tool::tool_definition()) {
        return error_envelope("init", error);
    }
    if let Err(error) = host.call("registerCommand", tool::command_definition()) {
        return error_envelope("init", error);
    }
    for event in [
        "mode_change",
        "session_start",
        "session_tree",
        "before_agent_start",
        "mcp_servers_change",
    ] {
        if let Err(error) = host.call("on", json!({ "event": event })) {
            return error_envelope("init", error);
        }
    }
    tracing::info!("rpi-plan-mode installed");
    json!({"ok": true})
}

/// `/plan` command handler: toggle / status / file / edit.
fn handle_plan_command(host: &dyn HostCall, args: &str) {
    match args.trim() {
        "" => toggle_plan_mode(host),
        "status" => {
            let mode = get_mode(host);
            let path = mode::current_plan_path(host).unwrap_or_else(|| "(unavailable)".to_owned());
            notify(host, &format!("plan mode: {mode} · plan file: {path}"));
        }
        "file" => {
            let path = mode::current_plan_path(host).unwrap_or_else(|| "(unavailable)".to_owned());
            notify(host, &path);
        }
        "edit" => edit_plan_file(host),
        _ => notify(host, i18n::COMMAND_USAGE),
    }
}

fn toggle_plan_mode(host: &dyn HostCall) {
    let current = get_mode(host);
    let target = if current == "plan" { "default" } else { "plan" };
    if let Err(error) = host.call("setMode", json!({ "mode": target })) {
        tracing::warn!(%error, "rpi-plan-mode: setMode failed");
        return;
    }
    mode::reconcile(host);
    if target == "plan" && get_mode(host) != "plan" {
        notify(host, i18n::NON_INTERACTIVE_NOTE);
    }
}

/// `/plan edit`: external-editor round trip (R-U11), with the
/// `ui.editor` fallback when `ui.editExternal` is unavailable.
fn edit_plan_file(host: &dyn HostCall) {
    if !host::has_ui(host) {
        notify(host, i18n::NON_INTERACTIVE_NOTE);
        return;
    }
    let Some(path) = mode::existing_plan_path(host) else {
        notify(host, "no plan file yet — write one with write_plan first");
        return;
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        notify(host, "the plan file could not be read");
        return;
    };
    let path_text = path.to_string_lossy().into_owned();
    let edited = match host.call("ui.editExternal", json!({ "text": text })) {
        Ok(value) => value.get("text").and_then(Value::as_str).map(str::to_owned),
        Err(error) => {
            tracing::debug!(%error, "rpi-plan-mode: ui.editExternal unavailable; falling back");
            match host.call(
                "ui.editor",
                json!({ "title": i18n::EDIT_PLAN_TITLE, "prefill": text }),
            ) {
                Ok(Value::String(edited)) => Some(edited),
                _ => None,
            }
        }
    };
    let Some(edited) = edited else {
        return;
    };
    if edited == text {
        return;
    }
    match std::fs::write(&path, edited) {
        Ok(()) => notify(host, &format!("plan file updated: {path_text}")),
        Err(error) => notify(host, &format!("plan file write failed: {error}")),
    }
}

/// One event dispatch. `before_agent_start` returns the prompt-options
/// payload; every other subscribed event returns `null`.
fn handle_event(host: &dyn HostCall, event: &str, payload: &Value) -> Value {
    match event {
        "before_agent_start" => prompt::handle_before_agent_start(host, payload),
        "session_start" => {
            mode::on_session_start(host);
            Value::Null
        }
        "mode_change" | "session_tree" | "mcp_servers_change" => {
            mode::reconcile(host);
            Value::Null
        }
        _ => Value::Null,
    }
}

/// Dispatch one host → plugin message against a host surface (the
/// cookie lookup wrapper is [`dispatch_message`]).
fn dispatch_with_host(host: &dyn HostCall, message: &Value) -> Value {
    match message.get("kind").and_then(Value::as_str) {
        Some("toolExecute")
            if message.get("toolName").and_then(Value::as_str) == Some(tool::TOOL_NAME) =>
        {
            let params = message.get("params").cloned().unwrap_or(Value::Null);
            tool::execute(host, &params)
        }
        Some("command")
            if message.get("name").and_then(Value::as_str) == Some(tool::COMMAND_NAME) =>
        {
            let args = message.get("args").and_then(Value::as_str).unwrap_or("");
            handle_plan_command(host, args);
            Value::Null
        }
        Some("render")
            if message.get("toolName").and_then(Value::as_str) == Some(tool::TOOL_NAME) =>
        {
            match message.get("what").and_then(Value::as_str) {
                Some("toolCall") => tool::render_call_dispatch(message),
                Some("toolResult") => tool::render_result_dispatch(message),
                _ => Value::Null,
            }
        }
        Some("event") => {
            let event = message.get("event").and_then(Value::as_str).unwrap_or("");
            let payload = message.get("payload").cloned().unwrap_or(Value::Null);
            handle_event(host, event, &payload)
        }
        _ => Value::Null,
    }
}

/// Dispatch one host → plugin message (resolved through the cookie's
/// channel).
fn dispatch_message(cookie: PluginCookie, message: &Value) -> Value {
    match channel_for(cookie) {
        Some(host) => dispatch_with_host(&host, message),
        None => Value::Null,
    }
}

// ---------------------------------------------------------------------------
// abi_stable entry points
// ---------------------------------------------------------------------------

fn pack(value: &Value) -> RVec<u8> {
    RVec::from(serde_json::to_vec(value).unwrap_or_else(|_| b"null".to_vec()))
}

/// Load entry (abi_stable). A panic must cross the ABI as an error
/// envelope, not unwind into the host (rpi-todo precedent).
#[allow(clippy::missing_safety_doc)]
pub extern "C" fn init(calls: RpiHostCalls, cookie: PluginCookie) -> RVec<u8> {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| install(calls, cookie)))
        .unwrap_or_else(|panic| {
            error_envelope(
                "internal",
                format!("rpi-plan-mode init panicked: {panic:?}"),
            )
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
                "text": "rpi-plan-mode panicked while handling a dispatch",
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
/// boundary.
#[doc(hidden)]
pub fn install_for_test(calls: RpiHostCalls, cookie: PluginCookie) -> Value {
    install(calls, cookie)
}

/// Test seam: dispatch a message against a host surface directly.
#[doc(hidden)]
pub fn dispatch_for_test(host: &dyn HostCall, message: &Value) -> Value {
    dispatch_with_host(host, message)
}

/// Test seam: install against a host surface directly.
#[doc(hidden)]
pub fn install_host_for_test(host: &dyn HostCall) -> Value {
    install_with_host(host)
}

/// Shared serializing lock for every test that touches process-global
/// state (channels, plan-mode state, config override, env).
#[cfg(test)]
pub(crate) static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Test seam: reset the process-global plugin state.
#[doc(hidden)]
pub fn __reset_state() {
    channels()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clear();
    #[cfg(test)]
    mode::reset_for_test();
    #[cfg(test)]
    config::set_test_config(Some(None));
}

#[cfg(test)]
mod tests {
    //! Wiring-level tests over the fake host: registration, dispatch
    //! routing, and the command surface.

    use super::*;
    use crate::test_host::{FakeHost, fake_reply};

    fn install_host() -> FakeHost {
        let host = FakeHost::new();
        host.set("registerTool", fake_reply(json!(null)));
        host.set("registerCommand", fake_reply(json!(null)));
        host.set("on", fake_reply(json!(null)));
        assert_eq!(install_with_host(&host), json!({"ok": true}));
        host
    }

    #[test]
    fn install_registers_the_tool_command_and_five_events() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        __reset_state();
        let host = FakeHost::new();
        host.set("registerTool", fake_reply(json!(null)));
        host.set("registerCommand", fake_reply(json!(null)));
        host.set("on", fake_reply(json!(null)));
        let value = install_with_host(&host);
        assert_eq!(value, json!({"ok": true}));
        let methods = host.methods();
        assert_eq!(
            methods,
            vec![
                "registerTool",
                "registerCommand",
                "on",
                "on",
                "on",
                "on",
                "on",
            ]
        );
        let events: Vec<String> = host
            .recorded()
            .into_iter()
            .filter(|(method, _)| method == "on")
            .filter_map(|(_, args)| args["event"].as_str().map(str::to_owned))
            .collect();
        assert_eq!(
            events,
            vec![
                "mode_change",
                "session_start",
                "session_tree",
                "before_agent_start",
                "mcp_servers_change",
            ]
        );
    }

    #[test]
    fn install_reports_registration_failures() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        __reset_state();
        let host = FakeHost::new();
        host.push(
            "registerTool",
            Err(HostError {
                kind: "invalidRequest".to_owned(),
                message: "bad schema".to_owned(),
            }),
        );
        let value = install_with_host(&host);
        assert_eq!(value["error"]["kind"], "init");
        assert!(
            value["error"]["message"]
                .as_str()
                .unwrap_or("")
                .contains("bad schema")
        );
    }

    #[test]
    fn dispatch_routes_the_plan_command() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        __reset_state();
        let host = install_host();
        host.set("getMode", fake_reply(json!("default")));
        host.set(
            "ctx.sessionFile",
            fake_reply(json!({"path": null, "id": "s-1"})),
        );
        host.set("ctx.cwd", fake_reply(json!("/work")));
        host.set("ui.notify", fake_reply(json!(null)));
        let value = dispatch_with_host(
            &host,
            &json!({"kind": "command", "name": "plan", "args": "status"}),
        );
        assert_eq!(value, Value::Null);
        assert!(host.called("ui.notify"));
        let message = host.args_of("ui.notify").expect("notify args");
        assert!(
            message["message"]
                .as_str()
                .unwrap_or("")
                .contains("plan mode: default"),
            "{message}"
        );
    }

    #[test]
    fn dispatch_routes_write_plan_tool_execution() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        __reset_state();
        let host = install_host();
        host.set("getMode", fake_reply(json!("plan")));
        host.set(
            "ctx.sessionFile",
            fake_reply(json!({"path": null, "id": "s-1"})),
        );
        host.set("ctx.cwd", fake_reply(json!(null)));
        let value = dispatch_with_host(
            &host,
            &json!({"kind": "toolExecute", "toolName": "write_plan", "params": {}}),
        );
        assert_eq!(value["isError"], true, "missing content: {value}");
    }

    #[test]
    fn dispatch_for_an_unknown_cookie_is_a_noop() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        __reset_state();
        let value = dispatch_message(
            std::ptr::null(),
            &json!({"kind": "command", "name": "plan"}),
        );
        assert_eq!(value, Value::Null);
    }

    #[test]
    fn event_dispatch_syncs_the_default_boundary() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        __reset_state();
        let host = install_host();
        host.set(
            "ctx.sessionFile",
            fake_reply(json!({"path": null, "id": "s-1"})),
        );
        host.set("getMode", fake_reply(json!("default")));
        let value = dispatch_with_host(
            &host,
            &json!({"kind": "event", "event": "mode_change", "payload": {"from": "plan", "to": "default"}}),
        );
        assert_eq!(value, Value::Null);
        let methods = host.methods();
        let tail = &methods[methods.len() - 2..];
        assert_eq!(tail, ["ctx.sessionFile", "getMode"]);
    }

    #[test]
    fn non_interactive_toggle_reports_the_note() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        __reset_state();
        let host = install_host();
        host.set("getMode", fake_reply(json!("default")));
        host.set("setMode", fake_reply(json!(null)));
        host.set(
            "ctx.sessionFile",
            fake_reply(json!({"path": null, "id": "s-1"})),
        );
        host.set("ui.notify", fake_reply(json!(null)));
        let value = dispatch_with_host(
            &host,
            &json!({"kind": "command", "name": "plan", "args": ""}),
        );
        assert_eq!(value, Value::Null);
        assert!(host.called("ui.notify"), "{:?}", host.methods());
    }
}
#[cfg(test)]
mod command_tests {
    //! `/plan edit` (FR-F): external-editor round trip + `ui.editor`
    //! fallback.

    use super::*;
    use crate::TEST_LOCK;
    use crate::test_host::{SessionFakeHost, TestDir};

    fn serialized() -> std::sync::MutexGuard<'static, ()> {
        TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn host_with_plan_file(dir: &TestDir) -> (SessionFakeHost, std::path::PathBuf) {
        let host = SessionFakeHost::new();
        let path = dir.path().join("s-1-1.md");
        std::fs::write(&path, "old plan").expect("write plan file");
        mode::remember_plan_path(&host, path.clone());
        (host, path)
    }

    #[test]
    fn edit_round_trips_through_edit_external() {
        let _guard = serialized();
        __reset_state();
        let dir = TestDir::new("edit");
        let (host, path) = host_with_plan_file(&dir);
        host.queue_edit_external(json!("new plan text"));
        assert_eq!(
            dispatch_with_host(
                &host,
                &json!({"kind": "command", "name": "plan", "args": "edit"}),
            ),
            Value::Null
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "new plan text"
        );
        assert!(
            host.view()
                .notifications
                .iter()
                .any(|note| note.contains("plan file updated")),
            "{:?}",
            host.view().notifications
        );
    }

    #[test]
    fn edit_falls_back_to_the_editor_dialog() {
        let _guard = serialized();
        __reset_state();
        let dir = TestDir::new("edit-fallback");
        let (host, path) = host_with_plan_file(&dir);
        host.fail_edit_external();
        host.queue_edit_external(json!("fallback text"));
        dispatch_with_host(
            &host,
            &json!({"kind": "command", "name": "plan", "args": "edit"}),
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "fallback text"
        );
        assert!(
            host.methods().iter().any(|method| method == "ui.editor"),
            "the editor fallback ran: {:?}",
            host.methods()
        );
    }

    #[test]
    fn edit_without_a_plan_file_notes_and_stops() {
        let _guard = serialized();
        __reset_state();
        let dir = TestDir::new("edit-none");
        let host = SessionFakeHost::new();
        let _ = dir;
        dispatch_with_host(
            &host,
            &json!({"kind": "command", "name": "plan", "args": "edit"}),
        );
        assert!(
            !host
                .methods()
                .iter()
                .any(|method| method == "ui.editExternal"),
            "no editor call without a file"
        );
        assert!(
            host.view()
                .notifications
                .iter()
                .any(|note| note.contains("no plan file yet")),
            "{:?}",
            host.view().notifications
        );
    }
}
