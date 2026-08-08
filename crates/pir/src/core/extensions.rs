//! Extension runner seam (T10).
//!
//! Upstream `AgentSession` talks to the extension system through
//! `ExtensionRunner` (`packages/coding-agent/src/core/extensions/runner.ts`).
//! The real extension host lands in T15; this module fixes the *surface* the
//! session/modes code calls, with a no-op default implementation
//! ([`NoopExtensionRunner`]) so headless modes behave exactly like upstream
//! with zero extensions loaded: `hasHandlers` is false everywhere, emits
//! return nothing, and there are no registered commands/tools/flags.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use pir_agent::messages::AgentMessage;
use pir_ai::types::{ImageContent, ProviderHeaders};

use crate::core::resource_loader::ResourceExtensionPaths;

fn read<T>(m: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    m.read().unwrap_or_else(|e| e.into_inner())
}

fn write<T>(m: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    m.write().unwrap_or_else(|e| e.into_inner())
}

/// `ExtensionMode` (extensions/types.ts): `"interactive" | "print" | "json" | "rpc"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtensionMode {
    Interactive,
    Print,
    Json,
    Rpc,
}

impl ExtensionMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            ExtensionMode::Interactive => "interactive",
            ExtensionMode::Print => "print",
            ExtensionMode::Json => "json",
            ExtensionMode::Rpc => "rpc",
        }
    }
}

/// `SessionStartEvent["reason"]` (extensions/types.ts).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStartReason {
    Startup,
    New,
    Resume,
    Fork,
    Reload,
}

impl SessionStartReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            SessionStartReason::Startup => "startup",
            SessionStartReason::New => "new",
            SessionStartReason::Resume => "resume",
            SessionStartReason::Fork => "fork",
            SessionStartReason::Reload => "reload",
        }
    }
}

/// `SessionStartEvent` (extensions/types.ts).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionStartEvent {
    pub reason: SessionStartReason,
    pub previous_session_file: Option<String>,
}

/// `SessionShutdownEvent["reason"]` (extensions/types.ts).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionShutdownReason {
    New,
    Resume,
    Fork,
    Reload,
    Quit,
}

impl SessionShutdownReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            SessionShutdownReason::New => "new",
            SessionShutdownReason::Resume => "resume",
            SessionShutdownReason::Fork => "fork",
            SessionShutdownReason::Reload => "reload",
            SessionShutdownReason::Quit => "quit",
        }
    }
}

/// `InputSource` (extensions/types.ts): origin of a prompt for `input`
/// handlers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputSource {
    Interactive,
    Print,
    Rpc,
    Extension,
}

impl InputSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            InputSource::Interactive => "interactive",
            InputSource::Print => "print",
            InputSource::Rpc => "rpc",
            InputSource::Extension => "extension",
        }
    }
}

/// `streamingBehavior` (`"steer" | "followUp"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum StreamingBehavior {
    #[serde(rename = "steer")]
    Steer,
    #[serde(rename = "followUp")]
    FollowUp,
}

impl StreamingBehavior {
    pub fn as_str(&self) -> &'static str {
        match self {
            StreamingBehavior::Steer => "steer",
            StreamingBehavior::FollowUp => "followUp",
        }
    }
}

/// Result of the `input` extension event (extensions/types.ts
/// `InputEventResult`).
#[derive(Debug, Clone, PartialEq)]
pub enum InputEventResult {
    /// Proceed with the original (or no) input.
    Continue,
    /// The extension consumed the input; no prompt is sent.
    Handled,
    /// Send transformed input instead.
    Transform {
        text: String,
        images: Option<Vec<ImageContent>>,
    },
}

/// `BeforeAgentStartResult` (extensions/types.ts).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BeforeAgentStartResult {
    /// Custom messages injected alongside the user message.
    pub messages: Vec<BeforeAgentStartMessage>,
    /// Extension-modified system prompt for this turn.
    pub system_prompt: Option<String>,
}

/// `BeforeAgentStartResult["messages"][number]` (extensions/types.ts).
#[derive(Debug, Clone, PartialEq)]
pub struct BeforeAgentStartMessage {
    pub custom_type: String,
    pub content: Option<pir_ai::types::UserContent>,
    pub display: bool,
    pub details: Option<serde_json::Value>,
}

/// `SessionBeforeCompactResult` (extensions/types.ts) — field-level subset
/// used by the compaction flow.
#[derive(Debug, Clone, Default)]
pub struct SessionBeforeCompactResult {
    pub cancel: Option<bool>,
    pub compaction: Option<pir_agent::compaction::CompactionResult>,
}

