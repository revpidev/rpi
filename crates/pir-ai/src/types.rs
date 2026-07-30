//! Port of `packages/ai/src/types.ts` @ pi 0.82.1 (2efa728).
//!
//! Core wire types: messages, content blocks, usage, tools, models (with
//! compat matrices), stream options and the assistant stream event protocol.
//!
//! Intentional differences:
//! - TS `Api = KnownApi | (string & {})` becomes the [`ApiKind`] newtype with
//!   associated constants for the known APIs (wire-compatible, open to custom
//!   API strings like upstream).
//! - The per-API conditional `Model.compat` type becomes the single flat
//!   [`ModelCompat`] struct merging `OpenAICompletionsCompat`,
//!   `OpenAIResponsesCompat`, `AnthropicMessagesCompat` and `BedrockCompat`.
//!   All overlapping field names share the same type upstream, so the merged
//!   struct is wire-compatible in both directions; which fields are meaningful
//!   is governed by `Model.api` (enforced by the adapters, not the type).
//! - `AbortSignal` becomes `tokio_util::sync::CancellationToken`
//!   (coding-standards §6.4).
//! - `onPayload` / `onResponse` callbacks become `Arc<dyn Fn>` returning boxed
//!   futures; they are absent from serialized form (StreamOptions is internal,
//!   never on the wire).

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// Api / Provider identifiers
// ---------------------------------------------------------------------------

/// API kind identifier. Mirrors `Api = KnownApi | (string & {})`: an open
/// string with associated constants for the ten known APIs (requirements §5.1).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ApiKind(pub String);

impl ApiKind {
    pub const OPENAI_COMPLETIONS: &'static str = "openai-completions";
    pub const MISTRAL_CONVERSATIONS: &'static str = "mistral-conversations";
    pub const OPENAI_RESPONSES: &'static str = "openai-responses";
    pub const AZURE_OPENAI_RESPONSES: &'static str = "azure-openai-responses";
    pub const OPENAI_CODEX_RESPONSES: &'static str = "openai-codex-responses";
    pub const ANTHROPIC_MESSAGES: &'static str = "anthropic-messages";
    pub const BEDROCK_CONVERSE_STREAM: &'static str = "bedrock-converse-stream";
    pub const GOOGLE_GENERATIVE_AI: &'static str = "google-generative-ai";
    pub const GOOGLE_VERTEX: &'static str = "google-vertex";
    pub const PI_MESSAGES: &'static str = "pi-messages";

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ApiKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for ApiKind {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

impl From<String> for ApiKind {
    fn from(s: String) -> Self {
        Self(s)
    }
}

// ---------------------------------------------------------------------------
// Thinking levels
// ---------------------------------------------------------------------------

/// `ThinkingLevel = "minimal" | "low" | "medium" | "high" | "xhigh" | "max"`.
///
/// Note: "xhigh" and "max" are only supported by selected model families; use
/// [`Model::thinking_level_map`] to detect support for a concrete model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum ThinkingLevel {
    #[serde(rename = "minimal")]
    Minimal,
    #[serde(rename = "low")]
    Low,
    #[serde(rename = "medium")]
    Medium,
    #[serde(rename = "high")]
    High,
    #[serde(rename = "xhigh")]
    Xhigh,
    #[serde(rename = "max")]
    Max,
}

/// `ModelThinkingLevel = "off" | ThinkingLevel`.
///
/// This is also the agent-side `ThinkingLevel` of `packages/agent` (which
/// includes "off"); `pir-agent` re-exports it under that name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum ModelThinkingLevel {
    #[serde(rename = "off")]
    Off,
    #[serde(rename = "minimal")]
    Minimal,
    #[serde(rename = "low")]
    Low,
    #[serde(rename = "medium")]
    Medium,
    #[serde(rename = "high")]
    High,
    #[serde(rename = "xhigh")]
    Xhigh,
    #[serde(rename = "max")]
    Max,
}

/// `ThinkingLevelMap = Partial<Record<ModelThinkingLevel, string | null>>`.
///
/// Three-state per level: key absent = provider default, `None` (JSON null) =
/// level unsupported, `Some(value)` = provider/model-specific mapped value.
pub type ThinkingLevelMap = std::collections::BTreeMap<ModelThinkingLevel, Option<String>>;

/// `ChatTemplateKwargValue`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ChatTemplateKwargValue {
    /// `{ $var: "thinking.enabled" | "thinking.effort", omitWhenOff?: boolean }`
    Var(ChatTemplateKwargVar),
    /// Plain `string | number | boolean | null`.
    Scalar(Value),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatTemplateKwargVar {
    #[serde(rename = "$var")]
    pub var: ChatTemplateKwargVarKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub omit_when_off: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChatTemplateKwargVarKind {
    #[serde(rename = "thinking.enabled")]
    ThinkingEnabled,
    #[serde(rename = "thinking.effort")]
    ThinkingEffort,
}

/// Token budgets for each thinking level (token-based providers only).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ThinkingBudgets {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub minimal: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub low: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub medium: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub high: Option<u32>,
}

// ---------------------------------------------------------------------------
// Stream options
// ---------------------------------------------------------------------------

/// `CacheRetention = "none" | "short" | "long"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CacheRetention {
    #[serde(rename = "none")]
    None,
    #[serde(rename = "short")]
    Short,
    #[serde(rename = "long")]
    Long,
}

/// `Transport = "sse" | "websocket" | "websocket-cached" | "auto"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Transport {
    #[serde(rename = "sse")]
    Sse,
    #[serde(rename = "websocket")]
    Websocket,
    #[serde(rename = "websocket-cached")]
    WebsocketCached,
    #[serde(rename = "auto")]
    Auto,
}

/// `SessionAffinityFormat = "openai" | "openai-nosession" | "openrouter"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionAffinityFormat {
    #[serde(rename = "openai")]
    Openai,
    #[serde(rename = "openai-nosession")]
    OpenaiNosession,
    #[serde(rename = "openrouter")]
    Openrouter,
}

/// Provider-scoped environment overrides. Values take precedence over process.env.
pub type ProviderEnv = HashMap<String, String>;
/// Header map; a `None` value suppresses a provider/API default header of the
/// same name.
pub type ProviderHeaders = HashMap<String, Option<String>>;

/// `ProviderResponse` — passed to the `on_response` callback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderResponse {
    pub status: u16,
    pub headers: HashMap<String, String>,
}

