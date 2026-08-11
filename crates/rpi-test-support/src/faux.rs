//! Port of `packages/ai/src/providers/faux.ts` @ pi 0.82.1 (2efa728).
//!
//! Deterministic faux provider for tests (coding-standards §12.4): scripted
//! response queue + response factories + `tokens_per_second` pacing + usage
//! estimation at 4 chars/token + prompt-cache simulation (session id present
//! and cache retention ≠ none) + fixed empty-queue error text +
//! `state.call_count`.
//!
//! Intentional differences from upstream (test-support internal only; none of
//! these affect persisted wire formats):
//! - Chunk sizes cycle deterministically through `min..=max` instead of
//!   `Math.random` — upstream chunk boundaries are nondeterministic, and delta
//!   boundaries never reach session JSONL.
//! - Default ids (`faux_tool_call`, default api name) come from a thread-local
//!   counter instead of `Date.now() + Math.random`.
//! - `faux_assistant_message` defaults `timestamp` to `0` instead of
//!   `Date.now()`; the parity normalizer strips timestamps anyway.
//! - Response factories are synchronous `Fn` closures (upstream allows async).
//! - Usage estimation counts Unicode scalar values / 4 (upstream counts UTF-16
//!   code units / 4); identical for BMP text, which fixtures use.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use futures::StreamExt;
use rpi_agent::stream_fn::StreamFn;
use rpi_ai::types::{
    ApiKind, AssistantContent, AssistantMessage, AssistantRole, CacheRetention, Context,
    DoneReason, ErrorReason, ImageContent, InputModality, Message, Model, ModelCost,
    ModelCostRates, ProviderResponse, StopReason, StreamEvent, StreamOptions, TextContent,
    ThinkingContent, ToolCall, ToolResultContent, ToolResultMessage, Usage, UsageCost,
    UserContentBlock,
};

pub const DEFAULT_API: &str = "faux";
pub const DEFAULT_PROVIDER: &str = "faux";
pub const DEFAULT_MODEL_ID: &str = "faux-1";
pub const DEFAULT_MODEL_NAME: &str = "Faux Model";
pub const DEFAULT_BASE_URL: &str = "http://localhost:0";
pub const DEFAULT_MIN_TOKEN_SIZE: usize = 3;
pub const DEFAULT_MAX_TOKEN_SIZE: usize = 5;
pub const DEFAULT_CONTEXT_WINDOW: u32 = 128000;
pub const DEFAULT_MAX_TOKENS: u32 = 16384;

/// Error text emitted when the scripted response queue is empty (fixed
/// upstream wording — tests assert on it).
pub const EMPTY_QUEUE_ERROR: &str = "No more faux responses queued";
/// Fixed abort error wording (upstream `createAbortedMessage`).
pub const ABORTED_ERROR: &str = "Request was aborted";

fn default_usage() -> Usage {
    Usage::default()
}

// ---------------------------------------------------------------------------
// Response factories
// ---------------------------------------------------------------------------

pub fn faux_text(text: impl Into<String>) -> AssistantContent {
    AssistantContent::Text(TextContent {
        text: text.into(),
        text_signature: None,
    })
}

pub fn faux_thinking(thinking: impl Into<String>) -> AssistantContent {
    AssistantContent::Thinking(ThinkingContent {
        thinking: thinking.into(),
        thinking_signature: None,
        redacted: None,
    })
}

/// Default tool-call ids: deterministic per thread (`tool:1`, `tool:2`, …).
fn next_tool_id() -> String {
    thread_local! {
        static COUNTER: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    }
    let n = COUNTER.with(|c| {
        let v = c.get() + 1;
        c.set(v);
        v
    });
    format!("tool:{n}")
}

pub fn faux_tool_call(
    name: impl Into<String>,
    arguments: serde_json::Map<String, serde_json::Value>,
    id: Option<String>,
) -> AssistantContent {
    AssistantContent::ToolCall(ToolCall {
        id: id.unwrap_or_else(next_tool_id),
        name: name.into(),
        arguments,
        thought_signature: None,
        namespace: None,
    })
}

/// Options for [`faux_assistant_message`] (upstream's options bag).
#[derive(Debug, Clone, Default)]
pub struct FauxAssistantOptions {
    pub stop_reason: Option<StopReason>,
    pub error_message: Option<String>,
    pub response_id: Option<String>,
    pub timestamp: Option<i64>,
}

pub fn faux_assistant_message(
    content: impl Into<FauxContent>,
    options: FauxAssistantOptions,
) -> AssistantMessage {
    AssistantMessage {
        role: AssistantRole::Assistant,
        content: content.into().0,
        api: ApiKind(DEFAULT_API.to_owned()),
        provider: DEFAULT_PROVIDER.to_owned(),
        model: DEFAULT_MODEL_ID.to_owned(),
        response_model: None,
        response_id: options.response_id,
        diagnostics: None,
        usage: default_usage(),
        stop_reason: options.stop_reason.unwrap_or(StopReason::Stop),
        error_message: options.error_message,
        timestamp: options.timestamp.unwrap_or(0),
        deferred: None,
        end_turn: None,
        raw_stop_reason: None,
    }
}

