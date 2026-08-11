//! RPC mode: headless JSON protocol over stdin/stdout.
//!
//! Port of `packages/coding-agent/src/modes/rpc/{rpc-mode,rpc-types,jsonl}.ts`
//! @ pi 0.82.1 (2efa728), contract-anchored to `docs/rpc.md`, with the event
//! emission path updated to pi 0.84.1+ (4181f66, T18): wire events pass
//! through `to_json_event` (delta-only `message_update`, docs/rpc.md:952-956)
//! and stdout writes go through a single blocking writer with backpressure
//! (`core::output_guard`, mapping `writeRawStdout` +
//! `waitForRawStdoutBackpressure`).
//!
//! Protocol:
//! - Commands: JSON objects on stdin, strict LF framing (`jsonl.ts`: split on
//!   `\n` only — U+2028/U+2029 stay inside records — tolerate a trailing
//!   `\r`, flush a non-empty tail at EOF).
//! - Responses: `{id?, type:"response", command, success, data?|error?}`.
//! - Events: `AgentSessionEvent` objects streamed as they occur.
//!
//! Extension UI sub-protocol (T15 seam): the request method names and the
//! degraded-method list are pinned as constants, and incoming
//! `extension_ui_response` frames are routed to a pending-request map. With
//! the no-op `ExtensionRunner` no requests are ever emitted.

use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};

use serde::Deserialize;
use serde_json::{json, Map, Value};
use tokio::io::{AsyncBufRead, AsyncBufReadExt};
use tokio::sync::mpsc;

use rpi_agent::types::{QueueMode, ThinkingLevel};
use rpi_ai::types::ImageContent;

use crate::config::CONFIG_DIR_NAME;
use crate::core::agent_session::{
    AgentSession, AgentSessionEvent, CycleDirection, ExtensionBindings, PromptOptions, SessionEvent,
};
use crate::core::agent_session_runtime::{AgentSessionRuntime, ForkPosition};
use crate::core::extensions::{ExtensionMode, InputSource, StreamingBehavior};
use crate::core::output_guard::RawStdout;
use crate::core::prompt_templates::PromptTemplate;
use crate::core::skills::{SourceInfo, SourceOrigin, SourceScope};
use crate::error::RpiError;
use crate::modes::json_event::to_json_event;

pub mod ui_bridge;

// ============================================================================
// Extension UI protocol reservation (rpc-types.ts RpcExtensionUIRequest)
// ============================================================================

/// Dialog methods: emit a request and block until the client answers with an
/// `extension_ui_response` carrying the matching `id`.
pub const EXTENSION_UI_DIALOG_METHODS: [&str; 4] = ["select", "confirm", "input", "editor"];

/// Fire-and-forget methods: emit a request, no response expected.
pub const EXTENSION_UI_FIRE_AND_FORGET_METHODS: [&str; 5] = [
    "notify",
    "setStatus",
    "setWidget",
    "setTitle",
    "set_editor_text",
];

/// `ExtensionUIContext` methods not supported / degraded in RPC mode
/// (docs/rpc.md "Extension UI Protocol"; rpc-mode.ts:162-309):
/// `custom()` → undefined; the working-indicator/footer/header/editor-
/// component/autocomplete setters are no-ops; `getEditorComponent()` →
/// undefined; `onTerminalInput()` → no-op unsubscribe; `getEditorText()` → "";
/// `getToolsExpanded()` → false; `pasteToEditor()` delegates to
/// `setEditorText()`; `getAllThemes()` → []; `getTheme()` → undefined;
/// `setTheme()` → `{success:false}`.
pub const EXTENSION_UI_DEGRADED_METHODS: [&str; 18] = [
    "custom",
    "setWorkingMessage",
    "setWorkingIndicator",
    "setWorkingVisible",
    "setHiddenThinkingLabel",
    "setFooter",
    "setHeader",
    "setEditorComponent",
    "getEditorComponent",
    "onTerminalInput",
    "setToolsExpanded",
    "addAutocompleteProvider",
    "getEditorText",
    "getToolsExpanded",
    "pasteToEditor",
    "getAllThemes",
    "getTheme",
    "setTheme",
];

// ============================================================================
// Commands (rpc-types.ts RpcCommand — 32 variants)
// ============================================================================

