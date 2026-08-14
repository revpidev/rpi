//! rpi-subagents: delegation extension (L0 native plugin, TE04 P0 core).
//!
//! Port of pi-subagents v0.48.0 (56f97234) — registers the `subagent` tool
//! (structured single delegation + management actions) and the `/run`,
//! `/subagents`, `/subagents-doctor` commands; spawns child `rpi --mode json
//! -p` sessions with per-agent prompts, tool allowlists and depth limits.
//! See docs: `rpi-docs/extensions/pi-subagents/` (requirements/design),
//! ADR-0016 (structured entry), ADR-0017 (missing-tool policy).
//!
//! Two runtime modes, split on `RPI_SUBAGENT_CHILD` (mirrors upstream
//! `extension/index.ts:345-347` + `fanout-child.ts:147-189`):
//! - parent (default): full tool/command surface;
//! - child: no tool registration unless `RPI_SUBAGENT_FANOUT_CHILD=1`
//!   (child-safe tool), plus the required-tools diagnostic on session_start.

#![allow(non_camel_case_types)]

mod actions;
mod agents;
mod artifacts;
mod commands;
mod config;
mod description;
mod diagnostic;
mod error;
mod launch;
mod p1;
mod prompts;
mod paths;
mod runner;
mod runtime;
mod session_fork;
mod tool;

use std::path::PathBuf;
use std::sync::OnceLock;

use abi_stable::prefix_type::PrefixTypeTrait;
use abi_stable::std_types::RVec;
use rpi_ext_host::native::{PluginCookie, RpiHostCalls, RpiNativeModule, RpiNativeModule_Ref};
use serde_json::{json, Value};

pub use runtime::PluginRuntime;

/// Host-call handle + plugin-owned runtime, established once by
/// `rpi_extension_init`. The cookie is stored as `usize` to keep the state
/// `Send + Sync` (mcp-adapter precedent).
struct PluginState {
    calls: RpiHostCalls,
    cookie: usize,
    runtime: PluginRuntime,
    mode: PluginMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PluginMode {
    Parent,
    ChildPlain,
    ChildFanout,
}

static STATE: OnceLock<PluginState> = OnceLock::new();

/// Clonable host-call channel for background tasks (FR-P1-04): the runner
/// task outlives the dispatch call that started it and needs `sendMessage` /
/// `events.emit` from the plugin runtime.
#[derive(Clone, Copy)]
pub struct AsyncHostCalls {
    pub call: extern "C" fn(
        rpi_ext_host::native::PluginCookie,
        abi_stable::std_types::RVec<u8>,
    ) -> abi_stable::std_types::RVec<u8>,
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
    let response =
        (channel.call)(channel.cookie as rpi_ext_host::native::PluginCookie, RVec::from(request));
    serde_json::from_slice(&response[..]).unwrap_or(Value::Null)
}

/// Host bridge the tool layer uses (cwd, parent model, parent session file,
/// and — from TE05 — the scoped model registry for fuzzy resolution).
pub trait HostContext {
    fn cwd(&self) -> PathBuf;
    fn parent_model(&self) -> Option<String>;
    fn parent_session_file(&self, settings: &config::SettingsPair) -> Option<PathBuf>;
    /// `ctx.scopedModels` projection: `(provider, id)` pairs for
    /// `launch::model` fuzzy resolution (FR-P1-05). Returns an empty list
    /// when the host reports none — resolution then falls back to verbatim.
    fn scoped_models(&self) -> Vec<launch::model::AvailableModel> {
        Vec::new()
    }

    /// Async notification channel for background runs (FR-P1-04); `None` in
    /// test fakes disables host notification (result files still land).
    fn async_calls(&self) -> Option<AsyncHostCalls> {
        None
    }
}

struct HostCallsContext<'a> {
    calls: &'a RpiHostCalls,
    cookie: usize,
}

