//! Port of `packages/agent/src/agent.ts` @ pi 0.82.1 (commit 2efa728), plus
//! the `AgentState` type of `packages/agent/src/types.ts`.
//!
//! Stateful wrapper around the low-level agent loop (`agent_loop.rs`). `Agent`
//! owns the current transcript, emits lifecycle events, and exposes queueing
//! APIs for steering and follow-up messages.
//!
//! Intentional differences from upstream (all structural, semantics kept):
//! - JS single-threaded mutable fields become `&self` methods over interior
//!   mutability (`Mutex`); no lock is ever held across an `.await`.
//! - `activeRun.promise` becomes a per-run "done" [`CancellationToken`];
//!   `wait_for_idle()` awaits its cancellation.
//! - `processEvents` looks up `activeRun` upstream to find the listener signal
//!   and throws when missing; that state is unreachable in both versions (the
//!   sink only exists inside a run), so the signal is passed explicitly.
//! - Thrown `Error`s become `Err(AgentError::Message)` whose `Display` is the
//!   verbatim upstream message text.
//! - `continue()` is named [`Agent::continue_run`] (`continue` is a Rust
//!   keyword).

use std::collections::HashSet;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use pir_ai::types::{
    ApiKind, AssistantContent, AssistantMessage, AssistantRole, ImageContent, Model, ModelCost,
    StopReason, StreamOptions, TextContent, ThinkingBudgets, Transport, Usage, UserContent,
    UserContentBlock, UserMessage, UserRole,
};
use tokio_util::sync::CancellationToken;

use crate::agent_loop::{
    now_millis, run_agent_loop, run_agent_loop_continue, thinking_level_from_model_level,
    AfterToolCallFn, AgentContext, AgentEventSink, AgentLoopConfig, AgentLoopTurnUpdate,
    BeforeToolCallFn, ConvertToLlmFn, GetApiKeyFn, GetQueuedMessagesFn, PrepareNextTurnContext,
    PrepareNextTurnFn, TransformContextFn,
};
use crate::error::AgentError;
use crate::messages::AgentMessage;
use crate::stream_fn::StreamFn;
use crate::types::{AgentEvent, AgentTool, QueueMode, ThinkingLevel, ToolExecutionMode};

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Agent event listener (upstream `Agent.subscribe` listener). Listeners are
/// awaited in subscription order as part of the run settlement; they receive
/// the active run's abort signal.
pub type AgentListener =
    Arc<dyn Fn(AgentEvent, CancellationToken) -> BoxFuture<'static, ()> + Send + Sync>;

/// `prepareNextTurn` (signal-only variant, agent.ts:191-193).
pub type PrepareNextTurnSignalFn =
    Arc<dyn Fn(CancellationToken) -> BoxFuture<'static, Option<AgentLoopTurnUpdate>> + Send + Sync>;

/// `prepareNextTurnWithContext` (agent.ts:194-197).
pub type PrepareNextTurnWithContextFn = Arc<
    dyn Fn(
            PrepareNextTurnContext,
            CancellationToken,
        ) -> BoxFuture<'static, Option<AgentLoopTurnUpdate>>
        + Send
        + Sync,
>;

// ---------------------------------------------------------------------------
// AgentState (types.ts:327-352)
// ---------------------------------------------------------------------------

/// `AgentState` — public agent state snapshot.
///
/// `tools` and `messages` use copy-on-assign semantics upstream; in Rust the
/// snapshot returned by [`Agent::state`] is already a copy, and the setters
/// ([`Agent::set_tools`]/[`Agent::set_messages`]) copy the provided arrays.
#[derive(Clone)]
pub struct AgentState {
    /// System prompt sent with each model request.
    pub system_prompt: String,
    /// Active model used for future turns.
    pub model: Model,
    /// Requested reasoning level for future turns.
    pub thinking_level: ThinkingLevel,
    /// Available tools.
    pub tools: Vec<Arc<dyn AgentTool>>,
    /// Conversation transcript.
    pub messages: Vec<AgentMessage>,
    /// True while the agent is processing a prompt or continuation. Remains
    /// true until awaited `agent_end` listeners settle.
    pub is_streaming: bool,
    /// Partial assistant message for the current streamed response, if any.
    pub streaming_message: Option<AgentMessage>,
    /// Tool call ids currently executing.
    pub pending_tool_calls: HashSet<String>,
    /// Error message from the most recent failed or aborted assistant turn.
    pub error_message: Option<String>,
}