/// `RpcCommand` (rpc-types.ts:20-73). Unknown `type` values never reach this
/// enum: [`parse_command`] rejects them with the upstream
/// `Unknown command: <type>` error first.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RpcCommand {
    // Prompting
    Prompt {
        id: Option<String>,
        message: String,
        images: Option<Vec<ImageContent>>,
        #[serde(rename = "streamingBehavior")]
        streaming_behavior: Option<StreamingBehavior>,
    },
    Steer {
        id: Option<String>,
        message: String,
        images: Option<Vec<ImageContent>>,
    },
    FollowUp {
        id: Option<String>,
        message: String,
        images: Option<Vec<ImageContent>>,
    },
    Abort {
        id: Option<String>,
    },
    NewSession {
        id: Option<String>,
        #[serde(rename = "parentSession")]
        parent_session: Option<String>,
    },

    // State
    GetState {
        id: Option<String>,
    },

    // Model
    SetModel {
        id: Option<String>,
        provider: String,
        #[serde(rename = "modelId")]
        model_id: String,
    },
    CycleModel {
        id: Option<String>,
    },
    GetAvailableModels {
        id: Option<String>,
    },

    // Thinking
    SetThinkingLevel {
        id: Option<String>,
        level: ThinkingLevel,
    },
    CycleThinkingLevel {
        id: Option<String>,
    },
    GetAvailableThinkingLevels {
        id: Option<String>,
    },

    // Queue modes
    SetSteeringMode {
        id: Option<String>,
        mode: QueueMode,
    },
    SetFollowUpMode {
        id: Option<String>,
        mode: QueueMode,
    },

    // Compaction
    Compact {
        id: Option<String>,
        #[serde(rename = "customInstructions")]
        custom_instructions: Option<String>,
    },
    SetAutoCompaction {
        id: Option<String>,
        enabled: bool,
    },

    // Retry
    SetAutoRetry {
        id: Option<String>,
        enabled: bool,
    },
    AbortRetry {
        id: Option<String>,
    },

    // Bash
    Bash {
        id: Option<String>,
        command: String,
        #[serde(rename = "excludeFromContext")]
        exclude_from_context: Option<bool>,
    },
    AbortBash {
        id: Option<String>,
    },

    // Session
    GetSessionStats {
        id: Option<String>,
    },
    ExportHtml {
        id: Option<String>,
        #[serde(rename = "outputPath")]
        output_path: Option<String>,
    },
    SwitchSession {
        id: Option<String>,
        #[serde(rename = "sessionPath")]
        session_path: String,
    },
    Fork {
        id: Option<String>,
        #[serde(rename = "entryId")]
        entry_id: String,
    },
    Clone {
        id: Option<String>,
    },
    GetForkMessages {
        id: Option<String>,
    },
    GetEntries {
        id: Option<String>,
        since: Option<String>,
    },
    GetTree {
        id: Option<String>,
    },
    GetLastAssistantText {
        id: Option<String>,
    },
    SetSessionName {
        id: Option<String>,
        name: String,
    },

    // Messages
    GetMessages {
        id: Option<String>,
    },

    // Commands
    GetCommands {
        id: Option<String>,
    },
}

impl RpcCommand {
    fn id(&self) -> &Option<String> {
        match self {
            RpcCommand::Prompt { id, .. }
            | RpcCommand::Steer { id, .. }
            | RpcCommand::FollowUp { id, .. }
            | RpcCommand::Abort { id }
            | RpcCommand::NewSession { id, .. }
            | RpcCommand::GetState { id }
            | RpcCommand::SetModel { id, .. }
            | RpcCommand::CycleModel { id }
            | RpcCommand::GetAvailableModels { id }
            | RpcCommand::SetThinkingLevel { id, .. }
            | RpcCommand::CycleThinkingLevel { id }
            | RpcCommand::GetAvailableThinkingLevels { id }
            | RpcCommand::SetSteeringMode { id, .. }
            | RpcCommand::SetFollowUpMode { id, .. }
            | RpcCommand::Compact { id, .. }
            | RpcCommand::SetAutoCompaction { id, .. }
            | RpcCommand::SetAutoRetry { id, .. }
            | RpcCommand::AbortRetry { id }
            | RpcCommand::Bash { id, .. }
            | RpcCommand::AbortBash { id }
            | RpcCommand::GetSessionStats { id }
            | RpcCommand::ExportHtml { id, .. }
            | RpcCommand::SwitchSession { id, .. }
            | RpcCommand::Fork { id, .. }
            | RpcCommand::Clone { id }
            | RpcCommand::GetForkMessages { id }
            | RpcCommand::GetEntries { id, .. }
            | RpcCommand::GetTree { id }
            | RpcCommand::GetLastAssistantText { id }
            | RpcCommand::SetSessionName { id, .. }
            | RpcCommand::GetMessages { id }
            | RpcCommand::GetCommands { id } => id,
        }
    }

    /// Wire `type` string (for response `command` fields).
    fn type_str(&self) -> &'static str {
        match self {
            RpcCommand::Prompt { .. } => "prompt",
            RpcCommand::Steer { .. } => "steer",
            RpcCommand::FollowUp { .. } => "follow_up",
            RpcCommand::Abort { .. } => "abort",
            RpcCommand::NewSession { .. } => "new_session",
            RpcCommand::GetState { .. } => "get_state",
            RpcCommand::SetModel { .. } => "set_model",
            RpcCommand::CycleModel { .. } => "cycle_model",
            RpcCommand::GetAvailableModels { .. } => "get_available_models",
            RpcCommand::SetThinkingLevel { .. } => "set_thinking_level",
            RpcCommand::CycleThinkingLevel { .. } => "cycle_thinking_level",
            RpcCommand::GetAvailableThinkingLevels { .. } => "get_available_thinking_levels",
            RpcCommand::SetSteeringMode { .. } => "set_steering_mode",
            RpcCommand::SetFollowUpMode { .. } => "set_follow_up_mode",
            RpcCommand::Compact { .. } => "compact",
            RpcCommand::SetAutoCompaction { .. } => "set_auto_compaction",
            RpcCommand::SetAutoRetry { .. } => "set_auto_retry",
            RpcCommand::AbortRetry { .. } => "abort_retry",
            RpcCommand::Bash { .. } => "bash",
            RpcCommand::AbortBash { .. } => "abort_bash",
            RpcCommand::GetSessionStats { .. } => "get_session_stats",
            RpcCommand::ExportHtml { .. } => "export_html",
            RpcCommand::SwitchSession { .. } => "switch_session",
            RpcCommand::Fork { .. } => "fork",
            RpcCommand::Clone { .. } => "clone",
            RpcCommand::GetForkMessages { .. } => "get_fork_messages",
            RpcCommand::GetEntries { .. } => "get_entries",
            RpcCommand::GetTree { .. } => "get_tree",
            RpcCommand::GetLastAssistantText { .. } => "get_last_assistant_text",
            RpcCommand::SetSessionName { .. } => "set_session_name",
            RpcCommand::GetMessages { .. } => "get_messages",
            RpcCommand::GetCommands { .. } => "get_commands",
        }
    }
}

