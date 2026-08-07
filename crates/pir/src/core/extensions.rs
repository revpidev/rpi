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

    async fn emit_session_before_compact(
        &self,
        _custom_instructions: Option<&str>,
        _reason: &str,
    ) -> Option<SessionBeforeCompactResult> {
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

    /// `before_agent_start` (extensions/types.ts).
    async fn emit_before_agent_start(
        &self,
        _text: &str,
        _images: Option<&[ImageContent]>,
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
