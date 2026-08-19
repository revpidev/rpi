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
/// TE11 FR-C fleet strip (`pub` for the e2e linger test seam).
pub mod fleet;
mod launch;
mod messages;
mod p1;
mod paths;
mod prompts;
mod render;
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

use crate::runner::foreground::StreamFrameSink;

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
    let response = (channel.call)(
        channel.cookie as rpi_ext_host::native::PluginCookie,
        RVec::from(request),
    );
    serde_json::from_slice(&response[..]).unwrap_or(Value::Null)
}

/// Host bridge the tool layer uses (cwd, parent model, parent session file,
/// and — from TE05 — the scoped model registry for fuzzy resolution).
pub trait HostContext {
    fn cwd(&self) -> PathBuf;
    fn parent_model(&self) -> Option<String>;
    fn parent_session_file(&self, settings: &config::SettingsPair) -> Option<PathBuf>;
    /// `ctx.hasUI`: authority-gated actions (grant-spawn-budget) are limited
    /// to the root interactive parent session (upstream
    /// subagent-executor.ts:5241-5260). Defaults to `false` — test fakes
    /// must opt in explicitly.
    fn has_ui(&self) -> bool {
        false
    }
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

    /// Streaming seam for a foreground dispatch (TE09 FR-A): the
    /// `toolUpdate` sender addressed to `tool_call_id` (ADR-0015), or `None`
    /// when the caller has no host channel (test fakes stay non-streaming).
    fn tool_update_sink(&self, tool_call_id: &str) -> Option<StreamFrameSink> {
        let _ = tool_call_id;
        None
    }

    /// Abort probe for an in-flight dispatch (extension-ABI abort-channel
    /// gap): polls the host's `ctx.aborted` for `tool_call_id`. `None` when
    /// the caller has no host channel — waits/foreground runs then simply
    /// run to completion (test fakes, detached contexts).
    fn abort_probe(&self, tool_call_id: &str) -> Option<crate::runner::foreground::AbortProbe> {
        let _ = tool_call_id;
        None
    }
}

/// Per-dispatch `toolUpdate` sender (ADR-0015, `tools` capability) — the
/// subagents counterpart of the smart-fetch UpdateSink: one partial
/// AgentToolResult frame; response errors are swallowed (the host drops
/// unknown/stale ids by design, settle semantics).
struct ToolUpdateSink {
    calls: AsyncHostCalls,
    tool_call_id: String,
}

