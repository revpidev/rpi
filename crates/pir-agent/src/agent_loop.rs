//! Port of `packages/agent/src/agent-loop.ts` @ pi 0.82.1 (commit 2efa728),
//! plus the loop-facing hook types of `packages/agent/src/types.ts`.
//!
//! The low-level loop is *observational*: events are pushed into an
//! [`AgentEventStream`] (or an injected [`AgentEventSink`]) without waiting for
//! consumers. The listener barrier lives in the `Agent` layer (`agent.rs`).
//!
//! Intentional differences from upstream (all structural, semantics kept):
//! - `AbortSignal | undefined` becomes `Option<CancellationToken>`; hooks
//!   always receive a token (a never-cancelled one when the caller passed
//!   `None`).
//! - `AgentLoopConfig extends SimpleStreamOptions` flattens into explicit
//!   fields: `reasoning`/`thinking_budgets` plus a base `stream_options`
//!   (`StreamOptions`) that is cloned per LLM call with the resolved
//!   `api_key`/`signal` overwritten (upstream `{...config, apiKey, signal}`).
//!   `reasoning`/`thinking_budgets` ride the config for `prepareNextTurn`
//!   semantics but are not forwarded through the pinned `StreamFn` shape
//!   (design doc §4.4 pins `StreamOptions`); the assembly layer binds them.
//! - Parallel tool execution uses a `tokio::task::JoinSet` (true concurrency)
//!   where upstream relies on the JS event loop; `tool_execution_end` is still
//!   emitted in completion order and tool-result artifacts in source order.
//! - A stream that ends without a terminal `done`/`error` event violates the
//!   `StreamFn` contract; upstream would hang on `response.result()`. Here a
//!   synthetic `error` assistant message is produced instead (logged).

use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context as TaskContext, Poll};

use futures::future::{BoxFuture, Shared};
use futures::prelude::*;
use futures::Stream;
use pir_ai::types::{
    AssistantContent, AssistantMessage, AssistantRole, Context, Message, Model, ModelThinkingLevel,
    StopReason, StreamEvent, StreamOptions, TextContent, ThinkingBudgets, ThinkingLevel, Tool,
    ToolResultContent, ToolResultMessage, ToolResultRole, Usage,
};
use pir_ai::utils::validation::validate_tool_arguments;
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::error::AgentError;
use crate::messages::AgentMessage;
use crate::stream_fn::StreamFn;
use crate::types::{AgentEvent, AgentTool, AgentToolCall, AgentToolResult, ToolExecutionMode};

/// Current time as Unix milliseconds (upstream `Date.now()`).
pub(crate) fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Agent-side `ThinkingLevel` (includes `"off"`) → pi-ai `ThinkingLevel`
/// (`"off"` maps to `None`, matching upstream
/// `thinkingLevel === "off" ? undefined : thinkingLevel`).
pub(crate) fn thinking_level_from_model_level(level: ModelThinkingLevel) -> Option<ThinkingLevel> {
    ThinkingLevel::from_model_level(level)
}

// ---------------------------------------------------------------------------
// AgentContext (types.ts)
// ---------------------------------------------------------------------------

/// `AgentContext` — context snapshot passed into the low-level agent loop.
#[derive(Clone, Default)]
pub struct AgentContext {
    /// System prompt included with the request.
    pub system_prompt: String,
    /// Transcript visible to the model.
    pub messages: Vec<AgentMessage>,
    /// Tools available for this run.
    pub tools: Option<Vec<Arc<dyn AgentTool>>>,
}

// ---------------------------------------------------------------------------
// Hook types (types.ts)
// ---------------------------------------------------------------------------

/// `BeforeToolCallResult` — returning `block: true` prevents execution; the
/// loop emits an error tool result instead.
#[derive(Debug, Clone, Default)]
pub struct BeforeToolCallResult {
    pub block: Option<bool>,
    /// Text shown in the error result; a default blocked message is used when
    /// omitted.
    pub reason: Option<String>,
    /// Replaces the validated arguments passed to `execute` (upstream mutates
    /// the validated args object in place; Rust hooks receive it by value, so
    /// the mutation is returned instead). Applied **without revalidation**,
    /// matching upstream.
    pub args: Option<Value>,
}

/// `AfterToolCallResult` — partial override with field-by-field replacement
/// semantics (no deep merge). Omitted fields keep the executed result values.
#[derive(Debug, Clone, Default)]
pub struct AfterToolCallResult {
    /// Replaces the tool result content array in full.
    pub content: Option<Vec<ToolResultContent>>,
    /// Replaces the tool result details value in full.
    pub details: Option<Value>,
    /// Replaces the tool result error flag.
    pub is_error: Option<bool>,
    /// Usage from the final tool execution itself, if available. Not used for
    /// main LLM context accounting.
    pub usage: Option<Usage>,
    /// Hint that the agent should stop after the current tool batch.
    pub terminate: Option<bool>,
}

/// `BeforeToolCallContext`.
#[derive(Clone)]
pub struct BeforeToolCallContext {
    /// The assistant message that requested the tool call.
    pub assistant_message: AssistantMessage,
    /// The raw tool call block from `assistant_message.content`.
    pub tool_call: AgentToolCall,
    /// Validated tool arguments for the target tool schema.
    pub args: Value,
    /// Current agent context at the time the tool call is prepared.
    pub context: AgentContext,
}

/// `AfterToolCallContext`.
#[derive(Clone)]
pub struct AfterToolCallContext {
    /// The assistant message that requested the tool call.
    pub assistant_message: AssistantMessage,
    /// The raw tool call block from `assistant_message.content`.
    pub tool_call: AgentToolCall,
    /// Validated tool arguments for the target tool schema.
    pub args: Value,
    /// The executed tool result before any overrides are applied.
    pub result: AgentToolResult,
    /// Whether the executed tool result is currently treated as an error.
    pub is_error: bool,
    /// Current agent context at the time the tool call is finalized.
    pub context: AgentContext,
}

/// `ShouldStopAfterTurnContext`.
#[derive(Clone)]
pub struct ShouldStopAfterTurnContext {
    /// The assistant message that completed the turn.
    pub message: AssistantMessage,
    /// Tool result messages passed to the preceding `turn_end` event.
    pub tool_results: Vec<ToolResultMessage>,
    /// Current agent context after the turn's assistant message and tool
    /// results have been appended.
    pub context: AgentContext,
    /// Messages that this loop invocation will return if it exits at this
    /// point. Prompt runs include the initial prompt messages.
    pub new_messages: Vec<AgentMessage>,
}