type MutableAgentState = AgentState;

/// `DEFAULT_MODEL` (agent.ts:47-58).
fn default_model() -> Model {
    Model {
        id: "unknown".to_owned(),
        name: "unknown".to_owned(),
        api: ApiKind::from("unknown"),
        provider: "unknown".to_owned(),
        base_url: String::new(),
        reasoning: false,
        thinking_level_map: None,
        input: Vec::new(),
        cost: ModelCost::default(),
        context_window: 0,
        max_tokens: 0,
        headers: None,
        compat: None,
    }
}

/// `createMutableAgentState` (agent.ts:67-94) — initial-state input for
/// [`AgentOptions`]. Unset fields fall back to the upstream defaults
/// (`""` / `DEFAULT_MODEL` / `"off"` / `[]` / `[]`).
#[derive(Default)]
pub struct InitialAgentState {
    pub system_prompt: Option<String>,
    pub model: Option<Model>,
    pub thinking_level: Option<ThinkingLevel>,
    pub tools: Option<Vec<Arc<dyn AgentTool>>>,
    pub messages: Option<Vec<AgentMessage>>,
}

// ---------------------------------------------------------------------------
// AgentOptions (agent.ts:96-121)
// ---------------------------------------------------------------------------

/// Options for constructing an [`Agent`].
pub struct AgentOptions {
    pub initial_state: InitialAgentState,
    pub convert_to_llm: Option<ConvertToLlmFn>,
    pub transform_context: Option<TransformContextFn>,
    pub stream_fn: StreamFn,
    pub get_api_key: Option<GetApiKeyFn>,
    pub on_payload: Option<pir_ai::types::OnPayloadCallback>,
    pub on_response: Option<pir_ai::types::OnResponseCallback>,
    pub before_tool_call: Option<BeforeToolCallFn>,
    pub after_tool_call: Option<AfterToolCallFn>,
    pub prepare_next_turn: Option<PrepareNextTurnSignalFn>,
    pub prepare_next_turn_with_context: Option<PrepareNextTurnWithContextFn>,
    pub steering_mode: Option<QueueMode>,
    pub follow_up_mode: Option<QueueMode>,
    pub session_id: Option<String>,
    pub thinking_budgets: Option<ThinkingBudgets>,
    pub transport: Option<Transport>,
    pub max_retry_delay_ms: Option<u64>,
    pub tool_execution: Option<ToolExecutionMode>,
}

impl AgentOptions {
    /// `streamFn` is required upstream; there is no default provider inside
    /// this crate (coding-standards §4.2).
    pub fn new(stream_fn: StreamFn) -> Self {
        Self {
            initial_state: InitialAgentState::default(),
            convert_to_llm: None,
            transform_context: None,
            stream_fn,
            get_api_key: None,
            on_payload: None,
            on_response: None,
            before_tool_call: None,
            after_tool_call: None,
            prepare_next_turn: None,
            prepare_next_turn_with_context: None,
            steering_mode: None,
            follow_up_mode: None,
            session_id: None,
            thinking_budgets: None,
            transport: None,
            max_retry_delay_ms: None,
            tool_execution: None,
        }
    }
}

// ---------------------------------------------------------------------------
// PendingMessageQueue (agent.ts:123-157)
// ---------------------------------------------------------------------------

struct PendingMessageQueue {
    messages: Vec<AgentMessage>,
    mode: QueueMode,
}

impl PendingMessageQueue {
    fn new(mode: QueueMode) -> Self {
        Self {
            messages: Vec::new(),
            mode,
        }
    }