/// `SessionBeforeCompactEvent` payload (extensions/types.ts:586-596), minus
/// the non-serializable `signal`. `preparation` is the
/// `CompactionPreparation` JSON, `branch_entries` the `SessionEntry[]` JSON.
#[derive(Debug, Clone)]
pub struct SessionBeforeCompactEvent {
    pub preparation: serde_json::Value,
    pub branch_entries: serde_json::Value,
    pub custom_instructions: Option<String>,
    /// `"manual" | "threshold" | "overflow"`.
    pub reason: String,
    pub will_retry: bool,
}

/// `ToolCallEventResult` (extensions/types.ts:1065-1069) plus the threaded
/// `input`: upstream handlers mutate `event.input` in place
/// (types.ts:892-896); in-place mutation cannot cross the host's JSON
/// dispatch boundary, so the host threads the mutated arguments back in the
/// result's `input` field (intentional deviation, see
/// `pir_ext_host::runner::ExtensionRunnerCore::emit_tool_call`). Applied
/// without revalidation, matching upstream.
#[derive(Debug, Clone, Default)]
pub struct ToolCallOutcome {
    pub block: Option<bool>,
    pub reason: Option<String>,
    /// Extension-mutated tool arguments (no revalidation upstream).
    pub input: Option<serde_json::Value>,
}

/// Aggregated `tool_result` patch (`ToolResultEventResult`,
/// extensions/types.ts:1079-1084). Fields left out by every handler carry
/// the current (executed) values.
#[derive(Debug, Clone, Default)]
pub struct ToolResultPatch {
    pub content: Option<Vec<pir_ai::types::ToolResultContent>>,
    pub details: Option<serde_json::Value>,
    pub is_error: Option<bool>,
    pub usage: Option<pir_ai::types::Usage>,
}

/// `SessionBeforeTreeResult` (extensions/types.ts).
#[derive(Debug, Clone, Default)]
pub struct SessionBeforeTreeResult {
    pub cancel: Option<bool>,
    pub summary: Option<ExtensionBranchSummary>,
    pub custom_instructions: Option<String>,
    pub replace_instructions: Option<bool>,
    pub label: Option<String>,
}

/// Extension-provided branch summary payload.
#[derive(Debug, Clone)]
pub struct ExtensionBranchSummary {
    pub summary: String,
    pub details: Option<serde_json::Value>,
    pub usage: Option<pir_ai::types::Usage>,
}

/// `SessionBeforeSwitchResult` / `SessionBeforeForkResult` shared shape.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionCancelResult {
    pub cancel: bool,
}

/// Error payload for `extension_error` events and the error listener
/// (extensions/types.ts `ExtensionErrorEvent`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionErrorInfo {
    pub extension_path: String,
    pub event: String,
    pub error: String,
}

/// `ProjectTrustEventDecision` (extensions/types.ts): `"yes" | "no" |
/// "undecided"`. `Undecided` falls through to the next handler
/// (runner.ts:216-218); the first yes/no wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectTrustEventDecision {
    Yes,
    No,
    Undecided,
}

impl ProjectTrustEventDecision {
    pub fn as_str(&self) -> &'static str {
        match self {
            ProjectTrustEventDecision::Yes => "yes",
            ProjectTrustEventDecision::No => "no",
            ProjectTrustEventDecision::Undecided => "undecided",
        }
    }
}

/// `ProjectTrustEventResult` (extensions/types.ts).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProjectTrustEventResult {
    pub trusted: ProjectTrustEventDecision,
    pub remember: Option<bool>,
}

/// Registered slash command handle (extensions/types.ts
/// `RegisteredCommand`). Never produced by the no-op runner.
#[derive(Debug, Clone)]
pub struct ExtensionCommand {
    pub invocation_name: String,
    pub description: Option<String>,
    /// Provenance for `getCommands` (`SlashCommandInfo.sourceInfo`).
    pub source_info: Option<crate::core::skills::SourceInfo>,
}

/// One extension-registered tool with its executable wrapper and the
/// `ToolInfo` metadata (types.ts:1546-1548). Produced by the host adapter.
pub struct ExtensionToolEntry {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
    pub prompt_snippet: Option<String>,
    pub prompt_guidelines: Option<Vec<String>>,
    pub source_info: crate::core::skills::SourceInfo,
    /// Executable wrapper (host tool definition + `addedToolNames` logic,
    /// wrapper.ts:17-37).
    pub tool: Arc<dyn pir_agent::types::AgentTool>,
}