/// `PrepareNextTurnContext extends ShouldStopAfterTurnContext` (identical
/// fields upstream).
pub type PrepareNextTurnContext = ShouldStopAfterTurnContext;

/// `AgentLoopTurnUpdate` — replacement runtime state applied before starting
/// another provider request.
#[derive(Clone, Default)]
pub struct AgentLoopTurnUpdate {
    /// Context for the next provider request.
    pub context: Option<AgentContext>,
    /// Model for the next provider request.
    pub model: Option<Model>,
    /// Thinking level for the next provider request (`"off"` clears
    /// `config.reasoning`).
    pub thinking_level: Option<ModelThinkingLevel>,
}

// ---------------------------------------------------------------------------
// Hook function aliases
// ---------------------------------------------------------------------------

/// `convertToLlm` — required; converts `AgentMessage[]` to LLM-compatible
/// `Message[]` before each LLM call. Contract: must not panic.
pub type ConvertToLlmFn =
    Arc<dyn Fn(Vec<AgentMessage>) -> BoxFuture<'static, Vec<Message>> + Send + Sync>;

/// `transformContext` — optional transform applied to the context messages
/// before `convertToLlm`. Contract: must not panic.
pub type TransformContextFn = Arc<
    dyn Fn(Vec<AgentMessage>, CancellationToken) -> BoxFuture<'static, Vec<AgentMessage>>
        + Send
        + Sync,
>;

/// `getApiKey` — resolves an API key dynamically for each LLM call (provider
/// id in, key out). Contract: must not panic; return `None` when no key is
/// available.
pub type GetApiKeyFn = Arc<dyn Fn(String) -> BoxFuture<'static, Option<String>> + Send + Sync>;

/// `shouldStopAfterTurn` — returning `true` makes the loop emit `agent_end`
/// and exit before polling steering/follow-up queues.
pub type ShouldStopAfterTurnFn =
    Arc<dyn Fn(ShouldStopAfterTurnContext) -> BoxFuture<'static, bool> + Send + Sync>;

/// `prepareNextTurn` — called after `turn_end`; return replacement
/// context/model/thinking state to affect the next turn in this run.
pub type PrepareNextTurnFn = Arc<
    dyn Fn(PrepareNextTurnContext) -> BoxFuture<'static, Option<AgentLoopTurnUpdate>> + Send + Sync,
>;

/// `getSteeringMessages` / `getFollowUpMessages` — queue drain hooks.
/// Contract: must not panic; return `vec![]` when empty.
pub type GetQueuedMessagesFn = Arc<dyn Fn() -> BoxFuture<'static, Vec<AgentMessage>> + Send + Sync>;

/// `beforeToolCall` — called after argument validation, before execution.
pub type BeforeToolCallFn = Arc<
    dyn Fn(
            BeforeToolCallContext,
            CancellationToken,
        ) -> BoxFuture<'static, Option<BeforeToolCallResult>>
        + Send
        + Sync,
>;

/// `afterToolCall` — called after execution, before `tool_execution_end` and
/// tool-result message events are emitted. Upstream wraps the hook in
/// try/catch and downgrades a throw to an error result (agent-loop.ts:743-746);
/// returning `Err` here gets the same treatment. Contract: must not panic.
pub type AfterToolCallFn = Arc<
    dyn Fn(
            AfterToolCallContext,
            CancellationToken,
        ) -> BoxFuture<'static, Result<Option<AfterToolCallResult>, AgentError>>
        + Send
        + Sync,
>;

/// `AgentEventSink` — shared event sink. `Fn` (not `FnMut`) so parallel tool
/// tasks can each hold a clone, mirroring the single upstream `emit` closure.
pub type AgentEventSink = Arc<dyn Fn(AgentEvent) -> BoxFuture<'static, ()> + Send + Sync>;

// ---------------------------------------------------------------------------
// AgentLoopConfig (types.ts)
// ---------------------------------------------------------------------------

/// `AgentLoopConfig` — configuration for one agent loop invocation.
pub struct AgentLoopConfig {
    /// Model used for provider requests.
    pub model: Model,
    /// `SimpleStreamOptions.reasoning` (`None` = thinking off). Carried for
    /// `prepareNextTurn` semantics; not forwarded through the pinned
    /// `StreamFn` shape (see file header).
    pub reasoning: Option<ThinkingLevel>,
    /// `SimpleStreamOptions.thinkingBudgets` (same forwarding caveat).
    pub thinking_budgets: Option<ThinkingBudgets>,
    /// Base stream options cloned per LLM call; `api_key` and `signal` are
    /// overwritten with the per-call values (upstream
    /// `{...config, apiKey: resolvedApiKey, signal}`). The static
    /// `stream_options.api_key` is the fallback key (upstream `config.apiKey`).
    pub stream_options: StreamOptions,
    /// Tool execution mode (upstream default: parallel).
    pub tool_execution: ToolExecutionMode,
    pub convert_to_llm: ConvertToLlmFn,
    pub transform_context: Option<TransformContextFn>,
    pub get_api_key: Option<GetApiKeyFn>,
    pub should_stop_after_turn: Option<ShouldStopAfterTurnFn>,
    pub prepare_next_turn: Option<PrepareNextTurnFn>,
    pub get_steering_messages: Option<GetQueuedMessagesFn>,
    pub get_follow_up_messages: Option<GetQueuedMessagesFn>,
    pub before_tool_call: Option<BeforeToolCallFn>,
    pub after_tool_call: Option<AfterToolCallFn>,
}

// ---------------------------------------------------------------------------
// AgentEventStream (mirrors pir-ai AssistantMessageEventStream shape)
// ---------------------------------------------------------------------------

struct Inner {
    tx: Mutex<Option<mpsc::UnboundedSender<AgentEvent>>>,
    rx: Mutex<mpsc::UnboundedReceiver<AgentEvent>>,
    result_tx: Mutex<Option<oneshot::Sender<Vec<AgentMessage>>>>,
    result_rx: Shared<oneshot::Receiver<Vec<AgentMessage>>>,
    done: AtomicBool,
}

