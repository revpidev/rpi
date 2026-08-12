//! `AgentSession` — core abstraction for agent lifecycle and session
//! management, shared by all run modes (print / json / rpc / interactive).
//!
//! Port of `packages/coding-agent/src/core/agent-session.ts` @ pi 0.82.1
//! (2efa728), updated to 4181f66 (v0.84.1+) for v0.11 T23 (prompt-during-
//! compaction rejection 8eda4f5b2, length-stop recovery 32850ef7c,
//! disconnect/reconnect removal e56893f4c, bash hint softening 4e64de695).
//!
//! Structural notes (behavior preserved):
//! - Upstream mutates plain class fields from the single-threaded event loop.
//!   Here the session is shared between the prompt task (agent-event
//!   listener persistence, auto-compaction) and mode command tasks
//!   (steer/abort/get_state/bash), so mutable state lives behind mutexes.
//!   Method semantics and event ordering are unchanged.
//! - Extension calls go through the [`ExtensionRunner`] seam
//!   (`core/extensions.rs`); the no-op default reproduces "zero extensions
//!   loaded" until the T15 host lands.
//! - The model is `Option`-like via the rpi-agent `"unknown"` placeholder
//!   (T05 structural decision): [`model_or_none`] maps it to `None`.
//! - `exportToHtml` is synchronous (pure CPU + file IO) and never emits
//!   `renderedTools` — rpi has no JS tool renderers (export_html.rs header).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use rpi_agent::compaction::branch_summarization::{
    collect_entries_for_branch_summary, generate_branch_summary, CollectEntriesResult,
    GenerateBranchSummaryOptions,
};
use rpi_agent::compaction::{estimate_context_tokens, CompactionResult, SummarizationArgs};
use rpi_agent::messages::{AgentMessage, BashExecutionMessage, CustomMessage, CustomRole};
use rpi_agent::session::SessionEntry;
use rpi_agent::types::{AgentEvent, AgentTool, QueueMode, ThinkingLevel};
use rpi_agent::{Agent, AgentError};
use rpi_ai::models::{clamp_thinking_level, get_supported_thinking_levels, models_are_equal};
use rpi_ai::models_json::OrderedMap;
use rpi_ai::types::{
    AssistantMessage, ImageContent, Model, StopReason, TextContent, Usage, UserContent,
    UserContentBlock, UserMessage, UserRole,
};
use rpi_ai::utils::overflow::is_context_overflow;
use rpi_ai::utils::retry::{is_retryable_assistant_error, RetryPolicy};
use rpi_ai::utils::text::content_text_user;
use serde::Serialize;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::core::auth_guidance::{
    format_no_api_key_found_message, format_no_model_selected_message,
};
use crate::core::compaction_runner::{CompactionEvent, CompactionRunner};
use crate::core::extensions::{
    read_runner, ExtensionMode, ExtensionRunner, ExtensionRunnerRef, InputEventResult, InputSource,
    SessionStartEvent, SessionStartReason, StreamingBehavior,
};
use crate::core::model_resolver::ScopedModel;
use crate::core::model_runtime::ModelRuntime;
use crate::core::prompt_templates::expand_prompt_template;
use crate::core::resource_loader::DefaultResourceLoader;
use crate::core::session_manager::{SessionManager, StoredEntry};
use crate::core::settings_manager::{RetryConfig, SettingsManager};
use crate::core::skills::strip_frontmatter;
use crate::core::system_prompt::{build_system_prompt, BuildSystemPromptOptions};
use crate::core::usage_totals::{add_usage_to_totals, create_usage_totals, UsageTotals};
use crate::error::RpiError;
use crate::tools::bash::create_local_bash_operations;
use crate::tools::bash_executor::{execute_bash, BashExecutorOptions, BashResult};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// The rpi-agent placeholder model represents upstream `undefined`
/// (agent.ts `DEFAULT_MODEL`; auth-guidance.ts uses the same `"unknown"`
/// sentinel for display).
pub fn model_or_none(model: &Model) -> Option<Model> {
    if model.provider == "unknown" && model.id == "unknown" {
        None
    } else {
        Some(model.clone())
    }
}

// ============================================================================
// AgentSessionEvent (agent-session.ts:139-181)
// ============================================================================

/// `agent_end` with the session-computed `willRetry` flag
/// (agent-session.ts:142-145).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename = "agent_end", rename_all = "camelCase")]
pub struct AgentEndEvent {
    pub messages: Vec<AgentMessage>,
    pub will_retry: bool,
}

/// Session-only events (everything in the `AgentSessionEvent` union that is
/// not an `AgentEvent`).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum SessionEvent {
    AgentSettled,
    QueueUpdate {
        steering: Vec<String>,
        follow_up: Vec<String>,
    },
    EntryAppended {
        entry: Box<SessionEntry>,
    },
    SessionInfoChanged {
        name: Option<String>,
    },
    ThinkingLevelChanged {
        level: ThinkingLevel,
    },
    AutoRetryStart {
        attempt: u32,
        max_attempts: u32,
        delay_ms: u64,
        error_message: String,
    },
    AutoRetryEnd {
        success: bool,
        attempt: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        final_error: Option<String>,
    },
    BashExecutionUpdate {
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        delta: String,
    },
    ExtensionError {
        extension_path: String,
        event: String,
        error: String,
    },
}

/// `AgentSessionEvent` (agent-session.ts:139-181) — serialization is the
/// JSON-mode/RPC wire shape.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum AgentSessionEvent {
    /// `agent_end` with `willRetry` (replaces the plain agent variant).
    AgentEnd(Box<AgentEndEvent>),
    /// All other `AgentEvent` variants, serialized exactly as upstream.
    Agent(Box<AgentEvent>),
    /// The compaction slice (`CompactionEvent` serde is already exact).
    Compaction(Box<CompactionEvent>),
    Session(SessionEvent),
}

/// Listener function for agent session events (sync, like upstream `_emit`).
pub type AgentSessionEventListener = Arc<dyn Fn(AgentSessionEvent) + Send + Sync>;

// ============================================================================
// Types
// ============================================================================

/// `AgentSessionConfig` (agent-session.ts:196-226), T10 subset.
pub struct AgentSessionConfig {
    pub agent: Arc<Agent>,
    pub session_manager: Arc<Mutex<SessionManager>>,
    pub cwd: String,
    /// Models to cycle through with Ctrl+P (from `--models`).
    pub scoped_models: Vec<ScopedModel>,
    pub resource_loader: Arc<Mutex<DefaultResourceLoader>>,
    /// SDK custom tools registered outside extensions.
    pub custom_tools: Vec<Arc<dyn AgentTool>>,
    pub model_runtime: Arc<ModelRuntime>,
    /// Initial active built-in tool names. Default: [read, bash, edit, write].
    pub initial_active_tool_names: Option<Vec<String>>,
    /// Optional allowlist of tool names.
    pub allowed_tool_names: Option<Vec<String>>,
    /// Optional denylist of tool names.
    pub excluded_tool_names: Option<Vec<String>>,
    /// Runner slot shared with the Agent's stream hooks.
    pub extension_runner_ref: ExtensionRunnerRef,
    pub session_start_event: SessionStartEvent,
}

/// `ExtensionBindings` (agent-session.ts:228-235), T10 subset.
#[derive(Default)]
pub struct ExtensionBindings {
    pub mode: Option<ExtensionMode>,
    pub on_error: Option<crate::core::extensions::ExtensionErrorListener>,
    /// `shutdownHandler` (agent-session.ts:2235-2238 area): mode-specific
    /// graceful shutdown behind extension `ctx.shutdown()`
    /// (docs/extensions.md:1018-1034). `None` = no-op (print modes).
    pub shutdown: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
}

/// `PromptOptions` (agent-session.ts:238-249).
#[derive(Default)]
pub struct PromptOptions {
    /// Whether to expand file-based prompt templates (default: true).
    pub expand_prompt_templates: Option<bool>,
    pub images: Option<Vec<ImageContent>>,
    /// Required when streaming: queue as "steer" (interrupt) or "followUp".
    pub streaming_behavior: Option<StreamingBehavior>,
    /// Source of input for extension input event handlers.
    pub source: Option<InputSource>,
    /// RPC preflight observer: called once with acceptance/rejection.
    pub preflight_result: Option<Box<dyn FnOnce(bool) + Send>>,
}

/// `ModelCycleResult` (agent-session.ts:252-257).
#[derive(Debug, Clone)]
pub struct ModelCycleResult {
    pub model: Model,
    pub thinking_level: ThinkingLevel,
    pub is_scoped: bool,
}

/// `SessionStats` (agent-session.ts:260-277).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStats {
    /// Upstream `sessionFile: this.sessionFile` — `undefined` is dropped by
    /// `JSON.stringify`, so the key is omitted for in-memory sessions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_file: Option<String>,
    pub session_id: String,
    pub user_messages: u64,
    pub assistant_messages: u64,
    pub tool_calls: u64,
    pub tool_results: u64,
    pub total_messages: u64,
    pub tokens: SessionTokenStats,
    pub cost: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_usage: Option<ContextUsage>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionTokenStats {
    pub input: u64,
    pub output: u64,
    #[serde(rename = "cacheRead")]
    pub cache_read: u64,
    #[serde(rename = "cacheWrite")]
    pub cache_write: u64,
    pub total: u64,
}

/// `ContextUsage` (extensions/types.ts).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextUsage {
    /// `null` when unknown (no post-compaction assistant usage yet).
    pub tokens: Option<u64>,
    pub context_window: u64,
    pub percent: Option<f64>,
}

/// `navigateTree` result (agent-session.ts:2897).
#[derive(Debug, Default)]
pub struct NavigateTreeResult {
    pub editor_text: Option<String>,
    pub cancelled: bool,
    pub aborted: bool,
    pub summary_entry_id: Option<String>,
}

/// `navigateTree` options (agent-session.ts:2896).
#[derive(Debug, Default)]
pub struct NavigateTreeOptions {
    pub summarize: bool,
    pub custom_instructions: Option<String>,
    pub replace_instructions: bool,
    pub label: Option<String>,
}

/// Parsed skill block from a user message (agent-session.ts:116-121).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedSkillBlock {
    pub name: String,
    pub location: String,
    pub content: String,
    pub user_message: Option<String>,
}

/// `parseSkillBlock` (agent-session.ts:127-136).
pub fn parse_skill_block(text: &str) -> Option<ParsedSkillBlock> {
    // ^<skill name="([^"]+)" location="([^"]+)">\n([\s\S]*?)\n</skill>(?:\n\n([\s\S]+))?$
    let rest = text.strip_prefix("<skill name=\"")?;
    let (name, rest) = rest.split_once('"')?;
    let rest = rest.strip_prefix(" location=\"")?;
    let (location, rest) = rest.split_once('"')?;
    let rest = rest.strip_prefix(">\n")?;
    let (content, rest) = rest.split_once("\n</skill>")?;
    let user_message = match rest {
        "" => None,
        _ => {
            let message = rest.strip_prefix("\n\n")?;
            let trimmed = message.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_owned())
            }
        }
    };
    Some(ParsedSkillBlock {
        name: name.to_owned(),
        location: location.to_owned(),
        content: content.to_owned(),
        user_message,
    })
}

/// Standard thinking levels (agent-session.ts:297).
const THINKING_LEVELS: [ThinkingLevel; 5] = [
    ThinkingLevel::Off,
    ThinkingLevel::Minimal,
    ThinkingLevel::Low,
    ThinkingLevel::Medium,
    ThinkingLevel::High,
];

/// Built-in tool prompt snippets/guidelines (upstream ToolDefinition fields:
/// tools/read.ts:213-214, bash.ts:328-331, edit.ts:297-304, write.ts:191-192,
/// grep.ts:132, find.ts:118, ls.ts:104). The bash guideline follows the
/// `RPI_` env rename (ADR-0001).
fn builtin_tool_snippet(name: &str) -> Option<&'static str> {
    Some(match name {
        "read" => crate::tools::READ_TOOL_SYSTEM_PROMPT_CONTRIBUTION.snippet,
        "bash" => crate::tools::BASH_TOOL_SYSTEM_PROMPT_CONTRIBUTION.snippet,
        "edit" => crate::tools::EDIT_TOOL_SYSTEM_PROMPT_CONTRIBUTION.snippet,
        "write" => crate::tools::WRITE_TOOL_SYSTEM_PROMPT_CONTRIBUTION.snippet,
        "grep" => crate::tools::GREP_TOOL_SYSTEM_PROMPT_CONTRIBUTION.snippet,
        "find" => crate::tools::FIND_TOOL_SYSTEM_PROMPT_CONTRIBUTION.snippet,
        "ls" => crate::tools::LS_TOOL_SYSTEM_PROMPT_CONTRIBUTION.snippet,
        _ => return None,
    })
}

fn builtin_tool_guidelines(name: &str) -> &'static [&'static str] {
    match name {
        "read" => crate::tools::READ_TOOL_SYSTEM_PROMPT_CONTRIBUTION.guidelines,
        "bash" => crate::tools::BASH_TOOL_SYSTEM_PROMPT_CONTRIBUTION.guidelines,
        "edit" => crate::tools::EDIT_TOOL_SYSTEM_PROMPT_CONTRIBUTION.guidelines,
        "write" => crate::tools::WRITE_TOOL_SYSTEM_PROMPT_CONTRIBUTION.guidelines,
        _ => &[],
    }
}

// ============================================================================
// AgentSession
// ============================================================================

struct AgentSessionInner {
    agent: Arc<Agent>,
    session_manager: Arc<Mutex<SessionManager>>,
    resource_loader: Arc<Mutex<DefaultResourceLoader>>,
    model_runtime: Arc<ModelRuntime>,
    extension_runner_ref: ExtensionRunnerRef,
    cwd: String,
    session_start_event: SessionStartEvent,

    compaction: tokio::sync::Mutex<CompactionRunner>,
    /// Shared compaction abort handle: works while the runner mutex is held
    /// by an in-flight compaction (`AbortController` upstream).
    compaction_abort: crate::core::compaction_runner::AbortTokenCell,

