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

/// Host bridge the tool layer uses (cwd, parent model, parent session file).
pub trait HostContext {
    fn cwd(&self) -> PathBuf;
    fn parent_model(&self) -> Option<String>;
    fn parent_session_file(&self, settings: &config::SettingsPair) -> Option<PathBuf>;
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
            for (name, description) in commands::command_definitions() {
                if let Err(error) = register(
                    "registerCommand",
                    json!({ "name": name, "description": description }),
                ) {
                    return json!({"error": {"kind": "init", "message": error.to_string()}});
                }
            }
            for event in ["session_start", "session_shutdown"] {
                if let Err(error) = register("on", json!({ "event": event })) {
                    return json!({"error": {"kind": "init", "message": error.to_string()}});
                }
            }
            // Startup artifact cleanup (index.ts:371-372).
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

/// Host-dispatched calls (`rpi_dispatch`): toolExecute / command / event.
fn dispatch_message(message: &Value) -> Value {
    let Some(state) = STATE.get() else {
        return Value::Null;
    };
    match message.get("kind").and_then(Value::as_str) {
        Some("toolExecute") => {
            let params = message.get("params").cloned().unwrap_or(Value::Null);
            let host = HostCallsContext {
                calls: &state.calls,
                cookie: state.cookie,
            };
            let settings = config::read_settings_pair(&host.cwd());
            let config = config::load_config();
            let result =
                tool::execute_subagent_tool(&params, &host, &settings, &config, &state.runtime);
            // Child-safe mode blocks mutating management actions; P0's four
            // actions are read-only, so the restriction is a forward guard.
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
            result
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
                    // Sweep live children (SIGTERM → 3s → SIGKILL).
                    state
                        .runtime
                        .spawn(runner::foreground::kill_all_children_for_shutdown());
                    Value::Null
                }
                _ => Value::Null,
            }
        }
        _ => Value::Null,
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