/// Observational event stream returned by [`agent_loop`]/[`agent_loop_continue`]
/// (upstream `EventStream<AgentEvent, AgentMessage[]>` from
/// `createAgentStream`, agent-loop.ts:145-150).
///
/// - [`push`](Self::push) is ignored after an `agent_end` event or
///   [`end`](Self::end).
/// - [`result`](Self::result) resolves with the `agent_end` messages (or the
///   value passed to `end`).
#[derive(Clone)]
pub struct AgentEventStream {
    inner: Arc<Inner>,
}

impl Default for AgentEventStream {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentEventStream {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let (result_tx, result_rx) = oneshot::channel();
        Self {
            inner: Arc::new(Inner {
                tx: Mutex::new(Some(tx)),
                rx: Mutex::new(rx),
                result_tx: Mutex::new(Some(result_tx)),
                result_rx: result_rx.shared(),
                done: AtomicBool::new(false),
            }),
        }
    }

    fn resolve_result(&self, messages: Vec<AgentMessage>) {
        // First resolution wins (upstream promise resolve is idempotent).
        if let Some(tx) = lock(&self.inner.result_tx).take() {
            let _ = tx.send(messages);
        }
    }

    /// Push an event to the stream.
    pub fn push(&self, event: AgentEvent) {
        if self.inner.done.load(Ordering::SeqCst) {
            return;
        }
        if let AgentEvent::AgentEnd { messages } = &event {
            self.inner.done.store(true, Ordering::SeqCst);
            self.resolve_result(messages.clone());
        }
        // A closed channel (consumer dropped / ended) is not an error upstream.
        if let Some(tx) = lock(&self.inner.tx).as_ref() {
            let _ = tx.send(event);
        }
    }

    /// End the stream. The provided messages resolve pending `result()` calls
    /// (unless already resolved by an `agent_end` event).
    pub fn end(&self, messages: Vec<AgentMessage>) {
        self.inner.done.store(true, Ordering::SeqCst);
        self.resolve_result(messages);
        // Dropping the sender closes the channel: iterators terminate after
        // draining buffered events.
        lock(&self.inner.tx).take();
    }

    /// The final `agent_end` messages (resolves on the terminal event; an
    /// abandoned producer resolves to an empty list, upstream `[]`).
    pub fn result(&self) -> BoxFuture<'static, Vec<AgentMessage>> {
        self.inner
            .result_rx
            .clone()
            .map(|r| r.ok().unwrap_or_default())
            .boxed()
    }
}

impl Stream for AgentEventStream {
    type Item = AgentEvent;

    fn poll_next(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
        lock(&self.inner.rx).poll_recv(cx)
    }
}

// ---------------------------------------------------------------------------
// Entry points (agent-loop.ts:31-143)
// ---------------------------------------------------------------------------

/// `agentLoop` — start an agent loop with new prompt messages. The prompts
/// are added to the context and events are emitted for them
/// (agent-loop.ts:31-54).
///
/// Cancellation mirrors the upstream `AbortSignal`: pass a
/// [`CancellationToken`] and cancel it to abort the run.
pub fn agent_loop(
    prompts: Vec<AgentMessage>,
    context: AgentContext,
    config: AgentLoopConfig,
    signal: Option<CancellationToken>,
    stream_fn: StreamFn,
) -> AgentEventStream {
    let stream = AgentEventStream::new();
    let sink_stream = stream.clone();
    let emit: AgentEventSink = Arc::new(move |event| {
        let stream = sink_stream.clone();
        Box::pin(async move {
            stream.push(event);
        })
    });
    let end_stream = stream.clone();
    tokio::spawn(async move {
        let messages = run_agent_loop(prompts, context, config, emit, signal, stream_fn).await;
        end_stream.end(messages);
    });
    stream
}

/// `agentLoopContinue` — continue an agent loop from the current context
/// without adding a new message (agent-loop.ts:64-93). Validation errors are
/// returned synchronously, mirroring the upstream throw-before-stream.
pub fn agent_loop_continue(
    context: AgentContext,
    config: AgentLoopConfig,
    signal: Option<CancellationToken>,
    stream_fn: StreamFn,
) -> Result<AgentEventStream, AgentError> {
    validate_continuation(&context)?;

    let stream = AgentEventStream::new();
    let sink_stream = stream.clone();
    let emit: AgentEventSink = Arc::new(move |event| {
        let stream = sink_stream.clone();
        Box::pin(async move {
            stream.push(event);
        })
    });
    let end_stream = stream.clone();
    tokio::spawn(async move {
        // Validation already ran above (identical checks), so `Err` is
        // unreachable here; upstream would leave the stream open on a
        // rejection, we resolve with an empty list instead.
        let messages = run_agent_loop_continue(context, config, emit, signal, stream_fn)
            .await
            .unwrap_or_default();
        end_stream.end(messages);
    });
    Ok(stream)
}

fn validate_continuation(context: &AgentContext) -> Result<(), AgentError> {
    if context.messages.is_empty() {
        return Err(AgentError::Message(
            "Cannot continue: no messages in context".to_owned(),
        ));
    }
    if matches!(context.messages.last(), Some(AgentMessage::Assistant(_))) {
        return Err(AgentError::Message(
            "Cannot continue from message role: assistant".to_owned(),
        ));
    }
    Ok(())
}

/// `runAgentLoop` (agent-loop.ts:95-118). The `Agent` layer drives this
/// directly with its barrier sink.
pub async fn run_agent_loop(
    prompts: Vec<AgentMessage>,
    context: AgentContext,
    config: AgentLoopConfig,
    emit: AgentEventSink,
    signal: Option<CancellationToken>,
    stream_fn: StreamFn,
) -> Vec<AgentMessage> {
    let mut new_messages: Vec<AgentMessage> = prompts.clone();
    let mut current_context = AgentContext {
        system_prompt: context.system_prompt,
        messages: context
            .messages
            .iter()
            .cloned()
            .chain(prompts.iter().cloned())
            .collect(),
        tools: context.tools,
    };

    emit(AgentEvent::AgentStart).await;
    emit(AgentEvent::TurnStart).await;
    for prompt in &prompts {
        emit(AgentEvent::MessageStart {
            message: prompt.clone(),
        })
        .await;
        emit(AgentEvent::MessageEnd {
            message: prompt.clone(),
        })
        .await;
    }

    run_loop(
        &mut current_context,
        &mut new_messages,
        config,
        &signal,
        &emit,
        &stream_fn,
    )
    .await;
    new_messages
}

