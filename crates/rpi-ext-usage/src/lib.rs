//! `rpi-ext-usage` — L0 native plugin (TE44): `/usage` plus a footer usage
//! line, built on the host's scripted usage-provider framework (V16-05).
//!
//! `/usage` resolves the current model provider (`ctx.model`), maps it to a
//! registered usage provider, and asks the host framework for the latest
//! envelope: `/usage` (current provider), `/usage <provider>`, `/usage all`.
//! The footer status line (`ui.setStatus("rpi-usage", displayText)`) follows
//! `session_start` / `model_select` and is refreshed on `message_end` under
//! the `usage.refreshMs` throttle. All script execution, credential
//! injection, caching, and failure retention live in the host; this plugin
//! ships the four provider scripts (embedded and materialized at install),
//! the command/formatting surface, and the refresh orchestration.
//!
//! Native runtime model (rpi-todo / rpi-plan-mode precedent):
//! `rpi_extension_init` registers through the host-call handle, records the
//! channel under the init cookie, and starts a per-cookie refresh worker so
//! dispatch never blocks on a script run.
//!
//! Docs: `rpi-docs/extensions/rpi-usage/{01,02}.md` and the TE44 task record
//! under `rpi-docs/plan/extensions/TE44-usage.md`.

pub mod command;
pub mod config;
pub mod footer;
pub mod format;
pub mod host;
pub mod providers;

#[cfg(test)]
mod test_host;

use std::collections::HashMap;
use std::sync::mpsc::Sender;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use abi_stable::prefix_type::PrefixTypeTrait;
use abi_stable::std_types::RVec;
use rpi_ext_host::native::{PluginCookie, RpiHostCalls, RpiNativeModule, RpiNativeModule_Ref};
use serde_json::{Value, json};

use crate::footer::{FooterState, Job};

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
/// `{"ok": ...} | {"error": {"kind", "message"}}`). Abstracted so the logic
/// is testable without the abi_stable boundary.
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
/// bounded by the reload count (rpi-todo / rpi-plan-mode precedent).
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

/// Install: register `/usage`, subscribe the lifecycle events, register the
/// built-in provider scripts, and start the refresh worker. Idempotent
/// across reloads (fresh cookie per load).
fn install(calls: RpiHostCalls, cookie: PluginCookie) -> Value {
    let host = NativeHostCall::new(calls, cookie);
    let result = install_with_host(&host);
    if result.get("ok").is_some() {
        store_channel(cookie, host);
        footer::start_worker(cookie, host);
    }
    result
}

/// The registration body, split from the ABI transport so it is testable
/// against a [`HostCall`] fake.
fn install_with_host(host: &dyn HostCall) -> Value {
    if let Err(error) = host.call("registerCommand", command::command_definition()) {
        return error_envelope("init", error);
    }
    for event in [
        "session_start",
        "model_select",
        "message_end",
        "session_shutdown",
    ] {
        if let Err(error) = host.call("on", json!({ "event": event })) {
            return error_envelope("init", error);
        }
    }
    // Fail-soft: a read-only agent dir leaves the framework intact and
    // `/usage` still explains how to configure a script.
    let agent_dir = config::agent_dir();
    match providers::register_builtin(host, &agent_dir) {
        Ok(count) => tracing::info!(count, "rpi-usage installed"),
        Err(error) => tracing::warn!(%error, "rpi-usage: built-in scripts unavailable"),
    }
    json!({"ok": true})
}

/// Run one job synchronously (the no-worker fallback; production dispatch
/// goes through the worker channel).
fn run_job_sync(host: &dyn HostCall, job: Job) {
    let config = config::load();
    let mut state = FooterState::default();
    let now = Instant::now();
    match job {
        Job::Refresh { force, throttled } => {
            footer::refresh(host, &config, &mut state, force, throttled, now);
        }
        Job::Command { args } => command::handle_now(host, &config, &mut state, &args, now),
        Job::Shutdown => {}
    }
}

/// Queue a job (or run it synchronously when no worker is available).
fn queue_job(host: &dyn HostCall, jobs: Option<&Sender<Job>>, job: Job) {
    match jobs {
        Some(sender) => {
            if sender.send(job).is_err() {
                tracing::debug!("rpi-usage: refresh worker is gone; job dropped");
            }
        }
        None => run_job_sync(host, job),
    }
}