    listeners: Mutex<Vec<(u64, AgentSessionEventListener)>>,
    next_listener_id: AtomicU64,
    unsubscribe_agent: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    is_agent_run_active: AtomicBool,
    idle_notify: Notify,

    steering_messages: Mutex<Vec<String>>,
    follow_up_messages: Mutex<Vec<String>>,
    pending_next_turn_messages: Mutex<Vec<AgentMessage>>,
    last_assistant_message: Mutex<Option<AssistantMessage>>,

    retry_attempt: Mutex<u32>,
    retry_abort: Mutex<Option<CancellationToken>>,

    bash_tokens: Mutex<Vec<(u64, CancellationToken)>>,
    pending_bash_messages: Mutex<Vec<BashExecutionMessage>>,

    scoped_models: Mutex<Vec<ScopedModel>>,
    base_system_prompt: Mutex<String>,
    base_system_prompt_options: Mutex<BuildSystemPromptOptions>,
    system_prompt_override: Mutex<Option<String>>,
    tool_registry: Mutex<OrderedMap<Arc<dyn AgentTool>>>,
    /// `ToolDefinitionEntry` registry (agent-session.ts:2460 `_toolDefinitions`).
    tool_definitions: Mutex<OrderedMap<ToolDefinitionEntry>>,
    /// `_customTools` (agent-session.ts:345) — SDK-provided tools.
    custom_tools: Vec<Arc<dyn AgentTool>>,
    session_env_cell: Arc<std::sync::RwLock<crate::tools::SessionEnv>>,
    allowed_tool_names: Option<HashSet<String>>,
    excluded_tool_names: Option<HashSet<String>>,
    extension_mode: Mutex<ExtensionMode>,
    /// `_extensionErrorListener` / `_extensionErrorUnsubscriber`
    /// (agent-session.ts:2307-2314).
    extension_error_listener: Mutex<Option<crate::core::extensions::ExtensionErrorListener>>,
    /// Mode-provided shutdown handler (`ExtensionBindings::shutdown`, T15 W5).
    extension_shutdown_handler: Mutex<Option<std::sync::Arc<dyn Fn() + Send + Sync>>>,
    extension_error_unsubscriber: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

/// `AgentSession` (agent-session.ts:303-3327). Cheap to clone (Arc inner);
/// all clones share the same session state.
pub struct AgentSession {
    inner: Arc<AgentSessionInner>,
}

/// Weak handle to a session. The host actions hold this to break the
/// session → runner-ref → host → actions → session Arc cycle (T15 W3).
#[derive(Clone)]
pub struct WeakAgentSession {
    inner: std::sync::Weak<AgentSessionInner>,
}

impl WeakAgentSession {
    pub fn upgrade(&self) -> Option<AgentSession> {
        self.inner.upgrade().map(|inner| AgentSession { inner })
    }
}

/// `_refreshToolRegistry` options (agent-session.ts:2550-2553).
#[derive(Debug, Clone, Default)]
pub struct RefreshToolRegistryOptions {
    /// Replacement active set; `None` keeps the current one.
    pub active_tool_names: Option<Vec<String>>,
    /// Activate every extension/SDK tool (initial build + reload).
    pub include_all_extension_tools: bool,
}

/// Tool definition metadata entry (`ToolDefinitionEntry`,
/// agent-session.ts:2458-2503) — drives `getAllTools` and the system
/// prompt's snippet/guideline sections.
#[derive(Debug, Clone)]
pub struct ToolDefinitionEntry {
    pub description: String,
    pub parameters: serde_json::Value,
    pub prompt_snippet: Option<String>,
    pub prompt_guidelines: Vec<String>,
    pub source_info: crate::core::skills::SourceInfo,
}

impl AgentSession {
    /// `constructor` (agent-session.ts:375-401).
    #[allow(clippy::too_many_arguments)]
    pub fn new(config: AgentSessionConfig) -> Self {
        let model = model_or_none(&config.agent.state().model);
        let (compaction_settings, retry, thinking_level, stream_fn) = {
            let loader = lock(&config.resource_loader);
            let settings = loader.settings_manager();
            let retry_config = settings.get_retry_settings();
            (
                settings.get_compaction_settings(),
                retry_config_to_policy(retry_config),
                config.agent.state().thinking_level,
                config.agent.stream_function.clone(),
            )
        };
        let mut compaction = CompactionRunner::new(
            config.agent.clone(),
            config.session_manager.clone(),
            model,
            rpi_agent::compaction::CompactionSettings {
                enabled: compaction_settings.enabled,
                reserve_tokens: compaction_settings.reserve_tokens,
                keep_recent_tokens: compaction_settings.keep_recent_tokens,
            },
            retry,
            stream_fn,
            thinking_level,
            Arc::new(|_event| {}),
        );
        // T15 W2: `session_before_compact` / `session_compact` ride the
        // shared runner slot (read per emit, swap-safe).
        compaction.set_extension_runner(config.extension_runner_ref.clone());
        let compaction_abort = compaction.abort_token_cell();

        let inner = Arc::new(AgentSessionInner {
            agent: config.agent,
            session_manager: config.session_manager,
            resource_loader: config.resource_loader,
            model_runtime: config.model_runtime,
            extension_runner_ref: config.extension_runner_ref,
            cwd: config.cwd,
            session_start_event: config.session_start_event,
            compaction: tokio::sync::Mutex::new(compaction),
            compaction_abort,
            listeners: Mutex::new(Vec::new()),
            next_listener_id: AtomicU64::new(0),
            unsubscribe_agent: Mutex::new(None),
            is_agent_run_active: AtomicBool::new(false),
            idle_notify: Notify::new(),
            steering_messages: Mutex::new(Vec::new()),
            follow_up_messages: Mutex::new(Vec::new()),
            pending_next_turn_messages: Mutex::new(Vec::new()),
            last_assistant_message: Mutex::new(None),
            retry_attempt: Mutex::new(0),
            retry_abort: Mutex::new(None),
            bash_tokens: Mutex::new(Vec::new()),
            pending_bash_messages: Mutex::new(Vec::new()),
            scoped_models: Mutex::new(config.scoped_models),
            base_system_prompt: Mutex::new(String::new()),
            base_system_prompt_options: Mutex::new(BuildSystemPromptOptions {
                custom_prompt: None,
                selected_tools: None,
                tool_snippets: None,
                prompt_guidelines: Vec::new(),
                append_system_prompt: None,
                cwd: PathBuf::new(),
                context_files: Vec::new(),
                skills_xml: None,
                doc_paths: None,
            }),
            system_prompt_override: Mutex::new(None),
            tool_registry: Mutex::new(OrderedMap::default()),
            tool_definitions: Mutex::new(OrderedMap::default()),
            custom_tools: config.custom_tools.clone(),
            session_env_cell: Arc::new(std::sync::RwLock::new(crate::tools::SessionEnv {
                session_id: String::new(),
                session_file: None,
                provider: None,
                model: None,
                reasoning_level: None,
            })),
            allowed_tool_names: config
                .allowed_tool_names
                .map(|names| names.into_iter().collect()),
            excluded_tool_names: config
                .excluded_tool_names
                .map(|names| names.into_iter().collect()),
            extension_mode: Mutex::new(ExtensionMode::Print),
            extension_error_listener: Mutex::new(None),
            extension_shutdown_handler: Mutex::new(None),
            extension_error_unsubscriber: Mutex::new(None),
        });

        // Subscribe to agent events for internal handling (persistence,
        // auto-compaction, retry logic) (agent-session.ts:393).
        let session = AgentSession { inner };
        let weak = Arc::downgrade(&session.inner);
        let unsubscribe = session
            .inner
            .agent
            .subscribe(Arc::new(move |event, _signal| {
                let weak = weak.clone();
                Box::pin(async move {
                    if let Some(inner) = weak.upgrade() {
                        AgentSession { inner }.handle_agent_event(event).await;
                    }
                })
            }));
        *lock(&session.inner.unsubscribe_agent) = Some(unsubscribe);

        // Re-emit compaction events as session events (agent-session.ts's
        // `_emit` for the compaction slice). The sink is set here because it
        // must reference the shared inner state.
        {
            let weak = Arc::downgrade(&session.inner);
            if let Ok(mut runner) = session.inner.compaction.try_lock() {
                runner.set_emit_sink(Arc::new(move |event| {
                    if let Some(inner) = weak.upgrade() {
                        AgentSession { inner }.emit(AgentSessionEvent::Compaction(Box::new(event)));
                    }
                }));
            }
        }

        session.build_tool_runtime(config.initial_active_tool_names);
        session
    }

    fn runner(&self) -> Arc<dyn ExtensionRunner> {
        read_runner(&self.inner.extension_runner_ref)
    }

    /// Weak handle for the host-action bridge (T15 W3).
    pub fn downgrade(&self) -> WeakAgentSession {
        WeakAgentSession {
            inner: Arc::downgrade(&self.inner),
        }
    }

    // ==================================================================
    // Event subscription
    // ==================================================================

    fn emit(&self, event: AgentSessionEvent) {
        let listeners = lock(&self.inner.listeners);
        for (_, listener) in listeners.iter() {
            listener(event.clone());
        }
    }

    fn emit_queue_update(&self) {
        let steering = lock(&self.inner.steering_messages).clone();
        let follow_up = lock(&self.inner.follow_up_messages).clone();
        self.emit(AgentSessionEvent::Session(SessionEvent::QueueUpdate {
            steering,
            follow_up,
        }));
    }

    fn resolve_idle_wait_if_idle(&self) {
        if !self.inner.is_agent_run_active.load(Ordering::SeqCst) {
            self.inner.idle_notify.notify_waiters();
        }
    }

    async fn emit_agent_settled(&self) {
        self.inner
            .is_agent_run_active
            .store(false, Ordering::SeqCst);
        self.runner().emit("agent_settled").await;
        self.emit(AgentSessionEvent::Session(SessionEvent::AgentSettled));
        self.resolve_idle_wait_if_idle();
    }

    /// `_handleAgentEvent` (agent-session.ts:595-666).
    async fn handle_agent_event(&self, event: AgentEvent) {
        // When a user message starts, remove it from the pending queues
        // BEFORE emitting (agent-session.ts:598-616).
        if let AgentEvent::MessageStart {
            message: AgentMessage::User(user),
        } = &event
        {
            self.inner.compaction.lock().await.reset_overflow_recovery();
            let message_text = content_text_user(&user.content, "");
            if !message_text.is_empty() {
                let mut steering = lock(&self.inner.steering_messages);
                if let Some(index) = steering.iter().position(|m| *m == message_text) {
                    steering.remove(index);
                    drop(steering);
                    self.emit_queue_update();
                } else {
                    drop(steering);
                    let mut follow_up = lock(&self.inner.follow_up_messages);
                    if let Some(index) = follow_up.iter().position(|m| *m == message_text) {
                        follow_up.remove(index);
                        drop(follow_up);
                        self.emit_queue_update();
                    }
                }
            }
        }

        // Emit to extensions first (agent-session.ts:619) — no-op seam.
        let replaced = self.emit_extension_event(&event).await;
        let event = match replaced {
            Some(message) => AgentEvent::MessageEnd { message },
            None => event,
        };

        // Notify all listeners (agent-session.ts:622).
        match &event {
            AgentEvent::AgentEnd { messages } => {
                let will_retry = self.will_retry_after_agent_end(messages);
                self.emit(AgentSessionEvent::AgentEnd(Box::new(AgentEndEvent {
                    messages: messages.clone(),
                    will_retry,
                })));
            }
            other => self.emit(AgentSessionEvent::Agent(Box::new(other.clone()))),
        }

        // Session persistence (agent-session.ts:624-665).
        if let AgentEvent::MessageEnd { message } = &event {
            match message {
                AgentMessage::Custom(custom) => {
                    let result = lock(&self.inner.session_manager).append_custom_message_entry(
                        &custom.custom_type,
                        custom.content.clone(),
                        custom.display,
                        custom.details.clone(),
                    );
                    if let Err(error) = result {
                        tracing::warn!("session append failed: {error}");
                    }
                }
                AgentMessage::User(_)
                | AgentMessage::Assistant(_)
                | AgentMessage::ToolResult(_) => {
                    let result = lock(&self.inner.session_manager).append_message(message.clone());
                    if let Err(error) = result {
                        tracing::warn!("session append failed: {error}");
                    }
                }
                _ => {}
            }

            self.sync_session_env();
            if let AgentMessage::Assistant(assistant) = message {
                *lock(&self.inner.last_assistant_message) = Some(assistant.clone());
                // A length stop must not reset the overflow recovery budget:
                // the truncated response may itself trigger recovery
                // (agent-session.ts:653-654 @ 32850ef7c).
                if assistant.stop_reason != StopReason::Error
                    && assistant.stop_reason != StopReason::Length
                {
                    self.inner.compaction.lock().await.reset_overflow_recovery();
                }
                // Reset the retry counter on a successful assistant response
                // (agent-session.ts:654-663).
                if assistant.stop_reason != StopReason::Error && self.retry_attempt() > 0 {
                    let attempt = self.retry_attempt();
                    self.emit(AgentSessionEvent::Session(SessionEvent::AutoRetryEnd {
                        success: true,
                        attempt,
                        final_error: None,
                    }));
                    self.set_retry_attempt(0);
                }
            }
        }
    }

    /// `_willRetryAfterAgentEnd` (agent-session.ts:668-681).
    fn will_retry_after_agent_end(&self, messages: &[AgentMessage]) -> bool {
        let settings = lock(&self.inner.resource_loader)
            .settings_manager_mut()
            .get_retry_settings();
        if !settings.enabled || u64::from(self.retry_attempt()) >= settings.max_retries {
            return false;
        }
        for message in messages.iter().rev() {
            if let AgentMessage::Assistant(assistant) = message {
                return self.is_retryable_error(assistant);
            }
        }
        false
    }