    fn enqueue(&mut self, message: AgentMessage) {
        self.messages.push(message);
    }

    fn has_items(&self) -> bool {
        !self.messages.is_empty()
    }

    fn drain(&mut self) -> Vec<AgentMessage> {
        if self.mode == QueueMode::All {
            return std::mem::take(&mut self.messages);
        }
        if self.messages.is_empty() {
            return Vec::new();
        }
        vec![self.messages.remove(0)]
    }

    fn clear(&mut self) {
        self.messages.clear();
    }
}

// ---------------------------------------------------------------------------
// ActiveRun (agent.ts:159-163)
// ---------------------------------------------------------------------------

/// Per-run handles. `signal` is the abort token (upstream
/// `abortController.signal`); cancelling `done` resolves `wait_for_idle()`
/// (upstream `activeRun.promise`).
#[derive(Clone)]
struct ActiveRun {
    signal: CancellationToken,
    done: CancellationToken,
}

// ---------------------------------------------------------------------------
// Prompt input (normalizePromptInput, agent.ts:379-396)
// ---------------------------------------------------------------------------

/// Input accepted by [`Agent::prompt`] (upstream
/// `string | AgentMessage | AgentMessage[]`).
pub enum PromptInput {
    Text(String),
    TextWithImages {
        text: String,
        images: Vec<ImageContent>,
    },
    /// Boxed to keep the enum small (`clippy::large_enum_variant`).
    Message(Box<AgentMessage>),
    Messages(Vec<AgentMessage>),
}

impl From<String> for PromptInput {
    fn from(text: String) -> Self {
        PromptInput::Text(text)
    }
}

impl From<&str> for PromptInput {
    fn from(text: &str) -> Self {
        PromptInput::Text(text.to_owned())
    }
}

impl From<AgentMessage> for PromptInput {
    fn from(message: AgentMessage) -> Self {
        PromptInput::Message(Box::new(message))
    }
}

impl From<Vec<AgentMessage>> for PromptInput {
    fn from(messages: Vec<AgentMessage>) -> Self {
        PromptInput::Messages(messages)
    }
}

impl PromptInput {
    fn into_messages(self) -> Vec<AgentMessage> {
        match self {
            PromptInput::Text(text) => Self::text_messages(text, Vec::new()),
            PromptInput::TextWithImages { text, images } => Self::text_messages(text, images),
            PromptInput::Message(message) => vec![*message],
            PromptInput::Messages(messages) => messages,
        }
    }

    fn text_messages(text: String, images: Vec<ImageContent>) -> Vec<AgentMessage> {
        let mut content: Vec<UserContentBlock> = vec![UserContentBlock::Text(TextContent {
            text,
            text_signature: None,
        })];
        content.extend(images.into_iter().map(UserContentBlock::Image));
        vec![AgentMessage::User(UserMessage {
            role: UserRole::User,
            content: UserContent::Blocks(content),
            timestamp: now_millis(),
        })]
    }
}

/// `defaultConvertToLlm` (agent.ts:32-36): keep only user/assistant/toolResult.
fn default_convert_to_llm() -> ConvertToLlmFn {
    Arc::new(|messages: Vec<AgentMessage>| {
        Box::pin(async move {
            messages
                .into_iter()
                .filter_map(|message| match message {
                    AgentMessage::User(u) => Some(pir_ai::types::Message::User(u)),
                    AgentMessage::Assistant(a) => Some(pir_ai::types::Message::Assistant(a)),
                    AgentMessage::ToolResult(t) => Some(pir_ai::types::Message::ToolResult(t)),
                    _ => None,
                })
                .collect()
        })
    })
}

// ---------------------------------------------------------------------------
// Agent (agent.ts:171-577)
// ---------------------------------------------------------------------------