/// `string | FauxContentBlock | FauxContentBlock[]` shorthand.
pub struct FauxContent(pub Vec<AssistantContent>);

impl From<&str> for FauxContent {
    fn from(s: &str) -> Self {
        Self(vec![faux_text(s)])
    }
}

impl From<String> for FauxContent {
    fn from(s: String) -> Self {
        Self(vec![faux_text(s)])
    }
}

impl From<AssistantContent> for FauxContent {
    fn from(block: AssistantContent) -> Self {
        Self(vec![block])
    }
}

impl From<Vec<AssistantContent>> for FauxContent {
    fn from(blocks: Vec<AssistantContent>) -> Self {
        Self(blocks)
    }
}

// ---------------------------------------------------------------------------
// Scripted responses
// ---------------------------------------------------------------------------

/// Snapshot of provider counters passed to response factories (upstream
/// `state: { callCount }`).
#[derive(Debug, Clone, Copy, Default)]
pub struct FauxState {
    pub call_count: u64,
}

/// Factory response step: computes the assistant message from the actual
/// request context/options (upstream `FauxResponseFactory`, synchronous).
pub type FauxResponseFactory = Box<
    dyn Fn(&Context, Option<&StreamOptions>, FauxState, &Model) -> AssistantMessage + Send + Sync,
>;

/// One scripted step: a fixed message or a factory. The message variant is
/// boxed to keep the enum small (`clippy::large_enum_variant`).
pub enum FauxResponseStep {
    Message(Box<AssistantMessage>),
    Factory(FauxResponseFactory),
}

impl From<AssistantMessage> for FauxResponseStep {
    fn from(m: AssistantMessage) -> Self {
        Self::Message(Box::new(m))
    }
}

// ---------------------------------------------------------------------------
// Provider registration
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct FauxModelDefinition {
    pub id: String,
    pub name: Option<String>,
    pub reasoning: Option<bool>,
    pub input: Option<Vec<InputModality>>,
    pub cost: Option<ModelCostRates>,
    pub context_window: Option<u32>,
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Clone, Default)]
pub struct FauxTokenSize {
    pub min: Option<usize>,
    pub max: Option<usize>,
}

#[derive(Debug, Clone, Default)]
pub struct FauxProviderOptions {
    pub api: Option<String>,
    pub provider: Option<String>,
    pub models: Option<Vec<FauxModelDefinition>>,
    pub tokens_per_second: Option<f64>,
    pub token_size: Option<FauxTokenSize>,
}

struct FauxInner {
    pending: std::collections::VecDeque<FauxResponseStep>,
    call_count: u64,
    prompt_cache: HashMap<String, String>,
}

/// Faux provider handle. Clone via `Arc`; the `stream_fn` closure shares the
/// same state so `state.call_count` reflects every issued stream.
pub struct FauxProvider {
    api: String,
    provider: String,
    models: Vec<Model>,
    tokens_per_second: Option<f64>,
    min_token_size: usize,
    max_token_size: usize,
    inner: Mutex<FauxInner>,
}

impl FauxProvider {
    pub fn new(options: FauxProviderOptions) -> Arc<Self> {
        let api = options.api.unwrap_or_else(|| DEFAULT_API.to_owned());
        let provider = options
            .provider
            .unwrap_or_else(|| DEFAULT_PROVIDER.to_owned());
        let (min_raw, max_raw) = options
            .token_size
            .map(|t| {
                (
                    t.min.unwrap_or(DEFAULT_MIN_TOKEN_SIZE),
                    t.max.unwrap_or(DEFAULT_MAX_TOKEN_SIZE),
                )
            })
            .unwrap_or((DEFAULT_MIN_TOKEN_SIZE, DEFAULT_MAX_TOKEN_SIZE));
        let min_token_size = min_raw.min(max_raw).max(1);
        let max_token_size = max_raw.max(min_token_size);

        let definitions = options.models.filter(|m| !m.is_empty()).unwrap_or_else(|| {
            vec![FauxModelDefinition {
                id: DEFAULT_MODEL_ID.to_owned(),
                name: Some(DEFAULT_MODEL_NAME.to_owned()),
                reasoning: Some(false),
                input: Some(vec![InputModality::Text, InputModality::Image]),
                cost: Some(ModelCostRates::default()),
                context_window: Some(DEFAULT_CONTEXT_WINDOW),
                max_tokens: Some(DEFAULT_MAX_TOKENS),
            }]
        });
        let models = definitions
            .into_iter()
            .map(|d| Model {
                name: d.name.clone().unwrap_or_else(|| d.id.clone()),
                id: d.id,
                api: ApiKind(api.clone()),
                provider: provider.clone(),
                base_url: DEFAULT_BASE_URL.to_owned(),
                reasoning: d.reasoning.unwrap_or(false),
                thinking_level_map: None,
                input: d
                    .input
                    .unwrap_or_else(|| vec![InputModality::Text, InputModality::Image]),
                cost: ModelCost {
                    rates: d.cost.unwrap_or_default(),
                    tiers: None,
                },
                context_window: d.context_window.unwrap_or(DEFAULT_CONTEXT_WINDOW),
                max_tokens: d.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
                headers: None,
                compat: None,
                sampling_params: None,
            })
            .collect();

        Arc::new(Self {
            api,
            provider,
            models,
            tokens_per_second: options.tokens_per_second,
            min_token_size,
            max_token_size,
            inner: Mutex::new(FauxInner {
                pending: std::collections::VecDeque::new(),
                call_count: 0,
                prompt_cache: HashMap::new(),
            }),
        })
    }