/// One event dispatch. Every subscribed event is a refresh trigger;
/// `session_shutdown` stops the worker.
fn handle_event(host: &dyn HostCall, jobs: Option<&Sender<Job>>, event: &str) {
    let job = match event {
        "session_start" | "model_select" => Job::Refresh {
            force: false,
            throttled: false,
        },
        "message_end" => Job::Refresh {
            force: false,
            throttled: true,
        },
        "session_shutdown" => Job::Shutdown,
        _ => return,
    };
    queue_job(host, jobs, job);
}

/// Dispatch one host → plugin message against a host surface (the cookie
/// lookup wrapper is [`dispatch_message`]).
fn dispatch_with_host(host: &dyn HostCall, jobs: Option<&Sender<Job>>, message: &Value) -> Value {
    match message.get("kind").and_then(Value::as_str) {
        Some("command")
            if message.get("name").and_then(Value::as_str) == Some(command::COMMAND_NAME) =>
        {
            let args = message
                .get("args")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            queue_job(host, jobs, Job::Command { args });
            Value::Null
        }
        Some("event") => {
            let event = message.get("event").and_then(Value::as_str).unwrap_or("");
            handle_event(host, jobs, event);
            Value::Null
        }
        _ => Value::Null,
    }
}

/// Whether an event begins a new session and therefore needs a live
/// refresh worker (v0.1.6 review P1-6).
fn event_starts_session(message: &Value) -> bool {
    message.get("kind").and_then(Value::as_str) == Some("event")
        && message.get("event").and_then(Value::as_str) == Some("session_start")
}