/// Stateful wrapper around the low-level agent loop.
///
/// All methods take `&self`; wrap in `Arc` to share across tasks. Event
/// listeners are awaited in subscription order (barrier semantics,
/// requirements §4.3.16): the agent does not become idle until all awaited
/// listeners for `agent_end` have settled.
pub struct Agent {
    state: Arc<Mutex<MutableAgentState>>,
    listeners: Arc<Mutex<Vec<(u64, AgentListener)>>>,
    next_listener_id: AtomicU64,
    steering_queue: Arc<Mutex<PendingMessageQueue>>,
    follow_up_queue: Arc<Mutex<PendingMessageQueue>>,
    active_run: Arc<Mutex<Option<ActiveRun>>>,

    pub convert_to_llm: ConvertToLlmFn,
    pub transform_context: Option<TransformContextFn>,
    pub stream_function: StreamFn,
    pub get_api_key: Option<GetApiKeyFn>,
    pub on_payload: Option<pir_ai::types::OnPayloadCallback>,
    pub on_response: Option<pir_ai::types::OnResponseCallback>,
    pub before_tool_call: Option<BeforeToolCallFn>,
    pub after_tool_call: Option<AfterToolCallFn>,
    pub prepare_next_turn: Option<PrepareNextTurnSignalFn>,
    pub prepare_next_turn_with_context: Option<PrepareNextTurnWithContextFn>,
    /// Session identifier forwarded to providers for cache-aware backends.
    pub session_id: Option<String>,
    /// Optional per-level thinking token budgets forwarded to the stream
    /// function.
    pub thinking_budgets: Option<ThinkingBudgets>,
    /// Preferred transport forwarded to the stream function.
    pub transport: Transport,
    /// Optional cap for provider-requested retry delays.
    pub max_retry_delay_ms: Option<u64>,
    /// Tool execution strategy for assistant messages with multiple tool calls.
    pub tool_execution: ToolExecutionMode,
}

impl Agent {
    pub fn new(options: AgentOptions) -> Self {
        let initial = options.initial_state;
        Self {
            state: Arc::new(Mutex::new(AgentState {
                system_prompt: initial.system_prompt.unwrap_or_default(),
                model: initial.model.unwrap_or_else(default_model),
                thinking_level: initial.thinking_level.unwrap_or(ThinkingLevel::Off),
                tools: initial.tools.unwrap_or_default(),
                messages: initial.messages.unwrap_or_default(),
                is_streaming: false,
                streaming_message: None,
                pending_tool_calls: HashSet::new(),
                error_message: None,
            })),
            listeners: Arc::new(Mutex::new(Vec::new())),
            next_listener_id: AtomicU64::new(0),
            steering_queue: Arc::new(Mutex::new(PendingMessageQueue::new(
                options.steering_mode.unwrap_or(QueueMode::OneAtATime),
            ))),
            follow_up_queue: Arc::new(Mutex::new(PendingMessageQueue::new(
                options.follow_up_mode.unwrap_or(QueueMode::OneAtATime),
            ))),
            active_run: Arc::new(Mutex::new(None)),
            convert_to_llm: options
                .convert_to_llm
                .unwrap_or_else(default_convert_to_llm),
            transform_context: options.transform_context,
            stream_function: options.stream_fn,
            get_api_key: options.get_api_key,
            on_payload: options.on_payload,
            on_response: options.on_response,
            before_tool_call: options.before_tool_call,
            after_tool_call: options.after_tool_call,
            prepare_next_turn: options.prepare_next_turn,
            prepare_next_turn_with_context: options.prepare_next_turn_with_context,
            session_id: options.session_id,
            thinking_budgets: options.thinking_budgets,
            transport: options.transport.unwrap_or(Transport::Auto),
            max_retry_delay_ms: options.max_retry_delay_ms,
            tool_execution: options
                .tool_execution
                .unwrap_or(ToolExecutionMode::Parallel),
        }
    }

    /// Subscribe to agent lifecycle events (agent.ts:243-246).
    ///
    /// Listener futures are awaited in subscription order and are included in
    /// the current run's settlement. Returns an unsubscribe closure.
    pub fn subscribe(&self, listener: AgentListener) -> Box<dyn FnOnce() + Send> {
        let id = self.next_listener_id.fetch_add(1, Ordering::SeqCst);
        lock(&self.listeners).push((id, listener));
        let listeners = self.listeners.clone();
        Box::new(move || {
            lock(&listeners).retain(|(listener_id, _)| *listener_id != id);
        })
    }