/// `onPayload` callback: inspect or replace a provider payload before sending.
/// Returning `None` keeps the payload unchanged.
pub type OnPayloadCallback =
    Arc<dyn Fn(Value, &Model) -> Pin<Box<dyn Future<Output = Option<Value>> + Send>> + Send + Sync>;

/// `onResponse` callback: invoked after an HTTP response is received and
/// before its body stream is consumed.
pub type OnResponseCallback =
    Arc<dyn Fn(ProviderResponse, &Model) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// `StreamOptions` — base options all providers share.
///
/// Internal type (never serialized); field docs mirror upstream.
#[derive(Clone, Default)]
pub struct StreamOptions {
    pub temperature: Option<f64>,
    pub max_tokens: Option<u32>,
    pub signal: Option<CancellationToken>,
    pub api_key: Option<String>,
    /// Preferred transport for providers that support multiple transports.
    pub transport: Option<Transport>,
    /// Prompt cache retention preference. Default: `Short`.
    pub cache_retention: Option<CacheRetention>,
    /// Optional session identifier for providers that support session-based caching.
    pub session_id: Option<String>,
    pub on_payload: Option<OnPayloadCallback>,
    pub on_response: Option<OnResponseCallback>,
    /// Custom HTTP headers, merged over provider defaults; `None` suppresses a
    /// default header. On Bedrock, reserved headers (`x-amz-*`,
    /// `authorization`, `host`) are silently ignored to preserve SigV4.
    pub headers: Option<ProviderHeaders>,
    /// HTTP request timeout in milliseconds.
    pub timeout_ms: Option<u64>,
    /// WebSocket connect (handshake) timeout in milliseconds.
    pub websocket_connect_timeout_ms: Option<u64>,
    /// Maximum client-side retry attempts.
    pub max_retries: Option<u32>,
    /// Maximum delay (ms) to wait for a server-requested long retry wait.
    /// Default: 60000; 0 disables the cap.
    pub max_retry_delay_ms: Option<u64>,
    /// Optional metadata; providers extract the fields they understand.
    pub metadata: Option<serde_json::Map<String, Value>>,
    /// Provider-scoped environment values, taking precedence over process.env.
    pub env: Option<ProviderEnv>,
}

// Manual Debug: the `on_payload` / `on_response` trait objects are not Debug.
// `api_key` and header values are redacted — credentials must never appear in
// Debug output (coding-standards §11.1/§11.2).
impl fmt::Debug for StreamOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StreamOptions")
            .field("temperature", &self.temperature)
            .field("max_tokens", &self.max_tokens)
            .field("signal", &self.signal)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("transport", &self.transport)
            .field("cache_retention", &self.cache_retention)
            .field("session_id", &self.session_id)
            .field(
                "on_payload",
                &self.on_payload.as_ref().map(|_| "<callback>"),
            )
            .field(
                "on_response",
                &self.on_response.as_ref().map(|_| "<callback>"),
            )
            .field(
                "headers",
                &self.headers.as_ref().map(|h| h.keys().collect::<Vec<_>>()),
            )
            .field("timeout_ms", &self.timeout_ms)
            .field(
                "websocket_connect_timeout_ms",
                &self.websocket_connect_timeout_ms,
            )
            .field("max_retries", &self.max_retries)
            .field("max_retry_delay_ms", &self.max_retry_delay_ms)
            .field("metadata", &self.metadata)
            .field("env", &self.env)
            .finish()
    }
}

/// `SimpleStreamOptions extends StreamOptions` — unified options with
/// reasoning, passed to `stream_simple` / `complete_simple`.
#[derive(Debug, Clone, Default)]
pub struct SimpleStreamOptions {
    pub stream: StreamOptions,
    pub reasoning: Option<ThinkingLevel>,
    /// Custom token budgets for thinking levels (token-based providers only).
    pub thinking_budgets: Option<ThinkingBudgets>,
}

// ---------------------------------------------------------------------------
// Content blocks and messages
// ---------------------------------------------------------------------------

/// Role of a [`Message`]. The per-struct `role` fields use single-variant
/// marker enums ([`UserRole`] etc.) so each message struct serializes with its
/// literal tag standalone (upstream includes `role` in every serialized
/// message object); this type is the ergonomic sum used by accessors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Role {
    #[serde(rename = "user")]
    User,
    #[serde(rename = "assistant")]
    Assistant,
    #[serde(rename = "toolResult")]
    ToolResult,
}

/// Role marker for [`UserMessage`] (`role: "user"`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum UserRole {
    #[default]
    #[serde(rename = "user")]
    User,
}

/// Role marker for [`AssistantMessage`] (`role: "assistant"`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum AssistantRole {
    #[default]
    #[serde(rename = "assistant")]
    Assistant,
}

/// Role marker for [`ToolResultMessage`] (`role: "toolResult"`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolResultRole {
    #[default]
    #[serde(rename = "toolResult")]
    ToolResult,
}

/// `TextSignatureV1` — structured payload that may appear inside
/// [`TextContent::text_signature`] as a JSON string (OpenAI responses message
/// metadata; legacy values are plain id strings).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextSignatureV1 {
    pub v: u8,
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<TextSignaturePhase>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TextSignaturePhase {
    #[serde(rename = "commentary")]
    Commentary,
    #[serde(rename = "final_answer")]
    FinalAnswer,
}

/// `TextContent`. The `type: "text"` tag is supplied by the enclosing
/// content-block enum during serialization.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextContent {
    pub text: String,
    /// e.g. for OpenAI responses, message metadata (legacy id string or
    /// [`TextSignatureV1`] JSON).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_signature: Option<String>,
}

/// `ThinkingContent`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThinkingContent {
    pub thinking: String,
    /// e.g. for OpenAI responses, the reasoning item ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_signature: Option<String>,
    /// When true, the thinking content was redacted by safety filters. The
    /// opaque encrypted payload is stored in `thinking_signature` so it can be
    /// passed back to the API for multi-turn continuity.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redacted: Option<bool>,
}

/// `ImageContent`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageContent {
    /// Base64 encoded image data.
    pub data: String,
    /// e.g. "image/jpeg", "image/png".
    pub mime_type: String,
}