impl ToolUpdateSink {
    fn push(&self, content_text: &str, details: &Value) {
        // Response errors are swallowed: the host drops unknown/stale ids by
        // design (ADR-0015 settle semantics), so a late frame is not this
        // plugin's failure to report.
        let _ = host_call_static(
            &self.calls,
            "toolUpdate",
            json!({
                "toolCallId": self.tool_call_id,
                "update": {
                    "content": [{ "type": "text", "text": content_text }],
                    "details": details,
                }
            }),
        );
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

    fn has_ui(&self) -> bool {
        host_call_ok(self.calls, self.cookie, "ctx.hasUI", json!({}))
            .and_then(|value| value.as_bool())
            .unwrap_or(false)
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

    fn tool_update_sink(&self, tool_call_id: &str) -> Option<StreamFrameSink> {
        let sink = ToolUpdateSink {
            calls: self.async_calls()?,
            tool_call_id: tool_call_id.to_string(),
        };
        Some(std::sync::Arc::new(move |text: &str, details: &Value| {
            sink.push(text, details)
        }))
    }

    fn abort_probe(&self, tool_call_id: &str) -> Option<crate::runner::foreground::AbortProbe> {
        let calls = self.async_calls()?;
        let tool_call_id = tool_call_id.to_string();
        Some(std::sync::Arc::new(move || {
            host_call_static(&calls, "ctx.aborted", json!({ "toolCallId": tool_call_id }))
                .get("ok")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        }))
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
                model
                    .and_then(|m| m.get("provider"))
                    .and_then(Value::as_str),
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
            // Supervisor client (FR-P1-10): children with a channel dir get
            // contact_supervisor (native-supervisor-channel.ts L298).
            // Registration is best-effort: a bare-file `--extension` injection
            // carries no manifest → capabilities=[] → registerTool is denied;
            // the child must survive that (packaged installs with the
            // manifest register normally).
            if crate::p1::supervisor::ChildSupervisorContext::from_env().is_some() {
                if let Err(error) = register(
                    "registerTool",
                    json!({
                        "name": "contact_supervisor",
                        "label": "Contact supervisor",
                        "description": "Contact the parent orchestrator session: need_decision (blocking clarification), interview_request (structured input), or progress_update (non-blocking note).",
                        "parameters": crate::p1::supervisor::ChildSupervisorContext::tool_schema(),
                    }),
                ) {
                    tracing::warn!(
                        error = %error.to_string(),
                        "contact_supervisor registration denied (bare-file extension load carries no capabilities); child continues without it"
                    );
                }
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
                        // TE11 FR-A/FR-B: the host attaches the render hooks
                        // and dispatches {"kind":"render","what":"toolCall"|
                        // "toolResult"} back here (host_call.rs render
                        // protocol, mcp-adapter precedent).
                        "renderCall": true,
                        "renderResult": true,
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
                    // TE11 FR-A/FR-B render hooks (see the ChildFanout
                    // registration note).
                    "renderCall": true,
                    "renderResult": true,
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
                if let Err(error) = register(
                    "registerCommand",
                    json!({ "name": name, "description": description }),
                ) {
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
            // Message renderers (TE09 FR-D, extension/index.ts:481-539 —
            // rpi registers the three with real injectors or defended
            // types): `subagent-notify` (completion notifications),
            // `subagent_steering_notice` and `subagent_control_notice`
            // (underscore constants, steering-notices.ts:4 /
            // control-notices.ts:5). The slash pair stays unregistered —
            // rpi never injects those types (TE09 Out list).
            for custom_type in [
                "subagent-notify",
                "subagent_steering_notice",
                "subagent_control_notice",
            ] {
                if let Err(error) = register(
                    "registerMessageRenderer",
                    json!({ "customType": custom_type }),
                ) {
                    return json!({"error": {"kind": "init", "message": error.to_string()}});
                }
            }
            // Parent-side supervisor tool (FR-P1-10).
            if let Err(error) = register(
                "registerTool",
                json!({
                    "name": "subagent_supervisor",
                    "label": "Subagent supervisor",
                    "description": "Handle child supervisor requests: {action: \"pending\"} lists requests from this session's children; {action: \"reply\", replyTo, message} answers one.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "action": { "type": "string", "enum": ["pending", "reply"] },
                            "replyTo": { "type": "string", "description": "Request id being answered (reply)." },
                            "message": { "type": "string", "description": "The reply text." }
                        },
                        "required": ["action"]
                    },
                }),
            ) {
                return json!({"error": {"kind": "init", "message": error.to_string()}});
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
            let tool_name = message
                .get("toolName")
                .and_then(Value::as_str)
                .unwrap_or("");
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
                let tool_call_id = message
                    .get("toolCallId")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty());
                return execute_subagent_wait(&params, state, tool_call_id);
            }
            if tool_name == "contact_supervisor" {
                if let Some(context) = crate::p1::supervisor::ChildSupervisorContext::from_env() {
                    return context.execute(&params);
                }
                return json!({
                    "content": [{ "type": "text", "text":
                        "contact_supervisor is unavailable: no supervisor channel was configured for this session."
                    }],
                    "details": { "mode": "single", "results": [] },
                    "isError": true,
                });
            }
            if tool_name == "subagent_supervisor" {
                let orchestrator_session_id =
                    std::env::var(launch::args::SUBAGENT_ORCHESTRATOR_SESSION_ID_ENV)
                        .unwrap_or_default();
                // The parent's own session id comes from its newest session
                // file (TE-D16 derivation), falling back to the env.
                let session_id = if orchestrator_session_id.is_empty() {
                    HostCallsContext {
                        calls: &state.calls,
                        cookie: state.cookie,
                    }
                    .parent_session_file(&config::read_settings_pair(
                        &std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
                    ))
                    .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().to_string()))
                    .unwrap_or_default()
                } else {
                    orchestrator_session_id
                };
                return crate::p1::supervisor::parent_supervisor_action(
                    params.get("action").and_then(Value::as_str).unwrap_or(""),
                    params.get("replyTo").and_then(Value::as_str),
                    params.get("message").and_then(Value::as_str),
                    &session_id,
                    &crate::p1::supervisor::channels_root(),
                );
            }
            let host = HostCallsContext {
                calls: &state.calls,
                cookie: state.cookie,
            };
            let settings = config::read_settings_pair(&host.cwd());
            let config = config::load_config();
            // ADR-0015: the host forwards the toolCallId so `toolUpdate`
            // frames can address the in-flight execution (test seams may
            // omit it — the run then skips streaming).
            let tool_call_id = message
                .get("toolCallId")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty());
            tool::execute_subagent_tool(
                &params,
                &host,
                &settings,
                &config,
                &state.runtime,
                tool_call_id,
            )
        }
        // Render protocol (host_call.rs:454-499): the host dispatches a
        // synchronous `{"kind":"render","what":"message"|"toolCall"|
        // "toolResult",...}`; pure JSON, never touches the runtime.
        // A `null` tree falls back to the host's default rendering.
        Some("render") => {
            let what = message.get("what").and_then(Value::as_str);
            if what == Some("message") {
                let custom_type = message
                    .get("message")
                    .and_then(|m| m.get("customType"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let payload = message.get("message").unwrap_or(&Value::Null);
                let options = message.get("options").unwrap_or(&Value::Null);
                return match custom_type {
                    "subagent-notify" => messages::render_subagent_notify(payload, options),
                    "subagent_steering_notice" => messages::render_subagent_steering(payload),
                    "subagent_control_notice" => messages::render_subagent_control(payload),
                    _ => Value::Null,
                };
            }
            // TE11 FR-A/FR-B: the `subagent` tool's call title and run card
            // (mcp-adapter render dispatch precedent — same envelope).
            if what == Some("toolCall") {
                let args = message
                    .get("context")
                    .and_then(|c| c.get("args"))
                    .unwrap_or(&Value::Null);
                return render::render_subagent_tool_call(args);
            }
            if what == Some("toolResult") {
                let result = message.get("result").unwrap_or(&Value::Null);
                let options = message.get("options").unwrap_or(&Value::Null);
                let call_args = message
                    .get("context")
                    .and_then(|c| c.get("args"))
                    .unwrap_or(&Value::Null);
                return render::render_subagent_tool_result(result, options, call_args);
            }
            Value::Null
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
            if let Some(outcome) = prompts::handle_prompt_command(
                name,
                args,
                &host,
                &settings,
                &config,
                &state.runtime,
            ) {
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
                    // sweep live children (SIGTERM → 3s → SIGKILL). Blocking
                    // here is the point: `dispose` waits for this dispatch to
                    // return, so a fire-and-forget spawn would be cancelled by
                    // process exit before the signal ladder ran. The ladder is
                    // bounded (3s grace), so interactive exit is not stalled.
                    state.runtime.block_on(async_runner_shutdown());
                    Value::Null
                }
                (PluginMode::Parent, "agent_end") => {
                    // Headless auto-drain (ADR-0019 print branch,
                    // auto-drain.ts): wait for outstanding runs before exit.
                    // Dispatch is synchronous, so blocking here blocks the
                    // host's `agent_end` handling — print mode therefore waits
                    // for async runs to finish (upstream parity), bounded by
                    // the 30min drain timeout.
                    let has_ui = host_call_ok(&state.calls, state.cookie, "ctx.hasUI", json!({}))
                        .and_then(|value| value.as_bool())
                        .unwrap_or(false);
                    if !has_ui {
                        state.runtime.block_on(async_move_drain());
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

/// `subagent_wait` execution (subagent-wait.ts waitForSubagents subset +
/// the per-cycle asyncWaitUpdate stream, TE09 FR-C).
fn execute_subagent_wait(params: &Value, state: &PluginState, tool_call_id: Option<&str>) -> Value {
    let config = config::load_config();
    if !config.wait_tool_enabled() {
        return json!({
            "content": [{ "type": "text", "text":
                "subagent_wait is disabled by config.waitTool or RPI_SUBAGENT_WAIT_TOOL_ENABLED; returning immediately without blocking background work. Active work keeps going, and you can inspect subagents with subagent({ action: \"status\" }) or rely on completion notifications."
            }],
            "details": { "mode": "management", "disabled": true, "results": [] },
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
            "details": { "mode": "management", "nonBlocking": true, "results": [] },
            "isError": false,
        });
    }
    let timeout_ms = params
        .get("timeoutMs")
        .and_then(Value::as_u64)
        .filter(|v| *v > 0)
        .unwrap_or(30 * 60 * 1000);
    // The live-activity stream rides the same toolUpdate seam as the
    // foreground snapshots (upstream deps.onUpdate, subagent-wait.ts:578).
    let (update_sink, abort_probe) = {
        let host = HostCallsContext {
            calls: &state.calls,
            cookie: state.cookie,
        };
        match tool_call_id {
            Some(tool_call_id) => (
                host.tool_update_sink(tool_call_id),
                host.abort_probe(tool_call_id),
            ),
            None => (None, None),
        }
    };
    let result = state.runtime.block_on(async {
        type WaitUpdate = Box<dyn Fn(&str) + Send + Sync>;
        let on_update: Option<WaitUpdate> = update_sink.as_ref().map(|sink| {
            let sink = std::sync::Arc::clone(sink);
            // Fire-and-forget (2026-08-16 intermittent wait-hang fix): the
            // toolUpdate FFI runs on the plugin runtime's worker; a host-side
            // race that stalls it must not stall the wait loop itself — the
            // loop's correctness (terminal detection, deadline) matters more
            // than any single live-activity frame, which is display-only.
            Box::new(move |text: &str| {
                let sink = std::sync::Arc::clone(&sink);
                let text = text.to_owned();
                tokio::spawn(async move {
                    sink(&text, &wait_update_details());
                });
            }) as WaitUpdate
        });
        let on_update = on_update
            .as_ref()
            .map(|boxed| &**boxed as &(dyn Fn(&str) + Send + Sync));
        let abort_probe = abort_probe
            .as_ref()
            .map(|probe| &**probe as &(dyn Fn() -> bool + Send + Sync));
        runner::background::wait_for_runs(id.as_deref(), all, timeout_ms, on_update, abort_probe)
            .await
    });
    match result {
        Ok(waited) => {
            // The aborted branch must read as an interruption, not a
            // completion — the runs keep going in the background.
            let text = if waited["aborted"].as_bool().unwrap_or(false) {
                format!(
                    "Wait interrupted by user abort: {} run(s) keep running in the background. \
                     Completion still arrives as a session message; inspect with \
                     subagent({{action:\"status\"}}) or wait again with subagent_wait.",
                    waited["runs"].as_array().map(Vec::len).unwrap_or(0)
                )
            }
            // The timeout branch (wait_for_runs deadline) must read as a
            // timeout, not as "0 finished" — the model would otherwise
            // misread the run set as empty and re-wait blindly.
            else if waited["timedOut"].as_bool().unwrap_or(false) {
                let active = waited["runs"].as_array().map(Vec::len).unwrap_or(0);
                format!(
                    "Timed out waiting for background run(s): {} run(s) still active. \
                     Wait again with subagent_wait (runs keep going) or inspect with \
                     subagent({{action:\"status\"}}); completion also arrives as a session message.",
                    active
                )
            } else {
                format!(
                    "Wait finished: {} run(s) reached a terminal state.",
                    waited["waited"].as_u64().unwrap_or(0)
                )
            };
            json!({
                "content": [{ "type": "text", "text": text }],
                "details": waited,
                "isError": false,
            })
        }
        Err(message) => json!({
            "content": [{ "type": "text", "text": message }],
            "isError": true,
        }),
    }
}

/// The `subagent_wait` partial-frame details: upstream `result()` carries
/// `mode:"management"` (subagent-wait.ts:309-319).
fn wait_update_details() -> Value {
    json!({ "mode": "management", "results": [] })
}

/// Integration-test probes (not part of the plugin surface): budget-ledger
/// access for deterministic rejection scenarios. Same shape as mcp-adapter's
/// test seam (kept ungated so integration tests share the process state).
#[doc(hidden)]
pub mod test_support {
    /// Exposed for integration tests of the skill-layout upgrade path
    /// (ADR-0021).
    pub use crate::prompts::install_orchestration_skill_at;

    /// Thin wrapper over the session spawn-budget ledger.
    pub struct SpawnBudgetLedgerProbe {
        ledger: crate::runner::background::SpawnBudgetLedger,
        session_id: String,
    }

    impl SpawnBudgetLedgerProbe {
        pub fn open(session_id: &str) -> Self {
            Self {
                ledger: crate::runner::background::SpawnBudgetLedger::open(session_id),
                session_id: session_id.to_string(),
            }
        }

        /// Remove the ledger file (test isolation).
        pub fn reset_for_test(&self) {
            let _ = std::fs::remove_file(
                crate::runner::background::spawn_budgets_dir()
                    .join(format!("{}.json", self.session_id)),
            );
            // The real filename is hashed; remove via the probe path list.
            // (SpawnBudgetLedger paths are hash-keyed; the reset removes
            // every file in the budgets dir for test isolation.)
            if let Ok(entries) = std::fs::read_dir(crate::runner::background::spawn_budgets_dir()) {
                for entry in entries.flatten() {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }

        pub fn reserve_for_test(&self, amount: u64, limit: Option<u64>) -> Result<(), String> {
            self.ledger.reserve(amount, limit)
        }
    }
}

/// Test seam: drive a tool execution against an installed state.
#[doc(hidden)]
pub fn execute_for_test(params: &Value) -> Value {
    dispatch_message(&json!({
        "kind": "toolExecute",
        "toolName": "subagent",
        "toolCallId": "test",
        "params": params,
    }))
}

/// Test seam: drive a named tool execution (subagent_wait, supervisor tools)
#[doc(hidden)]
/// against an installed state.
pub fn execute_tool_for_test(tool_name: &str, params: &Value) -> Value {
    dispatch_message(&json!({
        "kind": "toolExecute",
        "toolName": tool_name,
        "toolCallId": "test",
        "params": params,
    }))
}

/// Test seam: drive the message-render dispatch for a registered customType
/// (TE09 FR-D) — the same `{"kind":"render","what":"message"}` envelope the
/// host forwards through `registerMessageRenderer`.
#[doc(hidden)]
pub fn render_message_for_test(custom_type: &str, message: &Value, options: &Value) -> Value {
    let mut payload = serde_json::Map::new();
    payload.insert("customType".to_string(), json!(custom_type));
    if let Some(object) = message.as_object() {
        for (key, value) in object {
            payload.insert(key.clone(), value.clone());
        }
    }
    dispatch_message(&json!({
        "kind": "render",
        "what": "message",
        "message": Value::Object(payload),
        "options": options,
    }))
}

/// Test seam: drive the tool render dispatch (TE11 FR-A/FR-B) — the same
/// `{"kind":"render","what":"toolCall"|"toolResult"}` envelope the host
/// forwards through the registerTool render hooks.
#[doc(hidden)]
pub fn render_tool_for_test(
    what: &str,
    context_args: &Value,
    result: &Value,
    options: &Value,
) -> Value {
    dispatch_message(&json!({
        "kind": "render",
        "what": what,
        "context": { "args": context_args },
        "result": result,
        "options": options,
    }))
}

/// Test seam: install with a fake host (mcp-adapter `install_for_test`
#[doc(hidden)]
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
        /// Supervisor channel dir (FR-P1-10); None clears the env.
        pub supervisor_channel: Option<PathBuf>,
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
            supervisor_channel: None,
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
            let _ = self.process_line(line, crate::artifacts::now_millis());
        }

        pub fn messages_public(&self) -> &[serde_json::Value] {
            &self.messages
        }
    }
}