    /// Current agent state (snapshot copy).
    pub fn state(&self) -> AgentState {
        lock(&self.state).clone()
    }

    pub fn set_system_prompt(&self, system_prompt: String) {
        lock(&self.state).system_prompt = system_prompt;
    }

    pub fn set_model(&self, model: Model) {
        lock(&self.state).model = model;
    }

    pub fn set_thinking_level(&self, thinking_level: ThinkingLevel) {
        lock(&self.state).thinking_level = thinking_level;
    }

    /// Assigning `tools` copies the provided top-level array (upstream setter).
    pub fn set_tools(&self, tools: Vec<Arc<dyn AgentTool>>) {
        lock(&self.state).tools = tools;
    }

    /// Assigning `messages` copies the provided top-level array (upstream
    /// setter).
    pub fn set_messages(&self, messages: Vec<AgentMessage>) {
        lock(&self.state).messages = messages;
    }

    /// Controls how queued steering messages are drained.
    pub fn steering_mode(&self) -> QueueMode {
        lock(&self.steering_queue).mode
    }

    pub fn set_steering_mode(&self, mode: QueueMode) {
        lock(&self.steering_queue).mode = mode;
    }

    /// Controls how queued follow-up messages are drained.
    pub fn follow_up_mode(&self) -> QueueMode {
        lock(&self.follow_up_queue).mode
    }

    pub fn set_follow_up_mode(&self, mode: QueueMode) {
        lock(&self.follow_up_queue).mode = mode;
    }

    /// Queue a message to be injected after the current assistant turn
    /// finishes.
    pub fn steer(&self, message: AgentMessage) {
        lock(&self.steering_queue).enqueue(message);
    }

    /// Queue a message to run only after the agent would otherwise stop.
    pub fn follow_up(&self, message: AgentMessage) {
        lock(&self.follow_up_queue).enqueue(message);
    }

    /// Remove all queued steering messages.
    pub fn clear_steering_queue(&self) {
        lock(&self.steering_queue).clear();
    }

    /// Remove all queued follow-up messages.
    pub fn clear_follow_up_queue(&self) {
        lock(&self.follow_up_queue).clear();
    }

    /// Remove all queued steering and follow-up messages.
    pub fn clear_all_queues(&self) {
        self.clear_steering_queue();
        self.clear_follow_up_queue();
    }

    /// Returns true when either queue still contains pending messages.
    pub fn has_queued_messages(&self) -> bool {
        lock(&self.steering_queue).has_items() || lock(&self.follow_up_queue).has_items()
    }

    /// Active abort signal for the current run, if any.
    pub fn signal(&self) -> Option<CancellationToken> {
        lock(&self.active_run)
            .as_ref()
            .map(|run| run.signal.clone())
    }

    /// Abort the current run, if one is active.
    pub fn abort(&self) {
        if let Some(run) = lock(&self.active_run).as_ref() {
            run.signal.cancel();
        }
    }

    /// Resolve when the current run and all awaited event listeners have
    /// finished (after `agent_end` listeners settle).
    pub async fn wait_for_idle(&self) {
        let done = lock(&self.active_run).as_ref().map(|run| run.done.clone());
        if let Some(done) = done {
            done.cancelled().await;
        }
    }

    /// `isStreaming` — true until awaited `agent_end` listeners settle.
    pub fn is_streaming(&self) -> bool {
        lock(&self.state).is_streaming
    }

    /// Clear transcript state, runtime state, and queued messages.
    pub fn reset(&self) {
        {
            let mut state = lock(&self.state);
            state.messages = Vec::new();
            state.is_streaming = false;
            state.streaming_message = None;
            state.pending_tool_calls = HashSet::new();
            state.error_message = None;
        }
        self.clear_follow_up_queue();
        self.clear_steering_queue();
    }