/// `ToolCall`. The `type: "toolCall"` tag is supplied by the enclosing
/// content-block enum; use [`tagged_tool_call`] when a standalone `ToolCall`
/// must serialize with its tag (e.g. inside [`StreamEvent::ToolCallEnd`]).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Map<String, Value>,
    /// Google-specific: opaque signature for reusing thought context.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thought_signature: Option<String>,
}

/// Serde `with` module that serializes a [`ToolCall`] with its upstream
/// `type: "toolCall"` tag (for positions where a bare ToolCall object is on
/// the wire, e.g. the `toolcall_end` stream event).
pub mod tagged_tool_call {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use super::ToolCall;

    #[derive(Serialize, Deserialize)]
    struct Tagged {
        #[serde(rename = "type")]
        kind: ToolCallTag,
        #[serde(flatten)]
        call: ToolCall,
    }

    #[derive(Serialize, Deserialize)]
    enum ToolCallTag {
        #[serde(rename = "toolCall")]
        ToolCall,
    }

    pub fn serialize<S: Serializer>(call: &ToolCall, serializer: S) -> Result<S::Ok, S::Error> {
        Tagged {
            kind: ToolCallTag::ToolCall,
            call: call.clone(),
        }
        .serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<ToolCall, D::Error> {
        Ok(Tagged::deserialize(deserializer)?.call)
    }
}

/// `AssistantMessage["content"][number]` = Text | Thinking | ToolCall.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum AssistantContent {
    Text(TextContent),
    Thinking(ThinkingContent),
    ToolCall(ToolCall),
}

/// `UserMessage` content block = Text | Image.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum UserContentBlock {
    Text(TextContent),
    Image(ImageContent),
}

/// `ToolResultMessage["content"][number]` = Text | Image.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ToolResultContent {
    Text(TextContent),
    Image(ImageContent),
}

/// `UserMessage["content"]` = `string | (TextContent | ImageContent)[]`.
/// Also used for `CustomMessage.content` and `CustomMessageEntry.content`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum UserContent {
    Text(String),
    Blocks(Vec<UserContentBlock>),
}

/// `Usage`. Token counts are integers upstream; costs are dollars (floats).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    /// Subset of `cache_write` written with 1h retention. Only Anthropic
    /// reports this split.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write1h: Option<u64>,
    /// Reasoning/thinking tokens when the provider reports them; a subset of
    /// `output`. Absent when the provider exposes no reasoning breakdown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<u64>,
    pub total_tokens: u64,
    pub cost: UsageCost,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageCost {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub total: f64,
}

/// `StopReason = "pending" | "stop" | "length" | "toolUse" | "error" | "aborted"`.
///
/// `pending` exists in the type but is transient only: it is never persisted
/// to session JSONL (only `message_end` persists, and partials never produce
/// `message_end`) — requirements §4.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StopReason {
    #[serde(rename = "pending")]
    Pending,
    #[serde(rename = "stop")]
    Stop,
    #[serde(rename = "length")]
    Length,
    #[serde(rename = "toolUse")]
    ToolUse,
    #[serde(rename = "error")]
    Error,
    #[serde(rename = "aborted")]
    Aborted,
}

/// `UserMessage`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserMessage {
    pub role: UserRole,
    pub content: UserContent,
    /// Unix timestamp in milliseconds.
    pub timestamp: i64,
}

/// `AssistantMessage`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantMessage {
    pub role: AssistantRole,
    pub content: Vec<AssistantContent>,
    pub api: ApiKind,
    pub provider: String,
    pub model: String,
    /// Concrete `chunk.model` when different from the requested `model`
    /// (e.g. OpenRouter `auto` -> `anthropic/...`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_model: Option<String>,
    /// Provider-specific response/message identifier when exposed upstream.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
    /// Redacted provider/runtime diagnostics for failures and recoveries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnostics: Option<Vec<AssistantMessageDiagnostic>>,
    pub usage: Usage,
    pub stop_reason: StopReason,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    /// Unix timestamp in milliseconds.
    pub timestamp: i64,
}

/// `ToolResultMessage`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultMessage {
    pub role: ToolResultRole,
    pub tool_call_id: String,
    pub tool_name: String,
    /// Supports text and images.
    pub content: Vec<ToolResultContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    /// Usage from the tool execution itself, if available. Not part of main
    /// LLM context accounting.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// Names from `Context.tools` that became available after this result
    /// (deferred tool loading load point).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub added_tool_names: Option<Vec<String>>,
    pub is_error: bool,
    /// Unix timestamp in milliseconds.
    pub timestamp: i64,
}

/// `Message = UserMessage | AssistantMessage | ToolResultMessage`.
///
/// Untagged: the per-struct role markers disambiguate the variants.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Message {
    User(UserMessage),
    Assistant(AssistantMessage),
    ToolResult(ToolResultMessage),
}

impl Message {
    pub fn role(&self) -> Role {
        match self {
            Message::User(_) => Role::User,
            Message::Assistant(_) => Role::Assistant,
            Message::ToolResult(_) => Role::ToolResult,
        }
    }
}

// ---------------------------------------------------------------------------
// Diagnostics (packages/ai/src/utils/diagnostics.ts)
// ---------------------------------------------------------------------------

/// `AssistantMessageDiagnostic` (see `utils/diagnostics.ts`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssistantMessageDiagnostic {
    #[serde(rename = "type")]
    pub kind: String,
    pub timestamp: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<DiagnosticErrorInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Map<String, Value>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiagnosticErrorInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stack: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<NumberOrString>,
}

/// JSON `number | string` union (used by diagnostic error codes and OpenRouter
/// routing prices).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum NumberOrString {
    Number(f64),
    String(String),
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

/// OpenAI grammar variants for constrained sampling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum GrammarFormat {
    #[serde(rename = "openai_lark")]
    OpenaiLark,
    #[serde(rename = "openai_regex")]
    OpenaiRegex,
}

/// `GrammarVariants = Partial<Record<GrammarFormat, string>>`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrammarVariants {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub openai_lark: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub openai_regex: Option<String>,
}

/// `ConstrainedSamplingConfig`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ConstrainedSamplingConfig {
    /// Roughly maps to the concept of `strict` in APIs which is implemented as
    /// json-schema constrained sampling.
    JsonSchema { strict: ConstrainedSamplingStrict },
    /// Provider-specific encodings of the same intended language.
    Grammar { variants: GrammarVariants },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConstrainedSamplingStrict {
    #[serde(rename = "prefer")]
    Prefer,
    #[serde(rename = "require")]
    Require,
}