/// Error listener registered through [`ExtensionRunner::on_error`].
pub type ExtensionErrorListener = Arc<dyn Fn(ExtensionErrorInfo) + Send + Sync>;

/// The session-facing extension surface. Default method bodies are the
/// no-extension behavior; the T15 host overrides them.
#[async_trait::async_trait]
pub trait ExtensionRunner: Send + Sync {
    /// `hasHandlers(eventType)`.
    fn has_handlers(&self, _event_type: &str) -> bool {
        false
    }

    /// Generic event emit with an optional structured result
    /// (`session_before_compact` / `session_before_tree` /
    /// `session_before_switch` / `session_before_fork`).
    async fn emit_cancelable(&self, _event_type: &str) -> Option<SessionCancelResult> {
        None
    }

    /// Payload-carrying variant of [`ExtensionRunner::emit_cancelable`]
    /// (handlers observe `reason`/`entryId`/… fields). Default falls back
    /// to the payload-less form.
    async fn emit_cancelable_with(
        &self,
        event_type: &str,
        _payload: serde_json::Value,
    ) -> Option<SessionCancelResult> {
        self.emit_cancelable(event_type).await
    }

    /// `session_before_compact` with the full event payload
    /// (agent-session.ts:1812-1831 manual, :2079-2105 auto).
    async fn emit_session_before_compact(
        &self,
        _event: &SessionBeforeCompactEvent,
    ) -> Option<SessionBeforeCompactResult> {
        None
    }

    /// `session_compact` — fire-and-forget after a compaction entry is
    /// saved (agent-session.ts:1883-1891 / :2164-2172). `compaction_entry`
    /// is the saved entry's JSON.
    async fn emit_session_compact(
        &self,
        _compaction_entry: serde_json::Value,
        _from_extension: bool,
        _reason: &str,
        _will_retry: bool,
    ) {
    }

    /// Payload-carrying variant of [`ExtensionRunner::emit`] for events
    /// whose handlers observe event fields (`after_provider_response`,
    /// `session_compact`-style emits). Default falls back to the bare
    /// [`ExtensionRunner::emit`].
    async fn emit_event(&self, event_type: &str, _payload: serde_json::Value) {
        self.emit(event_type).await;
    }

    /// `tool_call` interception (agent-session.ts:466-485). Returns the
    /// aggregated outcome; a failing handler is the caller's fail-safe
    /// decision (upstream rethrows, aborting the run).
    async fn emit_tool_call(
        &self,
        _tool_call_id: &str,
        _tool_name: &str,
        _input: serde_json::Value,
    ) -> Option<ToolCallOutcome> {
        None
    }

    /// `tool_result` interception (agent-session.ts:487-517): chained
    /// partial patch, `None` when no handler modified anything.
    #[allow(clippy::too_many_arguments)]
    async fn emit_tool_result(
        &self,
        _tool_call_id: &str,
        _tool_name: &str,
        _input: serde_json::Value,
        _content: &[pir_ai::types::ToolResultContent],
        _details: &serde_json::Value,
        _is_error: bool,
        _usage: Option<&pir_ai::types::Usage>,
    ) -> Option<ToolResultPatch> {
        None
    }

    /// `user_bash` interception (interactive-mode.ts:5931-5940): the first
    /// non-empty handler result wins. Only the full-replacement `result`
    /// branch crosses the JSON boundary; a handler returning custom
    /// `operations` (a closure bundle upstream) cannot be honored by the
    /// host and is dropped with a warning (candidate deviation, T15 W2).
    async fn emit_user_bash(
        &self,
        _command: &str,
        _exclude_from_context: bool,
        _cwd: &str,
    ) -> Option<crate::tools::bash_executor::BashResult> {
        None
    }

    async fn emit_session_before_tree(&self) -> Option<SessionBeforeTreeResult> {
        None
    }

    /// Fire-and-forget lifecycle/agent events (`agent_start`, `turn_start`,
    /// `session_start`, `session_shutdown`, `model_select`, …).
    async fn emit(&self, _event_type: &str) {}

    /// `input` event (extensions/types.ts `InputEvent`).
    async fn emit_input(
        &self,
        _text: &str,
        _images: Option<&[ImageContent]>,
        _source: InputSource,
        _streaming_behavior: Option<StreamingBehavior>,
    ) -> InputEventResult {
        InputEventResult::Continue
    }

    /// `before_agent_start` (extensions/types.ts). `system_prompt` is the
    /// fully assembled base prompt and `system_prompt_options` the
    /// `BuildSystemPromptOptions` JSON (agent-session.ts:1224-1230).
    async fn emit_before_agent_start(
        &self,
        _text: &str,
        _images: Option<&[ImageContent]>,
        _system_prompt: &str,
        _system_prompt_options: serde_json::Value,
    ) -> Option<BeforeAgentStartResult> {
        None
    }