    /// Start a new prompt from text, a single message, or a batch of messages
    /// (agent.ts:337-347).
    pub async fn prompt(&self, input: impl Into<PromptInput>) -> Result<(), AgentError> {
        if lock(&self.active_run).is_some() {
            return Err(AgentError::Message(
                "Agent is already processing a prompt. Use steer() or followUp() to queue messages, or wait for completion."
                    .to_owned(),
            ));
        }
        let messages = input.into().into_messages();
        self.run_prompt_messages(messages, false).await
    }

    /// `prompt(input: string, images?: ImageContent[])`.
    pub async fn prompt_with_images(
        &self,
        text: &str,
        images: Vec<ImageContent>,
    ) -> Result<(), AgentError> {
        self.prompt(PromptInput::TextWithImages {
            text: text.to_owned(),
            images,
        })
        .await
    }

    /// Continue from the current transcript (agent.ts:349-377, upstream
    /// `continue()`). The last message must be a user or tool-result message;
    /// with an assistant tail the steering queue is drained first (skipping
    /// the initial steering poll), then the follow-up queue.
    pub async fn continue_run(&self) -> Result<(), AgentError> {
        if lock(&self.active_run).is_some() {
            return Err(AgentError::Message(
                "Agent is already processing. Wait for completion before continuing.".to_owned(),
            ));
        }

        let last_message = lock(&self.state).messages.last().cloned();
        let Some(last_message) = last_message else {
            return Err(AgentError::Message(
                "No messages to continue from".to_owned(),
            ));
        };

        if matches!(last_message, AgentMessage::Assistant(_)) {
            let queued_steering = lock(&self.steering_queue).drain();
            if !queued_steering.is_empty() {
                return self.run_prompt_messages(queued_steering, true).await;
            }

            let queued_follow_ups = lock(&self.follow_up_queue).drain();
            if !queued_follow_ups.is_empty() {
                return self.run_prompt_messages(queued_follow_ups, false).await;
            }

            return Err(AgentError::Message(
                "Cannot continue from message role: assistant".to_owned(),
            ));
        }

        self.run_continuation().await
    }

    /// `runPromptMessages` (agent.ts:398-412).
    async fn run_prompt_messages(
        &self,
        messages: Vec<AgentMessage>,
        skip_initial_steering_poll: bool,
    ) -> Result<(), AgentError> {
        let context = self.create_context_snapshot();
        let config = self.create_loop_config(skip_initial_steering_poll);
        let stream_function = self.stream_function.clone();
        self.run_with_lifecycle(move |signal, emit| async move {
            run_agent_loop(
                messages,
                context,
                config,
                emit,
                Some(signal),
                stream_function,
            )
            .await;
            Ok(())
        })
        .await
    }

    /// `runContinuation` (agent.ts:414-424). Loop-level validation failures
    /// flow into `handleRunFailure` via the executor error, as upstream.
    async fn run_continuation(&self) -> Result<(), AgentError> {
        let context = self.create_context_snapshot();
        let config = self.create_loop_config(false);
        let stream_function = self.stream_function.clone();
        self.run_with_lifecycle(move |signal, emit| async move {
            run_agent_loop_continue(context, config, emit, Some(signal), stream_function)
                .await
                .map(|_| ())
        })
        .await
    }

    /// `createContextSnapshot` (agent.ts:426-432).
    fn create_context_snapshot(&self) -> AgentContext {
        let state = lock(&self.state);
        AgentContext {
            system_prompt: state.system_prompt.clone(),
            messages: state.messages.clone(),
            tools: Some(state.tools.clone()),
        }
    }