    /// `_findLastAssistantMessage` (agent-session.ts:684-693).
    fn find_last_assistant_message(&self) -> Option<AssistantMessage> {
        self.inner
            .agent
            .state()
            .messages
            .iter()
            .rev()
            .find_map(|message| match message {
                AgentMessage::Assistant(assistant) => Some(assistant.clone()),
                _ => None,
            })
    }

    /// `_emitExtensionEvent` (agent-session.ts:712-793) — no-op seam calls,
    /// same call sites as upstream. Returns the (possibly
    /// extension-replaced) `message_end` message for the persistence path.
    async fn emit_extension_event(&self, event: &AgentEvent) -> Option<AgentMessage> {
        let runner = self.runner();
        match event {
            AgentEvent::AgentStart => runner.emit("agent_start").await,
            AgentEvent::AgentEnd { .. } => runner.emit("agent_end").await,
            AgentEvent::TurnStart => runner.emit("turn_start").await,
            AgentEvent::TurnEnd { .. } => runner.emit("turn_end").await,
            AgentEvent::MessageStart { .. } => runner.emit("message_start").await,
            AgentEvent::MessageUpdate { .. } => runner.emit("message_update").await,
            AgentEvent::MessageEnd { message } => {
                if let Some(replacement) = runner.emit_message_end(message).await {
                    // Extension-replaced messages re-enter state before
                    // persistence (agent-session.ts:752-765).
                    let mut messages = self.inner.agent.state().messages;
                    if let Some(slot) = messages.iter_mut().rev().find(|m| *m == message) {
                        *slot = replacement.clone();
                        self.inner.agent.set_messages(messages);
                    }
                    return Some(replacement);
                }
            }
            AgentEvent::ToolExecutionStart { .. } => runner.emit("tool_execution_start").await,
            AgentEvent::ToolExecutionUpdate { .. } => runner.emit("tool_execution_update").await,
            AgentEvent::ToolExecutionEnd { .. } => runner.emit("tool_execution_end").await,
        }
        None
    }

    /// `subscribe` (agent-session.ts:800-810).
    pub fn subscribe(&self, listener: AgentSessionEventListener) -> Box<dyn FnOnce() + Send> {
        let id = self.inner.next_listener_id.fetch_add(1, Ordering::SeqCst);
        lock(&self.inner.listeners).push((id, listener));
        let inner = self.inner.clone();
        Box::new(move || {
            lock(&inner.listeners).retain(|(listener_id, _)| *listener_id != id);
        })
    }

    /// `dispose` (agent-session.ts:837-854).
    pub fn dispose(&self) {
        self.abort_retry();
        self.abort_compaction();
        self.abort_bash();
        self.inner.agent.abort();
        // Unsubscribe before the aborted run's tail events (aborted
        // `message_end`, retry/compaction triggers) can still reach this
        // session's persistence (agent-session.ts:851 `_disconnectFromAgent`).
        self.disconnect_from_agent();
        // Detach the extension error listener (T15 W3): it lives in the
        // host's runner core, and RPC-mode listeners capture the stdout
        // sender — leaving it subscribed cycles session ↔ host ↔ output and
        // the process never reaches exit.
        if let Some(unsubscribe) = lock(&self.inner.extension_error_unsubscriber).take() {
            unsubscribe();
        }
        lock(&self.inner.extension_error_listener).take();
        // Drop the UI bridge for the same reason (T15 W4: the RPC bridge
        // holds the stdout sender; the interactive bridge holds the UI).
        if let Some(host) = crate::core::extension_host_adapter::host_of_runner(&self.runner()) {
            host.clear_ui();
        }
        self.runner().invalidate(
            "This extension ctx is stale after session replacement or reload. Do not use a captured pi or command ctx after ctx.newSession(), ctx.fork(), ctx.switchSession(), or ctx.reload(). For newSession, fork, and switchSession, move post-replacement work into withSession and use the ctx passed to withSession. For reload, do not use the old ctx after await ctx.reload().",
        );
        lock(&self.inner.listeners).clear();
    }

    // ==================================================================
    // Read-only state access
    // ==================================================================

    pub fn agent(&self) -> &Arc<Agent> {
        &self.inner.agent
    }

    pub fn session_manager(&self) -> Arc<Mutex<SessionManager>> {
        self.inner.session_manager.clone()
    }

    /// Settings access (upstream `session.settingsManager`): the settings
    /// manager is owned by the resource loader (T09 ownership model).
    pub fn settings_manager<R>(&self, f: impl FnOnce(&mut SettingsManager) -> R) -> R {
        let mut loader = lock(&self.inner.resource_loader);
        f(loader.settings_manager_mut())
    }

    pub fn resource_loader(&self) -> Arc<Mutex<DefaultResourceLoader>> {
        self.inner.resource_loader.clone()
    }

    pub fn model_runtime(&self) -> &Arc<ModelRuntime> {
        &self.inner.model_runtime
    }

    pub fn cwd(&self) -> &str {
        &self.inner.cwd
    }

    /// `get model` (agent-session.ts:866-868).
    pub fn model(&self) -> Option<Model> {
        model_or_none(&self.inner.agent.state().model)
    }

    /// `get thinkingLevel` (agent-session.ts:871-873).
    pub fn thinking_level(&self) -> ThinkingLevel {
        self.inner.agent.state().thinking_level
    }

    /// `get isStreaming` (agent-session.ts:876-878).
    pub fn is_streaming(&self) -> bool {
        self.inner.is_agent_run_active.load(Ordering::SeqCst)
    }

    /// `get isIdle` (agent-session.ts:881-883).
    pub fn is_idle(&self) -> bool {
        !self.is_streaming()
    }

    /// `get systemPrompt` (agent-session.ts:886-888).
    pub fn system_prompt(&self) -> String {
        self.inner.agent.state().system_prompt
    }

    /// `get retryAttempt` (agent-session.ts:891-893).
    pub fn retry_attempt(&self) -> u32 {
        *lock(&self.inner.retry_attempt)
    }

    fn set_retry_attempt(&self, attempt: u32) {
        *lock(&self.inner.retry_attempt) = attempt;
    }

    /// `getActiveToolNames` (agent-session.ts:899-901).
    pub fn get_active_tool_names(&self) -> Vec<String> {
        self.inner
            .agent
            .state()
            .tools
            .iter()
            .map(|tool| tool.name().to_owned())
            .collect()
    }

    /// `get isCompacting` (agent-session.ts:944-950) — true while a manual
    /// or auto compaction/summary holds the runner.
    pub fn is_compacting(&self) -> bool {
        self.inner.compaction.try_lock().is_err()
    }

    /// `get messages` (agent-session.ts:953-955).
    pub fn messages(&self) -> Vec<AgentMessage> {
        self.inner.agent.state().messages
    }

    /// `state.messages.length > 0` (interactive-mode.ts:993) without cloning
    /// the history.
    pub fn has_messages(&self) -> bool {
        !self.inner.agent.state().messages.is_empty()
    }

    pub fn steering_mode(&self) -> QueueMode {
        self.inner.agent.steering_mode()
    }

    pub fn follow_up_mode(&self) -> QueueMode {
        self.inner.agent.follow_up_mode()
    }

    pub fn session_file(&self) -> Option<PathBuf> {
        lock(&self.inner.session_manager)
            .get_session_file()
            .map(|p| p.to_path_buf())
    }

    pub fn session_id(&self) -> String {
        lock(&self.inner.session_manager)
            .get_session_id()
            .to_owned()
    }

    pub fn session_name(&self) -> Option<String> {
        lock(&self.inner.session_manager).get_session_name()
    }

    pub fn scoped_models(&self) -> Vec<ScopedModel> {
        lock(&self.inner.scoped_models).clone()
    }

    /// `setScopedModels` (agent-session.ts:988-990).
    pub fn set_scoped_models(&self, scoped_models: Vec<ScopedModel>) {
        *lock(&self.inner.scoped_models) = scoped_models;
    }

    /// `get promptTemplates` (agent-session.ts:993-995).
    pub fn prompt_templates(&self) -> Vec<crate::core::prompt_templates::PromptTemplate> {
        lock(&self.inner.resource_loader)
            .resources()
            .prompts
            .clone()
    }

    // ==================================================================
    // Tool registry & system prompt
    // ==================================================================

    fn is_allowed_tool(&self, name: &str) -> bool {
        self.inner
            .allowed_tool_names
            .as_ref()
            .is_none_or(|allowed| allowed.contains(name))
            && !self
                .inner
                .excluded_tool_names
                .as_ref()
                .is_some_and(|excluded| excluded.contains(name))
    }