    /// `message_end` with optional message replacement
    /// (agent-session.ts:747-765).
    async fn emit_message_end(&self, _message: &AgentMessage) -> Option<AgentMessage> {
        None
    }

    /// `context` transform hook (sdk.ts `transformContext`).
    async fn emit_context(&self, messages: Vec<AgentMessage>) -> Vec<AgentMessage> {
        messages
    }

    /// `before_provider_request` (sdk.ts `onPayload`).
    async fn emit_before_provider_request(&self, payload: serde_json::Value) -> serde_json::Value {
        payload
    }

    /// `before_provider_headers` (sdk.ts `transformHeaders`).
    async fn emit_before_provider_headers(&self, headers: ProviderHeaders) -> ProviderHeaders {
        headers
    }

    /// `resources_discover` (agent-session.ts:2254-2277).
    async fn emit_resources_discover(&self, _cwd: &str, _reason: &str) -> ResourceExtensionPaths {
        ResourceExtensionPaths::default()
    }

    /// `project_trust` event (`emitProjectTrustEvent`, runner.ts:201-231):
    /// handlers run in order, `undecided` falls through, the first yes/no
    /// wins. Returns the winning result, or `None` when no handler decides
    /// (or none are registered — the default until the T15 host lands).
    /// The restricted [`ProjectTrustContext`](crate::core::trust_manager::ProjectTrustContext)
    /// is supplied by the trust resolver, not by this method.
    async fn emit_project_trust(&self, _cwd: &std::path::Path) -> Option<ProjectTrustEventResult> {
        None
    }

    /// `getCommand(name)` — extension slash command lookup.
    fn get_command(&self, _name: &str) -> Option<ExtensionCommand> {
        None
    }

    /// `getRegisteredCommands()` — all extension slash commands with
    /// conflict-resolved invocation names (runner.ts:595-638).
    fn registered_commands(&self) -> Vec<ExtensionCommand> {
        Vec::new()
    }

    /// Execute an extension slash command by invocation name
    /// (`_tryExecuteExtensionCommand`, agent-session.ts:1267-1294). Returns
    /// false when no command matches; handler errors are reported through
    /// `emit_error` (and still return true).
    async fn execute_extension_command(&self, _name: &str, _args: &str) -> bool {
        false
    }

    /// All extension-registered tools, conflict-resolved (first
    /// registration wins, runner.ts:447-457), as executable wrappers plus
    /// `ToolInfo` metadata. Drives the session's tool-registry refresh.
    fn extension_tool_entries(&self) -> Vec<ExtensionToolEntry> {
        Vec::new()
    }

    /// Downcast support: the host adapter returns itself so mode code can
    /// reach host-only surfaces (shortcut resolution, `ExtensionContext`).
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        None
    }

    /// Registered extension CLI flag values (`runtime.flagValues`).
    fn flag_values(&self) -> HashMap<String, crate::cli::args::UnknownFlagValue> {
        HashMap::new()
    }

    fn set_flag_value(&self, _name: &str, _value: crate::cli::args::UnknownFlagValue) {}

    /// `invalidate(message)` — marks captured contexts stale (dispose path).
    fn invalidate(&self, _message: &str) {}

    /// `onError(listener)` — returns an unsubscribe closure.
    fn on_error(&self, _listener: ExtensionErrorListener) -> Option<Box<dyn FnOnce() + Send>> {
        None
    }

    /// `emitError` — report an extension error (forwarded to listeners and,
    /// in RPC mode, as an `extension_error` event).
    fn emit_error(&self, _error: ExtensionErrorInfo) {}
}

/// No-op runner: zero extensions loaded (T10 default).
pub struct NoopExtensionRunner {
    flag_values: RwLock<HashMap<String, crate::cli::args::UnknownFlagValue>>,
}

impl Default for NoopExtensionRunner {
    fn default() -> Self {
        Self::new()
    }
}

impl NoopExtensionRunner {
    pub fn new() -> Self {
        NoopExtensionRunner {
            flag_values: RwLock::new(HashMap::new()),
        }
    }

    pub fn shared() -> Arc<Self> {
        Arc::new(Self::new())
    }
}

#[async_trait::async_trait]
impl ExtensionRunner for NoopExtensionRunner {
    fn flag_values(&self) -> HashMap<String, crate::cli::args::UnknownFlagValue> {
        read(&self.flag_values).clone()
    }