    /// `createLoopConfig` (agent.ts:434-469).
    fn create_loop_config(&self, skip_initial_steering_poll: bool) -> AgentLoopConfig {
        let (model, reasoning) = {
            let state = lock(&self.state);
            (
                state.model.clone(),
                thinking_level_from_model_level(state.thinking_level),
            )
        };

        let skip = Arc::new(AtomicBool::new(skip_initial_steering_poll));
        let get_steering_messages: GetQueuedMessagesFn = {
            let skip = skip.clone();
            let queue = self.steering_queue.clone();
            Arc::new(move || {
                let skip = skip.clone();
                let queue = queue.clone();
                Box::pin(async move {
                    if skip.swap(false, Ordering::SeqCst) {
                        return Vec::new();
                    }
                    lock(&queue).drain()
                })
            })
        };
        let get_follow_up_messages: GetQueuedMessagesFn = {
            let queue = self.follow_up_queue.clone();
            Arc::new(move || {
                let queue = queue.clone();
                Box::pin(async move { lock(&queue).drain() })
            })
        };

        let prepare_next_turn: Option<PrepareNextTurnFn> =
            if self.prepare_next_turn_with_context.is_some() || self.prepare_next_turn.is_some() {
                let with_context = self.prepare_next_turn_with_context.clone();
                let signal_only = self.prepare_next_turn.clone();
                let active_run = self.active_run.clone();
                Some(Arc::new(move |context: PrepareNextTurnContext| {
                    let with_context = with_context.clone();
                    let signal_only = signal_only.clone();
                    let active_run = active_run.clone();
                    Box::pin(async move {
                        // Upstream reads `this.signal` at call time.
                        let signal = lock(&active_run)
                            .as_ref()
                            .map(|run| run.signal.clone())
                            .unwrap_or_default();
                        if let Some(prepare) = with_context {
                            return prepare(context, signal).await;
                        }
                        match signal_only {
                            Some(prepare) => prepare(signal).await,
                            None => None,
                        }
                    })
                }))
            } else {
                None
            };

        AgentLoopConfig {
            model,
            reasoning,
            thinking_budgets: self.thinking_budgets.clone(),
            stream_options: StreamOptions {
                session_id: self.session_id.clone(),
                on_payload: self.on_payload.clone(),
                on_response: self.on_response.clone(),
                transport: Some(self.transport),
                max_retry_delay_ms: self.max_retry_delay_ms,
                ..Default::default()
            },
            tool_execution: self.tool_execution,
            convert_to_llm: self.convert_to_llm.clone(),
            transform_context: self.transform_context.clone(),
            get_api_key: self.get_api_key.clone(),
            // The Agent layer exposes no shouldStopAfterTurn option upstream.
            should_stop_after_turn: None,
            prepare_next_turn,
            get_steering_messages: Some(get_steering_messages),
            get_follow_up_messages: Some(get_follow_up_messages),
            before_tool_call: self.before_tool_call.clone(),
            after_tool_call: self.after_tool_call.clone(),
        }
    }

    /// `runWithLifecycle` (agent.ts:471-494).
    async fn run_with_lifecycle<F, Fut>(&self, executor: F) -> Result<(), AgentError>
    where
        F: FnOnce(CancellationToken, AgentEventSink) -> Fut,
        Fut: Future<Output = Result<(), AgentError>>,
    {
        let run = ActiveRun {
            signal: CancellationToken::new(),
            done: CancellationToken::new(),
        };
        {
            let mut active_run = lock(&self.active_run);
            if active_run.is_some() {
                return Err(AgentError::Message(
                    "Agent is already processing.".to_owned(),
                ));
            }
            *active_run = Some(run.clone());
        }

        {
            let mut state = lock(&self.state);
            state.is_streaming = true;
            state.streaming_message = None;
            state.error_message = None;
        }

        let emit = self.make_sink(run.signal.clone());
        let result = executor(run.signal.clone(), emit).await;
        if let Err(error) = result {
            let aborted = run.signal.is_cancelled();
            self.handle_run_failure(&error, aborted, &run.signal).await;
        }
        self.finish_run();
        Ok(())
    }

    /// The barrier sink handed to the low-level loop
    /// (`(event) => this.processEvents(event)`).
    fn make_sink(&self, signal: CancellationToken) -> AgentEventSink {
        let state = self.state.clone();
        let listeners = self.listeners.clone();
        Arc::new(move |event| {
            let state = state.clone();
            let listeners = listeners.clone();
            let signal = signal.clone();
            Box::pin(async move {
                process_events(&state, &listeners, &event, &signal).await;
            })
        })
    }