/// `Tool.constrainedSampling?: false | ConstrainedSamplingConfig`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ConstrainedSampling {
    Config(ConstrainedSamplingConfig),
    Disabled(bool),
}

/// `Tool`. `parameters` is a JSON Schema object (TypeBox upstream).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tool {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub constrained_sampling: Option<ConstrainedSampling>,
}

/// `Context`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Context {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    pub messages: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Tool>>,
}

// ---------------------------------------------------------------------------
// Stream events (AssistantMessageEvent)
// ---------------------------------------------------------------------------

/// `Extract<StopReason, "stop" | "length" | "toolUse">` — reason of a `done` event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DoneReason {
    #[serde(rename = "stop")]
    Stop,
    #[serde(rename = "length")]
    Length,
    #[serde(rename = "toolUse")]
    ToolUse,
}

/// `Extract<StopReason, "aborted" | "error">` — reason of an `error` event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorReason {
    #[serde(rename = "aborted")]
    Aborted,
    #[serde(rename = "error")]
    Error,
}

/// `AssistantMessageEvent` — event protocol for the assistant message event
/// stream. Called `StreamEvent` in pir (design §3.2, coding-standards §4.1).
///
/// Streams emit `Start` before partial updates, then terminate with either
/// `Done` carrying the final successful message, or `Error` carrying the final
/// message with stopReason "error"/"aborted" and errorMessage.
///
/// Contract: events for different content blocks may interleave; consumers
/// correlate by `content_index` (requirements §4.2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum StreamEvent {
    Start {
        partial: AssistantMessage,
    },
    TextStart {
        content_index: usize,
        partial: AssistantMessage,
    },
    TextDelta {
        content_index: usize,
        delta: String,
        partial: AssistantMessage,
    },
    TextEnd {
        content_index: usize,
        content: String,
        partial: AssistantMessage,
    },
    ThinkingStart {
        content_index: usize,
        partial: AssistantMessage,
    },
    ThinkingDelta {
        content_index: usize,
        delta: String,
        partial: AssistantMessage,
    },
    ThinkingEnd {
        content_index: usize,
        content: String,
        partial: AssistantMessage,
    },
    #[serde(rename = "toolcall_start")]
    ToolCallStart {
        content_index: usize,
        partial: AssistantMessage,
    },
    #[serde(rename = "toolcall_delta")]
    ToolCallDelta {
        content_index: usize,
        delta: String,
        partial: AssistantMessage,
    },
    #[serde(rename = "toolcall_end")]
    ToolCallEnd {
        content_index: usize,
        #[serde(with = "tagged_tool_call")]
        tool_call: ToolCall,
        partial: AssistantMessage,
    },
    Done {
        reason: DoneReason,
        message: AssistantMessage,
    },
    Error {
        reason: ErrorReason,
        error: AssistantMessage,
    },
}

// ---------------------------------------------------------------------------
// Model and compat matrices
// ---------------------------------------------------------------------------

/// `ModelCostRates` — $/million tokens.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCostRates {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

/// `ModelCostTier extends ModelCostRates`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCostTier {
    #[serde(flatten)]
    pub rates: ModelCostRates,
    /// Use this tier for requests whose total input usage exceeds this token count.
    pub input_tokens_above: u64,
}

/// `ModelCost extends ModelCostRates`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCost {
    #[serde(flatten)]
    pub rates: ModelCostRates,
    /// Request-wide pricing tiers. The highest matching input threshold
    /// applies to the full request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tiers: Option<Vec<ModelCostTier>>,
}

/// `Model.input` entry: `"text" | "image"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InputModality {
    #[serde(rename = "text")]
    Text,
    #[serde(rename = "image")]
    Image,
}

/// `Model` — unified model system entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Model {
    pub id: String,
    pub name: String,
    pub api: ApiKind,
    pub provider: String,
    pub base_url: String,
    pub reasoning: bool,
    /// Maps pi thinking levels to provider/model-specific values.
    /// Missing keys use provider defaults; null marks a level as unsupported.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_level_map: Option<ThinkingLevelMap>,
    pub input: Vec<InputModality>,
    pub cost: ModelCost,
    pub context_window: u32,
    pub max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<std::collections::BTreeMap<String, String>>,
    /// Compatibility overrides; which fields apply is governed by `api`
    /// (upstream types this conditionally per API; see file header note).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compat: Option<ModelCompat>,
}

/// `OpenAICompletionsCompat.maxTokensField`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MaxTokensField {
    #[serde(rename = "max_completion_tokens")]
    MaxCompletionTokens,
    #[serde(rename = "max_tokens")]
    MaxTokens,
}

/// `OpenAICompletionsCompat.thinkingFormat` (10 values).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThinkingFormat {
    /// Uses `reasoning_effort`. Default.
    #[serde(rename = "openai")]
    Openai,
    /// Uses `reasoning: { effort }`.
    #[serde(rename = "openrouter")]
    Openrouter,
    /// Uses `thinking: { type }` plus `reasoning_effort` when supported.
    #[serde(rename = "deepseek")]
    Deepseek,
    /// Uses `reasoning: { enabled }` plus `reasoning_effort` when supported.
    #[serde(rename = "together")]
    Together,
    /// Uses `thinking: { type }`.
    #[serde(rename = "zai")]
    Zai,
    /// Uses top-level `enable_thinking: boolean`.
    #[serde(rename = "qwen")]
    Qwen,
    /// Uses configurable `chat_template_kwargs`.
    #[serde(rename = "chat-template")]
    ChatTemplate,
    /// Uses `chat_template_kwargs.enable_thinking` and `preserve_thinking`.
    #[serde(rename = "qwen-chat-template")]
    QwenChatTemplate,
    /// Uses top-level `thinking: string`.
    #[serde(rename = "string-thinking")]
    StringThinking,
    /// Uses `reasoning: { effort }` only when the mapped effort is non-null.
    #[serde(rename = "ant-ling")]
    AntLing,
}

/// `OpenAICompletionsCompat.cacheControlFormat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CacheControlFormat {
    /// Anthropic-style `cache_control` markers on the system prompt, last tool
    /// definition, and last user/assistant/tool-result text content.
    #[serde(rename = "anthropic")]
    Anthropic,
}