impl HostContext for HostCallsContext<'_> {
    fn cwd(&self) -> PathBuf {
        if let Some(cwd) = host_call_ok(self.calls, self.cookie, "ctx.cwd", json!({})) {
            if let Some(cwd) = cwd.as_str() {
                if !cwd.is_empty() {
                    return PathBuf::from(cwd);
                }
            }
        }
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    }

    fn parent_model(&self) -> Option<String> {
        let model = host_call_ok(self.calls, self.cookie, "ctx.model", json!({}))?;
        let provider = model.get("provider").and_then(Value::as_str)?;
        let id = model.get("id").and_then(Value::as_str)?;
        Some(format!("{provider}/{id}"))
    }

    /// No ABI accessor for the session file (TE04 must not change
    /// rpi-ext-host), so derive it from the deterministic session-dir layout
    /// plus the newest `.jsonl` — upstream's own `findLatestSessionFile`
    /// heuristic (deviation TE-D16).
    fn parent_session_file(&self, settings: &config::SettingsPair) -> Option<PathBuf> {
        let cwd = self.cwd();
        let dir = paths::resolve_parent_session_dir(&cwd, settings.session_dir.as_deref());
        paths::find_latest_session_file(&dir)
    }

    fn async_calls(&self) -> Option<AsyncHostCalls> {
        Some(AsyncHostCalls {
            call: self.calls.call,
            cookie: self.cookie,
        })
    }

    fn scoped_models(&self) -> Vec<launch::model::AvailableModel> {
        let Some(scoped) = host_call_ok(self.calls, self.cookie, "ctx.scopedModels", json!({}))
        else {
            return Vec::new();
        };
        let Some(items) = scoped.as_array() else {
            return Vec::new();
        };
        let mut models = Vec::new();
        for item in items {
            let model = item.get("model");
            let (Some(provider), Some(id)) = (
                model.and_then(|m| m.get("provider")).and_then(Value::as_str),
                model.and_then(|m| m.get("id")).and_then(Value::as_str),
            ) else {
                continue;
            };
            models.push(launch::model::AvailableModel {
                full_id: format!("{provider}/{id}"),
                provider: provider.to_string(),
                id: id.to_string(),
            });
        }
        models
    }
}

/// Shared host-call helper (request envelope per rpi-test-native-plugin).
fn host_call(calls: &RpiHostCalls, cookie: usize, method: &str, args: Value) -> Value {
    let request = serde_json::to_vec(&json!({
        "call": method,
        "args": args,
        "seq": 0,
    }))
    .unwrap_or_default();
    let response = (calls.call)(cookie as PluginCookie, RVec::from(request));
    serde_json::from_slice(&response[..]).unwrap_or(Value::Null)
}

fn host_call_ok(calls: &RpiHostCalls, cookie: usize, method: &str, args: Value) -> Option<Value> {
    let response = host_call(calls, cookie, method, args);
    if response.get("error").is_some() {
        return None;
    }
    let ok = response.get("ok").cloned().unwrap_or(Value::Null);
    if ok.is_null() {
        None
    } else {
        Some(ok)
    }
}

fn pack(value: &Value) -> RVec<u8> {
    RVec::from(serde_json::to_vec(value).unwrap_or_else(|_| b"null".to_vec()))
}

fn plugin_mode() -> PluginMode {
    let child = std::env::var(launch::args::SUBAGENT_CHILD_ENV)
        .ok()
        .as_deref()
        == Some("1");
    if !child {
        return PluginMode::Parent;
    }
    let fanout = std::env::var(launch::args::SUBAGENT_FANOUT_CHILD_ENV)
        .ok()
        .as_deref()
        == Some("1");
    if fanout {
        PluginMode::ChildFanout
    } else {
        PluginMode::ChildPlain
    }
}

/// Child-safe tool description (fanout-child.ts:178-182, P0 action set).
fn child_safe_tool_description() -> String {
    [
        "Delegate to subagents from child-safe fanout mode.",
        "Allowed management/control actions: list, get, status, doctor.",
        "Mutating management actions (create, update, delete, eject, disable, enable, reset, grant-spawn-budget) are blocked in this mode.",
    ]
    .join("\n")
}