/// `runAgentLoopContinue` (agent-loop.ts:120-143).
pub async fn run_agent_loop_continue(
    context: AgentContext,
    config: AgentLoopConfig,
    emit: AgentEventSink,
    signal: Option<CancellationToken>,
    stream_fn: StreamFn,
) -> Result<Vec<AgentMessage>, AgentError> {
    validate_continuation(&context)?;

    let mut new_messages: Vec<AgentMessage> = Vec::new();
    let mut current_context = context;

    emit(AgentEvent::AgentStart).await;
    emit(AgentEvent::TurnStart).await;

    run_loop(
        &mut current_context,
        &mut new_messages,
        config,
        &signal,
        &emit,
        &stream_fn,
    )
    .await;
    Ok(new_messages)
}

// ---------------------------------------------------------------------------
// runLoop (agent-loop.ts:155-275)
// ---------------------------------------------------------------------------

fn is_aborted(signal: &Option<CancellationToken>) -> bool {
    signal.as_ref().is_some_and(CancellationToken::is_cancelled)
}

/// `runLoop` — main loop logic shared by both entry points. Mutates
/// `new_messages` in place like the upstream shared array.
async fn run_loop(
    current_context: &mut AgentContext,
    new_messages: &mut Vec<AgentMessage>,
    mut config: AgentLoopConfig,
    signal: &Option<CancellationToken>,
    emit: &AgentEventSink,
    stream_function: &StreamFn,
) {
    let mut first_turn = true;
    // Check for steering messages at start (user may have typed while waiting).
    let mut pending_messages: Vec<AgentMessage> = match &config.get_steering_messages {
        Some(get_steering) => get_steering().await,
        None => Vec::new(),
    };

    // Outer loop: continues when queued follow-up messages arrive after the
    // agent would stop.
    loop {
        let mut has_more_tool_calls = true;

        // Inner loop: process tool calls and steering messages.
        while has_more_tool_calls || !pending_messages.is_empty() {
            if !first_turn {
                emit(AgentEvent::TurnStart).await;
            } else {
                first_turn = false;
            }

            // Process pending messages (inject before next assistant response).
            if !pending_messages.is_empty() {
                for message in std::mem::take(&mut pending_messages) {
                    emit(AgentEvent::MessageStart {
                        message: message.clone(),
                    })
                    .await;
                    emit(AgentEvent::MessageEnd {
                        message: message.clone(),
                    })
                    .await;
                    current_context.messages.push(message.clone());
                    new_messages.push(message);
                }
            }

            // Stream assistant response.
            let message =
                stream_assistant_response(current_context, &config, signal, emit, stream_function)
                    .await;
            new_messages.push(AgentMessage::Assistant(message.clone()));

            if matches!(message.stop_reason, StopReason::Error | StopReason::Aborted) {
                emit(AgentEvent::TurnEnd {
                    message: AgentMessage::Assistant(message),
                    tool_results: Vec::new(),
                })
                .await;
                emit(AgentEvent::AgentEnd {
                    messages: new_messages.clone(),
                })
                .await;
                return;
            }

            // Check for tool calls.
            let tool_calls: Vec<AgentToolCall> = message
                .content
                .iter()
                .filter_map(|c| match c {
                    AssistantContent::ToolCall(tc) => Some(tc.clone()),
                    _ => None,
                })
                .collect();

            let mut tool_results: Vec<ToolResultMessage> = Vec::new();
            has_more_tool_calls = false;
            if !tool_calls.is_empty() {
                // A "length" stop means the output was cut off by the token
                // limit, so every tool call in the message may carry truncated
                // arguments. Fail them all instead of executing potentially
                // borked calls.
                let executed_batch = if message.stop_reason == StopReason::Length {
                    fail_tool_calls_from_truncated_message(&tool_calls, emit).await
                } else {
                    execute_tool_calls(
                        current_context,
                        &message,
                        &tool_calls,
                        &config,
                        signal,
                        emit,
                    )
                    .await
                };
                tool_results = executed_batch.messages;
                has_more_tool_calls = !executed_batch.terminate;

                for result in &tool_results {
                    current_context
                        .messages
                        .push(AgentMessage::ToolResult(result.clone()));
                    new_messages.push(AgentMessage::ToolResult(result.clone()));
                }
            }

            emit(AgentEvent::TurnEnd {
                message: AgentMessage::Assistant(message.clone()),
                tool_results: tool_results.clone(),
            })
            .await;

            if let Some(prepare_next_turn) = &config.prepare_next_turn {
                let next_turn_context = PrepareNextTurnContext {
                    message: message.clone(),
                    tool_results: tool_results.clone(),
                    context: current_context.clone(),
                    new_messages: new_messages.clone(),
                };
                if let Some(update) = prepare_next_turn(next_turn_context).await {
                    if let Some(next_context) = update.context {
                        *current_context = next_context;
                    }
                    if let Some(model) = update.model {
                        config.model = model;
                    }
                    if let Some(thinking_level) = update.thinking_level {
                        config.reasoning = thinking_level_from_model_level(thinking_level);
                    }
                }
            }

            if let Some(should_stop_after_turn) = &config.should_stop_after_turn {
                let stop_context = ShouldStopAfterTurnContext {
                    message: message.clone(),
                    tool_results: tool_results.clone(),
                    context: current_context.clone(),
                    new_messages: new_messages.clone(),
                };
                if should_stop_after_turn(stop_context).await {
                    emit(AgentEvent::AgentEnd {
                        messages: new_messages.clone(),
                    })
                    .await;
                    return;
                }
            }

            pending_messages = match &config.get_steering_messages {
                Some(get_steering) => get_steering().await,
                None => Vec::new(),
            };
        }

        // Agent would stop here. Check for follow-up messages.
        let follow_up_messages = match &config.get_follow_up_messages {
            Some(get_follow_up) => get_follow_up().await,
            None => Vec::new(),
        };
        if !follow_up_messages.is_empty() {
            // Set as pending so the inner loop processes them.
            pending_messages = follow_up_messages;
            continue;
        }

        // No more messages, exit.
        break;
    }

    emit(AgentEvent::AgentEnd {
        messages: new_messages.clone(),
    })
    .await;
}