    /// `handleRunFailure` (agent.ts:496-512): synthesize a failure assistant
    /// message (empty text content, stopReason aborted|error) and emit the
    /// full trailing event sequence through the barrier.
    async fn handle_run_failure(
        &self,
        error: &AgentError,
        aborted: bool,
        signal: &CancellationToken,
    ) {
        let model = lock(&self.state).model.clone();
        let failure_message = AssistantMessage {
            role: AssistantRole::Assistant,
            content: vec![AssistantContent::Text(TextContent {
                text: String::new(),
                text_signature: None,
            })],
            api: model.api.clone(),
            provider: model.provider.clone(),
            model: model.id.clone(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            usage: Usage::default(),
            stop_reason: if aborted {
                StopReason::Aborted
            } else {
                StopReason::Error
            },
            error_message: Some(error.to_string()),
            timestamp: now_millis(),
        };
        let message = AgentMessage::Assistant(failure_message);
        let state = self.state.clone();
        let listeners = self.listeners.clone();
        process_events(
            &state,
            &listeners,
            &AgentEvent::MessageStart {
                message: message.clone(),
            },
            signal,
        )
        .await;
        process_events(
            &state,
            &listeners,
            &AgentEvent::MessageEnd {
                message: message.clone(),
            },
            signal,
        )
        .await;
        process_events(
            &state,
            &listeners,
            &AgentEvent::TurnEnd {
                message: message.clone(),
                tool_results: Vec::new(),
            },
            signal,
        )
        .await;
        process_events(
            &state,
            &listeners,
            &AgentEvent::AgentEnd {
                messages: vec![message],
            },
            signal,
        )
        .await;
    }

    /// `finishRun` (agent.ts:514-520).
    fn finish_run(&self) {
        {
            let mut state = lock(&self.state);
            state.is_streaming = false;
            state.streaming_message = None;
            state.pending_tool_calls = HashSet::new();
        }
        let run = lock(&self.active_run).take();
        if let Some(run) = run {
            run.done.cancel();
        }
    }
}

/// `processEvents` (agent.ts:529-576): reduce internal state for a loop
/// event, then await all listeners in subscription order.
///
/// `agent_end` only means no further loop events will be emitted; the run is
/// idle later, after all awaited listeners for `agent_end` finish and
/// `finish_run` clears runtime-owned state.
async fn process_events(
    state: &Arc<Mutex<MutableAgentState>>,
    listeners: &Arc<Mutex<Vec<(u64, AgentListener)>>>,
    event: &AgentEvent,
    signal: &CancellationToken,
) {
    {
        let mut state = lock(state);
        match event {
            AgentEvent::MessageStart { message } | AgentEvent::MessageUpdate { message, .. } => {
                state.streaming_message = Some(message.clone());
            }
            AgentEvent::MessageEnd { message } => {
                state.streaming_message = None;
                state.messages.push(message.clone());
            }
            AgentEvent::ToolExecutionStart { tool_call_id, .. } => {
                state.pending_tool_calls.insert(tool_call_id.clone());
            }
            AgentEvent::ToolExecutionEnd { tool_call_id, .. } => {
                state.pending_tool_calls.remove(tool_call_id);
            }
            AgentEvent::TurnEnd { message, .. } => {
                if let AgentMessage::Assistant(assistant) = message {
                    if let Some(error_message) = &assistant.error_message {
                        state.error_message = Some(error_message.clone());
                    }
                }
            }
            AgentEvent::AgentEnd { .. } => {
                state.streaming_message = None;
            }
            AgentEvent::AgentStart
            | AgentEvent::TurnStart
            | AgentEvent::ToolExecutionUpdate { .. } => {}
        }
    }

    let snapshot: Vec<AgentListener> = lock(listeners)
        .iter()
        .map(|(_, listener)| listener.clone())
        .collect();
    for listener in snapshot {
        listener(event.clone(), signal.clone()).await;
    }
}