/// `OpenAICompletionsCompat.deferredToolsMode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeferredToolsMode {
    #[serde(rename = "kimi")]
    Kimi,
}

/// Merged compat settings for OpenAI-completions / OpenAI-responses /
/// Anthropic-messages / Bedrock APIs (see file header note for why this is one
/// flat struct). All fields optional; absent fields are omitted on the wire,
/// matching upstream objects.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCompat {
    // --- OpenAICompletionsCompat ---
    /// Whether the provider supports the `store` field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_store: Option<bool>,
    /// Whether the provider supports the `developer` role (vs `system`).
    /// Also in OpenAIResponsesCompat.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_developer_role: Option<bool>,
    /// Whether the provider supports `reasoning_effort`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_reasoning_effort: Option<bool>,
    /// Whether the provider supports `stream_options: { include_usage: true }`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_usage_in_streaming: Option<bool>,
    /// Which field to use for max tokens.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens_field: Option<MaxTokensField>,
    /// Whether tool results require the `name` field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requires_tool_result_name: Option<bool>,
    /// Whether a user message after tool results requires an assistant message
    /// in between.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requires_assistant_after_tool_result: Option<bool>,
    /// Whether thinking blocks must be converted to text blocks with
    /// `<thinking>` delimiters.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requires_thinking_as_text: Option<bool>,
    /// Whether all replayed assistant messages must include an empty
    /// `reasoning_content` field when reasoning is enabled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requires_reasoning_content_on_assistant_messages: Option<bool>,
    /// Format for the reasoning/thinking parameter.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_format: Option<ThinkingFormat>,
    /// Kwargs sent as `chat_template_kwargs` when `thinking_format` is
    /// `chat-template`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chat_template_kwargs: Option<std::collections::BTreeMap<String, ChatTemplateKwargValue>>,
    /// OpenRouter routing preferences, sent as the `provider` request field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub open_router_routing: Option<OpenRouterRouting>,
    /// Vercel AI Gateway routing preferences.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vercel_gateway_routing: Option<VercelGatewayRouting>,
    /// Whether z.ai supports top-level `tool_stream: true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zai_tool_stream: Option<bool>,
    /// Whether the provider supports OpenAI custom tools with Lark/regex
    /// grammar formats. Also in OpenAIResponsesCompat.
    #[serde(rename = "supportsOpenAIGrammarTools")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_open_ai_grammar_tools: Option<bool>,
    /// Whether the provider supports the `strict` field in tool definitions.
    /// Also in OpenAIResponsesCompat (strict JSON-schema function tools) and
    /// BedrockCompat (Bedrock strict tool schemas).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_strict_mode: Option<bool>,
    /// Cache control convention for prompt caching.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control_format: Option<CacheControlFormat>,
    /// Whether to send session-affinity data from `options.session_id`.
    /// Also in AnthropicMessagesCompat.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub send_session_affinity_headers: Option<bool>,
    /// Provider-specific deferred tool serialization mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deferred_tools_mode: Option<DeferredToolsMode>,
    /// Session-affinity header format. Also in OpenAIResponsesCompat.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_affinity_format: Option<SessionAffinityFormat>,
    /// Whether the provider supports long prompt cache retention. Also in
    /// OpenAIResponsesCompat and AnthropicMessagesCompat.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_long_cache_retention: Option<bool>,

    // --- OpenAIResponsesCompat only ---
    /// Whether the model supports client-executed tool search for deferred tools.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_tool_search: Option<bool>,
    /// Whether the model accepts `prompt_cache_options` (OpenAI GPT-5.6+
    /// explicit prompt caching).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_explicit_prompt_cache_mode: Option<bool>,

    // --- AnthropicMessagesCompat only ---
    /// Whether the provider accepts per-tool `eager_input_streaming`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_eager_tool_input_streaming: Option<bool>,
    /// Whether the provider supports Anthropic-style `cache_control` markers
    /// on tool definitions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_cache_control_on_tools: Option<bool>,
    /// Whether the model accepts the Anthropic `temperature` request field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_temperature: Option<bool>,
    /// Whether to force adaptive thinking (`thinking.type: "adaptive"` plus
    /// `output_config.effort`) regardless of the model id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub force_adaptive_thinking: Option<bool>,
    /// Whether to replay empty thinking signatures as `signature: ""` instead
    /// of converting thinking to text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_empty_signature: Option<bool>,
    /// Whether the provider supports Anthropic strict tool schemas.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_strict_tools: Option<bool>,
    /// Whether the provider supports deferred tools loaded by
    /// `tool_reference` blocks in tool results.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_tool_references: Option<bool>,
}

// ---------------------------------------------------------------------------
// Routing preferences
// ---------------------------------------------------------------------------

/// `OpenRouterRouting.data_collection`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DataCollection {
    #[serde(rename = "deny")]
    Deny,
    #[serde(rename = "allow")]
    Allow,
}

/// `OpenRouterRouting.sort` — string or object form.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum OpenRouterSort {
    Name(String),
    Spec(OpenRouterSortSpec),
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OpenRouterSortSpec {
    /// The sorting metric: "price", "throughput", "latency".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    /// Partitioning strategy: "model" (default) or "none".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub partition: Option<String>,
}

/// `OpenRouterRouting.max_price` — prices per million tokens (USD).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OpenRouterMaxPrice {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<NumberOrString>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion: Option<NumberOrString>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<NumberOrString>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio: Option<NumberOrString>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request: Option<NumberOrString>,
}

/// `OpenRouterRouting.preferred_min_throughput` — number or percentile object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum OpenRouterThroughput {
    Value(f64),
    Percentiles(OpenRouterPercentiles),
}

/// `OpenRouterRouting.preferred_max_latency` — number or percentile object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum OpenRouterLatency {
    Value(f64),
    Percentiles(OpenRouterPercentiles),
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OpenRouterPercentiles {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p50: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p75: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p90: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p99: Option<f64>,
}