// ---------------------------------------------------------------------------
// streamAssistantResponse (agent-loop.ts:281-372)
// ---------------------------------------------------------------------------

fn stream_event_partial(event: &StreamEvent) -> Option<&AssistantMessage> {
    match event {
        StreamEvent::TextStart { partial, .. }
        | StreamEvent::TextDelta { partial, .. }
        | StreamEvent::TextEnd { partial, .. }
        | StreamEvent::ThinkingStart { partial, .. }
        | StreamEvent::ThinkingDelta { partial, .. }
        | StreamEvent::ThinkingEnd { partial, .. }
        | StreamEvent::ToolCallStart { partial, .. }
        | StreamEvent::ToolCallDelta { partial, .. }
        | StreamEvent::ToolCallEnd { partial, .. } => Some(partial),
        StreamEvent::Start { .. } | StreamEvent::Done { .. } | StreamEvent::Error { .. } => None,
    }
}

fn replace_context_tail(context: &mut AgentContext, message: AssistantMessage) {
    if let Some(last) = context.messages.last_mut() {
        *last = AgentMessage::Assistant(message);
    } else {
        context.messages.push(AgentMessage::Assistant(message));
    }
}

/// Shared tail of the upstream done/error branch and the post-loop block:
/// replace the partial (or push), emit `message_start` when no partial was
/// ever added, then emit `message_end`.
async fn finalize_streamed_message(
    context: &mut AgentContext,
    final_message: AssistantMessage,
    added_partial: bool,
    emit: &AgentEventSink,
) -> AssistantMessage {
    if added_partial {
        replace_context_tail(context, final_message.clone());
    } else {
        context
            .messages
            .push(AgentMessage::Assistant(final_message.clone()));
        emit(AgentEvent::MessageStart {
            message: AgentMessage::Assistant(final_message.clone()),
        })
        .await;
    }
    emit(AgentEvent::MessageEnd {
        message: AgentMessage::Assistant(final_message.clone()),
    })
    .await;
    final_message
}

/// `streamAssistantResponse` — stream one assistant response from the LLM.
/// This is where `AgentMessage[]` gets transformed to `Message[]`.
async fn stream_assistant_response(
    context: &mut AgentContext,
    config: &AgentLoopConfig,
    signal: &Option<CancellationToken>,
    emit: &AgentEventSink,
    stream_function: &StreamFn,
) -> AssistantMessage {
    // Apply context transform if configured (AgentMessage[] → AgentMessage[]).
    let messages = match &config.transform_context {
        Some(transform) => {
            transform(context.messages.clone(), signal.clone().unwrap_or_default()).await
        }
        None => context.messages.clone(),
    };

    // Convert to LLM-compatible messages (AgentMessage[] → Message[]).
    let llm_messages = (config.convert_to_llm)(messages).await;

    // Build LLM context.
    let llm_context = Context {
        system_prompt: Some(context.system_prompt.clone()),
        messages: llm_messages,
        tools: context.tools.as_ref().map(|tools| {
            tools
                .iter()
                .map(|tool| Tool {
                    name: tool.name().to_owned(),
                    description: tool.description().to_owned(),
                    parameters: tool.parameters().clone(),
                    constrained_sampling: tool.constrained_sampling(),
                })
                .collect()
        }),
    };

    // Resolve API key (important for expiring tokens). Upstream `||` treats an
    // empty string as missing, hence the non-empty filter.
    let resolved_api_key = match &config.get_api_key {
        Some(get_api_key) => get_api_key(config.model.provider.clone()).await,
        None => None,
    }
    .filter(|key| !key.is_empty())
    .or_else(|| config.stream_options.api_key.clone());

    let mut options = config.stream_options.clone();
    options.api_key = resolved_api_key;
    options.signal = signal.clone();
    // Upstream spreads the whole config (`{...config, apiKey, signal}`), so
    // `config.reasoning` — the live thinking level, updated by
    // `prepareNextTurn` — reaches the request. The pinned `StreamFn` shape
    // takes plain `StreamOptions`, so bind the channel here instead (design
    // doc §4.4 D-013). Without this the level set via /settings, the thinking
    // selector or `setThinkingLevel` never reaches the provider request.
    options.reasoning = config.reasoning.map(|level| level.to_model_level());

    let mut response = stream_function(config.model.clone(), llm_context, options);

    let mut partial_message: Option<AssistantMessage> = None;
    let mut added_partial = false;

    while let Some(event) = response.next().await {
        match event {
            StreamEvent::Start { partial } => {
                partial_message = Some(partial.clone());
                context
                    .messages
                    .push(AgentMessage::Assistant(partial.clone()));
                added_partial = true;
                emit(AgentEvent::MessageStart {
                    message: AgentMessage::Assistant(partial),
                })
                .await;
            }
            StreamEvent::Done { message, .. } => {
                return finalize_streamed_message(context, message, added_partial, emit).await;
            }
            StreamEvent::Error { error, .. } => {
                return finalize_streamed_message(context, error, added_partial, emit).await;
            }
            other => {
                if partial_message.is_some() {
                    if let Some(partial) = stream_event_partial(&other) {
                        let partial = partial.clone();
                        partial_message = Some(partial.clone());
                        replace_context_tail(context, partial.clone());
                        emit(AgentEvent::MessageUpdate {
                            message: AgentMessage::Assistant(partial),
                            assistant_message_event: Box::new(other),
                        })
                        .await;
                    }
                }
            }
        }
    }

    // The stream ended without a terminal done/error event, violating the
    // StreamFn contract. Upstream would await `response.result()` here (which
    // cannot resolve for such a stream); synthesize an error message instead.
    tracing::warn!("stream ended without a terminal done/error event");
    let final_message = AssistantMessage {
        role: AssistantRole::Assistant,
        content: Vec::new(),
        api: config.model.api.clone(),
        provider: config.model.provider.clone(),
        model: config.model.id.clone(),
        response_model: None,
        response_id: None,
        diagnostics: None,
        usage: Usage::default(),
        stop_reason: StopReason::Error,
        error_message: Some("Stream ended without a terminal done/error event".to_owned()),
        timestamp: now_millis(),
    };
    finalize_streamed_message(context, final_message, added_partial, emit).await
}