    /// `_buildRuntime` tool half (agent-session.ts:2547-2599 →
    /// `_refreshToolRegistry` with the initial active set and
    /// `includeAllExtensionTools: true`, agent-session.ts:394-400).
    fn build_tool_runtime(&self, initial_active_tool_names: Option<Vec<String>>) {
        let default_active: Vec<String> = match initial_active_tool_names {
            Some(names) => names,
            None => ["read", "bash", "edit", "write"]
                .map(str::to_owned)
                .to_vec(),
        };
        self.refresh_tool_registry(RefreshToolRegistryOptions {
            active_tool_names: Some(default_active),
            include_all_extension_tools: true,
        });
    }
    /// `_normalizePromptSnippet` (agent-session.ts:997-1004).
    fn normalize_prompt_snippet(text: Option<&str>) -> Option<String> {
        let text = text?;
        let mut one_line = String::with_capacity(text.len());
        let mut pending_space = false;
        for ch in text.chars() {
            if ch.is_whitespace() {
                pending_space = true;
            } else {
                if pending_space && !one_line.is_empty() {
                    one_line.push(' ');
                }
                one_line.push(ch);
                pending_space = false;
            }
        }
        let trimmed = one_line.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_owned())
        }
    }

    /// `_normalizePromptGuidelines` (agent-session.ts:1006-1017).
    fn normalize_prompt_guidelines(guidelines: Option<&[String]>) -> Vec<String> {
        let mut unique: Vec<String> = Vec::new();
        for guideline in guidelines.unwrap_or(&[]) {
            let normalized = guideline.trim();
            if !normalized.is_empty() && !unique.iter().any(|g| g == normalized) {
                unique.push(normalized.to_owned());
            }
        }
        unique
    }

    fn synthetic_tool_source_info(path: &str, source: &str) -> crate::core::skills::SourceInfo {
        crate::core::skills::SourceInfo {
            path: PathBuf::from(path),
            source: source.to_owned(),
            scope: crate::core::skills::SourceScope::Temporary,
            origin: crate::core::skills::SourceOrigin::TopLevel,
            base_dir: None,
        }
    }

    /// Build fresh built-in tool executables with settings-derived options
    /// (agent-session.ts:2552-2564).
    fn build_builtin_tools(&self) -> Vec<Arc<dyn AgentTool>> {
        let ctx = crate::tools::ToolContext {
            cwd: PathBuf::from(&self.inner.cwd),
            session_env: Some(self.inner.session_env_cell.clone()),
        };
        let tool_options = {
            let loader = lock(&self.inner.resource_loader);
            let settings = loader.settings_manager();
            crate::tools::BuiltinToolOptions {
                auto_resize_images: Some(settings.get_image_auto_resize()),
                shell_command_prefix: settings.get_shell_command_prefix(),
                shell_path: settings.get_shell_path(),
            }
        };
        const ALL_BUILTIN_TOOL_NAMES: [&str; 7] =
            ["read", "bash", "edit", "write", "grep", "find", "ls"];
        crate::tools::create_builtin_tools(
            &ctx,
            &ALL_BUILTIN_TOOL_NAMES.map(str::to_owned),
            &tool_options,
        )
    }

    /// `_refreshToolRegistry` (agent-session.ts:2454-2545): full rebuild of
    /// the executable registry + definition metadata (built-in → extension →
    /// SDK custom, later `Map.set` wins per name), then the active-set
    /// computation and system-prompt rebuild.
    fn refresh_tool_registry(&self, options: RefreshToolRegistryOptions) {
        self.sync_session_env();
        let previous_registry_names: HashSet<String> =
            lock(&self.inner.tool_registry).keys().cloned().collect();
        let previous_active_tool_names = self.get_active_tool_names();

        let mut definitions: OrderedMap<ToolDefinitionEntry> = OrderedMap::default();
        let mut registry: OrderedMap<Arc<dyn AgentTool>> = OrderedMap::default();

        for tool in self.build_builtin_tools() {
            if !self.is_allowed_tool(tool.name()) {
                continue;
            }
            let name = tool.name().to_owned();
            definitions.insert(
                name.clone(),
                ToolDefinitionEntry {
                    description: tool.description().to_owned(),
                    parameters: tool.parameters().clone(),
                    prompt_snippet: Self::normalize_prompt_snippet(builtin_tool_snippet(&name)),
                    prompt_guidelines: Self::normalize_prompt_guidelines(Some(
                        &builtin_tool_guidelines(&name)
                            .iter()
                            .map(|g| (*g).to_owned())
                            .collect::<Vec<_>>(),
                    )),
                    source_info: Self::synthetic_tool_source_info(
                        &format!("<builtin:{name}>"),
                        "builtin",
                    ),
                },
            );
            registry.insert(name, tool);
        }
        // Extension tools (override built-ins by name, agent-session.ts:2514-2517).
        for entry in self.runner().extension_tool_entries() {
            if !self.is_allowed_tool(&entry.name) {
                continue;
            }
            definitions.insert(
                entry.name.clone(),
                ToolDefinitionEntry {
                    description: entry.description,
                    parameters: entry.parameters,
                    prompt_snippet: Self::normalize_prompt_snippet(entry.prompt_snippet.as_deref()),
                    prompt_guidelines: Self::normalize_prompt_guidelines(
                        entry.prompt_guidelines.as_deref(),
                    ),
                    source_info: entry.source_info,
                },
            );
            registry.insert(entry.name.clone(), entry.tool);
        }
        // SDK custom tools (agent-session.ts:2465-2468).
        for tool in &self.inner.custom_tools {
            if !self.is_allowed_tool(tool.name()) {
                continue;
            }
            let tool = tool.clone();
            let name = tool.name().to_owned();
            definitions.insert(
                name.clone(),
                ToolDefinitionEntry {
                    description: tool.description().to_owned(),
                    parameters: tool.parameters().clone(),
                    prompt_snippet: None,
                    prompt_guidelines: Vec::new(),
                    source_info: Self::synthetic_tool_source_info(&format!("<sdk:{name}>"), "sdk"),
                },
            );
            registry.insert(name, tool);
        }

        *lock(&self.inner.tool_definitions) = definitions;
        *lock(&self.inner.tool_registry) = registry;

        // Active-set computation (agent-session.ts:2518-2543).
        let mut next_active: Vec<String> = options
            .active_tool_names
            .clone()
            .unwrap_or(previous_active_tool_names)
            .into_iter()
            .filter(|name| self.is_allowed_tool(name))
            .collect();
        if self.inner.allowed_tool_names.is_some() {
            for name in lock(&self.inner.tool_registry).keys() {
                if self.is_allowed_tool(name) {
                    next_active.push(name.clone());
                }
            }
        } else if options.include_all_extension_tools {
            // `wrappedExtensionTools` = extension + SDK custom tools
            // (built-ins are never auto-activated here).
            let definitions = lock(&self.inner.tool_definitions);
            for name in definitions.keys() {
                if definitions
                    .get(name)
                    .is_some_and(|entry| entry.source_info.source != "builtin")
                {
                    next_active.push(name.clone());
                }
            }
        } else if options.active_tool_names.is_none() {
            for name in lock(&self.inner.tool_registry).keys() {
                if !previous_registry_names.contains(name) {
                    next_active.push(name.clone());
                }
            }
        }
        // Upstream `[...new Set(names)]`: dedup keeping first-occurrence
        // order (agent-session.ts:2544) — no sorting.
        let mut seen: HashSet<String> = HashSet::new();
        next_active.retain(|name| seen.insert(name.clone()));
        self.set_active_tools_by_name(next_active);
    }

    /// Refresh the shared `RPI_*` session env cell (requirements §3.3: bash
    /// resolves the tools' env per command spawn; model switches take effect immediately).
    fn sync_session_env(&self) {
        let model = self.model();
        let mut cell = self
            .inner
            .session_env_cell
            .write()
            .unwrap_or_else(|e| e.into_inner());
        cell.session_id = self.session_id();
        cell.session_file = self.session_file();
        cell.provider = model.as_ref().map(|m| m.provider.clone());
        cell.model = model.as_ref().map(|m| m.id.clone());
        cell.reasoning_level = Some(thinking_level_str(self.thinking_level()).to_owned());
    }

    /// `refreshTools` action target (`_refreshToolRegistry()` no-options
    /// call, agent-session.ts:2395): re-reads the extension runner's
    /// registered tools, so `registerTool` at runtime takes effect.
    pub fn refresh_extension_tools(&self) {
        self.refresh_tool_registry(RefreshToolRegistryOptions::default());
    }

    /// `getAllTools` (agent-session.ts:906-913) — `ToolInfo[]` JSON.
    pub fn get_all_tools(&self) -> Vec<serde_json::Value> {
        let definitions = lock(&self.inner.tool_definitions);
        definitions
            .keys()
            .map(|name| {
                let entry = definitions.get(name).expect("key from the same map");
                serde_json::json!({
                    "name": name,
                    "description": entry.description,
                    "parameters": entry.parameters,
                    "promptGuidelines": if entry.prompt_guidelines.is_empty() {
                        None
                    } else {
                        Some(entry.prompt_guidelines.clone())
                    },
                    "sourceInfo": entry.source_info,
                })
            })
            .collect()
    }

    /// `getCommands` (agent-session.ts:2332-2355): extension commands (with
    /// `:N` invocation names) → prompt templates → skills.
    pub fn get_commands_info(&self) -> Vec<serde_json::Value> {
        let mut commands: Vec<serde_json::Value> = Vec::new();
        for command in self.runner().registered_commands() {
            commands.push(serde_json::json!({
                "name": command.invocation_name,
                "description": command.description,
                "source": "extension",
                "sourceInfo": command.source_info,
            }));
        }
        let (agent_dir, skills) = {
            let loader = lock(&self.inner.resource_loader);
            (
                loader.agent_dir().to_path_buf(),
                loader.resources().skills.clone(),
            )
        };
        let cwd = PathBuf::from(&self.inner.cwd);
        for template in self.prompt_templates() {
            commands.push(serde_json::json!({
                "name": template.name,
                "description": template.description,
                "source": "prompt",
                "sourceInfo": crate::modes::rpc::prompt_template_source_info(&template, &agent_dir, &cwd),
            }));
        }
        for skill in skills {
            commands.push(serde_json::json!({
                "name": format!("skill:{}", skill.name),
                "description": skill.description,
                "source": "skill",
                "sourceInfo": skill.source_info,
            }));
        }
        commands
    }

    /// `setActiveToolsByName` (agent-session.ts:926-941).
    pub fn set_active_tools_by_name(&self, tool_names: Vec<String>) {
        let registry = lock(&self.inner.tool_registry);
        let mut tools: Vec<Arc<dyn AgentTool>> = Vec::new();
        let mut valid_names: Vec<String> = Vec::new();
        for name in &tool_names {
            if let Some(tool) = registry.get(name) {
                tools.push(tool.clone());
                valid_names.push(name.clone());
            }
        }
        drop(registry);
        self.inner.agent.set_tools(tools);

        let base = self.rebuild_system_prompt(&valid_names);
        *lock(&self.inner.base_system_prompt) = base.clone();
        let override_ = lock(&self.inner.system_prompt_override).clone();
        self.inner
            .agent
            .set_system_prompt(override_.unwrap_or(base));
    }

    /// `_rebuildSystemPrompt` (agent-session.ts:1021-1055).
    fn rebuild_system_prompt(&self, tool_names: &[String]) -> String {
        let (valid, tool_snippets, prompt_guidelines) = {
            let definitions = lock(&self.inner.tool_definitions);
            let valid: Vec<String> = tool_names
                .iter()
                .filter(|name| definitions.get(name).is_some())
                .cloned()
                .collect();
            // Snippets/guidelines ride the (possibly extension-overridden)
            // definitions — overrides do NOT inherit the built-in text
            // (agent-session.ts:2489-2503).
            let mut tool_snippets: HashMap<String, String> = HashMap::new();
            let mut prompt_guidelines: Vec<String> = Vec::new();
            for name in &valid {
                let entry = definitions.get(name).expect("filtered above");
                if let Some(snippet) = &entry.prompt_snippet {
                    tool_snippets.insert(name.clone(), snippet.clone());
                }
                for guideline in &entry.prompt_guidelines {
                    if !prompt_guidelines.iter().any(|g| g == guideline) {
                        prompt_guidelines.push(guideline.clone());
                    }
                }
            }
            (valid, tool_snippets, prompt_guidelines)
        };

        let loader = lock(&self.inner.resource_loader);
        let resources = loader.resources();
        let custom_prompt = resources.system_prompt.clone();
        let append_system_prompt = if resources.append_system_prompt.is_empty() {
            None
        } else {
            Some(resources.append_system_prompt.join("\n\n"))
        };
        let skills = resources.skills.clone();
        let context_files = resources.context_files.clone();
        let read_active = valid.iter().any(|name| name == "read");
        let skills_xml = {
            let xml = crate::core::skills::format_skills_for_prompt(&skills, read_active);
            if xml.is_empty() {
                None
            } else {
                Some(xml)
            }
        };
        drop(loader);

        let options = BuildSystemPromptOptions {
            custom_prompt,
            selected_tools: Some(valid),
            tool_snippets: Some(tool_snippets),
            prompt_guidelines,
            append_system_prompt,
            cwd: PathBuf::from(&self.inner.cwd),
            context_files,
            skills_xml,
            doc_paths: None,
        };
        let prompt = build_system_prompt(&options);
        *lock(&self.inner.base_system_prompt_options) = options;
        prompt
    }

    // ==================================================================
    // Prompting
    // ==================================================================

    /// `_runAgentPrompt` (agent-session.ts:1061-1073).
    async fn run_agent_prompt(&self, messages: Vec<AgentMessage>) -> Result<(), RpiError> {
        self.inner.is_agent_run_active.store(true, Ordering::SeqCst);
        let result = async {
            self.inner
                .agent
                .prompt(messages)
                .await
                .map_err(agent_error_to_rpi)?;
            while self.handle_post_agent_run().await {
                self.inner
                    .agent
                    .continue_run()
                    .await
                    .map_err(agent_error_to_rpi)?;
            }
            Ok(())
        }
        .await;
        // finally (agent-session.ts:1068-1072).
        *lock(&self.inner.system_prompt_override) = None;
        self.flush_pending_bash_messages();
        self.emit_agent_settled().await;
        result
    }

    /// `_handlePostAgentRun` (agent-session.ts:1075-1103).
    async fn handle_post_agent_run(&self) -> bool {
        let msg = lock(&self.inner.last_assistant_message).take();
        let Some(msg) = msg else {
            return false;
        };

        if self.is_retryable_error(&msg) && self.prepare_retry(&msg).await {
            return true;
        }

        if msg.stop_reason == StopReason::Error && self.retry_attempt() > 0 {
            let attempt = self.retry_attempt();
            self.emit(AgentSessionEvent::Session(SessionEvent::AutoRetryEnd {
                success: false,
                attempt,
                final_error: msg.error_message.clone(),
            }));
            self.set_retry_attempt(0);
        }

        if self.check_compaction(&msg, true).await {
            return true;
        }

        // Messages queued by agent_end handlers need a continuation.
        self.inner.agent.has_queued_messages()
    }

    /// `prompt` (agent-session.ts:1114-1265).
    pub async fn prompt(&self, text: &str, options: PromptOptions) -> Result<(), RpiError> {
        let expand_prompt_templates = options.expand_prompt_templates.unwrap_or(true);
        let mut preflight_result = options.preflight_result;
        let mut images = options.images;

        let result: Result<Option<Vec<AgentMessage>>, RpiError> = async {
            // Extension commands first (agent-session.ts:1121-1129) — no-op
            // runner never has commands.
            if expand_prompt_templates
                && text.starts_with('/')
                && self.try_execute_extension_command(text).await
            {
                return Ok(None);
            }

            // Reject prompts while manual compaction is in progress
            // (agent-session.ts:1133-1137 @ 8eda4f5b2). The abort-token cell
            // is `Some` only during compaction; auto-compaction holds the
            // runner mutex but the token is also set there.
            if !self
                .inner
                .compaction_abort
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_none()
            {
                return Err(RpiError::Session(
                    "Cannot submit a prompt while compaction is in progress. Wait for compaction to finish and retry."
                        .to_owned(),
                ));
            }

            // Input event for extension interception (agent-session.ts:1132-1149).
            let mut current_text = text.to_owned();
            if self.runner().has_handlers("input") {
                match self
                    .runner()
                    .emit_input(
                        &current_text,
                        images.as_deref(),
                        options.source.unwrap_or(InputSource::Interactive),
                        if self.is_streaming() {
                            options.streaming_behavior
                        } else {
                            None
                        },
                    )
                    .await
                {
                    InputEventResult::Handled => return Ok(None),
                    InputEventResult::Transform { text, images: new_images } => {
                        current_text = text;
                        if let Some(new_images) = new_images {
                            images = Some(new_images);
                        }
                    }
                    InputEventResult::Continue => {}
                }
            }

            // Skill commands and prompt templates (agent-session.ts:1151-1156).
            let mut expanded_text = current_text;
            if expand_prompt_templates {
                expanded_text = self.expand_skill_command(&expanded_text);
                expanded_text = expand_prompt_template(&expanded_text, &self.prompt_templates());
            }

            // Queue while streaming (agent-session.ts:1158-1172).
            if self.is_streaming() {
                let Some(behavior) = options.streaming_behavior else {
                    return Err(RpiError::Session(
                        "Agent is already processing. Specify streamingBehavior ('steer' or 'followUp') to queue the message."
                            .to_owned(),
                    ));
                };
                match behavior {
                    StreamingBehavior::FollowUp => {
                        self.queue_follow_up(&expanded_text, images).await
                    }
                    StreamingBehavior::Steer => self.queue_steer(&expanded_text, images).await,
                }
                return Ok(None);
            }

            // Flush pending bash messages before the new prompt.
            self.flush_pending_bash_messages();

            // Validate model (agent-session.ts:1177-1195).
            let Some(model) = self.model() else {
                return Err(RpiError::Session(format_no_model_selected_message()));
            };
            let has_configured_auth = self.inner.model_runtime.has_configured_auth(&model.provider)
                || self
                    .inner
                    .model_runtime
                    .check_auth(&model.provider)
                    .await
                    .map_err(|error| RpiError::Session(error.message))?
                    .is_some();
            if !has_configured_auth {
                if self.inner.model_runtime.is_using_oauth(&model.provider) {
                    return Err(RpiError::Session(format!(
                        "Authentication failed for \"{}\". Credentials may have expired or network is unavailable. Run '/login {}' to re-authenticate.",
                        model.provider, model.provider
                    )));
                }
                return Err(RpiError::Session(format_no_api_key_found_message(
                    &model.provider,
                )));
            }

            // Pre-prompt compaction check (agent-session.ts:1197-1202).
            if let Some(last_assistant) = self.find_last_assistant_message() {
                self.check_compaction(&last_assistant, false).await;
            }

            // Build the message batch (agent-session.ts:1204-1253).
            let mut messages: Vec<AgentMessage> = Vec::new();
            let mut user_content: Vec<UserContentBlock> = vec![UserContentBlock::Text(
                TextContent {
                    text: expanded_text.clone(),
                    text_signature: None,
                },
            )];
            if let Some(images) = images.clone() {
                user_content.extend(images.into_iter().map(UserContentBlock::Image));
            }
            messages.push(AgentMessage::User(UserMessage {
                role: UserRole::User,
                content: UserContent::Blocks(user_content),
                timestamp: now_millis(),
            }));

            // Pending "nextTurn" asides.
            messages.extend(lock(&self.inner.pending_next_turn_messages).drain(..));

            // before_agent_start extension event (agent-session.ts:1224-1253).
            // The event carries the fully assembled base system prompt and
            // the build options; the runner chains `systemPrompt`
            // replacements across handlers (runner.ts:1068-1132).
            let (base_prompt, prompt_options_json) = {
                let base = lock(&self.inner.base_system_prompt).clone();
                (base, self.system_prompt_options_json())
            };
            if let Some(result) = self
                .runner()
                .emit_before_agent_start(
                    &expanded_text,
                    images.as_deref(),
                    &base_prompt,
                    prompt_options_json,
                )
                .await
            {
                for msg in result.messages {
                    messages.push(AgentMessage::Custom(CustomMessage {
                        role: CustomRole::Custom,
                        custom_type: msg.custom_type,
                        content: msg.content.unwrap_or_default(),
                        display: msg.display,
                        details: msg.details,
                        timestamp: now_millis(),
                    }));
                }
                if let Some(system_prompt) = result.system_prompt {
                    *lock(&self.inner.system_prompt_override) = Some(system_prompt.clone());
                    self.inner.agent.set_system_prompt(system_prompt);
                } else {
                    *lock(&self.inner.system_prompt_override) = None;
                    let base = lock(&self.inner.base_system_prompt).clone();
                    self.inner.agent.set_system_prompt(base);
                }
            } else {
                *lock(&self.inner.system_prompt_override) = None;
                let base = lock(&self.inner.base_system_prompt).clone();
                self.inner.agent.set_system_prompt(base);
            }

            Ok(Some(messages))
        }
        .await;

        match result {
            Ok(messages) => {
                if let Some(preflight) = preflight_result.take() {
                    preflight(true);
                }
                let Some(messages) = messages else {
                    return Ok(());
                };
                self.run_agent_prompt(messages).await
            }
            Err(error) => {
                if let Some(preflight) = preflight_result.take() {
                    preflight(false);
                }
                Err(error)
            }
        }
    }

    /// `_tryExecuteExtensionCommand` (agent-session.ts:1267-1294): parse
    /// name + args, dispatch to the runner; handler errors are reported via
    /// `emit_error` inside the runner adapter.
    async fn try_execute_extension_command(&self, text: &str) -> bool {
        let space_index = text.find(' ');
        let (command_name, args) = match space_index {
            Some(index) => (&text[1..index], &text[index + 1..]),
            None => (&text[1..], ""),
        };
        self.runner()
            .execute_extension_command(command_name, args)
            .await
    }

    /// `_expandSkillCommand` (agent-session.ts:1301-1325).
    fn expand_skill_command(&self, text: &str) -> String {
        let Some(rest) = text.strip_prefix("/skill:") else {
            return text.to_owned();
        };
        let space_index = rest.find(' ');
        let (skill_name, args) = match space_index {
            Some(index) => (&rest[..index], rest[index + 1..].trim()),
            None => (rest, ""),
        };

        let skills = lock(&self.inner.resource_loader).resources().skills.clone();
        let Some(skill) = skills.iter().find(|s| s.name == skill_name) else {
            return text.to_owned();
        };

        match std::fs::read_to_string(&skill.file_path) {
            Ok(content) => {
                let body = strip_frontmatter(&content).trim().to_owned();
                let skill_block = format!(
                    "<skill name=\"{}\" location=\"{}\">\nReferences are relative to {}.\n\n{body}\n</skill>",
                    skill.name,
                    skill.file_path.display(),
                    skill.base_dir.display()
                );
                if args.is_empty() {
                    skill_block
                } else {
                    format!("{skill_block}\n\n{args}")
                }
            }
            Err(error) => {
                self.runner()
                    .emit_error(crate::core::extensions::ExtensionErrorInfo {
                        extension_path: skill.file_path.to_string_lossy().into_owned(),
                        event: "skill_expansion".to_owned(),
                        error: error.to_string(),
                    });
                text.to_owned()
            }
        }
    }

    /// `steer` (agent-session.ts:1335-1346).
    pub async fn steer(
        &self,
        text: &str,
        images: Option<Vec<ImageContent>>,
    ) -> Result<(), RpiError> {
        if text.starts_with('/') {
            self.throw_if_extension_command(text)?;
        }
        let mut expanded = self.expand_skill_command(text);
        expanded = expand_prompt_template(&expanded, &self.prompt_templates());
        self.queue_steer(&expanded, images).await;
        Ok(())
    }

    /// `followUp` (agent-session.ts:1355-1366).
    pub async fn follow_up(
        &self,
        text: &str,
        images: Option<Vec<ImageContent>>,
    ) -> Result<(), RpiError> {
        if text.starts_with('/') {
            self.throw_if_extension_command(text)?;
        }
        let mut expanded = self.expand_skill_command(text);
        expanded = expand_prompt_template(&expanded, &self.prompt_templates());
        self.queue_follow_up(&expanded, images).await;
        Ok(())
    }

    /// `_queueSteer` (agent-session.ts:1371-1383).
    async fn queue_steer(&self, text: &str, images: Option<Vec<ImageContent>>) {
        lock(&self.inner.steering_messages).push(text.to_owned());
        self.emit_queue_update();
        self.inner.agent.steer(user_message(text, images));
    }

    /// `_queueFollowUp` (agent-session.ts:1388-1400).
    async fn queue_follow_up(&self, text: &str, images: Option<Vec<ImageContent>>) {
        lock(&self.inner.follow_up_messages).push(text.to_owned());
        self.emit_queue_update();
        self.inner.agent.follow_up(user_message(text, images));
    }

    /// `_throwIfExtensionCommand` (agent-session.ts:1405-1415).
    fn throw_if_extension_command(&self, text: &str) -> Result<(), RpiError> {
        let space_index = text.find(' ');
        let command_name = match space_index {
            Some(index) => &text[1..index],
            None => &text[1..],
        };
        if self.runner().get_command(command_name).is_some() {
            return Err(RpiError::Session(format!(
                "Extension command \"/{command_name}\" cannot be queued. Use prompt() or execute the command when not streaming."
            )));
        }
        Ok(())
    }

    /// `sendCustomMessage` (agent-session.ts:1429-1463).
    pub async fn send_custom_message(
        &self,
        custom_type: &str,
        content: Option<UserContent>,
        display: bool,
        details: Option<serde_json::Value>,
        trigger_turn: bool,
        deliver_as: Option<CustomDeliverAs>,
    ) -> Result<(), RpiError> {
        let message = AgentMessage::Custom(CustomMessage {
            role: CustomRole::Custom,
            custom_type: custom_type.to_owned(),
            content: content.clone().unwrap_or_default(),
            display,
            details: details.clone(),
            timestamp: now_millis(),
        });
        if deliver_as == Some(CustomDeliverAs::NextTurn) {
            lock(&self.inner.pending_next_turn_messages).push(message);
        } else if self.is_streaming() {
            match deliver_as {
                Some(CustomDeliverAs::FollowUp) => self.inner.agent.follow_up(message),
                _ => self.inner.agent.steer(message),
            }
        } else if trigger_turn {
            self.run_agent_prompt(vec![message]).await?;
        } else {
            let mut messages = self.inner.agent.state().messages;
            messages.push(message.clone());
            self.inner.agent.set_messages(messages);
            let result = lock(&self.inner.session_manager).append_custom_message_entry(
                custom_type,
                content.unwrap_or_default(),
                display,
                details,
            );
            if let Err(error) = result {
                tracing::warn!("session append failed: {error}");
            }
            self.emit(AgentSessionEvent::Agent(Box::new(
                AgentEvent::MessageStart {
                    message: message.clone(),
                },
            )));
            self.emit(AgentSessionEvent::Agent(Box::new(AgentEvent::MessageEnd {
                message,
            })));
        }
        Ok(())
    }

    /// `sendUserMessage` (agent-session.ts:1472-1503).
    pub async fn send_user_message(
        &self,
        text: &str,
        images: Option<Vec<ImageContent>>,
        deliver_as: Option<StreamingBehavior>,
    ) -> Result<(), RpiError> {
        self.prompt(
            text,
            PromptOptions {
                expand_prompt_templates: Some(false),
                streaming_behavior: deliver_as,
                images,
                source: Some(InputSource::Extension),
                ..Default::default()
            },
        )
        .await
    }

    /// `appendEntry` host action body (agent-session.ts:2375-2382):
    /// persist a custom entry, then emit `entry_appended` with it.
    pub fn append_entry(&self, custom_type: &str, data: Option<serde_json::Value>) {
        let entry_id = {
            let result = lock(&self.inner.session_manager).append_custom_entry(custom_type, data);
            match result {
                Ok(entry_id) => entry_id,
                Err(error) => {
                    tracing::warn!("session append failed: {error}");
                    return;
                }
            }
        };
        let entry = lock(&self.inner.session_manager)
            .get_entry(&entry_id)
            .and_then(|stored| stored.known().cloned());
        if let Some(entry) = entry {
            self.emit(AgentSessionEvent::Session(SessionEvent::EntryAppended {
                entry: Box::new(entry),
            }));
        }
    }

    /// `clearQueue` (agent-session.ts:1510-1518).
    pub fn clear_queue(&self) -> (Vec<String>, Vec<String>) {
        let steering = std::mem::take(&mut *lock(&self.inner.steering_messages));
        let follow_up = std::mem::take(&mut *lock(&self.inner.follow_up_messages));
        self.inner.agent.clear_all_queues();
        self.emit_queue_update();
        (steering, follow_up)
    }

    /// `get pendingMessageCount` (agent-session.ts:1521-1523).
    pub fn pending_message_count(&self) -> usize {
        lock(&self.inner.steering_messages).len() + lock(&self.inner.follow_up_messages).len()
    }

    pub fn get_steering_messages(&self) -> Vec<String> {
        lock(&self.inner.steering_messages).clone()
    }

    pub fn get_follow_up_messages(&self) -> Vec<String> {
        lock(&self.inner.follow_up_messages).clone()
    }

    /// `abort` (agent-session.ts:1542-1546).
    pub async fn abort(&self) {
        self.abort_retry();
        self.inner.agent.abort();
        self.wait_for_idle().await;
    }

    /// `waitForIdle` (agent-session.ts:1548-1553).
    pub async fn wait_for_idle(&self) {
        loop {
            let notified = self.inner.idle_notify.notified();
            tokio::pin!(notified);
            // Register the waiter *before* checking the condition:
            // `notify_waiters` stores no permit, so a notification landing
            // between the check and the await would otherwise be lost.
            notified.as_mut().enable();
            if self.is_idle() {
                return;
            }
            notified.await;
        }
    }

    // ==================================================================
    // Model management
    // ==================================================================

    async fn emit_model_select(&self, next: &Model, source: &str) {
        let previous = self.model();
        if models_are_equal(previous.as_ref(), Some(next)) {
            return;
        }
        let _ = source;
        self.runner().emit("model_select").await;
    }

    /// `setModel` (agent-session.ts:1578-1593).
    pub async fn set_model(&self, model: Model) -> Result<(), RpiError> {
        if self
            .inner
            .model_runtime
            .check_auth(&model.provider)
            .await
            .map_err(|error| RpiError::Session(error.message))?
            .is_none()
        {
            return Err(RpiError::Session(format!(
                "No API key for {}/{}",
                model.provider, model.id
            )));
        }

        let thinking_level = self.thinking_level_for_model_switch(None);
        self.inner.agent.set_model(model.clone());
        self.sync_compaction_model();
        self.sync_session_env();
        let result =
            lock(&self.inner.session_manager).append_model_change(&model.provider, &model.id);
        if let Err(error) = result {
            tracing::warn!("session append failed: {error}");
        }
        lock(&self.inner.resource_loader)
            .settings_manager_mut()
            .set_default_model_and_provider(&model.provider, &model.id);

        self.set_thinking_level(thinking_level);
        self.emit_model_select(&model, "set").await;
        Ok(())
    }

    fn sync_compaction_model(&self) {
        let model = self.model();
        let (compaction, retry) = {
            let mut loader = lock(&self.inner.resource_loader);
            let settings = loader.settings_manager_mut();
            (
                settings.get_compaction_settings(),
                retry_config_to_policy(settings.get_retry_settings()),
            )
        };
        let thinking_level = self.thinking_level();
        if let Ok(mut runner) = self.inner.compaction.try_lock() {
            runner.set_model(model);
            runner.set_settings(rpi_agent::compaction::CompactionSettings {
                enabled: compaction.enabled,
                reserve_tokens: compaction.reserve_tokens,
                keep_recent_tokens: compaction.keep_recent_tokens,
            });
            runner.set_retry(retry);
            runner.set_thinking_level(thinking_level);
        }
    }

    /// `cycleModel` (agent-session.ts:1601-1606).
    pub async fn cycle_model(
        &self,
        direction: CycleDirection,
    ) -> Result<Option<ModelCycleResult>, RpiError> {
        if !lock(&self.inner.scoped_models).is_empty() {
            return self.cycle_scoped_model(direction).await;
        }
        self.cycle_available_model(direction).await
    }

    async fn cycle_scoped_model(
        &self,
        direction: CycleDirection,
    ) -> Result<Option<ModelCycleResult>, RpiError> {
        let scoped = lock(&self.inner.scoped_models).clone();
        let mut authenticated = Vec::new();
        for scoped_model in scoped {
            if self
                .inner
                .model_runtime
                .check_auth(&scoped_model.model.provider)
                .await
                .map_err(|error| RpiError::Session(error.message))?
                .is_some()
            {
                authenticated.push(scoped_model);
            }
        }
        if authenticated.len() <= 1 {
            return Ok(None);
        }

        let current_model = self.model();
        let current_index = authenticated
            .iter()
            .position(|sm| models_are_equal(Some(&sm.model), current_model.as_ref()))
            .unwrap_or(0);
        let len = authenticated.len();
        let next_index = match direction {
            CycleDirection::Forward => (current_index + 1) % len,
            CycleDirection::Backward => (current_index + len - 1) % len,
        };
        let next = authenticated[next_index].clone();
        let thinking_level = self.thinking_level_for_model_switch(next.thinking_level);

        self.inner.agent.set_model(next.model.clone());
        self.sync_compaction_model();
        self.sync_session_env();
        let result = lock(&self.inner.session_manager)
            .append_model_change(&next.model.provider, &next.model.id);
        if let Err(error) = result {
            tracing::warn!("session append failed: {error}");
        }
        lock(&self.inner.resource_loader)
            .settings_manager_mut()
            .set_default_model_and_provider(&next.model.provider, &next.model.id);
        self.set_thinking_level(thinking_level);
        self.emit_model_select(&next.model, "cycle").await;

        Ok(Some(ModelCycleResult {
            model: next.model,
            thinking_level: self.thinking_level(),
            is_scoped: true,
        }))
    }

    async fn cycle_available_model(
        &self,
        direction: CycleDirection,
    ) -> Result<Option<ModelCycleResult>, RpiError> {
        let available = self
            .inner
            .model_runtime
            .get_available(None)
            .await
            .map_err(|error| RpiError::Session(error.message))?;
        if available.len() <= 1 {
            return Ok(None);
        }

        let current_model = self.model();
        let current_index = available
            .iter()
            .position(|m| models_are_equal(Some(m), current_model.as_ref()))
            .unwrap_or(0);
        let len = available.len();
        let next_index = match direction {
            CycleDirection::Forward => (current_index + 1) % len,
            CycleDirection::Backward => (current_index + len - 1) % len,
        };
        let next_model = available[next_index].clone();
        let thinking_level = self.thinking_level_for_model_switch(None);

        self.inner.agent.set_model(next_model.clone());
        self.sync_compaction_model();
        self.sync_session_env();
        let result = lock(&self.inner.session_manager)
            .append_model_change(&next_model.provider, &next_model.id);
        if let Err(error) = result {
            tracing::warn!("session append failed: {error}");
        }
        lock(&self.inner.resource_loader)
            .settings_manager_mut()
            .set_default_model_and_provider(&next_model.provider, &next_model.id);
        self.set_thinking_level(thinking_level);
        self.emit_model_select(&next_model, "cycle").await;

        Ok(Some(ModelCycleResult {
            model: next_model,
            thinking_level: self.thinking_level(),
            is_scoped: false,
        }))
    }

    // ==================================================================
    // Thinking level management
    // ==================================================================

    /// `setThinkingLevel` (agent-session.ts:1677-1699).
    pub fn set_thinking_level(&self, level: ThinkingLevel) {
        let available_levels = self.get_available_thinking_levels();
        let effective = if available_levels.contains(&level) {
            level
        } else {
            match self.model() {
                Some(model) => clamp_thinking_level(&model, level),
                None => ThinkingLevel::Off,
            }
        };

        let previous_level = self.inner.agent.state().thinking_level;
        let is_changing = effective != previous_level;
        self.inner.agent.set_thinking_level(effective);
        self.sync_session_env();

        if is_changing {
            let level_str = thinking_level_str(effective);
            let result = lock(&self.inner.session_manager).append_thinking_level_change(level_str);
            if let Err(error) = result {
                tracing::warn!("session append failed: {error}");
            }
            if self.supports_thinking() || effective != ThinkingLevel::Off {
                lock(&self.inner.resource_loader)
                    .settings_manager_mut()
                    .set_default_thinking_level(effective);
            }
            self.emit(AgentSessionEvent::Session(
                SessionEvent::ThinkingLevelChanged { level: effective },
            ));
            // `void this._extensionRunner.emit(...)` (agent-session.ts:1693).
            let runner = self.runner();
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    runner.emit("thinking_level_select").await;
                });
            }
        }
    }

    /// `cycleThinkingLevel` (agent-session.ts:1705-1715).
    pub fn cycle_thinking_level(&self) -> Option<ThinkingLevel> {
        if !self.supports_thinking() {
            return None;
        }
        let levels = self.get_available_thinking_levels();
        // `indexOf` miss yields -1 upstream, so (-1 + 1) % len = 0 — the
        // first level, not the second (agent-session.ts:1709-1710).
        let next_level = match levels.iter().position(|l| *l == self.thinking_level()) {
            Some(current_index) => levels[(current_index + 1) % levels.len()],
            None => levels[0],
        };
        self.set_thinking_level(next_level);
        Some(next_level)
    }

    /// `getAvailableThinkingLevels` (agent-session.ts:1721-1724).
    pub fn get_available_thinking_levels(&self) -> Vec<ThinkingLevel> {
        match self.model() {
            None => THINKING_LEVELS.to_vec(),
            Some(model) => get_supported_thinking_levels(&model),
        }
    }

    /// `supportsThinking` (agent-session.ts:1729-1731).
    pub fn supports_thinking(&self) -> bool {
        self.model().map(|model| model.reasoning).unwrap_or(false)
    }

    /// `_getThinkingLevelForModelSwitch` (agent-session.ts:1733-1741).
    fn thinking_level_for_model_switch(
        &self,
        explicit_level: Option<ThinkingLevel>,
    ) -> ThinkingLevel {
        if let Some(level) = explicit_level {
            return level;
        }
        if !self.supports_thinking() {
            return lock(&self.inner.resource_loader)
                .settings_manager_mut()
                .get_default_thinking_level()
                .unwrap_or(crate::core::model_resolver::DEFAULT_THINKING_LEVEL);
        }
        self.thinking_level()
    }

    // ==================================================================
    // Queue mode management
    // ==================================================================

    /// `setSteeringMode` (agent-session.ts:1760-1763).
    pub fn set_steering_mode(&self, mode: QueueMode) {
        self.inner.agent.set_steering_mode(mode);
        lock(&self.inner.resource_loader)
            .settings_manager_mut()
            .set_steering_mode(mode);
    }

    /// `setFollowUpMode` (agent-session.ts:1769-1772).
    pub fn set_follow_up_mode(&self, mode: QueueMode) {
        self.inner.agent.set_follow_up_mode(mode);
        lock(&self.inner.resource_loader)
            .settings_manager_mut()
            .set_follow_up_mode(mode);
    }

    // ==================================================================
    // Compaction
    // ==================================================================

    /// `_disconnectFromAgent` (agent-session.ts:814-818): disconnect from
    /// agent events during disposal only (no longer used during compaction
    /// per e56893f4c — manual compaction already waits for the active run to
    /// settle, and summary generation does not emit Agent events).
    fn disconnect_from_agent(&self) {
        if let Some(unsubscribe) = lock(&self.inner.unsubscribe_agent).take() {
            unsubscribe();
        }
    }

    /// `compact` (agent-session.ts:1783-1925). Per e56893f4c the agent-event
    /// subscription is NOT disconnected during compaction: manual compaction
    /// waits for the active run to settle, and summary generation does not
    /// emit Agent events, so concurrent events should be preserved rather
    /// than dropped.
    pub async fn compact(
        &self,
        custom_instructions: Option<&str>,
    ) -> Result<CompactionResult, RpiError> {
        self.abort().await;
        self.sync_compaction_model();
        let result = {
            let mut runner = self.inner.compaction.lock().await;
            runner.compact(custom_instructions).await
        };
        result
    }

    /// `_checkCompaction` trigger (agent-session.ts:1953-2042).
    async fn check_compaction(
        &self,
        assistant_message: &AssistantMessage,
        skip_aborted_check: bool,
    ) -> bool {
        self.sync_compaction_model();
        let mut runner = self.inner.compaction.lock().await;
        runner
            .check_compaction(assistant_message, skip_aborted_check)
            .await
    }

    /// `abortCompaction` (agent-session.ts:1930-1933). Cancels through the
    /// shared token, so it also works while a compaction holds the runner
    /// mutex (upstream aborts an AbortController directly).
    pub fn abort_compaction(&self) {
        if let Some(token) = lock(&self.inner.compaction_abort).as_ref() {
            token.cancel();
        }
    }

    /// `setAutoCompactionEnabled` (agent-session.ts:2220-2222).
    pub fn set_auto_compaction_enabled(&self, enabled: bool) {
        lock(&self.inner.resource_loader)
            .settings_manager_mut()
            .set_compaction_enabled(enabled);
    }

    pub fn auto_compaction_enabled(&self) -> bool {
        lock(&self.inner.resource_loader)
            .settings_manager_mut()
            .get_compaction_enabled()
    }

    // ==================================================================
    // Auto-retry
    // ==================================================================

    /// `_isRetryableError` (agent-session.ts:2634-2638).
    fn is_retryable_error(&self, message: &AssistantMessage) -> bool {
        let context_window = self
            .model()
            .map(|model| u64::from(model.context_window))
            .unwrap_or(0);
        if is_context_overflow(message, Some(context_window)) {
            return false;
        }
        is_retryable_assistant_error(message)
    }

    /// `_prepareRetry` (agent-session.ts:2675-2725).
    async fn prepare_retry(&self, message: &AssistantMessage) -> bool {
        let settings = lock(&self.inner.resource_loader)
            .settings_manager_mut()
            .get_retry_settings();
        if !settings.enabled {
            return false;
        }

        let attempt = self.retry_attempt() + 1;
        if u64::from(attempt) > settings.max_retries {
            // Preserve the completed attempt count for the final failure
            // event (agent-session.ts:2683-2687).
            return false;
        }
        self.set_retry_attempt(attempt);

        let delay_ms = settings
            .base_delay_ms
            .saturating_mul(1u64 << (attempt - 1).min(31));

        self.emit(AgentSessionEvent::Session(SessionEvent::AutoRetryStart {
            attempt,
            max_attempts: settings.max_retries as u32,
            delay_ms,
            error_message: message
                .error_message
                .clone()
                .unwrap_or_else(|| "Unknown error".to_owned()),
        }));

        // Remove the error message from agent state (kept in the session).
        let mut messages = self.inner.agent.state().messages;
        if matches!(messages.last(), Some(AgentMessage::Assistant(_))) {
            messages.pop();
            self.inner.agent.set_messages(messages);
        }

        let token = CancellationToken::new();
        *lock(&self.inner.retry_abort) = Some(token.clone());
        let cancelled = tokio::select! {
            () = tokio::time::sleep(std::time::Duration::from_millis(delay_ms)) => false,
            () = token.cancelled() => true,
        };
        *lock(&self.inner.retry_abort) = None;

        if cancelled {
            let attempt = self.retry_attempt();
            self.set_retry_attempt(0);
            self.emit(AgentSessionEvent::Session(SessionEvent::AutoRetryEnd {
                success: false,
                attempt,
                final_error: Some("Retry cancelled".to_owned()),
            }));
            return false;
        }
        true
    }

    /// `abortRetry` (agent-session.ts:2730-2732).
    pub fn abort_retry(&self) {
        if let Some(token) = lock(&self.inner.retry_abort).take() {
            token.cancel();
        }
    }

    /// `get isRetrying` (agent-session.ts:2735-2737).
    pub fn is_retrying(&self) -> bool {
        lock(&self.inner.retry_abort).is_some()
    }

    pub fn auto_retry_enabled(&self) -> bool {
        lock(&self.inner.resource_loader)
            .settings_manager_mut()
            .get_retry_enabled()
    }

    /// `setAutoRetryEnabled` (agent-session.ts:2747-2749).
    pub fn set_auto_retry_enabled(&self, enabled: bool) {
        lock(&self.inner.resource_loader)
            .settings_manager_mut()
            .set_retry_enabled(enabled);
    }

    // ==================================================================
    // Bash execution
    // ==================================================================

    /// `executeBash` (agent-session.ts:2764-2796).
    pub async fn execute_bash(
        &self,
        command: &str,
        options: ExecuteBashOptions,
    ) -> Result<BashResult, RpiError> {
        let token = CancellationToken::new();
        let token_id = self.inner.next_listener_id.fetch_add(1, Ordering::SeqCst);
        lock(&self.inner.bash_tokens).push((token_id, token.clone()));

        // Apply command prefix if configured (agent-session.ts:2773-2775).
        let (prefix, shell_path) = {
            let mut loader = lock(&self.inner.resource_loader);
            let settings = loader.settings_manager_mut();
            (
                settings.get_shell_command_prefix(),
                settings.get_shell_path(),
            )
        };
        let resolved_command = match prefix {
            Some(prefix) => format!("{prefix}\n{command}"),
            None => command.to_owned(),
        };

        let session = self.clone();
        let id = options.id.clone();
        let on_chunk = options.on_chunk;
        let cwd = lock(&self.inner.session_manager).get_cwd().to_path_buf();
        let operations = create_local_bash_operations(shell_path);

        let result = execute_bash(
            &resolved_command,
            &cwd,
            operations.as_ref(),
            BashExecutorOptions {
                on_chunk: Some(Box::new(move |delta: &str| {
                    if let Some(on_chunk) = &on_chunk {
                        on_chunk(delta);
                    }
                    session.emit(AgentSessionEvent::Session(
                        SessionEvent::BashExecutionUpdate {
                            id: id.clone(),
                            delta: delta.to_owned(),
                        },
                    ));
                })),
                signal: token.clone(),
            },
        )
        .await;

        lock(&self.inner.bash_tokens).retain(|(id, _)| *id != token_id);

        match result {
            Ok(result) => {
                self.record_bash_result(command, &result, options.exclude_from_context);
                Ok(result)
            }
            Err(error) => Err(RpiError::Session(error.to_string())),
        }
    }

    /// `recordBashResult` (agent-session.ts:2802-2826).
    pub fn record_bash_result(
        &self,
        command: &str,
        result: &BashResult,
        exclude_from_context: bool,
    ) {
        let message = BashExecutionMessage {
            role: rpi_agent::messages::BashExecutionRole::BashExecution,
            command: command.to_owned(),
            output: result.output.clone(),
            exit_code: result.exit_code,
            cancelled: result.cancelled,
            truncated: result.truncated,
            full_output_path: result
                .full_output_path
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
            timestamp: now_millis(),
            exclude_from_context: if exclude_from_context {
                Some(true)
            } else {
                None
            },
        };

        if self.is_streaming() {
            lock(&self.inner.pending_bash_messages).push(message);
        } else {
            let mut messages = self.inner.agent.state().messages;
            messages.push(AgentMessage::BashExecution(message.clone()));
            self.inner.agent.set_messages(messages);
            let result = lock(&self.inner.session_manager)
                .append_message(AgentMessage::BashExecution(message));
            if let Err(error) = result {
                tracing::warn!("session append failed: {error}");
            }
        }
    }

    /// `abortBash` (agent-session.ts:2831-2835).
    pub fn abort_bash(&self) {
        for (_, token) in lock(&self.inner.bash_tokens).iter() {
            token.cancel();
        }
    }

    /// `get isBashRunning` (agent-session.ts:2838-2840).
    pub fn is_bash_running(&self) -> bool {
        !lock(&self.inner.bash_tokens).is_empty()
    }

    pub fn has_pending_bash_messages(&self) -> bool {
        !lock(&self.inner.pending_bash_messages).is_empty()
    }

    /// `_flushPendingBashMessages` (agent-session.ts:2851-2863).
    fn flush_pending_bash_messages(&self) {
        let pending: Vec<BashExecutionMessage> =
            std::mem::take(&mut *lock(&self.inner.pending_bash_messages));
        if pending.is_empty() {
            return;
        }
        for message in pending {
            let mut messages = self.inner.agent.state().messages;
            messages.push(AgentMessage::BashExecution(message.clone()));
            self.inner.agent.set_messages(messages);
            let result = lock(&self.inner.session_manager)
                .append_message(AgentMessage::BashExecution(message));
            if let Err(error) = result {
                tracing::warn!("session append failed: {error}");
            }
        }
    }

    // ==================================================================
    // Session management
    // ==================================================================

    /// `setSessionName` (agent-session.ts:2872-2877).
    pub fn set_session_name(&self, name: &str) {
        let result = lock(&self.inner.session_manager).append_session_info(name);
        if let Err(error) = result {
            tracing::warn!("session append failed: {error}");
        }
        let name = lock(&self.inner.session_manager).get_session_name();
        self.emit(AgentSessionEvent::Session(
            SessionEvent::SessionInfoChanged { name },
        ));
        // `void this._extensionRunner.emit(event)` (agent-session.ts:2876).
        let runner = self.runner();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                runner.emit("session_info_changed").await;
            });
        }
    }

    // ==================================================================
    // Tree navigation
    // ==================================================================

    /// `navigateTree` (agent-session.ts:2894-3080), T10 subset (extension
    /// hooks via the no-op seam).
    pub async fn navigate_tree(
        &self,
        target_id: &str,
        options: NavigateTreeOptions,
    ) -> Result<NavigateTreeResult, RpiError> {
        let old_leaf_id = lock(&self.inner.session_manager)
            .get_leaf_id()
            .map(str::to_owned);

        if Some(target_id) == old_leaf_id.as_deref() {
            return Ok(NavigateTreeResult::default());
        }

        if options.summarize && self.model().is_none() {
            return Err(RpiError::Session(
                "No model available for summarization".to_owned(),
            ));
        }

        let target_entry = {
            let session = lock(&self.inner.session_manager);
            session.get_entry(target_id)
        };
        let Some(target_entry) = target_entry else {
            return Err(RpiError::Session(format!("Entry {target_id} not found")));
        };

        // Collect entries to summarize (old leaf → common ancestor).
        let (old_path, target_path) = {
            let session = lock(&self.inner.session_manager);
            let typed = |entries: Vec<StoredEntry>| {
                entries
                    .iter()
                    .filter_map(|entry| entry.known().cloned())
                    .collect::<Vec<SessionEntry>>()
            };
            (
                typed(session.get_branch(None)),
                typed(session.get_branch(Some(target_id))),
            )
        };
        let CollectEntriesResult {
            entries: entries_to_summarize,
            ..
        } = collect_entries_for_branch_summary(&old_path, old_leaf_id.as_deref(), &target_path);

        let mut custom_instructions = options.custom_instructions.clone();
        let mut replace_instructions = options.replace_instructions;
        let mut label = options.label.clone();

        let token = CancellationToken::new();

        // session_before_tree extension event (no-op seam).
        let mut extension_summary = None;
        let mut from_extension = false;
        if self.runner().has_handlers("session_before_tree") {
            if let Some(result) = self.runner().emit_session_before_tree().await {
                if result.cancel == Some(true) {
                    return Ok(NavigateTreeResult {
                        cancelled: true,
                        ..Default::default()
                    });
                }
                if let Some(summary) = result.summary.filter(|_| options.summarize) {
                    extension_summary = Some(summary);
                    from_extension = true;
                }
                if result.custom_instructions.is_some() {
                    custom_instructions = result.custom_instructions;
                }
                if let Some(replace) = result.replace_instructions {
                    replace_instructions = replace;
                }
                if result.label.is_some() {
                    label = result.label;
                }
            }
        }

        // Run the default summarizer if needed (agent-session.ts:2975-3011).
        let mut summary_text: Option<String> = None;
        let mut summary_details: Option<serde_json::Value> = None;
        let mut summary_usage: Option<Usage> = None;
        if options.summarize && !entries_to_summarize.is_empty() && extension_summary.is_none() {
            let model = self.model().ok_or_else(|| {
                RpiError::Session("No model available for summarization".to_owned())
            })?;
            let reserve_tokens = lock(&self.inner.resource_loader)
                .settings_manager_mut()
                .get_branch_summary_settings()
                .reserve_tokens;
            let session = self.clone();
            let callbacks = summarization_retry_callbacks(
                move |event| session.emit(AgentSessionEvent::Compaction(Box::new(event))),
                RetrySourceRef::BranchSummary,
            );
            let result = generate_branch_summary(
                &entries_to_summarize,
                &GenerateBranchSummaryOptions {
                    model: &model,
                    stream_fn: &self.inner.agent.stream_function,
                    args: &SummarizationArgs {
                        signal: Some(token.clone()),
                        thinking_level: Some(self.thinking_level()),
                        retry: {
                            let mut loader = lock(&self.inner.resource_loader);
                            retry_config_to_policy(
                                loader.settings_manager_mut().get_retry_settings(),
                            )
                        },
                        ..Default::default()
                    },
                    custom_instructions: custom_instructions.as_deref(),
                    replace_instructions,
                    reserve_tokens,
                    callbacks: Some(&callbacks),
                },
            )
            .await;
            if result.aborted == Some(true) {
                return Ok(NavigateTreeResult {
                    cancelled: true,
                    aborted: true,
                    ..Default::default()
                });
            }
            if let Some(error) = result.error {
                return Err(RpiError::Session(error));
            }
            summary_text = result.summary;
            summary_usage = result.usage;
            summary_details = Some(serde_json::json!({
                "readFiles": result.read_files,
                "modifiedFiles": result.modified_files,
            }));
        } else if let Some(extension_summary) = extension_summary {
            summary_text = Some(extension_summary.summary);
            summary_details = extension_summary.details;
            summary_usage = extension_summary.usage;
        }

        // Determine the new leaf position (agent-session.ts:3013-3028).
        let (new_leaf_id, editor_text) = match target_entry.known() {
            Some(SessionEntry::Message(message_entry))
                if matches!(message_entry.message, AgentMessage::User(_)) =>
            {
                let text = match &message_entry.message {
                    AgentMessage::User(user) => content_text_user(&user.content, ""),
                    _ => String::new(),
                };
                (message_entry.parent_id.clone(), Some(text))
            }
            Some(SessionEntry::CustomMessage(custom)) => (
                custom.parent_id.clone(),
                Some(content_text_user(&custom.content, "")),
            ),
            _ => (Some(target_id.to_owned()), None),
        };

        // Switch leaf (with or without summary).
        let mut summary_entry_id: Option<String> = None;
        {
            let mut session = lock(&self.inner.session_manager);
            if let Some(summary) = &summary_text {
                let id = session.branch_with_summary(
                    new_leaf_id.as_deref(),
                    summary,
                    summary_details.clone(),
                    Some(from_extension),
                    summary_usage.clone(),
                )?;
                summary_entry_id = Some(id.clone());
                if let Some(label) = &label {
                    session.append_label_change(&id, Some(label))?;
                }
            } else if new_leaf_id.is_none() {
                session.reset_leaf();
            } else if let Some(new_leaf) = &new_leaf_id {
                session.branch(new_leaf)?;
            }
            if label.is_some() && summary_text.is_none() {
                session.append_label_change(target_id, label.as_deref())?;
            }
        }

        // Update agent state.
        let session_context = lock(&self.inner.session_manager).build_session_context();
        self.inner.agent.set_messages(session_context.messages);

        self.runner().emit("session_tree").await;

        Ok(NavigateTreeResult {
            editor_text,
            cancelled: false,
            aborted: false,
            summary_entry_id,
        })
    }

    /// `getUserMessagesForForking` (agent-session.ts:3085-3100).
    pub fn get_user_messages_for_forking(&self) -> Vec<(String, String)> {
        let session = lock(&self.inner.session_manager);
        let mut result = Vec::new();
        for entry in session.get_entries() {
            if let Some(SessionEntry::Message(message_entry)) = entry.known() {
                if let AgentMessage::User(user) = &message_entry.message {
                    let text = content_text_user(&user.content, "");
                    if !text.is_empty() {
                        result.push((message_entry.id.clone(), text));
                    }
                }
            }
        }
        result
    }

    // ==================================================================
    // Statistics
    // ==================================================================

    /// `getSessionStats` (agent-session.ts:3107-3157).
    pub fn get_session_stats(&self) -> SessionStats {
        let mut user_messages = 0u64;
        let mut assistant_messages = 0u64;
        let mut tool_results = 0u64;
        let mut total_messages = 0u64;
        let mut tool_calls = 0u64;
        let mut totals: UsageTotals = create_usage_totals();

        for entry in lock(&self.inner.session_manager).get_entries() {
            match entry.known() {
                Some(SessionEntry::BranchSummary(branch_summary)) => {
                    if let Some(usage) = &branch_summary.usage {
                        add_usage_to_totals(&mut totals, usage);
                    }
                }
                Some(SessionEntry::Compaction(compaction)) => {
                    if let Some(usage) = &compaction.usage {
                        add_usage_to_totals(&mut totals, usage);
                    }
                }
                Some(SessionEntry::Message(message_entry)) => {
                    total_messages += 1;
                    match &message_entry.message {
                        AgentMessage::User(_) => user_messages += 1,
                        AgentMessage::ToolResult(tool_result) => {
                            tool_results += 1;
                            if let Some(usage) = &tool_result.usage {
                                add_usage_to_totals(&mut totals, usage);
                            }
                        }
                        AgentMessage::Assistant(assistant) => {
                            assistant_messages += 1;
                            tool_calls += assistant
                                .content
                                .iter()
                                .filter(|c| {
                                    matches!(c, rpi_ai::types::AssistantContent::ToolCall(_))
                                })
                                .count() as u64;
                            add_usage_to_totals(&mut totals, &assistant.usage);
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }

        SessionStats {
            session_file: self
                .session_file()
                .map(|p| p.to_string_lossy().into_owned()),
            session_id: self.session_id(),
            user_messages,
            assistant_messages,
            tool_calls,
            tool_results,
            total_messages,
            tokens: SessionTokenStats {
                input: totals.input,
                output: totals.output,
                cache_read: totals.cache_read,
                cache_write: totals.cache_write,
                total: totals.input + totals.output + totals.cache_read + totals.cache_write,
            },
            cost: totals.cost,
            context_usage: self.get_context_usage(),
        }
    }

    /// `getContextUsage` (agent-session.ts:3159-3203).
    pub fn get_context_usage(&self) -> Option<ContextUsage> {
        let model = self.model()?;
        let context_window = u64::from(model.context_window);
        if context_window == 0 {
            return None;
        }

        // Only trust usage from an assistant that responded after the latest
        // compaction (agent-session.ts:3169-3193).
        let branch_entries = lock(&self.inner.session_manager).get_branch(None);
        let typed: Vec<SessionEntry> = branch_entries
            .iter()
            .filter_map(|entry| entry.known().cloned())
            .collect();
        let latest_compaction = rpi_agent::session::get_latest_compaction_entry(&typed);
        if let Some(compaction) = latest_compaction {
            let compaction_index = typed.iter().rposition(
                |entry| matches!(entry, SessionEntry::Compaction(c) if c.id == compaction.id),
            );
            let mut has_post_compaction_usage = false;
            if let Some(index) = compaction_index {
                for entry in typed.iter().skip(index + 1).rev() {
                    if let SessionEntry::Message(message_entry) = entry {
                        if let AgentMessage::Assistant(assistant) = &message_entry.message {
                            if assistant.stop_reason != StopReason::Aborted
                                && assistant.stop_reason != StopReason::Error
                            {
                                let context_tokens =
                                    rpi_ai::utils::estimate::calculate_context_tokens(
                                        &assistant.usage,
                                    );
                                if context_tokens > 0 {
                                    has_post_compaction_usage = true;
                                    break;
                                }
                            }
                        }
                    }
                }
            }
            if !has_post_compaction_usage {
                return Some(ContextUsage {
                    tokens: None,
                    context_window,
                    percent: None,
                });
            }
        }

        let estimate = estimate_context_tokens(&self.messages());
        let percent = (estimate.tokens as f64 / context_window as f64) * 100.0;
        Some(ContextUsage {
            tokens: Some(estimate.tokens),
            context_window,
            percent: Some(percent),
        })
    }

    // ==================================================================
    // Export
    // ==================================================================

    /// `exportToHtml` (agent-session.ts:3210-3231): theme from settings when
    /// it names a loadable theme, state slices from the agent. Synchronous —
    /// the upstream `async` only awaits the (pure CPU + file IO) export.
    ///
    /// Upstream also builds a `ToolHtmlRenderer` for extension custom tools
    /// (agent-session.ts:3217-3222); rpi has no JS tool renderers, so
    /// `renderedTools` is never emitted (export_html.rs module header).
    pub fn export_to_html(&self, output_path: Option<&str>) -> Result<String, RpiError> {
        let configured_theme_name = self.settings_manager(|settings| settings.get_theme());
        let theme_name = configured_theme_name
            .filter(|name| crate::core::themes::get_theme_by_name(name).is_some());

        let state = self.inner.agent.state();
        let tools = state
            .tools
            .iter()
            .map(|tool| crate::core::export_html::ExportToolInfo {
                name: tool.name().to_string(),
                description: tool.description().to_string(),
                parameters: tool.parameters().clone(),
            })
            .collect();

        let session_manager = lock(&self.inner.session_manager);
        crate::core::export_html::export_session_to_html(
            &session_manager,
            Some(state.system_prompt.clone()),
            Some(tools),
            &crate::core::export_html::ExportOptions {
                output_path: output_path.map(str::to_owned),
                theme_name,
            },
        )
    }

    /// `exportToJsonl` (agent-session.ts:3234-3265).
    pub fn export_to_jsonl(&self, output_path: Option<&str>) -> Result<PathBuf, RpiError> {
        let file_path = match output_path {
            Some(path) => crate::tools::path_utils::resolve_path(
                path,
                &std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
            ),
            None => {
                let iso = now_iso8601_compact();
                std::env::current_dir()
                    .unwrap_or_else(|_| PathBuf::from("/"))
                    .join(format!("session-{iso}.jsonl"))
            }
        };
        if let Some(dir) = file_path.parent() {
            if !dir.exists() {
                std::fs::create_dir_all(dir)?;
            }
        }

        let session = lock(&self.inner.session_manager);
        let header = rpi_agent::session::FileEntry::Session(rpi_agent::session::SessionHeader {
            version: Some(rpi_agent::session::CURRENT_SESSION_VERSION),
            id: session.get_session_id().to_owned(),
            timestamp: crate::core::session_manager::now_iso8601(),
            cwd: session.get_cwd().to_string_lossy().into_owned(),
            parent_session: None,
        });

        let mut lines = vec![serde_json::to_string(&header)?];
        // Re-chain parentIds to form a linear sequence.
        let mut prev_id: Option<String> = None;
        for entry in session.get_branch(None) {
            let mut value = entry.raw_value().clone();
            if let Some(object) = value.as_object_mut() {
                match &prev_id {
                    Some(prev) => {
                        object.insert("parentId".to_owned(), serde_json::Value::from(prev.clone()));
                    }
                    None => {
                        object.insert("parentId".to_owned(), serde_json::Value::Null);
                    }
                }
            }
            lines.push(serde_json::to_string(&value)?);
            prev_id = Some(entry.id().to_owned());
        }

        std::fs::write(&file_path, format!("{}\n", lines.join("\n")))?;
        Ok(file_path)
    }

    // ==================================================================
    // Utilities
    // ==================================================================

    /// `getLastAssistantText` (agent-session.ts:3276-3298).
    pub fn get_last_assistant_text(&self) -> Option<String> {
        let last_assistant = self.messages().into_iter().rev().find(|message| {
            match message {
                AgentMessage::Assistant(assistant) => {
                    // Skip aborted messages with no content.
                    !(assistant.stop_reason == StopReason::Aborted && assistant.content.is_empty())
                }
                _ => false,
            }
        });
        let AgentMessage::Assistant(assistant) = last_assistant? else {
            return None;
        };
        let mut text = String::new();
        for content in &assistant.content {
            if let rpi_ai::types::AssistantContent::Text(text_content) = content {
                text.push_str(&text_content.text);
            }
        }
        let trimmed = text.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_owned())
        }
    }

    // ==================================================================
    // Extension system
    // ==================================================================

    /// `bindExtensions` (agent-session.ts:2229-2252), T10 subset.
    pub async fn bind_extensions(&self, bindings: ExtensionBindings) {
        if let Some(mode) = bindings.mode {
            *lock(&self.inner.extension_mode) = mode;
        }
        if let Some(on_error) = bindings.on_error {
            *lock(&self.inner.extension_error_listener) = Some(on_error);
        }
        if let Some(shutdown) = bindings.shutdown {
            *lock(&self.inner.extension_shutdown_handler) = Some(shutdown);
        }
        // `_applyExtensionBindings` (agent-session.ts:2307-2314): the previous
        // subscription is dropped before re-subscribing — never leaked.
        if let Some(unsubscribe) = lock(&self.inner.extension_error_unsubscriber).take() {
            unsubscribe();
        }
        let listener = lock(&self.inner.extension_error_listener).clone();
        *lock(&self.inner.extension_error_unsubscriber) =
            listener.and_then(|listener| self.runner().on_error(listener));
        let event = &self.inner.session_start_event;
        self.runner()
            .emit_event(
                "session_start",
                serde_json::json!({
                    "type": "session_start",
                    "reason": event.reason.as_str(),
                    "previousSessionFile": event.previous_session_file,
                }),
            )
            .await;

        // `extendResourcesFromExtensions` (agent-session.ts:2251).
        let discover_reason = match self.inner.session_start_event.reason {
            SessionStartReason::Reload => "reload",
            _ => "startup",
        };
        self.extend_resources_from_extensions(discover_reason).await;
    }

    /// `extendResourcesFromExtensions` (agent-session.ts:2254-2277):
    /// extension-provided resource paths extend the loader, then the base
    /// system prompt is rebuilt over the extended resources.
    async fn extend_resources_from_extensions(&self, reason: &str) {
        if !self.runner().has_handlers("resources_discover") {
            return;
        }
        let paths = self
            .runner()
            .emit_resources_discover(&self.inner.cwd, reason)
            .await;
        if paths.skill_paths.is_empty()
            && paths.prompt_paths.is_empty()
            && paths.theme_paths.is_empty()
        {
            return;
        }
        lock(&self.inner.resource_loader).extend_resources(&paths);
        let tool_names = self.get_active_tool_names();
        let base = self.rebuild_system_prompt(&tool_names);
        *lock(&self.inner.base_system_prompt) = base.clone();
        self.inner.agent.set_system_prompt(base);
    }

    /// Invoke the mode-provided extension shutdown handler (T15 W5; no-op
    /// when the mode bound none, e.g. print).
    pub(crate) fn extension_shutdown(&self) {
        if let Some(handler) = lock(&self.inner.extension_shutdown_handler).clone() {
            handler();
        }
    }

    /// `_baseSystemPromptOptions` as the extension-facing JSON
    /// (`getSystemPromptOptions`, types.ts:349). Subset: the
    /// serializable fields extensions inspect.
    pub(crate) fn system_prompt_options_json(&self) -> serde_json::Value {
        let options = lock(&self.inner.base_system_prompt_options);
        let mut json = serde_json::json!({
            "promptGuidelines": options.prompt_guidelines,
            "cwd": options.cwd.to_string_lossy(),
        });
        if let Some(map) = json.as_object_mut() {
            if let Some(custom) = &options.custom_prompt {
                map.insert("customPrompt".to_owned(), custom.clone().into());
            }
            if let Some(tools) = &options.selected_tools {
                map.insert("selectedTools".to_owned(), tools.clone().into());
            }
            if let Some(append) = &options.append_system_prompt {
                map.insert("appendSystemPrompt".to_owned(), append.clone().into());
            }
        }
        json
    }

    /// `reload` (agent-session.ts:2600-2628): session_shutdown(reload) →
    /// settings/resources reload → host reload (factories re-run, factory
    /// cache generation bumped, flag values preserved) → actions re-bind →
    /// tool registry rebuild → session_start(reload) +
    /// resources_discover(reload).
    pub async fn reload(&self) {
        let runner = self.runner();
        if runner.has_handlers("session_shutdown") {
            runner
                .emit_event(
                    "session_shutdown",
                    serde_json::json!({"type": "session_shutdown", "reason": "reload"}),
                )
                .await;
        }
        {
            let mut loader = lock(&self.inner.resource_loader);
            loader.settings_manager_mut().reload();
            loader.reload();
        }
        if let Some(host) = crate::core::extension_host_adapter::host_of_runner(&runner) {
            let errors = host.reload().await;
            for error in errors {
                tracing::warn!("extension reload: {}: {}", error.path, error.error);
            }
            // The fresh runtime is unbound — re-bind the session actions
            // (flushes provider registrations from the re-run factories).
            crate::core::extension_actions::bind_session_actions(&host, self).await;
        }
        // `_buildRuntime({activeToolNames, includeAllExtensionTools: true})`
        // (agent-session.ts:2610-2615).
        self.refresh_tool_registry(RefreshToolRegistryOptions {
            active_tool_names: Some(self.get_active_tool_names()),
            include_all_extension_tools: true,
        });
        // session_start + extendResources with reason "reload"
        // (agent-session.ts:2623-2626).
        runner
            .emit_event(
                "session_start",
                serde_json::json!({"type": "session_start", "reason": "reload"}),
            )
            .await;
        self.extend_resources_from_extensions("reload").await;
    }

    /// `hasExtensionHandlers` (agent-session.ts:3317-3319).
    pub fn has_extension_handlers(&self, event_type: &str) -> bool {
        self.runner().has_handlers(event_type)
    }

    pub fn extension_runner(&self) -> Arc<dyn ExtensionRunner> {
        self.runner()
    }

    pub fn extension_mode(&self) -> ExtensionMode {
        *lock(&self.inner.extension_mode)
    }
}

impl Clone for AgentSession {
    fn clone(&self) -> Self {
        AgentSession {
            inner: self.inner.clone(),
        }
    }
}

/// `streamingBehavior`/deliverAs for custom messages
/// (agent-session.ts:1431).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CustomDeliverAs {
    Steer,
    FollowUp,
    NextTurn,
}