/// `OpenRouterRouting` — controls which upstream providers OpenRouter routes
/// requests to. Sent as the `provider` field in the OpenRouter API request
/// body. Field names are snake_case upstream (unlike the rest of the model
/// catalog), so no rename rule is applied here.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OpenRouterRouting {
    /// Whether to allow backup providers to serve requests. Default: true.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_fallbacks: Option<bool>,
    /// Filter to providers supporting all parameters in the request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub require_parameters: Option<bool>,
    /// Data collection setting.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_collection: Option<DataCollection>,
    /// Restrict to ZDR (Zero Data Retention) endpoints.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zdr: Option<bool>,
    /// Restrict to models that allow text distillation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enforce_distillable_text: Option<bool>,
    /// Ordered list of provider names/slugs to try in sequence.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order: Option<Vec<String>>,
    /// Provider names/slugs to exclusively allow.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub only: Option<Vec<String>>,
    /// Provider names/slugs to skip.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ignore: Option<Vec<String>>,
    /// Quantization levels to filter by.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quantizations: Option<Vec<String>>,
    /// Sorting strategy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort: Option<OpenRouterSort>,
    /// Maximum price per million tokens (USD).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_price: Option<OpenRouterMaxPrice>,
    /// Preferred minimum throughput (tokens/second).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preferred_min_throughput: Option<OpenRouterThroughput>,
    /// Preferred maximum latency (seconds).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preferred_max_latency: Option<OpenRouterLatency>,
}

/// `VercelGatewayRouting` — routing preferences for Vercel AI Gateway.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VercelGatewayRouting {
    /// Provider slugs to exclusively use (e.g. ["bedrock", "anthropic"]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub only: Option<Vec<String>>,
    /// Provider slugs to try in order.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order: Option<Vec<String>>,
}