// ---------------------------------------------------------------------------
// Tool execution (agent-loop.ts:374-792)
// ---------------------------------------------------------------------------

/// `ExecutedToolCallBatch`.
struct ExecutedToolCallBatch {
    messages: Vec<ToolResultMessage>,
    terminate: bool,
}

/// `PreparedToolCall`.
struct PreparedToolCall {
    tool_call: AgentToolCall,
    tool: Arc<dyn AgentTool>,
    args: Value,
}

/// `PreparedToolCall | ImmediateToolCallOutcome`.
enum Preparation {
    Prepared(PreparedToolCall),
    Immediate {
        result: AgentToolResult,
        is_error: bool,
    },
}

/// `ExecutedToolCallOutcome`.
struct ExecutedToolCallOutcome {
    result: AgentToolResult,
    is_error: bool,
}

/// `FinalizedToolCallOutcome`.
struct FinalizedToolCallOutcome {
    tool_call: AgentToolCall,
    result: AgentToolResult,
    is_error: bool,
}

/// `failToolCallsFromTruncatedMessage` (agent-loop.ts:381-406).
async fn fail_tool_calls_from_truncated_message(
    tool_calls: &[AgentToolCall],
    emit: &AgentEventSink,
) -> ExecutedToolCallBatch {
    let mut messages: Vec<ToolResultMessage> = Vec::new();
    for tool_call in tool_calls {
        emit(AgentEvent::ToolExecutionStart {
            tool_call_id: tool_call.id.clone(),
            tool_name: tool_call.name.clone(),
            args: Value::Object(tool_call.arguments.clone()),
        })
        .await;
        let finalized = FinalizedToolCallOutcome {
            tool_call: tool_call.clone(),
            result: create_error_tool_result(format!(
                "Tool call \"{}\" was not executed: the response hit the output token limit, so its arguments may be truncated. Re-issue the tool call with complete arguments.",
                tool_call.name
            )),
            is_error: true,
        };
        emit_tool_execution_end(&finalized, emit).await;
        let tool_result_message = create_tool_result_message(&finalized);
        emit_tool_result_message(&tool_result_message, emit).await;
        messages.push(tool_result_message);
    }
    ExecutedToolCallBatch {
        messages,
        terminate: false,
    }
}

/// `executeToolCalls` (agent-loop.ts:411-426).
async fn execute_tool_calls(
    current_context: &AgentContext,
    assistant_message: &AssistantMessage,
    tool_calls: &[AgentToolCall],
    config: &AgentLoopConfig,
    signal: &Option<CancellationToken>,
    emit: &AgentEventSink,
) -> ExecutedToolCallBatch {
    let has_sequential_tool_call = tool_calls.iter().any(|tc| {
        current_context
            .tools
            .as_ref()
            .and_then(|tools| tools.iter().find(|t| t.name() == tc.name))
            .and_then(|t| t.execution_mode())
            == Some(ToolExecutionMode::Sequential)
    });
    if config.tool_execution == ToolExecutionMode::Sequential || has_sequential_tool_call {
        return execute_tool_calls_sequential(
            current_context,
            assistant_message,
            tool_calls,
            config,
            signal,
            emit,
        )
        .await;
    }
    execute_tool_calls_parallel(
        current_context,
        assistant_message,
        tool_calls,
        config,
        signal,
        emit,
    )
    .await
}

/// `executeToolCallsSequential` (agent-loop.ts:433-487).
async fn execute_tool_calls_sequential(
    current_context: &AgentContext,
    assistant_message: &AssistantMessage,
    tool_calls: &[AgentToolCall],
    config: &AgentLoopConfig,
    signal: &Option<CancellationToken>,
    emit: &AgentEventSink,
) -> ExecutedToolCallBatch {
    let mut finalized_calls: Vec<FinalizedToolCallOutcome> = Vec::new();
    let mut messages: Vec<ToolResultMessage> = Vec::new();

    for tool_call in tool_calls {
        emit(AgentEvent::ToolExecutionStart {
            tool_call_id: tool_call.id.clone(),
            tool_name: tool_call.name.clone(),
            args: Value::Object(tool_call.arguments.clone()),
        })
        .await;

        let preparation = prepare_tool_call(
            current_context,
            assistant_message,
            tool_call,
            config,
            signal,
        )
        .await;
        let finalized = match preparation {
            Preparation::Immediate { result, is_error } => FinalizedToolCallOutcome {
                tool_call: tool_call.clone(),
                result,
                is_error,
            },
            Preparation::Prepared(prepared) => {
                let executed = execute_prepared_tool_call(&prepared, signal, emit).await;
                finalize_executed_tool_call(
                    current_context,
                    assistant_message,
                    &prepared,
                    executed,
                    &config.after_tool_call,
                    signal,
                )
                .await
            }
        };

        emit_tool_execution_end(&finalized, emit).await;
        let tool_result_message = create_tool_result_message(&finalized);
        emit_tool_result_message(&tool_result_message, emit).await;
        finalized_calls.push(finalized);
        messages.push(tool_result_message);

        if is_aborted(signal) {
            break;
        }
    }

    ExecutedToolCallBatch {
        messages,
        terminate: should_terminate_tool_batch(&finalized_calls),
    }
}