/// Known command `type` strings (for the `Unknown command` pre-check).
const KNOWN_COMMAND_TYPES: [&str; 32] = [
    "prompt",
    "steer",
    "follow_up",
    "abort",
    "new_session",
    "get_state",
    "set_model",
    "cycle_model",
    "get_available_models",
    "set_thinking_level",
    "cycle_thinking_level",
    "get_available_thinking_levels",
    "set_steering_mode",
    "set_follow_up_mode",
    "compact",
    "set_auto_compaction",
    "set_auto_retry",
    "abort_retry",
    "bash",
    "abort_bash",
    "get_session_stats",
    "export_html",
    "switch_session",
    "fork",
    "clone",
    "get_fork_messages",
    "get_entries",
    "get_tree",
    "get_last_assistant_text",
    "set_session_name",
    "get_messages",
    "get_commands",
];

/// Outcome of parsing one input line.
enum ParsedLine {
    /// A valid command.
    Command(RpcCommand),
    /// An `extension_ui_response` frame (routed to pending dialog requests).
    ExtensionUiResponse(Value),
    /// Error response to emit (already serialized).
    Error(Value),
}

/// Render a JSON value the way upstream's template literal would
/// (`Unknown command: ${unknownCommand.type}`).
fn js_render(value: Option<&Value>) -> String {
    match value {
        None => "undefined".to_owned(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

/// `serializeJsonLine` (jsonl.ts:10-12): one JSON value + `\n`.
fn serialize_json_line(value: &Value) -> String {
    let mut line = serde_json::to_string(value).unwrap_or_else(|_| "null".to_owned());
    line.push('\n');
    line
}

/// `handleInputLine` parse stage (rpc-mode.ts:732-762).
fn parse_command(line: &str) -> ParsedLine {
    let parsed: Value = match serde_json::from_str(line) {
        Ok(value) => value,
        Err(error) => {
            // `command: "parse"` parse failure (rpc-mode.ts:736-746).
            return ParsedLine::Error(error_response(
                &None,
                "parse",
                &format!("Failed to parse command: {error}"),
            ));
        }
    };

    if parsed.get("type").and_then(Value::as_str) == Some("extension_ui_response") {
        return ParsedLine::ExtensionUiResponse(parsed);
    }

    let type_value = parsed.get("type");
    let id = parsed.get("id").and_then(Value::as_str).map(str::to_owned);

    let Some(type_str) = type_value.and_then(Value::as_str).map(str::to_owned) else {
        // Upstream `error(id, unknownCommand.type, ...)`: a missing type
        // omits the `command` key; a non-string type echoes verbatim.
        return ParsedLine::Error(error_response_raw_command(
            &id,
            type_value,
            &format!("Unknown command: {}", js_render(type_value)),
        ));
    };
    if !KNOWN_COMMAND_TYPES.contains(&type_str.as_str()) {
        // rpc-mode.ts:695-698 default branch.
        return ParsedLine::Error(error_response(
            &id,
            &type_str,
            &format!("Unknown command: {type_str}"),
        ));
    }

    match serde_json::from_value::<RpcCommand>(parsed) {
        Ok(command) => ParsedLine::Command(command),
        // Malformed fields for a known command type. Upstream casts without
        // validation and fails inside the handler with a JS TypeError; rpi
        // rejects at the boundary (engine-level error text, same
        // success:false shape).
        Err(error) => ParsedLine::Error(error_response(
            &id,
            &type_str,
            &format!("Invalid command: {error}"),
        )),
    }
}

// ============================================================================
// Responses (rpc-types.ts RpcResponse)
// ============================================================================

/// `{id?, type:"response", command, success:true, data?}` — `data` is
/// `Some(Value::Null)` for the explicit-`null` cases (cycle_model /
/// cycle_thinking_level), omitted when `None` (rpc-mode.ts:63-72).
fn success_response(id: &Option<String>, command: &str, data: Option<Value>) -> Value {
    let mut object = Map::new();
    if let Some(id) = id {
        object.insert("id".to_owned(), Value::String(id.clone()));
    }
    object.insert("type".to_owned(), Value::String("response".to_owned()));
    object.insert("command".to_owned(), Value::String(command.to_owned()));
    object.insert("success".to_owned(), Value::Bool(true));
    if let Some(data) = data {
        object.insert("data".to_owned(), data);
    }
    Value::Object(object)
}

/// `{id?, type:"response", command, success:false, error}` (rpc-mode.ts:74-76).
fn error_response(id: &Option<String>, command: &str, message: &str) -> Value {
    let mut object = Map::new();
    if let Some(id) = id {
        object.insert("id".to_owned(), Value::String(id.clone()));
    }
    object.insert("type".to_owned(), Value::String("response".to_owned()));
    object.insert("command".to_owned(), Value::String(command.to_owned()));
    object.insert("success".to_owned(), Value::Bool(false));
    object.insert("error".to_owned(), Value::String(message.to_owned()));
    Value::Object(object)
}

/// Raw-`command` variant: upstream passes `unknownCommand.type` through, so
/// `undefined` drops the key and non-strings echo verbatim (rpc-mode.ts:695-698).
fn error_response_raw_command(
    id: &Option<String>,
    command: Option<&Value>,
    message: &str,
) -> Value {
    let mut object = Map::new();
    if let Some(id) = id {
        object.insert("id".to_owned(), Value::String(id.clone()));
    }
    object.insert("type".to_owned(), Value::String("response".to_owned()));
    if let Some(command) = command {
        object.insert("command".to_owned(), command.clone());
    }
    object.insert("success".to_owned(), Value::Bool(false));
    object.insert("error".to_owned(), Value::String(message.to_owned()));
    Value::Object(object)
}

/// Raw error message without the `RpiError` Display prefix (upstream sends
/// `error.message` verbatim).
fn error_message(error: &RpiError) -> String {
    error.raw_message()
}

/// `BashResult` wire shape (bash-executor.ts:29-40): optional fields are
/// omitted when absent (`exitCode` when killed, `fullOutputPath` when not
/// truncated).
fn bash_result_json(result: &crate::tools::bash_executor::BashResult) -> Value {
    let mut object = Map::new();
    object.insert("output".to_owned(), Value::String(result.output.clone()));
    if let Some(exit_code) = result.exit_code {
        object.insert("exitCode".to_owned(), json!(exit_code));
    }
    object.insert("cancelled".to_owned(), Value::Bool(result.cancelled));
    object.insert("truncated".to_owned(), Value::Bool(result.truncated));
    if let Some(path) = &result.full_output_path {
        object.insert(
            "fullOutputPath".to_owned(),
            Value::String(path.to_string_lossy().into_owned()),
        );
    }
    Value::Object(object)
}

/// `SessionTreeNode` wire shape (session-manager.ts:159-166): entries are
/// serialized from their raw JSON object (persistence source of truth);
/// `label`/`labelTimestamp` are omitted when absent.
fn tree_node_json(node: &crate::core::session_manager::SessionTreeNode) -> Value {
    let mut object = Map::new();
    object.insert("entry".to_owned(), node.entry.raw_value().clone());
    object.insert(
        "children".to_owned(),
        Value::Array(node.children.iter().map(tree_node_json).collect()),
    );
    if let Some(label) = &node.label {
        object.insert("label".to_owned(), Value::String(label.clone()));
    }
    if let Some(label_timestamp) = &node.label_timestamp {
        object.insert(
            "labelTimestamp".to_owned(),
            Value::String(label_timestamp.clone()),
        );
    }
    Value::Object(object)
}

/// `RpcSessionState` (rpc-types.ts:95-108; rpc-mode.ts:445-461). Optional
/// fields (`model`, `sessionFile`, `sessionName`) are omitted when unset,
/// matching `JSON.stringify` on `undefined`.
fn session_state_json(session: &AgentSession) -> Value {
    let mut object = Map::new();
    if let Some(model) = session.model() {
        object.insert(
            "model".to_owned(),
            serde_json::to_value(&model).unwrap_or(Value::Null),
        );
    }
    object.insert(
        "thinkingLevel".to_owned(),
        serde_json::to_value(session.thinking_level()).unwrap_or(Value::Null),
    );
    object.insert(
        "isStreaming".to_owned(),
        Value::Bool(session.is_streaming()),
    );
    object.insert(
        "isCompacting".to_owned(),
        Value::Bool(session.is_compacting()),
    );
    object.insert(
        "steeringMode".to_owned(),
        serde_json::to_value(session.steering_mode()).unwrap_or(Value::Null),
    );
    object.insert(
        "followUpMode".to_owned(),
        serde_json::to_value(session.follow_up_mode()).unwrap_or(Value::Null),
    );
    if let Some(session_file) = session.session_file() {
        object.insert(
            "sessionFile".to_owned(),
            Value::String(session_file.to_string_lossy().into_owned()),
        );
    }
    object.insert("sessionId".to_owned(), Value::String(session.session_id()));
    if let Some(name) = session.session_name() {
        object.insert("sessionName".to_owned(), Value::String(name));
    }
    object.insert(
        "autoCompactionEnabled".to_owned(),
        Value::Bool(session.auto_compaction_enabled()),
    );
    object.insert("messageCount".to_owned(), json!(session.messages().len()));
    object.insert(
        "pendingMessageCount".to_owned(),
        json!(session.pending_message_count()),
    );
    Value::Object(object)
}

/// Prompt-template `SourceInfo` (prompt-templates.ts `getSourceInfo`,
/// :213-233): synthetic local/top-level info scoped by the default prompt
/// directories. rpi's `PromptTemplate` carries no `sourceInfo` (D-014 §7);
/// the RPC layer reconstructs it from the file path.
pub(crate) fn prompt_template_source_info(
    template: &PromptTemplate,
    agent_dir: &Path,
    cwd: &Path,
) -> SourceInfo {
    let global_prompts_dir = agent_dir.join("prompts");
    let project_prompts_dir = cwd.join(CONFIG_DIR_NAME).join("prompts");
    let file_path = &template.file_path;
    let (scope, base_dir) = if file_path.starts_with(&global_prompts_dir) {
        (SourceScope::User, global_prompts_dir)
    } else if file_path.starts_with(&project_prompts_dir) {
        (SourceScope::Project, project_prompts_dir)
    } else {
        (
            SourceScope::Temporary,
            file_path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_default(),
        )
    };
    SourceInfo {
        path: file_path.clone(),
        source: "local".to_owned(),
        scope,
        origin: SourceOrigin::TopLevel,
        base_dir: Some(base_dir),
    }
}

// ============================================================================
// Shared mode state
// ============================================================================

/// State shared between the input loop, spawned command handlers and the
/// rebind callback.
struct RpcState {
    /// Current session; swapped by the rebind callback on session
    /// replacement (new/fork/switch/clone).
    session: RwLock<AgentSession>,
    /// Unsubscribe handle of the current event subscription.
    unsubscribe: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Pending extension UI dialog requests (T15 W4): shared with the
    /// [`ui_bridge::RpcUiBridge`]; `extension_ui_response` frames resolve
    /// these.
    pending_ui: ui_bridge::PendingUiTable,
    /// Set by the (T15) extension `ctx.shutdown()` handler; honored on the
    /// next `agent_settled` event (rpc-mode.ts:727-730).
    shutdown_requested: AtomicBool,
    /// Single ordered stdout write path for responses + events (upstream
    /// `writeRawStdout`'s promise chain, output-guard.ts:85-93). The blocking
    /// write doubles as backpressure: producers wait while the consumer's
    /// pipe is full instead of piling lines into an unbounded buffer (T18).
    ///
    /// Trade-off (coding-standards §6.1, deliberate): a blocking `Write` call
    /// inside an async task parks its tokio worker until the fd drains.
    /// Upstream stalls the exact same producers — the agent loop's listener
    /// barrier awaits `waitForRawStdoutBackpressure` (rpc-mode.ts:361-363)
    /// and each command handler awaits it after writing its response
    /// (rpc-mode.ts:785,796) — so blocking here is behaviorally identical,
    /// and no deadlock is possible because progress depends only on the
    /// external reader, never on another tokio task.
    output: RawStdout,
    /// Shutdown trigger for the main loop.
    shutdown: mpsc::UnboundedSender<i32>,
}

impl RpcState {
    fn session(&self) -> AgentSession {
        self.session
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn emit_value(&self, value: &Value) {
        self.output.write(&serialize_json_line(value));
    }
}

/// `rebindSession` (rpc-mode.ts:316-363): bind extensions on the given
/// session, then (re)subscribe its event stream to stdout. Used for the
/// initial bind at startup and after every session replacement.
async fn rebind_session(
    state: &Arc<RpcState>,
    session: AgentSession,
    runtime: &Weak<tokio::sync::Mutex<AgentSessionRuntime>>,
) {
    let error_output = state.output.clone();
    // `ctx.shutdown()` → flag honored at the next `agent_settled`
    // (rpc-mode.ts:727-730).
    let shutdown_state = Arc::downgrade(state);
    session
        .bind_extensions(ExtensionBindings {
            mode: Some(ExtensionMode::Rpc),
            shutdown: Some(Arc::new(move || {
                if let Some(state) = shutdown_state.upgrade() {
                    state.shutdown_requested.store(true, Ordering::SeqCst);
                }
            })),
            on_error: Some(Arc::new(move |error| {
                error_output.write(&serialize_json_line(&json!({
                    "type": "extension_error",
                    "extensionPath": error.extension_path,
                    "event": error.event,
                    "error": error.error,
                })));
            })),
        })
        .await;

    // `bindCommandContext` (runner.ts:410-418): runtime-backed command
    // actions on the session's host (Weak — the runtime owns this closure's
    // inverse path).
    if let Some(runtime) = runtime.upgrade() {
        if let Some(host) =
            crate::core::extension_host_adapter::host_of_runner(&session.extension_runner())
        {
            host.runtime().set_command_actions(Some(Arc::new(
                crate::core::extension_context::RuntimeCommandActions::new(&runtime),
            )));
        }
    }

    let event_output = state.output.clone();
    let state_weak = Arc::downgrade(state);
    let unsubscribe = session.subscribe(Arc::new(move |event: AgentSessionEvent| {
        let is_settled = matches!(
            &event,
            AgentSessionEvent::Session(SessionEvent::AgentSettled)
        );
        // `output(toJsonEvent(event))` (rpc-mode.ts:355-356, T18): the wire
        // event is delta-only — `to_json_event` strips the cumulative
        // `message`/`partial` snapshots (docs/rpc.md:952-956). The blocking
        // write runs inside the agent loop's ordered listener barrier, so a
        // slow consumer stalls the event source (rpc-mode.ts:361-363).
        if let Ok(mut line) = serde_json::to_string(&to_json_event(&event)) {
            line.push('\n');
            event_output.write(&line);
        }
        // `checkShutdownRequested` on agent_settled (rpc-mode.ts:357-359).
        if is_settled {
            if let Some(state) = state_weak.upgrade() {
                if state.shutdown_requested.load(Ordering::SeqCst) {
                    let _ = state.shutdown.send(0);
                }
            }
        }
    }));

    {
        let mut current = state.session.write().unwrap_or_else(|e| e.into_inner());
        *current = session;
    }
    if let Some(previous) = state
        .unsubscribe
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .replace(unsubscribe)
    {
        previous();
    }
}

// ============================================================================
// Command handlers
// ============================================================================

/// `handleCommand` (rpc-mode.ts:385-700). Returns `Ok(Some(data))` for a
/// success response with data, `Ok(None)` for a bare success response, and
/// `Err(message)` for an error response. The `prompt` arm spawns the real
/// work and reports through the preflight observer; [`handle_command`]
/// skips the shared response for it.
async fn dispatch(
    state: Arc<RpcState>,
    runtime: Arc<tokio::sync::Mutex<AgentSessionRuntime>>,
    command: RpcCommand,
) -> Result<Option<Value>, String> {
    match command {
        // ---------------- Prompting ----------------
        RpcCommand::Prompt {
            id,
            message,
            images,
            streaming_behavior,
        } => {
            // Async acceptance: the authoritative response is emitted by the
            // preflight observer; post-acceptance failures surface through
            // the event stream (rpc-mode.ts:393-415, docs/rpc.md §prompt).
            let session = state.session();
            let accepted = Arc::new(AtomicBool::new(false));
            let accepted_observer = accepted.clone();
            let output = state.output.clone();
            let response_id = id.clone();
            let prompt_output = state.output.clone();
            tokio::spawn(async move {
                let result = session
                    .prompt(
                        &message,
                        PromptOptions {
                            images,
                            streaming_behavior,
                            source: Some(InputSource::Rpc),
                            preflight_result: Some(Box::new(move |did_succeed| {
                                if did_succeed {
                                    accepted_observer.store(true, Ordering::SeqCst);
                                    output.write(&serialize_json_line(&success_response(
                                        &response_id,
                                        "prompt",
                                        None,
                                    )));
                                }
                            })),
                            ..Default::default()
                        },
                    )
                    .await;
                if let Err(error) = result {
                    if !accepted.load(Ordering::SeqCst) {
                        prompt_output.write(&serialize_json_line(&error_response(
                            &id,
                            "prompt",
                            &error_message(&error),
                        )));
                    }
                }
            });
            Ok(None)
        }
        RpcCommand::Steer {
            message, images, ..
        } => state
            .session()
            .steer(&message, images)
            .await
            .map(|_| None)
            .map_err(|error| error_message(&error)),
        RpcCommand::FollowUp {
            message, images, ..
        } => state
            .session()
            .follow_up(&message, images)
            .await
            .map(|_| None)
            .map_err(|error| error_message(&error)),
        RpcCommand::Abort { .. } => {
            state.session().abort().await;
            Ok(None)
        }
        RpcCommand::NewSession { parent_session, .. } => {
            let cancelled = runtime
                .lock()
                .await
                .new_session(parent_session.as_deref(), None, None)
                .await
                .map_err(|error| error_message(&error))?;
            Ok(Some(json!({ "cancelled": cancelled })))
        }

        // ---------------- State ----------------
        RpcCommand::GetState { .. } => Ok(Some(session_state_json(&state.session()))),

        // ---------------- Model ----------------
        RpcCommand::SetModel {
            provider, model_id, ..
        } => {
            let session = state.session();
            let models = session
                .model_runtime()
                .get_available(None)
                .await
                .map_err(|error| error.message)?;
            let Some(model) = models
                .into_iter()
                .find(|model| model.provider == provider && model.id == model_id)
            else {
                return Err(format!("Model not found: {provider}/{model_id}"));
            };
            session
                .set_model(model.clone())
                .await
                .map_err(|error| error_message(&error))?;
            Ok(Some(serde_json::to_value(&model).unwrap_or(Value::Null)))
        }
        RpcCommand::CycleModel { .. } => {
            let result = state
                .session()
                .cycle_model(CycleDirection::Forward)
                .await
                .map_err(|error| error_message(&error))?;
            Ok(Some(match result {
                None => Value::Null,
                Some(result) => json!({
                    "model": result.model,
                    "thinkingLevel": result.thinking_level,
                    "isScoped": result.is_scoped,
                }),
            }))
        }
        RpcCommand::GetAvailableModels { .. } => {
            let models = state
                .session()
                .model_runtime()
                .get_available(None)
                .await
                .map_err(|error| error.message)?;
            Ok(Some(json!({ "models": models })))
        }

        // ---------------- Thinking ----------------
        RpcCommand::SetThinkingLevel { level, .. } => {
            state.session().set_thinking_level(level);
            Ok(None)
        }
        RpcCommand::CycleThinkingLevel { .. } => {
            Ok(Some(match state.session().cycle_thinking_level() {
                None => Value::Null,
                Some(level) => json!({ "level": level }),
            }))
        }
        RpcCommand::GetAvailableThinkingLevels { .. } => {
            let levels = state.session().get_available_thinking_levels();
            Ok(Some(json!({ "levels": levels })))
        }

        // ---------------- Queue modes ----------------
        RpcCommand::SetSteeringMode { mode, .. } => {
            state.session().set_steering_mode(mode);
            Ok(None)
        }
        RpcCommand::SetFollowUpMode { mode, .. } => {
            state.session().set_follow_up_mode(mode);
            Ok(None)
        }

        // ---------------- Compaction ----------------
        RpcCommand::Compact {
            custom_instructions,
            ..
        } => {
            let result = state
                .session()
                .compact(custom_instructions.as_deref())
                .await
                .map_err(|error| error_message(&error))?;
            Ok(Some(serde_json::to_value(&result).unwrap_or(Value::Null)))
        }
        RpcCommand::SetAutoCompaction { enabled, .. } => {
            state.session().set_auto_compaction_enabled(enabled);
            Ok(None)
        }

        // ---------------- Retry ----------------
        RpcCommand::SetAutoRetry { enabled, .. } => {
            state.session().set_auto_retry_enabled(enabled);
            Ok(None)
        }
        RpcCommand::AbortRetry { .. } => {
            state.session().abort_retry();
            Ok(None)
        }

        // ---------------- Bash ----------------
        RpcCommand::Bash {
            id,
            command: bash_command,
            exclude_from_context,
        } => {
            let result = state
                .session()
                .execute_bash(
                    &bash_command,
                    crate::core::agent_session::ExecuteBashOptions {
                        exclude_from_context: exclude_from_context.unwrap_or(false),
                        id,
                        on_chunk: None,
                    },
                )
                .await
                .map_err(|error| error_message(&error))?;
            Ok(Some(bash_result_json(&result)))
        }
        RpcCommand::AbortBash { .. } => {
            state.session().abort_bash();
            Ok(None)
        }

        // ---------------- Session ----------------
        RpcCommand::GetSessionStats { .. } => {
            let stats = state.session().get_session_stats();
            Ok(Some(serde_json::to_value(&stats).unwrap_or(Value::Null)))
        }
        RpcCommand::ExportHtml { output_path, .. } => {
            let path = state
                .session()
                .export_to_html(output_path.as_deref())
                .map_err(|error| error_message(&error))?;
            Ok(Some(json!({ "path": path })))
        }
        RpcCommand::SwitchSession { session_path, .. } => {
            let cancelled = runtime
                .lock()
                .await
                .switch_session(&session_path, None, None, None)
                .await
                .map_err(|error| error_message(&error))?;
            Ok(Some(json!({ "cancelled": cancelled })))
        }
        RpcCommand::Fork { entry_id, .. } => {
            let result = runtime
                .lock()
                .await
                .fork(&entry_id, ForkPosition::Before, None)
                .await
                .map_err(|error| error_message(&error))?;
            let mut data = Map::new();
            if let Some(text) = result.selected_text {
                data.insert("text".to_owned(), Value::String(text));
            }
            data.insert("cancelled".to_owned(), Value::Bool(result.cancelled));
            Ok(Some(Value::Object(data)))
        }
        RpcCommand::Clone { .. } => {
            let session = state.session();
            let leaf_id = {
                let manager = session.session_manager();
                let manager = manager.lock().unwrap_or_else(|e| e.into_inner());
                manager.get_leaf_id().map(str::to_owned)
            };
            let Some(leaf_id) = leaf_id else {
                return Err("Cannot clone session: no current entry selected".to_owned());
            };
            let cancelled = runtime
                .lock()
                .await
                .fork(&leaf_id, ForkPosition::At, None)
                .await
                .map_err(|error| error_message(&error))?
                .cancelled;
            Ok(Some(json!({ "cancelled": cancelled })))
        }
        RpcCommand::GetForkMessages { .. } => {
            let messages: Vec<Value> = state
                .session()
                .get_user_messages_for_forking()
                .into_iter()
                .map(|(entry_id, text)| json!({ "entryId": entry_id, "text": text }))
                .collect();
            Ok(Some(json!({ "messages": messages })))
        }
        RpcCommand::GetEntries { since, .. } => {
            let session = state.session();
            let manager = session.session_manager();
            let (entries, leaf_id) = {
                let manager = manager.lock().unwrap_or_else(|e| e.into_inner());
                let mut entries = manager.get_entries();
                if let Some(since) = &since {
                    let Some(index) = entries.iter().position(|entry| entry.id() == since) else {
                        return Err(format!("Entry not found: {since}"));
                    };
                    entries = entries.split_off(index + 1);
                }
                let leaf_id = manager.get_leaf_id().map(str::to_owned);
                (entries, leaf_id)
            };
            Ok(Some(json!({
                "entries": entries.iter().map(|entry| entry.raw_value().clone()).collect::<Vec<_>>(),
                "leafId": leaf_id,
            })))
        }
        RpcCommand::GetTree { .. } => {
            let session = state.session();
            let manager = session.session_manager();
            let (tree, leaf_id) = {
                let manager = manager.lock().unwrap_or_else(|e| e.into_inner());
                (manager.get_tree(), manager.get_leaf_id().map(str::to_owned))
            };
            Ok(Some(json!({
                "tree": tree.iter().map(tree_node_json).collect::<Vec<_>>(),
                "leafId": leaf_id,
            })))
        }
        RpcCommand::GetLastAssistantText { .. } => {
            let text = state.session().get_last_assistant_text();
            Ok(Some(json!({ "text": text })))
        }
        RpcCommand::SetSessionName { name, .. } => {
            let name = name.trim();
            if name.is_empty() {
                return Err("Session name cannot be empty".to_owned());
            }
            state.session().set_session_name(name);
            Ok(None)
        }

        // ---------------- Messages ----------------
        RpcCommand::GetMessages { .. } => {
            let messages = state.session().messages();
            Ok(Some(json!({ "messages": messages })))
        }

        // ---------------- Commands ----------------
        RpcCommand::GetCommands { .. } => {
            let session = state.session();
            // Extension commands (with `:N` invocation names) → prompt
            // templates → skills (agent-session.ts:2332-2355). The session
            // helper scopes template sourceInfo against the *current* cwd,
            // so a cross-project `switch_session` does not keep the startup
            // cwd.
            let commands = session.get_commands_info();
            Ok(Some(json!({ "commands": commands })))
        }
    }
}

/// Handle one parsed command: dispatch, emit the response, then check the
/// shutdown flag (rpc-mode.ts:764-782).
async fn handle_command(
    state: Arc<RpcState>,
    runtime: Arc<tokio::sync::Mutex<AgentSessionRuntime>>,
    command: RpcCommand,
) {
    let id = command.id().clone();
    let command_type = command.type_str();
    // `prompt` emits its own response asynchronously via the preflight
    // observer; `handleCommand` returns `undefined` for it upstream
    // (rpc-mode.ts:393-415), so the shared emitter skips it here.
    let emits_own_response = matches!(&command, RpcCommand::Prompt { .. });
    match dispatch(state.clone(), runtime, command).await {
        Ok(data) if !emits_own_response => {
            state.emit_value(&success_response(&id, command_type, data))
        }
        Ok(_) => {}
        Err(message) => state.emit_value(&error_response(&id, command_type, &message)),
    }

    // `checkShutdownRequested` after each command (rpc-mode.ts:771).
    if state.shutdown_requested.load(Ordering::SeqCst) {
        let _ = state.shutdown.send(0);
    }
}

// ============================================================================
// Input framing (jsonl.ts attachJsonlLineReader)
// ============================================================================

/// Read one strict-LF record: bytes up to and including `\n`, or the
/// remaining tail at EOF. Returns `None` at clean EOF. A trailing `\r` is
/// stripped (jsonl.ts:25-27). U+2028/U+2029 inside the payload do not
/// terminate a record.
async fn read_record<R: AsyncBufRead + Unpin>(reader: &mut R) -> std::io::Result<Option<String>> {
    let mut buffer = Vec::new();
    let read = reader.read_until(b'\n', &mut buffer).await?;
    if read == 0 {
        return Ok(None);
    }
    if buffer.last() == Some(&b'\n') {
        buffer.pop();
    }
    if buffer.last() == Some(&b'\r') {
        buffer.pop();
    }
    Ok(Some(String::from_utf8_lossy(&buffer).into_owned()))
}

// ============================================================================
// Entry point
// ============================================================================

/// Register SIGTERM/SIGHUP handlers exiting 143/129 (rpc-mode.ts:365-379).
/// Same approach as print mode: upstream kills tracked detached children
/// (no registry in rpi, D-011) and skips the stdout flush on SIGTERM.
fn register_signal_handlers() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        for (kind, code) in [(SignalKind::terminate(), 143), (SignalKind::hangup(), 129)] {
            if let Ok(mut stream) = signal(kind) {
                std::thread::spawn(move || {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build();
                    if let Ok(runtime) = runtime {
                        runtime.block_on(async move {
                            stream.recv().await;
                        });
                    }
                    std::process::exit(code);
                });
            }
        }
    }
}

/// `runRpcMode` (rpc-mode.ts:54-807). Consumes the runtime; returns the
/// process exit code (stdin EOF / shutdown → 0; signals exit the process
/// directly with 143/129).
pub async fn run_rpc_mode<R: AsyncBufRead + Unpin>(
    runtime: AgentSessionRuntime,
    mut input: R,
    out: Box<dyn Write + Send>,
) -> i32 {
    register_signal_handlers();

    // Single ordered write path: all responses and events go through this
    // blocking writer, preserving emission order (upstream `writeRawStdout`,
    // output-guard.ts:85-93) and applying backpressure to producers when the
    // consumer is slow (T18; replaces the v0.1 unbounded channel + writer
    // task, which let slow consumers pile lines up without bound).
    let output = RawStdout::new(out);
    let (shutdown_tx, mut shutdown_rx) = mpsc::unbounded_channel::<i32>();

    let runtime = Arc::new(tokio::sync::Mutex::new(runtime));
    let initial_session = runtime.lock().await.session().clone();

    let state = Arc::new(RpcState {
        session: RwLock::new(initial_session.clone()),
        unsubscribe: Mutex::new(None),
        pending_ui: ui_bridge::new_pending_ui_table(),
        shutdown_requested: AtomicBool::new(false),
        output: output.clone(),
        shutdown: shutdown_tx,
    });

    // `runtimeHost.setRebindSession` (rpc-mode.ts:312-314).
    {
        let rebind_state = state.clone();
        let rebind_runtime = Arc::downgrade(&runtime);
        runtime
            .lock()
            .await
            .set_rebind_session(Some(Box::new(move |session| {
                let rebind_state = rebind_state.clone();
                let rebind_runtime = rebind_runtime.clone();
                Box::pin(async move {
                    rebind_session(&rebind_state, session, &rebind_runtime).await;
                })
            })));
    }

    // Initial bind (rpc-mode.ts:381).
    rebind_session(&state, initial_session.clone(), &Arc::downgrade(&runtime)).await;

    // `setUIContext(createExtensionUIContext(), "rpc")` (T15 W4): the RPC
    // extension UI bridge rides the session's extension host.
    if let Some(host) = initial_session
        .extension_runner()
        .as_any()
        .and_then(|any| {
            any.downcast_ref::<crate::core::extension_host_adapter::ExtensionHostAdapter>()
        })
        .map(|adapter| adapter.host().clone())
    {
        host.set_ui(
            Some(Arc::new(ui_bridge::RpcUiBridge::new(
                output.clone(),
                state.pending_ui.clone(),
                crate::core::themes::default_theme_json(),
            ))),
            rpi_ext_host::types::ExtensionMode::Rpc,
        );
    }

    // Input loop: each command is handled on its own task so `abort` /
    // `abort_bash` can land while a long-running `bash`/`prompt` is in
    // flight (upstream fires `void handleInputLine(line)` per record). The
    // tasks are tracked so shutdown can wait out their responses (below).
    let mut command_tasks = tokio::task::JoinSet::new();
    let exit_code = loop {
        tokio::select! {
            record = read_record(&mut input) => {
                match record {
                    Ok(Some(line)) => {
                        // No empty-line filtering: upstream feeds every line
                        // to JSON.parse, so blank lines get a `command:
                        // "parse"` error response (rpc-mode.ts:732-746).
                        match parse_command(&line) {
                            ParsedLine::Command(command) => {
                                let state = state.clone();
                                let runtime = runtime.clone();
                                command_tasks.spawn(async move {
                                    handle_command(state, runtime, command).await;
                                });
                            }
                            ParsedLine::ExtensionUiResponse(response) => {
                                let id = response
                                    .get("id")
                                    .and_then(Value::as_str)
                                    .map(str::to_owned);
                                if let Some(id) = id {
                                    let pending = state
                                        .pending_ui
                                        .lock()
                                        .unwrap_or_else(|e| e.into_inner())
                                        .remove(&id);
                                    if let Some(sender) = pending {
                                        let _ = sender.send(response);
                                    }
                                }
                            }
                            ParsedLine::Error(response) => state.emit_value(&response),
                        }
                    }
                    Ok(None) => break 0, // stdin EOF → shutdown (rpc-mode.ts:784-787)
                    Err(_) => break 1,
                }
            }
            code = shutdown_rx.recv() => {
                break code.unwrap_or(0);
            }
        }
    };

    // `shutdown` (rpc-mode.ts:708-725): dispose runtime, flush stdout.
    {
        let runtime_guard = runtime.lock().await;
        runtime_guard.dispose().await;
    }
    // Detach the event subscription so late tail events (aborted message_end,
    // retry triggers) cannot write after the mode ends.
    if let Some(unsubscribe) = state
        .unsubscribe
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
    {
        unsubscribe();
    }
    drop(state);
    drop(runtime);
    drop(initial_session);
    // Wait out in-flight command tasks so their responses land on the wire
    // before the process exits. The v0.1 writer task did this implicitly
    // (the output channel only closed once every task's sender dropped);
    // with direct writes the wait must be explicit. Dispose ran first, so
    // in-flight prompts have already been aborted and every task terminates.
    while command_tasks.join_next().await.is_some() {}
    // `flushRawStdout` (rpc-mode.ts `shutdown`); a recorded write error maps
    // to exit code 1 (upstream exits 1 on write-chain rejection,
    // output-guard.ts:90-92).
    output.flush();
    if output.has_error() && exit_code == 0 {
        return 1;
    }
    exit_code
}