/// Dispatch one host → plugin message (resolved through the cookie's
/// channel and worker).
fn dispatch_message(cookie: PluginCookie, message: &Value) -> Value {
    match channel_for(cookie) {
        Some(host) => {
            // `/new` and `/resume` reuse the loaded plugin but emit
            // `session_shutdown` first, which stops the previous worker;
            // start a fresh one so the new session's refresh stays async
            // and throttled (P1-6).
            if event_starts_session(message) {
                footer::start_worker(cookie, host);
            }
            let jobs = footer::worker_for(cookie);
            dispatch_with_host(&host, jobs.as_ref(), message)
        }
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
/// envelope, not unwind into the host (rpi-todo / rpi-plan-mode precedent).
#[allow(clippy::missing_safety_doc)]
pub extern "C" fn init(calls: RpiHostCalls, cookie: PluginCookie) -> RVec<u8> {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| install(calls, cookie)))
        .unwrap_or_else(|panic| {
            error_envelope("internal", format!("rpi-usage init panicked: {panic:?}"))
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
                "text": "rpi-usage panicked while handling a dispatch",
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
pub fn dispatch_for_test(
    host: &dyn HostCall,
    jobs: Option<&Sender<Job>>,
    message: &Value,
) -> Value {
    dispatch_with_host(host, jobs, message)
}

/// Test seam: install against a host surface directly.
#[doc(hidden)]
pub fn install_host_for_test(host: &dyn HostCall) -> Value {
    install_with_host(host)
}

/// Shared serializing lock for every test that touches process-global
/// state (channels, workers, the pinned agent dir).
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
    {
        footer::clear_workers_for_test();
        config::set_test_agent_dir(None);
    }
}

#[cfg(test)]
mod tests {
    //! Wiring-level tests over the fake host: registration, dispatch
    //! routing, and job selection.

    use super::*;
    use crate::test_host::{TestDir, UsageFakeHost};

    fn install_host(agent_dir: &std::path::Path) -> UsageFakeHost {
        config::set_test_agent_dir(Some(agent_dir.to_path_buf()));
        let host = UsageFakeHost::new();
        assert_eq!(install_with_host(&host), json!({"ok": true}));
        host
    }

    #[test]
    fn install_registers_the_command_events_and_four_providers() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        __reset_state();
        let dir = TestDir::new("install");
        let host = install_host(dir.path());
        assert_eq!(
            host.calls()
                .into_iter()
                .map(|(method, _)| method)
                .collect::<Vec<_>>(),
            vec![
                "registerCommand",
                "on",
                "on",
                "on",
                "on",
                "ctx.usage.register",
                "ctx.usage.register",
                "ctx.usage.register",
                "ctx.usage.register",
            ]
        );
        let command = host
            .calls()
            .into_iter()
            .find(|(method, _)| method == "registerCommand")
            .map(|(_, args)| args)
            .expect("command args");
        assert_eq!(command["name"], "usage");
        let registered = host.registrations();
        let providers: Vec<&str> = registered.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            providers,
            vec![
                "deepseek",
                "glm-coding-plan",
                "minimax-token-plan",
                "kimi-code"
            ]
        );
        for (_provider, path) in registered {
            assert!(
                std::path::Path::new(&path).is_file(),
                "materialized script exists: {path}"
            );
        }
    }

    #[test]
    fn install_reports_registration_failures() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        __reset_state();
        let dir = TestDir::new("install-fail");
        config::set_test_agent_dir(Some(dir.path().to_path_buf()));
        let host = UsageFakeHost::new();
        host.set_method_error("registerCommand", "bad schema");
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
    fn dispatch_routes_commands_and_events_to_the_worker() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        __reset_state();
        let host = UsageFakeHost::new();
        let (tx, rx) = std::sync::mpsc::channel();
        dispatch_for_test(
            &host,
            Some(&tx),
            &json!({"kind": "command", "name": "usage", "args": "all"}),
        );
        assert_eq!(
            rx.recv().expect("command job"),
            Job::Command {
                args: "all".to_owned()
            }
        );
        for (event, expected) in [
            (
                "session_start",
                Job::Refresh {
                    force: false,
                    throttled: false,
                },
            ),
            (
                "model_select",
                Job::Refresh {
                    force: false,
                    throttled: false,
                },
            ),
            (
                "message_end",
                Job::Refresh {
                    force: false,
                    throttled: true,
                },
            ),
            ("session_shutdown", Job::Shutdown),
        ] {
            dispatch_for_test(&host, Some(&tx), &json!({"kind": "event", "event": event}));
            assert_eq!(rx.recv().expect("event job"), expected, "{event}");
        }
        // Unknown events and unknown commands are no-ops.
        dispatch_for_test(&host, Some(&tx), &json!({"kind": "event", "event": "nope"}));
        dispatch_for_test(
            &host,
            Some(&tx),
            &json!({"kind": "command", "name": "other"}),
        );
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn dispatch_without_a_worker_runs_synchronously() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        __reset_state();
        let dir = TestDir::new("sync");
        config::set_test_agent_dir(Some(dir.path().to_path_buf()));
        let host = UsageFakeHost::new();
        host.set_providers(vec!["deepseek".to_owned()]);
        host.set_envelope(
            "deepseek",
            json!({"schemaVersion": 1, "provider": "deepseek", "displayText": "deepseek: CNY 1"}),
        );
        dispatch_for_test(
            &host,
            None,
            &json!({"kind": "command", "name": "usage", "args": "deepseek"}),
        );
        assert_eq!(host.notifications(), vec!["deepseek: CNY 1".to_owned()]);
    }

    #[test]
    fn dispatch_for_an_unknown_cookie_is_a_noop() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        __reset_state();
        let value = dispatch_message(
            std::ptr::null(),
            &json!({"kind": "command", "name": "usage"}),
        );
        assert_eq!(value, Value::Null);
    }

    #[test]
    fn install_start_and_stop_the_worker_through_the_cookie_map() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        __reset_state();
        let cookie = 0x1234usize as PluginCookie;
        let (tx, rx) = std::sync::mpsc::channel();
        footer::install_worker_for_test(cookie as usize, tx);
        assert!(footer::worker_for(cookie).is_some());
        footer::send_job(cookie, Job::Shutdown);
        assert_eq!(rx.recv().expect("shutdown"), Job::Shutdown);
    }
    #[test]
    fn session_start_is_the_worker_restart_trigger() {
        assert!(event_starts_session(
            &json!({"kind": "event", "event": "session_start"})
        ));
        assert!(!event_starts_session(
            &json!({"kind": "event", "event": "session_shutdown"})
        ));
        assert!(!event_starts_session(
            &json!({"kind": "event", "event": "message_end"})
        ));
        assert!(!event_starts_session(
            &json!({"kind": "command", "name": "usage"})
        ));
    }
}