/// Streaming output callback for [`AgentSession::execute_bash`].
pub type BashChunkCallback = Box<dyn Fn(&str) + Send + Sync>;

/// `executeBash` options (agent-session.ts:2767).
#[derive(Default)]
pub struct ExecuteBashOptions {
    pub exclude_from_context: bool,
    pub id: Option<String>,
    pub on_chunk: Option<BashChunkCallback>,
}

/// `cycleModel` direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CycleDirection {
    Forward,
    Backward,
}

/// Retry-callback source marker for `summarization_retry_attempt_start`.
#[allow(dead_code)] // Compaction variant wired when manual compaction retry sources land.
enum RetrySourceRef {
    BranchSummary,
    Compaction(crate::core::compaction_runner::CompactionReason),
}

/// `_summarizationRetryCallbacks` (agent-session.ts:2646-2669).
fn summarization_retry_callbacks(
    emit: impl Fn(CompactionEvent) + Send + Sync + 'static,
    source: RetrySourceRef,
) -> rpi_ai::utils::retry::RetryCallbacks {
    use rpi_ai::utils::retry::RetryCallbacks;
    let emit = Arc::new(emit);
    let source = match source {
        RetrySourceRef::BranchSummary => crate::core::compaction_runner::RetrySource::BranchSummary,
        RetrySourceRef::Compaction(reason) => {
            crate::core::compaction_runner::RetrySource::Compaction { reason }
        }
    };
    let emit_scheduled = emit.clone();
    let on_retry_scheduled =
        move |(attempt, max_attempts, delay_ms, error_message): (u32, u32, u64, String)| {
            let emit = emit_scheduled.clone();
            Box::pin(async move {
                emit(CompactionEvent::SummarizationRetryScheduled {
                    attempt,
                    max_attempts,
                    delay_ms,
                    error_message,
                });
            }) as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        };
    let emit_start = emit.clone();
    let on_retry_attempt_start = move |(): ()| {
        let emit = emit_start.clone();
        let source = source;
        Box::pin(async move {
            emit(CompactionEvent::SummarizationRetryAttemptStart { source });
        }) as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
    };
    let emit_finished = emit;
    let on_retry_finished = move |_: (bool, u32, Option<String>)| {
        let emit = emit_finished.clone();
        Box::pin(async move {
            emit(CompactionEvent::SummarizationRetryFinished);
        }) as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
    };
    RetryCallbacks {
        on_retry_scheduled: Some(Box::new(on_retry_scheduled)),
        on_retry_attempt_start: Some(Box::new(on_retry_attempt_start)),
        on_retry_finished: Some(Box::new(on_retry_finished)),
    }
}