    pub fn api(&self) -> &str {
        &self.api
    }

    pub fn provider(&self) -> &str {
        &self.provider
    }

    pub fn models(&self) -> &[Model] {
        &self.models
    }

    /// First registered model (upstream `getModel()`), or by id.
    pub fn get_model(&self, model_id: Option<&str>) -> Option<Model> {
        match model_id {
            None => self.models.first().cloned(),
            Some(id) => self.models.iter().find(|m| m.id == id).cloned(),
        }
    }

    pub fn call_count(&self) -> u64 {
        self.lock().call_count
    }

    pub fn pending_response_count(&self) -> usize {
        self.lock().pending.len()
    }

    pub fn set_responses(&self, responses: Vec<FauxResponseStep>) {
        self.lock().pending = responses.into();
    }

    pub fn append_responses(&self, responses: Vec<FauxResponseStep>) {
        self.lock().pending.extend(responses);
    }

    fn lock(&self) -> MutexGuard<'_, FauxInner> {
        // Invariant: locked sections never panic (pure data manipulation), so
        // the mutex can only be poisoned by an outside abort.
        self.inner.lock().expect("faux mutex poisoned")
    }

    /// The injected stream function (rpi-agent `StreamFn` shape). Requires a
    /// tokio runtime at call time (events are produced on a spawned task).
    pub fn stream_fn(self: &Arc<Self>) -> StreamFn {
        let this = Arc::clone(self);
        Arc::new(
            move |model: Model, context: Context, options: StreamOptions| {
                let this = Arc::clone(&this);
                let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<StreamEvent>();
                tokio::spawn(async move {
                    this.produce(tx, model, context, options).await;
                });
                futures::stream::poll_fn(move |cx| rx.poll_recv(cx)).boxed()
            },
        )
    }

    async fn produce(
        &self,
        tx: tokio::sync::mpsc::UnboundedSender<StreamEvent>,
        model: Model,
        context: Context,
        options: StreamOptions,
    ) {
        if let Some(on_response) = &options.on_response {
            on_response(
                ProviderResponse {
                    status: 200,
                    headers: HashMap::new(),
                },
                &model,
            )
            .await;
        }

        let (step, call_count) = {
            let mut inner = self.lock();
            inner.call_count += 1;
            (inner.pending.pop_front(), inner.call_count)
        };

        let Some(step) = step else {
            let mut message =
                create_error_message(EMPTY_QUEUE_ERROR, &self.api, &self.provider, &model.id);
            message.usage = self.usage_estimate(&message, &context, &options);
            let _ = tx.send(StreamEvent::Error {
                reason: ErrorReason::Error,
                error: message,
            });
            return;
        };

        let mut message = match step {
            FauxResponseStep::Message(m) => *m,
            FauxResponseStep::Factory(f) => {
                f(&context, Some(&options), FauxState { call_count }, &model)
            }
        };
        // cloneMessage: registration identity overrides the scripted one.
        message.api = ApiKind(self.api.clone());
        message.provider = self.provider.clone();
        message.model = model.id.clone();
        message.usage = self.usage_estimate(&message, &context, &options);

        stream_with_deltas(
            &tx,
            message,
            self.min_token_size,
            self.max_token_size,
            self.tokens_per_second,
            &options,
        )
        .await;
    }

    /// Port of upstream `withUsageEstimate`: 4 chars/token estimation plus
    /// prompt-cache simulation keyed by `session_id` (active unless cache
    /// retention is explicitly `"none"`).
    fn usage_estimate(
        &self,
        message: &AssistantMessage,
        context: &Context,
        options: &StreamOptions,
    ) -> Usage {
        let prompt_text = serialize_context(context);
        let prompt_tokens = estimate_tokens(&prompt_text);
        let output_tokens = estimate_tokens(&assistant_content_to_text(&message.content));
        let mut input = prompt_tokens;
        let mut cache_read = 0u64;
        let mut cache_write = 0u64;

        if let Some(session_id) = &options.session_id {
            if options.cache_retention != Some(CacheRetention::None) {
                let mut inner = self.lock();
                match inner.prompt_cache.get(session_id) {
                    Some(previous) => {
                        let cached_chars = common_prefix_len(previous, &prompt_text);
                        cache_read = estimate_tokens(&previous[..cached_chars]);
                        cache_write = estimate_tokens(&prompt_text[cached_chars..]);
                        input = prompt_tokens.saturating_sub(cache_read);
                    }
                    None => {
                        cache_write = prompt_tokens;
                    }
                }
                inner.prompt_cache.insert(session_id.clone(), prompt_text);
            }
        }

        Usage {
            input,
            output: output_tokens,
            cache_read,
            cache_write,
            cache_write1h: None,
            reasoning: None,
            total_tokens: input + output_tokens + cache_read + cache_write,
            cost: UsageCost::default(),
        }
    }
}