/// `executeToolCallsParallel` (agent-loop.ts:489-554).
///
/// Preflight is always sequential; `tool_execution_end` for prepared calls is
/// emitted in completion order from the spawned tasks; tool-result artifacts
/// are emitted afterwards in assistant source order.
async fn execute_tool_calls_parallel(
    current_context: &AgentContext,
    assistant_message: &AssistantMessage,
    tool_calls: &[AgentToolCall],
    config: &AgentLoopConfig,
    signal: &Option<CancellationToken>,
    emit: &AgentEventSink,
) -> ExecutedToolCallBatch {
    // One slot per preflighted tool call, in source order.
    let mut slots: Vec<Option<FinalizedToolCallOutcome>> = Vec::new();
    let mut set = tokio::task::JoinSet::new();

    for tool_call in tool_calls {
        emit(AgentEvent::ToolExecutionStart {
            tool_call_id: tool_call.id.clone(),
            tool_name: tool_call.name.clone(),
            args: Value::Object(tool_call.arguments.clone()),
        })
        .await;

        let preparation = prepare_tool_call(
            current_context,
            assistant_message,
            tool_call,
            config,
            signal,
        )
        .await;
        match preparation {
            Preparation::Immediate { result, is_error } => {
                let finalized = FinalizedToolCallOutcome {
                    tool_call: tool_call.clone(),
                    result,
                    is_error,
                };
                emit_tool_execution_end(&finalized, emit).await;
                slots.push(Some(finalized));
                if is_aborted(signal) {
                    break;
                }
                continue;
            }
            Preparation::Prepared(prepared) => {
                let index = slots.len();
                slots.push(None);
                let task_emit = emit.clone();
                let task_context = current_context.clone();
                let task_assistant_message = assistant_message.clone();
                let task_after_tool_call = config.after_tool_call.clone();
                let task_signal = signal.clone();
                set.spawn(async move {
                    let executed =
                        execute_prepared_tool_call(&prepared, &task_signal, &task_emit).await;
                    let finalized = finalize_executed_tool_call(
                        &task_context,
                        &task_assistant_message,
                        &prepared,
                        executed,
                        &task_after_tool_call,
                        &task_signal,
                    )
                    .await;
                    emit_tool_execution_end(&finalized, &task_emit).await;
                    (index, finalized)
                });
                if is_aborted(signal) {
                    break;
                }
            }
        }
    }

    let mut join_errors: Vec<String> = Vec::new();
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok((index, finalized)) => slots[index] = Some(finalized),
            // Tasks are infallible by construction; a JoinError means a panic
            // or abort. Fill the missing slots with error results below.
            Err(error) => {
                tracing::error!(%error, "parallel tool task failed");
                join_errors.push(error.to_string());
            }
        }
    }
    if !join_errors.is_empty() {
        let message = join_errors.join("; ");
        for (index, slot) in slots.iter_mut().enumerate() {
            if slot.is_none() {
                *slot = Some(FinalizedToolCallOutcome {
                    tool_call: tool_calls[index].clone(),
                    result: create_error_tool_result(format!("Tool task failed: {message}")),
                    is_error: true,
                });
            }
        }
    }

    let finalized_calls: Vec<FinalizedToolCallOutcome> = slots.into_iter().flatten().collect();
    let mut messages: Vec<ToolResultMessage> = Vec::new();
    for finalized in &finalized_calls {
        let tool_result_message = create_tool_result_message(finalized);
        emit_tool_result_message(&tool_result_message, emit).await;
        messages.push(tool_result_message);
    }

    ExecutedToolCallBatch {
        messages,
        terminate: should_terminate_tool_batch(&finalized_calls),
    }
}

/// `shouldTerminateToolBatch` (agent-loop.ts:582-584).
fn should_terminate_tool_batch(finalized_calls: &[FinalizedToolCallOutcome]) -> bool {
    !finalized_calls.is_empty()
        && finalized_calls
            .iter()
            .all(|finalized| finalized.result.terminate == Some(true))
}

/// `prepareToolCallArguments` (agent-loop.ts:586-598).
fn prepare_tool_call_arguments(
    tool: &Arc<dyn AgentTool>,
    tool_call: &AgentToolCall,
) -> AgentToolCall {
    let original = Value::Object(tool_call.arguments.clone());
    let prepared = tool.prepare_arguments(original.clone());
    if prepared == original {
        return tool_call.clone();
    }
    let mut prepared_call = tool_call.clone();
    if let Value::Object(map) = prepared {
        prepared_call.arguments = map;
    }
    prepared_call
}

/// `prepareToolCall` (agent-loop.ts:600-664). Validation/lookup failures
/// become immediate error results; they never interrupt the loop (upstream
/// catch-all).
async fn prepare_tool_call(
    current_context: &AgentContext,
    assistant_message: &AssistantMessage,
    tool_call: &AgentToolCall,
    config: &AgentLoopConfig,
    signal: &Option<CancellationToken>,
) -> Preparation {
    let Some(tool) = current_context
        .tools
        .as_ref()
        .and_then(|tools| tools.iter().find(|t| t.name() == tool_call.name))
        .cloned()
    else {
        return Preparation::Immediate {
            result: create_error_tool_result(format!("Tool {} not found", tool_call.name)),
            is_error: true,
        };
    };

    let prepared_call = prepare_tool_call_arguments(&tool, tool_call);
    let schema_tool = Tool {
        name: tool.name().to_owned(),
        description: tool.description().to_owned(),
        parameters: tool.parameters().clone(),
        constrained_sampling: tool.constrained_sampling(),
    };
    let mut validated_args = match validate_tool_arguments(&schema_tool, &prepared_call) {
        Ok(args) => args,
        Err(message) => {
            return Preparation::Immediate {
                result: create_error_tool_result(message),
                is_error: true,
            };
        }
    };

    if let Some(before_tool_call) = &config.before_tool_call {
        let before_context = BeforeToolCallContext {
            assistant_message: assistant_message.clone(),
            tool_call: tool_call.clone(),
            args: validated_args.clone(),
            context: current_context.clone(),
        };
        let before_result =
            before_tool_call(before_context, signal.clone().unwrap_or_default()).await;
        if is_aborted(signal) {
            return Preparation::Immediate {
                result: create_error_tool_result("Operation aborted".to_owned()),
                is_error: true,
            };
        }
        if let Some(result) = before_result {
            if result.block == Some(true) {
                return Preparation::Immediate {
                    result: create_error_tool_result(
                        result
                            .reason
                            .unwrap_or_else(|| "Tool execution was blocked".to_owned()),
                    ),
                    is_error: true,
                };
            }
            // Mutated hook args reach `execute` without revalidation
            // (upstream in-place mutation of the validated args object).
            if let Some(mutated_args) = result.args {
                validated_args = mutated_args;
            }
        }
    }
    if is_aborted(signal) {
        return Preparation::Immediate {
            result: create_error_tool_result("Operation aborted".to_owned()),
            is_error: true,
        };
    }
    Preparation::Prepared(PreparedToolCall {
        tool_call: tool_call.clone(),
        tool,
        args: validated_args,
    })
}