    fn set_flag_value(&self, name: &str, value: crate::cli::args::UnknownFlagValue) {
        write(&self.flag_values).insert(name.to_owned(), value);
    }
}

/// Mutable slot for the "current" runner, mirroring upstream's
/// `extensionRunnerRef: { current?: ExtensionRunner }` (sdk.ts:292): the
/// Agent's stream/transform hooks read the runner at execution time so a
/// session replacement can swap it without rebuilding the Agent.
pub type ExtensionRunnerRef = Arc<RwLock<Arc<dyn ExtensionRunner>>>;

pub fn new_extension_runner_ref(initial: Arc<dyn ExtensionRunner>) -> ExtensionRunnerRef {
    Arc::new(RwLock::new(initial))
}

pub fn read_runner(slot: &ExtensionRunnerRef) -> Arc<dyn ExtensionRunner> {
    read(slot).clone()
}

pub fn swap_runner(slot: &ExtensionRunnerRef, runner: Arc<dyn ExtensionRunner>) {
    *write(slot) = runner;
}

// ============================================================================
// Agent hook builders (T15 W2)
// ============================================================================

/// `beforeToolCall` hook backed by the current extension runner
/// (`_installAgentToolHooks`, agent-session.ts:459-485). Reads the runner at
/// execution time so a runner swap (session replacement / reload) takes
/// effect without reinstalling hooks.
pub fn extension_before_tool_call_hook(
    runner_ref: ExtensionRunnerRef,
) -> pir_agent::agent_loop::BeforeToolCallFn {
    Arc::new(
        move |context: pir_agent::agent_loop::BeforeToolCallContext, _signal| {
            let runner = read_runner(&runner_ref);
            Box::pin(async move {
                if !runner.has_handlers("tool_call") {
                    return None;
                }
                let outcome = runner
                    .emit_tool_call(
                        &context.tool_call.id,
                        &context.tool_call.name,
                        context.args.clone(),
                    )
                    .await?;
                pir_agent::agent_loop::BeforeToolCallResult {
                    block: outcome.block,
                    reason: outcome.reason,
                    args: outcome.input,
                }
                .into()
            })
                as futures::future::BoxFuture<
                    'static,
                    Option<pir_agent::agent_loop::BeforeToolCallResult>,
                >
        },
    )
}

/// `afterToolCall` hook backed by the current extension runner
/// (agent-session.ts:487-517). The runner isolates per-handler errors, so
/// this hook never returns `Err` (the agent loop would downgrade it to an
/// error result, agent-loop.ts:743-746).
pub fn extension_after_tool_call_hook(
    runner_ref: ExtensionRunnerRef,
) -> pir_agent::agent_loop::AfterToolCallFn {
    Arc::new(
        move |context: pir_agent::agent_loop::AfterToolCallContext, _signal| {
            let runner = read_runner(&runner_ref);
            Box::pin(async move {
                if !runner.has_handlers("tool_result") {
                    return Ok(None);
                }
                let patch = runner
                    .emit_tool_result(
                        &context.tool_call.id,
                        &context.tool_call.name,
                        context.args.clone(),
                        &context.result.content,
                        &context.result.details,
                        context.is_error,
                        context.result.usage.as_ref(),
                    )
                    .await;
                Ok(
                    patch.map(|patch| pir_agent::agent_loop::AfterToolCallResult {
                        content: patch.content,
                        details: patch.details,
                        // `hookResult.isError ?? isError` (agent-session.ts:512).
                        is_error: patch.is_error.or(Some(context.is_error)),
                        usage: patch.usage,
                        terminate: None,
                    }),
                )
            })
                as futures::future::BoxFuture<
                    'static,
                    Result<
                        Option<pir_agent::agent_loop::AfterToolCallResult>,
                        pir_agent::AgentError,
                    >,
                >
        },
    )
}

/// `onResponse` callback emitting `after_provider_response`
/// (agent-harness.ts:1007-1010 shape; sdk.ts stream_fn wiring). Fire and
/// forget: handler errors are isolated by the runner.
pub fn extension_on_response_callback(
    runner_ref: ExtensionRunnerRef,
) -> pir_ai::types::OnResponseCallback {
    Arc::new(move |response: pir_ai::types::ProviderResponse, _model| {
        let runner = read_runner(&runner_ref);
        Box::pin(async move {
            if !runner.has_handlers("after_provider_response") {
                return;
            }
            runner
                .emit_event(
                    "after_provider_response",
                    serde_json::json!({
                        "type": "after_provider_response",
                        "status": response.status,
                        "headers": response.headers,
                    }),
                )
                .await;
        }) as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
    })
}