// ---------------------------------------------------------------------------
// Upstream helper ports
// ---------------------------------------------------------------------------

/// `estimateTokens`: 4 chars per token, rounded up.
pub fn estimate_tokens(text: &str) -> u64 {
    text.chars().count().div_ceil(4) as u64
}

fn content_to_text(content: &[UserContentBlock]) -> String {
    content
        .iter()
        .map(|block| match block {
            UserContentBlock::Text(t) => t.text.clone(),
            UserContentBlock::Image(ImageContent { data, mime_type }) => {
                format!("[image:{mime_type}:{}]", data.len())
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn assistant_content_to_text(content: &[AssistantContent]) -> String {
    content
        .iter()
        .map(|block| match block {
            AssistantContent::Text(t) => t.text.clone(),
            AssistantContent::Thinking(t) => t.thinking.clone(),
            AssistantContent::ToolCall(call) => {
                let args =
                    serde_json::to_string(&call.arguments).unwrap_or_else(|_| "{}".to_owned());
                format!("{}:{args}", call.name)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn tool_result_to_text(message: &ToolResultMessage) -> String {
    let mut parts = vec![message.tool_name.clone()];
    for block in &message.content {
        parts.push(match block {
            ToolResultContent::Text(t) => t.text.clone(),
            ToolResultContent::Image(ImageContent { data, mime_type }) => {
                format!("[image:{mime_type}:{}]", data.len())
            }
        });
    }
    parts.join("\n")
}

/// Port of upstream `serializeContext` (parity-sensitive: cache usage
/// estimates in session JSONL derive from this exact string shape).
fn serialize_context(context: &Context) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(system) = &context.system_prompt {
        parts.push(format!("system:{system}"));
    }
    for message in &context.messages {
        let text = match message {
            Message::User(u) => match &u.content {
                rpi_ai::types::UserContent::Text(t) => t.clone(),
                rpi_ai::types::UserContent::Blocks(blocks) => content_to_text(blocks),
            },
            Message::Assistant(a) => assistant_content_to_text(&a.content),
            Message::ToolResult(tr) => tool_result_to_text(tr),
        };
        let role = match message.role() {
            rpi_ai::types::Role::User => "user",
            rpi_ai::types::Role::Assistant => "assistant",
            rpi_ai::types::Role::ToolResult => "toolResult",
        };
        parts.push(format!("{role}:{text}"));
    }
    if let Some(tools) = &context.tools {
        if !tools.is_empty() {
            let json = serde_json::to_string(tools).unwrap_or_else(|_| "[]".to_owned());
            parts.push(format!("tools:{json}"));
        }
    }
    parts.join("\n\n")
}

fn common_prefix_len(a: &str, b: &str) -> usize {
    let mut len = 0;
    for ((ia, ca), (ib, cb)) in a.char_indices().zip(b.char_indices()) {
        if ca != cb {
            break;
        }
        debug_assert_eq!(ia, ib, "same prefix implies same byte offsets");
        let _ = ib;
        len = ia + ca.len_utf8();
    }
    len
}

fn create_error_message(
    error: &str,
    api: &str,
    provider: &str,
    model_id: &str,
) -> AssistantMessage {
    AssistantMessage {
        role: AssistantRole::Assistant,
        content: vec![],
        api: ApiKind(api.to_owned()),
        provider: provider.to_owned(),
        model: model_id.to_owned(),
        response_model: None,
        response_id: None,
        diagnostics: None,
        usage: default_usage(),
        stop_reason: StopReason::Error,
        error_message: Some(error.to_owned()),
        timestamp: 0,
        deferred: None,
        end_turn: None,
        raw_stop_reason: None,
    }
}

fn create_aborted_message(partial: &AssistantMessage) -> AssistantMessage {
    let mut m = partial.clone();
    m.stop_reason = StopReason::Aborted;
    m.error_message = Some(ABORTED_ERROR.to_owned());
    m
}

/// Deterministic port of `splitStringByTokenSize`: chunk sizes cycle
/// `min, min+1, …, max` tokens × 4 chars (upstream uses `Math.random`).
fn split_string_by_token_size(
    text: &str,
    min_token_size: usize,
    max_token_size: usize,
) -> Vec<String> {
    let mut chunks = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let mut index = 0;
    let mut token_size = min_token_size;
    while index < chars.len() {
        let char_size = (token_size * 4).max(1);
        let end = (index + char_size).min(chars.len());
        chunks.push(chars[index..end].iter().collect());
        index = end;
        token_size = if token_size >= max_token_size {
            min_token_size
        } else {
            token_size + 1
        };
    }
    if chunks.is_empty() {
        chunks.push(String::new());
    }
    chunks
}

async fn schedule_chunk(chunk: &str, tokens_per_second: Option<f64>) {
    let Some(tps) = tokens_per_second else {
        return;
    };
    if tps <= 0.0 {
        return;
    }
    let delay_ms = (estimate_tokens(chunk) as f64 / tps) * 1000.0;
    tokio::time::sleep(std::time::Duration::from_secs_f64(delay_ms / 1000.0)).await;
}

fn is_aborted(options: &StreamOptions) -> bool {
    options.signal.as_ref().is_some_and(|s| s.is_cancelled())
}

/// Port of upstream `streamWithDeltas`: emits `start` → per-block
/// start/delta/end → terminal `done` or `error`, honoring abort between
/// chunks.
async fn stream_with_deltas(
    tx: &tokio::sync::mpsc::UnboundedSender<StreamEvent>,
    message: AssistantMessage,
    min_token_size: usize,
    max_token_size: usize,
    tokens_per_second: Option<f64>,
    options: &StreamOptions,
) {
    let mut partial = AssistantMessage {
        content: vec![],
        stop_reason: StopReason::Pending,
        ..message.clone()
    };

    macro_rules! abort_if_cancelled {
        () => {
            if is_aborted(options) {
                let aborted = create_aborted_message(&partial);
                let _ = tx.send(StreamEvent::Error {
                    reason: ErrorReason::Aborted,
                    error: aborted,
                });
                return;
            }
        };
    }

    abort_if_cancelled!();
    let _ = tx.send(StreamEvent::Start {
        partial: partial.clone(),
    });

    for (index, block) in message.content.iter().enumerate() {
        abort_if_cancelled!();
        match block {
            AssistantContent::Thinking(t) => {
                partial
                    .content
                    .push(AssistantContent::Thinking(ThinkingContent {
                        thinking: String::new(),
                        thinking_signature: None,
                        redacted: None,
                    }));
                let _ = tx.send(StreamEvent::ThinkingStart {
                    content_index: index,
                    partial: partial.clone(),
                });
                for chunk in split_string_by_token_size(&t.thinking, min_token_size, max_token_size)
                {
                    schedule_chunk(&chunk, tokens_per_second).await;
                    abort_if_cancelled!();
                    if let Some(AssistantContent::Thinking(cur)) = partial.content.get_mut(index) {
                        cur.thinking.push_str(&chunk);
                    }
                    let _ = tx.send(StreamEvent::ThinkingDelta {
                        content_index: index,
                        delta: chunk,
                        partial: partial.clone(),
                    });
                }
                let _ = tx.send(StreamEvent::ThinkingEnd {
                    content_index: index,
                    content: t.thinking.clone(),
                    partial: partial.clone(),
                });
            }
            AssistantContent::Text(t) => {
                partial.content.push(AssistantContent::Text(TextContent {
                    text: String::new(),
                    text_signature: None,
                }));
                let _ = tx.send(StreamEvent::TextStart {
                    content_index: index,
                    partial: partial.clone(),
                });
                for chunk in split_string_by_token_size(&t.text, min_token_size, max_token_size) {
                    schedule_chunk(&chunk, tokens_per_second).await;
                    abort_if_cancelled!();
                    if let Some(AssistantContent::Text(cur)) = partial.content.get_mut(index) {
                        cur.text.push_str(&chunk);
                    }
                    let _ = tx.send(StreamEvent::TextDelta {
                        content_index: index,
                        delta: chunk,
                        partial: partial.clone(),
                    });
                }
                let _ = tx.send(StreamEvent::TextEnd {
                    content_index: index,
                    content: t.text.clone(),
                    partial: partial.clone(),
                });
            }
            AssistantContent::ToolCall(call) => {
                partial.content.push(AssistantContent::ToolCall(ToolCall {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    arguments: serde_json::Map::new(),
                    thought_signature: None,
                    namespace: None,
                }));
                let _ = tx.send(StreamEvent::ToolCallStart {
                    content_index: index,
                    partial: partial.clone(),
                });
                let args_json =
                    serde_json::to_string(&call.arguments).unwrap_or_else(|_| "{}".to_owned());
                for chunk in split_string_by_token_size(&args_json, min_token_size, max_token_size)
                {
                    schedule_chunk(&chunk, tokens_per_second).await;
                    abort_if_cancelled!();
                    let _ = tx.send(StreamEvent::ToolCallDelta {
                        content_index: index,
                        delta: chunk,
                        partial: partial.clone(),
                    });
                }
                if let Some(AssistantContent::ToolCall(cur)) = partial.content.get_mut(index) {
                    cur.arguments = call.arguments.clone();
                }
                let _ = tx.send(StreamEvent::ToolCallEnd {
                    content_index: index,
                    tool_call: call.clone(),
                    partial: partial.clone(),
                });
            }
        }
    }

    // `Deferred` shares the `Pending` path (R2.1.1): the faux provider never
    // produces it, but if a scripted response carried it, it is an incomplete
    // response just like a missing stop reason.
    if matches!(
        message.stop_reason,
        StopReason::Pending | StopReason::Deferred
    ) {
        let err = create_error_message(
            "Faux response ended without a stop reason",
            message.api.as_str(),
            &message.provider,
            &message.model,
        );
        let _ = tx.send(StreamEvent::Error {
            reason: ErrorReason::Error,
            error: err,
        });
        return;
    }
    match message.stop_reason {
        StopReason::Error | StopReason::Aborted => {
            let reason = if message.stop_reason == StopReason::Aborted {
                ErrorReason::Aborted
            } else {
                ErrorReason::Error
            };
            let _ = tx.send(StreamEvent::Error {
                reason,
                error: message,
            });
        }
        StopReason::Stop | StopReason::Length | StopReason::ToolUse => {
            let reason = match message.stop_reason {
                StopReason::Stop => DoneReason::Stop,
                StopReason::Length => DoneReason::Length,
                StopReason::ToolUse => DoneReason::ToolUse,
                _ => unreachable!("pending/error/aborted handled above"),
            };
            let _ = tx.send(StreamEvent::Done { reason, message });
        }
        StopReason::Pending | StopReason::Deferred => {
            unreachable!("pending/deferred handled above")
        }
    }
}

// ---------------------------------------------------------------------------
// rpi-ai `Provider` adapter
// ---------------------------------------------------------------------------

/// Static API-key auth for the faux provider: always resolves a dummy key so
/// `ModelRuntime` availability/auth checks pass in tests.
struct FauxApiKeyAuth {
    name: String,
}

#[async_trait::async_trait]
impl rpi_ai::auth::ApiKeyAuth for FauxApiKeyAuth {
    fn name(&self) -> &str {
        &self.name
    }

    async fn resolve(
        &self,
        _ctx: &dyn rpi_ai::auth::AuthContext,
        _credential: Option<&rpi_ai::auth::ApiKeyCredential>,
    ) -> Result<Option<rpi_ai::auth::AuthResult>, rpi_ai::auth::ModelsError> {
        Ok(Some(rpi_ai::auth::AuthResult {
            auth: rpi_ai::auth::ModelAuth {
                api_key: Some("faux-key".to_owned()),
                headers: None,
                base_url: None,
            },
            env: None,
            source: Some("FAUX_API_KEY".to_owned()),
        }))
    }
}

/// Adapter exposing a [`FauxProvider`] as a rpi-ai [`rpi_ai::models::Provider`]
/// so tests can register it into a `ModelRuntime` and drive a full
/// `AgentSession` / headless mode end to end with scripted responses.
pub struct FauxAiProvider {
    faux: Arc<FauxProvider>,
    provider_auth: rpi_ai::auth::ProviderAuth,
    /// Reasoning levels seen at the `stream_simple` boundary (each stream
    /// call appends `Some(level)` or `None`), in call order. Lets tests
    /// assert the agent path forwards the session thinking level into the
    /// provider request (sdk.rs `stream_simple` wiring).
    reasoning_seen: Arc<Mutex<Vec<Option<rpi_ai::types::ThinkingLevel>>>>,
}

impl FauxAiProvider {
    pub fn new(faux: Arc<FauxProvider>) -> Self {
        FauxAiProvider {
            provider_auth: rpi_ai::auth::ProviderAuth {
                api_key: Some(Arc::new(FauxApiKeyAuth {
                    name: format!("{} API key", faux.provider()),
                })),
                oauth: None,
            },
            faux,
            reasoning_seen: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// The underlying scripted provider (response queue, counters).
    pub fn faux(&self) -> &Arc<FauxProvider> {
        &self.faux
    }

    /// Reasoning levels received by [`Self::stream_simple`], in call order.
    pub fn reasoning_seen(&self) -> &Arc<Mutex<Vec<Option<rpi_ai::types::ThinkingLevel>>>> {
        &self.reasoning_seen
    }
}

impl rpi_ai::models::Provider for FauxAiProvider {
    fn id(&self) -> &str {
        self.faux.provider()
    }

    fn name(&self) -> &str {
        self.faux.provider()
    }

    fn base_url(&self) -> Option<&str> {
        Some(DEFAULT_BASE_URL)
    }

    fn headers(&self) -> Option<&rpi_ai::types::ProviderHeaders> {
        None
    }

    fn auth(&self) -> &rpi_ai::auth::ProviderAuth {
        &self.provider_auth
    }

    fn get_models(&self) -> Vec<Model> {
        self.faux.models().to_vec()
    }

    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: Option<StreamOptions>,
    ) -> rpi_ai::utils::event_stream::AssistantMessageEventStream {
        let stream = rpi_ai::utils::event_stream::AssistantMessageEventStream::new();
        let producer = stream.clone();
        let stream_fn = self.faux.stream_fn();
        let model = model.clone();
        let context = context.clone();
        let options = options.unwrap_or_default();
        tokio::spawn(async move {
            let mut events = stream_fn(model, context, options);
            while let Some(event) = events.next().await {
                producer.push(event);
            }
            producer.end(None);
        });
        stream
    }

    fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: Option<rpi_ai::types::SimpleStreamOptions>,
    ) -> rpi_ai::utils::event_stream::AssistantMessageEventStream {
        self.reasoning_seen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(options.as_ref().and_then(|simple| simple.reasoning));
        self.stream(model, context, options.map(|simple| simple.stream))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_context(text: &str) -> Context {
        Context {
            system_prompt: None,
            messages: vec![Message::User(rpi_ai::types::UserMessage {
                role: rpi_ai::types::UserRole::User,
                content: rpi_ai::types::UserContent::Text(text.to_owned()),
                timestamp: 0,
            })],
            tools: None,
        }
    }

    async fn collect(
        stream: rpi_agent::stream_fn::BoxStream<'static, StreamEvent>,
    ) -> Vec<StreamEvent> {
        stream.collect().await
    }

    #[tokio::test]
    async fn test_faux_streams_deterministic_event_sequence() {
        let provider = FauxProvider::new(FauxProviderOptions::default());
        provider.set_responses(vec![faux_assistant_message(
            vec![
                faux_thinking("hmm"),
                faux_text("hello world"),
                faux_tool_call(
                    "read",
                    serde_json::json!({"path": "a.txt"})
                        .as_object()
                        .unwrap()
                        .clone(),
                    Some("call_1".to_owned()),
                ),
            ],
            FauxAssistantOptions {
                stop_reason: Some(StopReason::ToolUse),
                ..Default::default()
            },
        )
        .into()]);

        let model = provider.get_model(None).unwrap();
        let stream = provider.stream_fn()(model, user_context("hi"), StreamOptions::default());
        let events = collect(stream).await;

        let types: Vec<&str> = events.iter().map(crate::diff::event_type_name).collect();
        assert_eq!(types[0], "start");
        assert_eq!(types.last().copied(), Some("done"));
        assert!(types.contains(&"thinking_start"));
        assert!(types.contains(&"text_delta"));
        assert!(types.contains(&"toolcall_end"));
        // Deterministic: same script twice yields identical sequences.
        provider.set_responses(vec![faux_assistant_message(
            "hello world",
            Default::default(),
        )
        .into()]);
        let model = provider.get_model(None).unwrap();
        let s1 = collect(provider.stream_fn()(
            model.clone(),
            user_context("hi"),
            StreamOptions::default(),
        ))
        .await;
        provider.set_responses(vec![faux_assistant_message(
            "hello world",
            Default::default(),
        )
        .into()]);
        let s2 = collect(provider.stream_fn()(
            model,
            user_context("hi"),
            StreamOptions::default(),
        ))
        .await;
        crate::diff::diff_events_normalized(&s1, &s2).unwrap();
        assert_eq!(provider.call_count(), 3);
    }

    #[tokio::test]
    async fn test_faux_empty_queue_fixed_error() {
        let provider = FauxProvider::new(FauxProviderOptions::default());
        let model = provider.get_model(None).unwrap();
        let events = collect(provider.stream_fn()(
            model,
            user_context("hi"),
            StreamOptions::default(),
        ))
        .await;
        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::Error { reason, error } => {
                assert_eq!(*reason, ErrorReason::Error);
                assert_eq!(error.error_message.as_deref(), Some(EMPTY_QUEUE_ERROR));
                assert_eq!(error.stop_reason, StopReason::Error);
            }
            other => panic!("expected error event, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_faux_usage_estimate_and_cache_simulation() {
        let provider = FauxProvider::new(FauxProviderOptions::default());
        provider.set_responses(vec![
            faux_assistant_message("abcd", Default::default()).into(),
            faux_assistant_message("abcd", Default::default()).into(),
        ]);
        let model = provider.get_model(None).unwrap();
        let options = StreamOptions {
            session_id: Some("s1".to_owned()),
            ..Default::default()
        };

        let e1 = collect(provider.stream_fn()(
            model.clone(),
            user_context("12345678"),
            options.clone(),
        ))
        .await;
        let StreamEvent::Done { message: m1, .. } = e1.last().unwrap() else {
            panic!("expected done");
        };
        // First call: no cache history → everything is cache_write.
        // prompt "user:12345678" = 13 chars → ceil(13/4) = 4 tokens.
        assert_eq!(m1.usage.cache_write, 4);
        assert_eq!(m1.usage.cache_read, 0);
        assert_eq!(m1.usage.input, 4);
        assert_eq!(m1.usage.output, 1); // "abcd" = 4 chars → 1 token

        let e2 = collect(provider.stream_fn()(
            model,
            user_context("12345678"),
            options,
        ))
        .await;
        let StreamEvent::Done { message: m2, .. } = e2.last().unwrap() else {
            panic!("expected done");
        };
        // Second call with identical prompt: full prefix cached.
        assert_eq!(m2.usage.cache_read, 4);
        assert_eq!(m2.usage.cache_write, 0);
        assert_eq!(m2.usage.input, 0);
    }

    #[tokio::test]
    async fn test_faux_cache_disabled_with_retention_none() {
        let provider = FauxProvider::new(FauxProviderOptions::default());
        provider.set_responses(vec![faux_assistant_message("x", Default::default()).into()]);
        let model = provider.get_model(None).unwrap();
        let options = StreamOptions {
            session_id: Some("s1".to_owned()),
            cache_retention: Some(CacheRetention::None),
            ..Default::default()
        };
        let events = collect(provider.stream_fn()(
            model,
            user_context("12345678"),
            options,
        ))
        .await;
        let StreamEvent::Done { message, .. } = events.last().unwrap() else {
            panic!("expected done");
        };
        assert_eq!(message.usage.cache_read, 0);
        assert_eq!(message.usage.cache_write, 0);
        assert_eq!(message.usage.input, 4);
    }

    #[tokio::test]
    async fn test_faux_abort_before_start() {
        let provider = FauxProvider::new(FauxProviderOptions::default());
        provider.set_responses(vec![faux_assistant_message("hi", Default::default()).into()]);
        let model = provider.get_model(None).unwrap();
        let token = tokio_util::sync::CancellationToken::new();
        token.cancel();
        let options = StreamOptions {
            request: rpi_ai::ProviderRequestOptions {
                signal: Some(token),
                ..Default::default()
            },
            ..Default::default()
        };
        let events = collect(provider.stream_fn()(model, user_context("hi"), options)).await;
        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::Error { reason, error } => {
                assert_eq!(*reason, ErrorReason::Aborted);
                assert_eq!(error.stop_reason, StopReason::Aborted);
                assert_eq!(error.error_message.as_deref(), Some(ABORTED_ERROR));
            }
            other => panic!("expected aborted error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_faux_tokens_per_second_paces_deltas() {
        // 100 output tokens at 500 tokens/second ≈ 200ms total pacing.
        let provider = FauxProvider::new(FauxProviderOptions {
            tokens_per_second: Some(500.0),
            ..Default::default()
        });
        let long_text = "abcd".repeat(100); // 400 chars = 100 tokens
        provider.set_responses(vec![
            faux_assistant_message(long_text, Default::default()).into()
        ]);
        let model = provider.get_model(None).unwrap();
        let started = std::time::Instant::now();
        let events = collect(provider.stream_fn()(
            model,
            user_context("hi"),
            StreamOptions::default(),
        ))
        .await;
        let elapsed = started.elapsed();
        assert!(matches!(events.last(), Some(StreamEvent::Done { .. })));
        assert!(
            elapsed >= std::time::Duration::from_millis(150),
            "pacing should delay streaming, took {elapsed:?}"
        );
        // tokens_per_second = 0 disables pacing entirely (upstream semantics).
        let provider = FauxProvider::new(FauxProviderOptions {
            tokens_per_second: Some(0.0),
            ..Default::default()
        });
        provider.set_responses(vec![faux_assistant_message(
            "x".repeat(400),
            Default::default(),
        )
        .into()]);
        let model = provider.get_model(None).unwrap();
        let started = std::time::Instant::now();
        let _ = collect(provider.stream_fn()(
            model,
            user_context("hi"),
            StreamOptions::default(),
        ))
        .await;
        assert!(started.elapsed() < std::time::Duration::from_millis(100));
    }

    #[tokio::test]
    async fn test_faux_factory_receives_context_and_state() {
        let provider = FauxProvider::new(FauxProviderOptions::default());
        provider.set_responses(vec![FauxResponseStep::Factory(Box::new(
            |context, _options, state, model| {
                assert_eq!(state.call_count, 1);
                assert_eq!(model.id, DEFAULT_MODEL_ID);
                let text = match &context.messages[0] {
                    Message::User(u) => match &u.content {
                        rpi_ai::types::UserContent::Text(t) => t.clone(),
                        _ => panic!("expected text"),
                    },
                    _ => panic!("expected user message"),
                };
                faux_assistant_message(format!("echo:{text}"), Default::default())
            },
        ))]);
        let model = provider.get_model(None).unwrap();
        let events = collect(provider.stream_fn()(
            model,
            user_context("ping"),
            StreamOptions::default(),
        ))
        .await;
        let StreamEvent::Done { message, .. } = events.last().unwrap() else {
            panic!("expected done");
        };
        match &message.content[0] {
            AssistantContent::Text(t) => assert_eq!(t.text, "echo:ping"),
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_faux_error_stop_reason_streams_error_event() {
        let provider = FauxProvider::new(FauxProviderOptions::default());
        provider.set_responses(vec![faux_assistant_message(
            "",
            FauxAssistantOptions {
                stop_reason: Some(StopReason::Error),
                error_message: Some("boom".to_owned()),
                ..Default::default()
            },
        )
        .into()]);
        let model = provider.get_model(None).unwrap();
        let events = collect(provider.stream_fn()(
            model,
            user_context("hi"),
            StreamOptions::default(),
        ))
        .await;
        match events.last().unwrap() {
            StreamEvent::Error { reason, error } => {
                assert_eq!(*reason, ErrorReason::Error);
                assert_eq!(error.error_message.as_deref(), Some("boom"));
            }
            other => panic!("expected error, got {other:?}"),
        }
    }

    #[test]
    fn test_split_string_deterministic_cycle() {
        let chunks = split_string_by_token_size(&"a".repeat(40), 1, 2);
        // sizes cycle 4, 8, 4, 8, … chars
        let sizes: Vec<usize> = chunks.iter().map(|c| c.chars().count()).collect();
        assert_eq!(sizes, vec![4, 8, 4, 8, 4, 8, 4]);
    }

    #[test]
    fn test_common_prefix_len_multibyte() {
        assert_eq!(common_prefix_len("héllo", "hélà"), 4); // 'h','é','l' = 1+2+1 bytes
    }
}