/// Required-tools diagnostic in the child (subagent-prompt-runtime.ts:100-107
/// `refreshChildToolDiagnostic`): compare `getAllTools` names against
/// `RPI_SUBAGENT_REQUIRED_TOOLS`, write the ADR-0017 diagnostic.
fn refresh_child_tool_diagnostic(calls: &RpiHostCalls, cookie: usize) {
    let Ok(path_raw) = std::env::var(launch::args::CHILD_TOOL_DIAGNOSTIC_PATH_ENV) else {
        return;
    };
    let path_raw = path_raw.trim().to_string();
    if path_raw.is_empty() {
        return;
    }
    let Ok(required_raw) = std::env::var(launch::args::REQUIRED_CHILD_TOOLS_ENV) else {
        return;
    };
    let Ok(required_value) = serde_json::from_str::<Value>(&required_raw) else {
        return;
    };
    let Some(required_list) = required_value.as_array() else {
        return;
    };
    let required: Vec<String> = required_list
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect();
    if required.is_empty() {
        return;
    }
    let available: Vec<String> = host_call_ok(calls, cookie, "getAllTools", json!({}))
        .and_then(|value| value.as_array().cloned())
        .map(|tools| {
            tools
                .iter()
                .filter_map(|entry| entry.get("name").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if let Some(diagnostic) = diagnostic::write_child_tool_diagnostic(
        &std::path::PathBuf::from(&path_raw),
        &required,
        &available,
        std::env::var(launch::args::SUBAGENT_CHILD_AGENT_ENV)
            .ok()
            .as_deref(),
    ) {
        tracing::warn!(
            missing = ?diagnostic.missing,
            "child subagent missing required tools (ADR-0017)"
        );
    }
}

fn install(calls: RpiHostCalls, cookie: PluginCookie) -> Value {
    let cookie = cookie as usize;
    let mode = plugin_mode();
    let Some(plugin_runtime) = PluginRuntime::new() else {
        return json!({"error": {"kind": "init", "message": "failed to start plugin tokio runtime"}});
    };
    let register = |method: &str, args: Value| -> Result<(), Value> {
        let response = host_call(&calls, cookie, method, args);
        if response.get("error").is_some() {
            return Err(response);
        }
        Ok(())
    };

    match mode {
        PluginMode::ChildPlain | PluginMode::ChildFanout => {
            // Child sessions get no parent surface. The required-tools
            // diagnostic runs at session_start (ADR-0017).
            if let Err(error) = register("on", json!({ "event": "session_start" })) {
                return json!({"error": {"kind": "init", "message": error.to_string()}});
            }
            // Steer inbox consumer (FR-P1-04, subagent-prompt-runtime.ts
            // registerSteeringInbox L333): when the parent launched this
            // child with a steer inbox, poll it and inject messages through
            // `sendUserMessage` (deliverAs steer|followUp), writing an ack
            // per consumed request.
            if let Ok(inbox) = std::env::var(launch::args::SUBAGENT_STEER_INBOX_ENV) {
                if !inbox.trim().is_empty() {
                    let inbox = std::path::PathBuf::from(inbox);
                    let calls_copy = RpiHostCalls { call: calls.call };
                    let cookie_copy = cookie;
                    plugin_runtime.spawn(async move {
                        steer_inbox_loop(calls_copy, cookie_copy, inbox).await;
                    });
                }
            }
            if mode == PluginMode::ChildFanout {
                if let Err(error) = register(
                    "registerTool",
                    json!({
                        "name": "subagent",
                        "label": "Subagent",
                        "description": child_safe_tool_description(),
                        "parameters": tool::tool_parameters_schema(),
                    }),
                ) {
                    return json!({"error": {"kind": "init", "message": error.to_string()}});
                }
            }
        }
        PluginMode::Parent => {
            let config = config::load_config();
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            if let Err(error) = register(
                "registerTool",
                json!({
                    "name": "subagent",
                    "label": "Subagent",
                    "description": description::build_subagent_tool_description(&config, &cwd),
                    "promptSnippet": "Delegate focused subtasks to child agent sessions (scout/researcher/worker/reviewer/oracle/delegate) or manage them via action.",
                    "parameters": tool::tool_parameters_schema(),
                }),
            ) {
                return json!({"error": {"kind": "init", "message": error.to_string()}});
            }
            // Bundled prompt templates register as commands (FR-P1-07).
            for (name, body) in prompts::BUNDLED_PROMPTS {
                let spec = prompts::parse_template(body);
                let description = format!(
                    "Prompt shortcut: {}",
                    spec.description.as_deref().unwrap_or(name)
                );
                if let Err(error) =
                    register("registerCommand", json!({ "name": name, "description": description }))
                {
                    return json!({"error": {"kind": "init", "message": error.to_string()}});
                }
            }
            if let Err(error) = register(
                "registerCommand",
                json!({
                    "name": "prompt-workflow",
                    "description": "Run a subagent prompt template (/prompt-workflow <name> [args] [--fork|--fresh|--bg|--subagent <agent>])"
                }),
            ) {
                return json!({"error": {"kind": "init", "message": error.to_string()}});
            }
            for (name, description) in commands::command_definitions() {
                if let Err(error) = register(
                    "registerCommand",
                    json!({ "name": name, "description": description }),
                ) {
                    return json!({"error": {"kind": "init", "message": error.to_string()}});
                }
            }
            for event in ["session_start", "session_shutdown", "agent_end"] {
                if let Err(error) = register("on", json!({ "event": event })) {
                    return json!({"error": {"kind": "init", "message": error.to_string()}});
                }
            }
            // `subagent_wait` (FR-P1-04, wait-tool.ts): registered unless
            // disabled by config.waitTool / RPI_SUBAGENT_WAIT_TOOL_ENABLED.
            if config.wait_tool_enabled() {
                if let Err(error) = register(
                    "registerTool",
                    json!({
                        "name": "subagent_wait",
                        "label": "Subagent wait",
                        "description": "Wait for background subagent runs to reach a terminal state (first-terminal by default, all with { all: true }); non-blocking with { nonBlocking: true }. Disabled by config.waitTool.",
                        "parameters": {
                            "type": "object",
                            "properties": {
                                "id": { "type": "string", "description": "Run id or unique id prefix; omit to wait on any background run." },
                                "all": { "type": "boolean", "description": "Wait for every background run to finish (default: first terminal)." },
                                "nonBlocking": { "type": "boolean", "description": "Register a wake and return immediately." },
                                "timeoutMs": { "type": "integer", "description": "Wait timeout in milliseconds (default 30 minutes)." }
                            }
                        },
                    }),
                ) {
                    return json!({"error": {"kind": "init", "message": error.to_string()}});
                }
            }
            // Startup: stale async-run reconciliation (ADR-0019 crash branch)
            // + artifact cleanup (index.ts:371-372).
            // Orchestration skill ships to the user skill dir (FR-P1-07;
            // parent sessions only — children never resolve it).
            prompts::install_orchestration_skill();
            runner::background::reconcile_stale_runs();
            artifacts::cleanup_all_artifact_dirs(config.cleanup_days_or_default());
        }
    }

    let state = PluginState {
        calls,
        cookie,
        runtime: plugin_runtime,
        mode,
    };
    match STATE.set(state) {
        Ok(()) => json!({ "ok": true }),
        // Idempotent init: a second load of the same cdylib (ambient +
        // explicit `--extension`) must not fail — the first instance serves
        // both (host loader dedupes canonical paths; this is the safety net).
        Err(_) => json!({ "ok": true }),
    }
}

/// Steer inbox poll loop (registerSteeringInbox L333 + writeSteerAck):
/// 500ms scan, inject via sendUserMessage, ack per request, stop on the
/// steering-capability file being closed (`steer-inbox-closed.json`).
async fn steer_inbox_loop(calls: RpiHostCalls, cookie: usize, inbox: PathBuf) {
    let _ = std::fs::create_dir_all(&inbox);
    // Capability beacon: the parent can probe readiness.
    let _ = std::fs::write(
        inbox.join("capability.json"),
        json!({ "protocolVersion": 1, "supported": true }).to_string(),
    );
    loop {
        if inbox.join("steer-inbox-closed.json").exists() {
            return;
        }
        let Ok(entries) = std::fs::read_dir(&inbox) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(raw) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(request) = serde_json::from_str::<Value>(&raw) else {
                continue;
            };
            if request.get("type").and_then(Value::as_str) != Some("steer") {
                continue;
            }
            let message = request
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let deliver_as = match request.get("mode").and_then(Value::as_str) {
                Some("follow_up") => "followUp",
                _ => "steer",
            };
            let _ = host_call(
                &calls,
                cookie,
                "sendUserMessage",
                json!({
                    "content": message,
                    "options": { "deliverAs": deliver_as },
                }),
            );
            // Ack + consume.
            if let Some(acks) = inbox.parent() {
                let ack_dir = acks.parent().map(|p| p.join("steer-acks"));
                if let Some(ack_dir) = ack_dir {
                    let _ = std::fs::create_dir_all(&ack_dir);
                    let id = request
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                        .to_string();
                    let _ = std::fs::write(
                        ack_dir.join(format!("{id}.json")),
                        json!({ "type": "steer-ack", "id": id }).to_string(),
                    );
                }
            }
            let _ = std::fs::remove_file(&path);
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}

/// Host-dispatched calls (`rpi_dispatch`): toolExecute / command / event.
fn dispatch_message(message: &Value) -> Value {
    let Some(state) = STATE.get() else {
        return Value::Null;
    };
    match message.get("kind").and_then(Value::as_str) {
        Some("toolExecute") => {
            let tool_name = message.get("toolName").and_then(Value::as_str).unwrap_or("");
            let params = message.get("params").cloned().unwrap_or(Value::Null);
            // Child-safe mode blocks mutating management actions before
            // execution (fanout-child.ts allowlist, L174-189).
            if state.mode == PluginMode::ChildFanout {
                if let Some(action) = params.get("action").and_then(Value::as_str) {
                    if !matches!(action, "list" | "get" | "status" | "doctor") {
                        return json!({
                            "content": [{ "type": "text", "text": format!(
                                "Management action \"{action}\" is blocked in child-safe fanout mode. Allowed: list, get, status, doctor."
                            )}],
                            "isError": true,
                            "details": { "mode": "single", "results": [] },
                        });
                    }
                }
            }
            if tool_name == "subagent_wait" {
                return execute_subagent_wait(&params, state);
            }
            let host = HostCallsContext {
                calls: &state.calls,
                cookie: state.cookie,
            };
            let settings = config::read_settings_pair(&host.cwd());
            let config = config::load_config();
            tool::execute_subagent_tool(&params, &host, &settings, &config, &state.runtime)
        }
        Some("command") => {
            let name = message.get("name").and_then(Value::as_str).unwrap_or("");
            let args = message.get("args").and_then(Value::as_str).unwrap_or("");
            if state.mode != PluginMode::Parent {
                return Value::Null;
            }
            let host = HostCallsContext {
                calls: &state.calls,
                cookie: state.cookie,
            };
            let settings = config::read_settings_pair(&host.cwd());
            let config = config::load_config();
            if let Some(outcome) =
                prompts::handle_prompt_command(name, args, &host, &settings, &config, &state.runtime)
            {
                return outcome;
            }
            commands::handle_command(name, args, &host, &settings, &config, &state.runtime)
        }
        Some("event") => {
            let event = message.get("event").and_then(Value::as_str).unwrap_or("");
            match (state.mode, event) {
                (PluginMode::ChildPlain | PluginMode::ChildFanout, "session_start") => {
                    refresh_child_tool_diagnostic(&state.calls, state.cookie);
                    Value::Null
                }
                (PluginMode::Parent, "session_start") => {
                    let config = config::load_config();
                    artifacts::cleanup_all_artifact_dirs(config.cleanup_days_or_default());
                    Value::Null
                }
                (PluginMode::Parent, "session_shutdown") => {
                    // Harvest async runs (ADR-0019 interactive branch), then
                    // sweep live children (SIGTERM → 3s → SIGKILL).
                    state
                        .runtime
                        .spawn(async_runner_shutdown());
                    Value::Null
                }
                (PluginMode::Parent, "agent_end") => {
                    // Headless auto-drain (ADR-0019 print branch,
                    // auto-drain.ts): wait for outstanding runs before exit.
                    let has_ui = host_call_ok(&state.calls, state.cookie, "ctx.hasUI", json!({}))
                        .and_then(|value| value.as_bool())
                        .unwrap_or(false);
                    if !has_ui {
                        state.runtime.spawn(async_move_drain());
                    }
                    Value::Null
                }
                _ => Value::Null,
            }
        }
        _ => Value::Null,
    }
}

/// Named async fn wrappers (async blocks around the registry calls trip the
/// rustc HRTB-closure limitation).
async fn async_runner_shutdown() {
    runner::background::harvest_for_shutdown().await;
    runner::foreground::kill_all_children_for_shutdown().await;
}

async fn async_move_drain() {
    // DEFAULT_AUTO_DRAIN_TIMEOUT_MS (auto-drain.ts:9): 30 minutes.
    if let Err(message) = runner::background::drain_outstanding_work(30 * 60 * 1000).await {
        tracing::error!(%message, "auto-drain failed");
    }
}

/// `subagent_wait` execution (subagent-wait.ts waitForSubagents subset).
fn execute_subagent_wait(params: &Value, state: &PluginState) -> Value {
    let config = config::load_config();
    if !config.wait_tool_enabled() {
        return json!({
            "content": [{ "type": "text", "text":
                "subagent_wait is disabled by config.waitTool or RPI_SUBAGENT_WAIT_TOOL_ENABLED; returning immediately without blocking background work. Active work keeps going, and you can inspect subagents with subagent({ action: \"status\" }) or rely on completion notifications."
            }],
            "isError": false,
        });
    }
    let id = params
        .get("id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let all = params.get("all").and_then(Value::as_bool).unwrap_or(false);
    let non_blocking = params
        .get("nonBlocking")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if non_blocking {
        // P0-shape stub for the persistent-subscription mode: return now;
        // completion notifications are the wake channel.
        return json!({
            "content": [{ "type": "text", "text":
                "Registered for background completion notifications; returning without blocking."
            }],
            "isError": false,
        });
    }
    let timeout_ms = params
        .get("timeoutMs")
        .and_then(Value::as_u64)
        .filter(|v| *v > 0)
        .unwrap_or(30 * 60 * 1000);
    let result = state.runtime.block_on(async {
        runner::background::wait_for_runs(id.as_deref(), all, timeout_ms).await
    });
    match result {
        Ok(waited) => json!({
            "content": [{ "type": "text", "text":
                format!("Wait finished: {} run(s) reached a terminal state.", waited["waited"].as_u64().unwrap_or(0))
            }],
            "details": waited,
            "isError": false,
        }),
        Err(message) => json!({
            "content": [{ "type": "text", "text": message }],
            "isError": true,
        }),
    }
}

/// Test seam: drive a tool execution against an installed state.
pub fn execute_for_test(params: &Value) -> Value {
    dispatch_message(&json!({
        "kind": "toolExecute",
        "toolName": "subagent",
        "toolCallId": "test",
        "params": params,
    }))
}

/// Test seam: install with a fake host (mcp-adapter `install_for_test`
/// pattern; one install per test binary because of the OnceLock state).
pub fn install_for_test(calls: RpiHostCalls, cookie: PluginCookie) -> Value {
    install(calls, cookie)
}

#[allow(clippy::missing_safety_doc)]
pub extern "C" fn init(calls: RpiHostCalls, cookie: PluginCookie) -> RVec<u8> {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| install(calls, cookie)))
        .unwrap_or_else(|_| json!({"error": {"kind": "init", "message": "panic during init"}}));
    pack(&result)
}

pub extern "C" fn dispatch(_cookie: PluginCookie, message: RVec<u8>) -> RVec<u8> {
    let parsed: Value = serde_json::from_slice(&message[..]).unwrap_or(Value::Null);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        dispatch_message(&parsed)
    }))
    .unwrap_or_else(|_| {
        json!({
            "content": [{ "type": "text", "text": "subagents extension panicked while handling a dispatch" }],
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

/// Public facade for the parity harness (`examples/parity_runner.rs` +
/// `scripts/subagents-parity/`): mirrors the internal launch/parsing APIs
/// with plain-data types so the harness needs no crate internals.
pub mod parity {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    pub use crate::agents::frontmatter::parse_frontmatter_list as parse_frontmatter_list_public;
    pub use crate::runner::events::get_final_output as get_final_output_public;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum TaskDeliveryPublic {
        Auto,
        File,
    }

    #[derive(Debug, Clone, Default)]
    pub struct BuildArgsInputPublic {
        pub base_args: Vec<String>,
        pub task: String,
        pub task_delivery: Option<TaskDeliveryPublic>,
        pub session_enabled: bool,
        pub session_dir: Option<PathBuf>,
        pub session_file: Option<PathBuf>,
        pub model: Option<String>,
        pub thinking: Option<String>,
        pub system_prompt: Option<String>,
        pub system_prompt_mode: &'static str,
        pub inherit_project_context: bool,
        pub inherit_skills: bool,
        pub require_read_tool: bool,
        pub tools: Option<Vec<String>>,
        pub extensions: Option<Vec<String>>,
        pub subagent_only_extensions: Option<Vec<String>>,
        pub prompt_file_stem: Option<String>,
        pub run_id: Option<String>,
        pub child_agent_name: Option<String>,
        pub child_index: Option<usize>,
        pub parent_session_id: Option<String>,
        pub fanout_authorized: bool,
        pub self_extension: Option<String>,
        /// Steer inbox dir (FR-P1-04); None clears the env.
        pub steer_inbox: Option<PathBuf>,
    }

    pub struct BuildArgsResultPublic {
        pub args: Vec<String>,
        pub env: BTreeMap<String, Option<String>>,
    }

    pub fn build_args_public(
        input: &BuildArgsInputPublic,
    ) -> crate::error::Result<BuildArgsResultPublic> {
        let internal = crate::launch::args::BuildArgsInput {
            base_args: input.base_args.clone(),
            task: input.task.clone(),
            task_delivery: input.task_delivery.map(|delivery| match delivery {
                TaskDeliveryPublic::File => crate::launch::args::TaskDelivery::File,
                TaskDeliveryPublic::Auto => crate::launch::args::TaskDelivery::Auto,
            }),
            session_enabled: input.session_enabled,
            session_dir: input.session_dir.clone(),
            session_file: input.session_file.clone(),
            model: input.model.clone(),
            thinking: input.thinking.clone(),
            system_prompt: input.system_prompt.clone(),
            system_prompt_mode: input.system_prompt_mode,
            inherit_project_context: input.inherit_project_context,
            inherit_skills: input.inherit_skills,
            require_read_tool: input.require_read_tool,
            tools: input.tools.clone(),
            extensions: input.extensions.clone(),
            subagent_only_extensions: input.subagent_only_extensions.clone(),
            mcp_direct_tools: Vec::new(),
            prompt_file_stem: input.prompt_file_stem.clone(),
            run_id: input.run_id.clone(),
            child_agent_name: input.child_agent_name.clone(),
            child_index: input.child_index,
            parent_session_id: input.parent_session_id.clone(),
            fanout_authorized: input.fanout_authorized,
            self_extension: input.self_extension.clone(),
            steer_inbox: input.steer_inbox.clone(),
        };
        let result = crate::launch::args::build_rpi_args(&internal)?;
        Ok(BuildArgsResultPublic {
            args: result.args,
            env: result.env,
        })
    }

    #[derive(Debug, Clone)]
    pub struct ParsedFrontmatterPublic {
        pub frontmatter: BTreeMap<String, String>,
        pub body: String,
    }

    pub fn parse_frontmatter_public(content: &str) -> ParsedFrontmatterPublic {
        let parsed = crate::agents::frontmatter::parse_frontmatter(content);
        ParsedFrontmatterPublic {
            frontmatter: parsed.frontmatter,
            body: parsed.body,
        }
    }
}

/// Replay surface for the recorded child stream fixture
/// (`tests/fixtures/child_stream.jsonl`): feed the captured stdout lines
/// through the real event parser (design §3.3 drift guard).
pub mod child_stream_replay {
    pub use crate::runner::events::ChildRunState as ChildRunStatePublic;

    impl ChildRunStatePublic {
        pub fn process_line_public(&mut self, line: &str) {
            let _ = self.process_line(line);
        }

        pub fn messages_public(&self) -> &[serde_json::Value] {
            &self.messages
        }
    }
}