fn retry_config_to_policy(config: RetryConfig) -> Option<RetryPolicy> {
    Some(RetryPolicy {
        enabled: config.enabled,
        max_retries: config.max_retries as u32,
        base_delay_ms: config.base_delay_ms,
    })
}

fn thinking_level_str(level: ThinkingLevel) -> &'static str {
    match level {
        ThinkingLevel::Off => "off",
        ThinkingLevel::Minimal => "minimal",
        ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High => "high",
        ThinkingLevel::Xhigh => "xhigh",
        ThinkingLevel::Max => "max",
    }
}

fn user_message(text: &str, images: Option<Vec<ImageContent>>) -> AgentMessage {
    let mut content: Vec<UserContentBlock> = vec![UserContentBlock::Text(TextContent {
        text: text.to_owned(),
        text_signature: None,
    })];
    if let Some(images) = images {
        content.extend(images.into_iter().map(UserContentBlock::Image));
    }
    AgentMessage::User(UserMessage {
        role: UserRole::User,
        content: UserContent::Blocks(content),
        timestamp: now_millis(),
    })
}

fn agent_error_to_rpi(error: AgentError) -> RpiError {
    RpiError::Session(error.to_string())
}

fn now_iso8601_compact() -> String {
    crate::core::session_manager::now_iso8601().replace([':', '.'], "-")
}