// ---------------------------------------------------------------------------
// Tests: serialization shape snapshots (T01 gate G3 substitute — locked type
// contracts verified against hand-checked upstream JSON shapes)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use serde_json::{json, Map, Value};

    use super::*;

    fn partial() -> AssistantMessage {
        AssistantMessage {
            role: AssistantRole::Assistant,
            content: vec![],
            api: ApiKind::from("anthropic-messages"),
            provider: "anthropic".to_owned(),
            model: "claude-x".to_owned(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: 0,
        }
    }

    /// Canonical JSON of `partial()` — pinned literal, mirrors the upstream
    /// AssistantMessage property order (types.ts).
    const PARTIAL_JSON: &str = r#"{"role":"assistant","content":[],"api":"anthropic-messages","provider":"anthropic","model":"claude-x","usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0,"cost":{"input":0.0,"output":0.0,"cacheRead":0.0,"cacheWrite":0.0,"total":0.0}},"stopReason":"stop","timestamp":0}"#;

    fn to_json<T: Serialize>(v: &T) -> String {
        serde_json::to_string(v).expect("serialization must succeed")
    }

    #[test]
    fn stream_event_start_shape() {
        let ev = StreamEvent::Start { partial: partial() };
        assert_eq!(
            to_json(&ev),
            format!(r#"{{"type":"start","partial":{PARTIAL_JSON}}}"#)
        );
        let back: StreamEvent = serde_json::from_str(&to_json(&ev)).expect("roundtrip");
        assert_eq!(back, ev);
    }

    #[test]
    fn stream_event_text_delta_shape() {
        let ev = StreamEvent::TextDelta {
            content_index: 1,
            delta: "hi".to_owned(),
            partial: partial(),
        };
        assert_eq!(
            to_json(&ev),
            format!(
                r#"{{"type":"text_delta","contentIndex":1,"delta":"hi","partial":{PARTIAL_JSON}}}"#
            )
        );
    }

    #[test]
    fn stream_event_toolcall_end_carries_tagged_tool_call() {
        let mut arguments = Map::new();
        arguments.insert("cmd".to_owned(), Value::from("ls"));
        let tool_call = ToolCall {
            id: "c1".to_owned(),
            name: "bash".to_owned(),
            arguments,
            thought_signature: Some("sig".to_owned()),
        };
        let ev = StreamEvent::ToolCallEnd {
            content_index: 2,
            tool_call,
            partial: partial(),
        };
        // Upstream ToolCall objects always carry their `type: "toolCall"` tag,
        // also standalone inside toolcall_end.
        assert_eq!(
            to_json(&ev),
            format!(
                r#"{{"type":"toolcall_end","contentIndex":2,"toolCall":{{"type":"toolCall","id":"c1","name":"bash","arguments":{{"cmd":"ls"}},"thoughtSignature":"sig"}},"partial":{PARTIAL_JSON}}}"#
            )
        );
        let back: StreamEvent = serde_json::from_str(&to_json(&ev)).expect("roundtrip");
        assert_eq!(back, ev);
    }

    #[test]
    fn stream_event_done_and_error_shapes() {
        let done = StreamEvent::Done {
            reason: DoneReason::ToolUse,
            message: partial(),
        };
        assert_eq!(
            to_json(&done),
            format!(r#"{{"type":"done","reason":"toolUse","message":{PARTIAL_JSON}}}"#)
        );

        let mut err_msg = partial();
        err_msg.stop_reason = StopReason::Aborted;
        err_msg.error_message = Some("Operation aborted".to_owned());
        let err = StreamEvent::Error {
            reason: ErrorReason::Aborted,
            error: err_msg,
        };
        let v: Value = serde_json::from_str(&to_json(&err)).expect("parse");
        assert_eq!(v["type"], json!("error"));
        assert_eq!(v["reason"], json!("aborted"));
        assert_eq!(v["error"]["stopReason"], json!("aborted"));
        assert_eq!(v["error"]["errorMessage"], json!("Operation aborted"));
    }

    #[test]
    fn stream_event_variant_type_literals() {
        // Every variant's `type` tag, checked against upstream types.ts.
        let p = || partial();
        let cases: Vec<(StreamEvent, &str)> = vec![
            (StreamEvent::Start { partial: p() }, "start"),
            (
                StreamEvent::TextStart {
                    content_index: 0,
                    partial: p(),
                },
                "text_start",
            ),
            (
                StreamEvent::TextDelta {
                    content_index: 0,
                    delta: String::new(),
                    partial: p(),
                },
                "text_delta",
            ),
            (
                StreamEvent::TextEnd {
                    content_index: 0,
                    content: String::new(),
                    partial: p(),
                },
                "text_end",
            ),
            (
                StreamEvent::ThinkingStart {
                    content_index: 0,
                    partial: p(),
                },
                "thinking_start",
            ),
            (
                StreamEvent::ThinkingDelta {
                    content_index: 0,
                    delta: String::new(),
                    partial: p(),
                },
                "thinking_delta",
            ),
            (
                StreamEvent::ThinkingEnd {
                    content_index: 0,
                    content: String::new(),
                    partial: p(),
                },
                "thinking_end",
            ),
            (
                StreamEvent::ToolCallStart {
                    content_index: 0,
                    partial: p(),
                },
                "toolcall_start",
            ),
            (
                StreamEvent::ToolCallDelta {
                    content_index: 0,
                    delta: String::new(),
                    partial: p(),
                },
                "toolcall_delta",
            ),
            (
                StreamEvent::ToolCallEnd {
                    content_index: 0,
                    tool_call: ToolCall::default(),
                    partial: p(),
                },
                "toolcall_end",
            ),
            (
                StreamEvent::Done {
                    reason: DoneReason::Stop,
                    message: p(),
                },
                "done",
            ),
            (
                StreamEvent::Error {
                    reason: ErrorReason::Error,
                    error: p(),
                },
                "error",
            ),
        ];
        for (ev, expected_type) in cases {
            let v: Value = serde_json::from_str(&to_json(&ev)).expect("parse");
            assert_eq!(v["type"], json!(expected_type));
        }
    }

    #[test]
    fn assistant_message_signature_fields_shape() {
        let mut msg = partial();
        msg.content = vec![
            AssistantContent::Text(TextContent {
                text: "t".to_owned(),
                text_signature: Some("ts".to_owned()),
            }),
            AssistantContent::Thinking(ThinkingContent {
                thinking: "th".to_owned(),
                thinking_signature: Some("ths".to_owned()),
                redacted: Some(true),
            }),
            AssistantContent::ToolCall(ToolCall {
                id: "1".to_owned(),
                name: "n".to_owned(),
                arguments: Map::new(),
                thought_signature: Some("s".to_owned()),
            }),
        ];
        msg.usage.cache_write1h = Some(5);
        msg.usage.reasoning = Some(7);
        msg.response_model = Some("resolved-model".to_owned());
        msg.response_id = Some("resp-1".to_owned());

        let v: Value = serde_json::from_str(&to_json(&msg)).expect("parse");
        assert_eq!(
            v["content"],
            json!([
                {"type": "text", "text": "t", "textSignature": "ts"},
                {"type": "thinking", "thinking": "th", "thinkingSignature": "ths", "redacted": true},
                {"type": "toolCall", "id": "1", "name": "n", "arguments": {}, "thoughtSignature": "s"},
            ])
        );
        assert_eq!(v["usage"]["cacheWrite1h"], json!(5));
        assert_eq!(v["usage"]["reasoning"], json!(7));
        assert_eq!(v["responseModel"], json!("resolved-model"));
        assert_eq!(v["responseId"], json!("resp-1"));

        let back: AssistantMessage = serde_json::from_str(&to_json(&msg)).expect("roundtrip");
        assert_eq!(back, msg);
    }

    #[test]
    fn stop_reason_literals() {
        let cases = [
            (StopReason::Pending, "pending"),
            (StopReason::Stop, "stop"),
            (StopReason::Length, "length"),
            (StopReason::ToolUse, "toolUse"),
            (StopReason::Error, "error"),
            (StopReason::Aborted, "aborted"),
        ];
        for (reason, expected) in cases {
            assert_eq!(to_json(&reason), format!("\"{expected}\""));
        }
    }

    #[test]
    fn user_message_content_string_or_blocks() {
        let text_msg = UserMessage {
            role: UserRole::User,
            content: UserContent::Text("hello".to_owned()),
            timestamp: 1,
        };
        assert_eq!(
            to_json(&text_msg),
            r#"{"role":"user","content":"hello","timestamp":1}"#
        );

        let blocks_msg = UserMessage {
            role: UserRole::User,
            content: UserContent::Blocks(vec![
                UserContentBlock::Text(TextContent {
                    text: "a".to_owned(),
                    text_signature: None,
                }),
                UserContentBlock::Image(ImageContent {
                    data: "AAAA".to_owned(),
                    mime_type: "image/png".to_owned(),
                }),
            ]),
            timestamp: 2,
        };
        assert_eq!(
            to_json(&blocks_msg),
            r#"{"role":"user","content":[{"type":"text","text":"a"},{"type":"image","data":"AAAA","mimeType":"image/png"}],"timestamp":2}"#
        );

        // Both forms deserialize through the Message union (role marker
        // disambiguates the untagged variants).
        let m1: Message = serde_json::from_str(&to_json(&text_msg)).expect("text roundtrip");
        assert_eq!(m1.role(), Role::User);
        let m2: Message = serde_json::from_str(&to_json(&blocks_msg)).expect("blocks roundtrip");
        assert_eq!(m2, Message::User(blocks_msg));
    }

    #[test]
    fn tool_result_message_full_shape() {
        let msg = ToolResultMessage {
            role: ToolResultRole::ToolResult,
            tool_call_id: "c1".to_owned(),
            tool_name: "read".to_owned(),
            content: vec![ToolResultContent::Text(TextContent {
                text: "out".to_owned(),
                text_signature: None,
            })],
            details: Some(json!({"truncated": false})),
            usage: None,
            added_tool_names: Some(vec!["grep".to_owned()]),
            is_error: false,
            timestamp: 3,
        };
        let v: Value = serde_json::from_str(&to_json(&msg)).expect("parse");
        assert_eq!(v["role"], json!("toolResult"));
        assert_eq!(v["toolCallId"], json!("c1"));
        assert_eq!(v["toolName"], json!("read"));
        assert_eq!(v["details"], json!({"truncated": false}));
        assert_eq!(v["addedToolNames"], json!(["grep"]));
        assert_eq!(v["isError"], json!(false));
        assert!(
            v.get("usage").is_none(),
            "absent optional fields are omitted"
        );
        let back: ToolResultMessage = serde_json::from_str(&to_json(&msg)).expect("roundtrip");
        assert_eq!(back, msg);
    }

    #[test]
    fn tool_constrained_sampling_shapes() {
        let base = |cs: Option<ConstrainedSampling>| Tool {
            name: "n".to_owned(),
            description: "d".to_owned(),
            parameters: json!({"type": "object"}),
            constrained_sampling: cs,
        };

        let json_schema = base(Some(ConstrainedSampling::Config(
            ConstrainedSamplingConfig::JsonSchema {
                strict: ConstrainedSamplingStrict::Require,
            },
        )));
        assert_eq!(
            to_json(&json_schema),
            r#"{"name":"n","description":"d","parameters":{"type":"object"},"constrainedSampling":{"type":"json_schema","strict":"require"}}"#
        );

        let grammar = base(Some(ConstrainedSampling::Config(
            ConstrainedSamplingConfig::Grammar {
                variants: GrammarVariants {
                    openai_lark: Some("x".to_owned()),
                    openai_regex: None,
                },
            },
        )));
        assert_eq!(
            to_json(&grammar),
            r#"{"name":"n","description":"d","parameters":{"type":"object"},"constrainedSampling":{"type":"grammar","variants":{"openai_lark":"x"}}}"#
        );

        let disabled = base(Some(ConstrainedSampling::Disabled(false)));
        assert_eq!(
            to_json(&disabled),
            r#"{"name":"n","description":"d","parameters":{"type":"object"},"constrainedSampling":false}"#
        );

        for tool in [json_schema, grammar, disabled] {
            let back: Tool = serde_json::from_str(&to_json(&tool)).expect("roundtrip");
            assert_eq!(back, tool);
        }
    }

    #[test]
    fn model_full_shape_roundtrip() {
        // Covers: thinkingLevelMap three-state, cost.tiers, headers, compat
        // (incl. the supportsOpenAIGrammarTools acronym rename and
        // chat_template_kwargs $var form).
        let model_json = json!({
            "id": "m1",
            "name": "M One",
            "api": "openai-completions",
            "provider": "openai",
            "baseUrl": "https://api.example.com",
            "reasoning": true,
            "thinkingLevelMap": {"off": null, "high": "high"},
            "input": ["text", "image"],
            "cost": {
                "input": 1.0, "output": 2.0, "cacheRead": 0.5, "cacheWrite": 1.5,
                "tiers": [
                    {"input": 3.0, "output": 4.0, "cacheRead": 0.1, "cacheWrite": 0.2,
                     "inputTokensAbove": 200000}
                ]
            },
            "contextWindow": 1000000,
            "maxTokens": 64000,
            "headers": {"x-a": "b"},
            "compat": {
                "supportsStore": true,
                "supportsOpenAIGrammarTools": true,
                "thinkingFormat": "chat-template",
                "chatTemplateKwargs": {"k": {"$var": "thinking.enabled", "omitWhenOff": true}},
                "openRouterRouting": {
                    "allow_fallbacks": true,
                    "data_collection": "deny",
                    "sort": {"by": "price", "partition": "model"},
                    "max_price": {"prompt": 1.5, "completion": "2"},
                    "preferred_min_throughput": {"p50": 10.0},
                    "preferred_max_latency": 5.0
                },
                "sessionAffinityFormat": "openai-nosession"
            }
        });

        let model: Model = serde_json::from_value(model_json.clone()).expect("deserialize");

        // thinkingLevelMap three-state: absent key (provider default),
        // null (unsupported), string (mapped).
        let map = model.thinking_level_map.as_ref().expect("map present");
        assert_eq!(map.get(&ModelThinkingLevel::Off), Some(&None));
        assert_eq!(
            map.get(&ModelThinkingLevel::High),
            Some(&Some("high".to_owned()))
        );
        assert_eq!(map.get(&ModelThinkingLevel::Low), None);

        let cost = &model.cost;
        assert_eq!(cost.rates.input, 1.0);
        let tiers = cost.tiers.as_ref().expect("tiers present");
        assert_eq!(tiers[0].input_tokens_above, 200_000);
        assert_eq!(tiers[0].rates.output, 4.0);

        let compat = model.compat.as_ref().expect("compat present");
        assert_eq!(compat.supports_open_ai_grammar_tools, Some(true));
        assert_eq!(compat.thinking_format, Some(ThinkingFormat::ChatTemplate));
        assert_eq!(
            compat.session_affinity_format,
            Some(SessionAffinityFormat::OpenaiNosession)
        );
        let kwargs = compat
            .chat_template_kwargs
            .as_ref()
            .expect("kwargs present");
        assert_eq!(
            kwargs.get("k"),
            Some(&ChatTemplateKwargValue::Var(ChatTemplateKwargVar {
                var: ChatTemplateKwargVarKind::ThinkingEnabled,
                omit_when_off: Some(true),
            }))
        );

        // Roundtrip is semantically identical (serde_json::Value map equality
        // is key-order independent).
        let serialized: Value = serde_json::from_str(&to_json(&model)).expect("re-parse");
        assert_eq!(serialized, model_json);
    }

    #[test]
    fn api_kind_known_constants_match_upstream() {
        // KnownApi literal values (requirements §5.1), order as upstream.
        let known = [
            ApiKind::OPENAI_COMPLETIONS,
            ApiKind::MISTRAL_CONVERSATIONS,
            ApiKind::OPENAI_RESPONSES,
            ApiKind::AZURE_OPENAI_RESPONSES,
            ApiKind::OPENAI_CODEX_RESPONSES,
            ApiKind::ANTHROPIC_MESSAGES,
            ApiKind::BEDROCK_CONVERSE_STREAM,
            ApiKind::GOOGLE_GENERATIVE_AI,
            ApiKind::GOOGLE_VERTEX,
            ApiKind::PI_MESSAGES,
        ];
        assert_eq!(
            known,
            [
                "openai-completions",
                "mistral-conversations",
                "openai-responses",
                "azure-openai-responses",
                "openai-codex-responses",
                "anthropic-messages",
                "bedrock-converse-stream",
                "google-generative-ai",
                "google-vertex",
                "pi-messages",
            ]
        );
        // Custom API strings stay possible (Api = KnownApi | (string & {})).
        assert_eq!(
            to_json(&ApiKind::from("my-custom-api")),
            "\"my-custom-api\""
        );
    }

    #[test]
    fn diagnostic_shape() {
        let mut details = Map::new();
        details.insert("a".to_owned(), json!(1));
        let diag = AssistantMessageDiagnostic {
            kind: "rewrite".to_owned(),
            timestamp: 1,
            error: Some(DiagnosticErrorInfo {
                name: Some("Error".to_owned()),
                message: "m".to_owned(),
                stack: None,
                code: Some(NumberOrString::String("E".to_owned())),
            }),
            details: Some(details),
        };
        assert_eq!(
            to_json(&diag),
            r#"{"type":"rewrite","timestamp":1,"error":{"name":"Error","message":"m","code":"E"},"details":{"a":1}}"#
        );
    }
}