/// `executePreparedToolCall` (agent-loop.ts:666-707).
///
/// Update settle semantics: updates emitted after `execute` returns are
/// ignored; already-queued update events are awaited before returning.
/// `Err` from `execute` becomes an error result.
async fn execute_prepared_tool_call(
    prepared: &PreparedToolCall,
    signal: &Option<CancellationToken>,
    emit: &AgentEventSink,
) -> ExecutedToolCallOutcome {
    let accepting_updates = Arc::new(AtomicBool::new(true));
    let update_events: Arc<Mutex<Vec<BoxFuture<'static, ()>>>> = Arc::new(Mutex::new(Vec::new()));

    let on_update: crate::types::AgentToolUpdateCallback = {
        let accepting_updates = accepting_updates.clone();
        let update_events = update_events.clone();
        let emit = emit.clone();
        let tool_call_id = prepared.tool_call.id.clone();
        let tool_name = prepared.tool_call.name.clone();
        let args = Value::Object(prepared.tool_call.arguments.clone());
        Box::new(move |partial_result: AgentToolResult| {
            if !accepting_updates.load(Ordering::SeqCst) {
                return;
            }
            let event = AgentEvent::ToolExecutionUpdate {
                tool_call_id: tool_call_id.clone(),
                tool_name: tool_name.clone(),
                args: args.clone(),
                partial_result: serde_json::to_value(&partial_result).unwrap_or(Value::Null),
            };
            let update = emit(event);
            lock(&update_events).push(update);
        })
    };

    let result = prepared
        .tool
        .execute(
            &prepared.tool_call.id,
            prepared.args.clone(),
            signal.clone().unwrap_or_default(),
            Some(on_update),
        )
        .await;
    accepting_updates.store(false, Ordering::SeqCst);
    // Await already-queued update events (upstream `Promise.all(updateEvents)`;
    // awaited sequentially here to keep event order deterministic).
    let queued = std::mem::take(&mut *lock(&update_events));
    for update in queued {
        update.await;
    }

    match result {
        Ok(result) => ExecutedToolCallOutcome {
            result,
            is_error: false,
        },
        Err(error) => ExecutedToolCallOutcome {
            result: create_error_tool_result(error.to_string()),
            is_error: true,
        },
    }
}

/// `finalizeExecutedToolCall` (agent-loop.ts:709-754). The five
/// `afterToolCall` fields replace independently (no deep merge). Upstream
/// wraps the hook in try/catch and downgrades a throw to an error result;
/// `Err` from the Rust hook gets the same treatment.
async fn finalize_executed_tool_call(
    current_context: &AgentContext,
    assistant_message: &AssistantMessage,
    prepared: &PreparedToolCall,
    executed: ExecutedToolCallOutcome,
    after_tool_call: &Option<AfterToolCallFn>,
    signal: &Option<CancellationToken>,
) -> FinalizedToolCallOutcome {
    let mut result = executed.result;
    let mut is_error = executed.is_error;

    if let Some(after_tool_call) = after_tool_call {
        let after_context = AfterToolCallContext {
            assistant_message: assistant_message.clone(),
            tool_call: prepared.tool_call.clone(),
            args: prepared.args.clone(),
            result: result.clone(),
            is_error,
            context: current_context.clone(),
        };
        match after_tool_call(after_context, signal.clone().unwrap_or_default()).await {
            Ok(Some(after_result)) => {
                if let Some(content) = after_result.content {
                    result.content = content;
                }
                if let Some(details) = after_result.details {
                    result.details = details;
                }
                if let Some(usage) = after_result.usage {
                    result.usage = Some(usage);
                }
                if let Some(terminate) = after_result.terminate {
                    result.terminate = Some(terminate);
                }
                if let Some(after_is_error) = after_result.is_error {
                    is_error = after_is_error;
                }
            }
            Ok(None) => {}
            // Upstream catch (agent-loop.ts:743-746): a failing hook replaces
            // the whole outcome with an error result carrying the error text.
            Err(error) => {
                result = create_error_tool_result(error.to_string());
                is_error = true;
            }
        }
    }

    FinalizedToolCallOutcome {
        tool_call: prepared.tool_call.clone(),
        result,
        is_error,
    }
}

/// `createErrorToolResult` (agent-loop.ts:756-761).
fn create_error_tool_result(message: String) -> AgentToolResult {
    AgentToolResult {
        content: vec![ToolResultContent::Text(TextContent {
            text: message,
            text_signature: None,
        })],
        details: json!({}),
        ..Default::default()
    }
}

/// `emitToolExecutionEnd` (agent-loop.ts:763-771).
async fn emit_tool_execution_end(finalized: &FinalizedToolCallOutcome, emit: &AgentEventSink) {
    emit(AgentEvent::ToolExecutionEnd {
        tool_call_id: finalized.tool_call.id.clone(),
        tool_name: finalized.tool_call.name.clone(),
        result: serde_json::to_value(&finalized.result).unwrap_or(Value::Null),
        is_error: finalized.is_error,
    })
    .await;
}

/// `createToolResultMessage` (agent-loop.ts:773-787).
fn create_tool_result_message(finalized: &FinalizedToolCallOutcome) -> ToolResultMessage {
    ToolResultMessage {
        role: ToolResultRole::ToolResult,
        tool_call_id: finalized.tool_call.id.clone(),
        tool_name: finalized.tool_call.name.clone(),
        // Untyped tools can return results without content; normalize so the
        // null never enters session history or provider payloads.
        content: finalized.result.content.clone(),
        // `AgentToolResult.details` is a required `Value`; `null` maps to the
        // upstream `undefined` (key omitted on the wire).
        details: if finalized.result.details.is_null() {
            None
        } else {
            Some(finalized.result.details.clone())
        },
        usage: finalized.result.usage.clone(),
        added_tool_names: finalized
            .result
            .added_tool_names
            .clone()
            .filter(|names| !names.is_empty()),
        is_error: finalized.is_error,
        timestamp: now_millis(),
    }
}

/// `emitToolResultMessage` (agent-loop.ts:789-792).
async fn emit_tool_result_message(message: &ToolResultMessage, emit: &AgentEventSink) {
    emit(AgentEvent::MessageStart {
        message: AgentMessage::ToolResult(message.clone()),
    })
    .await;
    emit(AgentEvent::MessageEnd {
        message: AgentMessage::ToolResult(message.clone()),
    })
    .await;
}
