//! Port of `packages/ai/src/api/anthropic-messages.ts` @ pi 0.82.1 (2efa728);
//! stream-termination semantics (rawStopReason, `sensitive` error text)
//! updated to 4181f66 (926eb15c1, 5a2539a7b).
//!
//! Anthropic Messages API adapter: request construction (system prompt,
//! cache control, thinking modes, tools, deferred tool references), Claude
//! Code identity for OAuth tokens, SSE stream decoding into
//! [`StreamEvent`]s, and `stream_simple` reasoning mapping.
//!
//! Intentional differences (upstream deviations):
//! - HTTP is a direct reqwest call, not the `@anthropic-ai/sdk`; the SDK's
//!   `x-stainless-*` telemetry headers and default `User-Agent` are not sent,
//!   and there is no SDK default timeout (callers set
//!   `StreamOptions::timeout_ms`).
//! - The legacy env var is renamed `PI_CACHE_RETENTION` →
//!   `RPI_CACHE_RETENTION` (requirements §5.5).
//! - `AnthropicOptions.client` (SDK client injection, e.g. AnthropicVertex) is
//!   not ported; there is no SDK client to inject.
//! - SSE events are decoded with the shared [`crate::api::sse::SseDecoder`]
//!   (ported from this file upstream).

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use futures::StreamExt;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::api::constrained_sampling::resolve_json_schema_strict_sampling;
use crate::api::copilot_headers::{build_copilot_dynamic_headers, has_copilot_vision_input};
use crate::api::lazy::immediate_error_stream;
use crate::api::simple_options::{
    adjust_max_tokens_for_thinking, build_base_options, clamp_max_tokens_to_context,
};
use crate::api::sse::{ServerSentEvent, SseDecoder};
use crate::models::ProviderStreams;
use crate::types::{
    AssistantContent, AssistantMessage, CacheRetention, Context, DoneReason, ErrorReason, Message,
    Model, ModelThinkingLevel, ProviderEnv, ProviderHeaders, ProviderResponse, SimpleStreamOptions,
    StopReason, StreamEvent, StreamOptions, ThinkingLevel, Tool, ToolResultContent,
    ToolResultMessage, Usage, UserContent, UserContentBlock,
};
use crate::utils::cost::calculate_cost;
use crate::utils::custom_fetch::send_provider_request;
use crate::utils::deferred_tools::split_deferred_tools;
use crate::utils::error_body::{format_provider_error, NormalizedProviderError};
use crate::utils::event_stream::AssistantMessageEventStream;
use crate::utils::headers::{
    headers_to_record, merge_headers_chain, model_headers, provider_headers_to_header_map,
};
use crate::utils::json_parse::{parse_json_with_repair, parse_streaming_json};
use crate::utils::provider_env::get_provider_env_value;
use crate::utils::provider_retry::{
    retry_provider_request, ProviderErrorInfo, ProviderRetryOptions,
};
use crate::utils::sanitize_unicode::sanitize_surrogates;
use crate::utils::transform_messages::transform_messages;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const FINE_GRAINED_TOOL_STREAMING_BETA: &str = "fine-grained-tool-streaming-2025-05-14";
const INTERLEAVED_THINKING_BETA: &str = "interleaved-thinking-2025-05-14";
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Stealth mode: mimic Claude Code's version in the OAuth user agent.
pub const CLAUDE_CODE_VERSION: &str = "2.1.75";

/// Claude Code 2.x tool names (canonical casing).
/// Source: https://cchistory.mariozechner.at/data/prompts-2.1.11.md
const CLAUDE_CODE_TOOLS: [&str; 17] = [
    "Read",
    "Write",
    "Edit",
    "Bash",
    "Grep",
    "Glob",
    "AskUserQuestion",
    "EnterPlanMode",
    "ExitPlanMode",
    "KillShell",
    "NotebookEdit",
    "Skill",
    "Task",
    "TaskOutput",
    "TodoWrite",
    "WebFetch",
    "WebSearch",
];

static CC_TOOL_LOOKUP: LazyLock<HashMap<String, &'static str>> = LazyLock::new(|| {
    CLAUDE_CODE_TOOLS
        .iter()
        .map(|tool| (tool.to_lowercase(), *tool))
        .collect()
});

/// `toClaudeCodeName`: CC canonical casing if it matches (case-insensitive).
pub fn to_claude_code_name(name: &str) -> String {
    CC_TOOL_LOOKUP
        .get(&name.to_lowercase())
        .map(|canonical| (*canonical).to_owned())
        .unwrap_or_else(|| name.to_owned())
}

/// `fromClaudeCodeName`: map a (canonical) CC name back to the tool's own
/// casing from the current tool list.
pub fn from_claude_code_name(name: &str, tools: Option<&[Tool]>) -> String {
    if let Some(tools) = tools {
        if !tools.is_empty() {
            let lower_name = name.to_lowercase();
            if let Some(matched) = tools
                .iter()
                .find(|tool| tool.name.to_lowercase() == lower_name)
            {
                return matched.name.clone();
            }
        }
    }
    name.to_owned()
}

const ANTHROPIC_MESSAGE_EVENTS: [&str; 6] = [
    "message_start",
    "message_delta",
    "message_stop",
    "content_block_start",
    "content_block_delta",
    "content_block_stop",
];

// ---------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------

/// `AnthropicThinkingDisplay = "summarized" | "omitted"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnthropicThinkingDisplay {
    Summarized,
    Omitted,
}

impl AnthropicThinkingDisplay {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Summarized => "summarized",
            Self::Omitted => "omitted",
        }
    }
}

/// `AnthropicOptions.toolChoice`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnthropicToolChoice {
    Auto,
    Any,
    None,
    /// `{ type: "tool", name }` — force a specific tool.
    Tool {
        name: String,
    },
}

/// `AnthropicOptions` — `StreamOptions` plus Anthropic-specific extras.
///
/// `effort` carries the canonical adaptive-thinking effort values
/// (`low|medium|high|xhigh|max`); values from
/// `Model::thinking_level_map` pass through verbatim, matching upstream's
/// unchecked cast.
#[derive(Debug, Clone, Default)]
pub struct AnthropicOptions {
    pub stream: StreamOptions,
    /// Enable extended thinking. `None` omits the `thinking` parameter.
    pub thinking_enabled: Option<bool>,
    /// Token budget for budget-based thinking (older models). Default: 1024.
    pub thinking_budget_tokens: Option<u32>,
    /// Effort level for adaptive-thinking models.
    pub effort: Option<String>,
    /// Thinking content display mode. Default: `summarized` when enabled.
    pub thinking_display: Option<AnthropicThinkingDisplay>,
    /// Whether to request the interleaved-thinking beta header. Default: true.
    pub interleaved_thinking: Option<bool>,
    pub tool_choice: Option<AnthropicToolChoice>,
}

// ---------------------------------------------------------------------------
// Compat
// ---------------------------------------------------------------------------

/// `Required<Omit<AnthropicMessagesCompat, "forceAdaptiveThinking">>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedAnthropicCompat {
    pub supports_eager_tool_input_streaming: bool,
    pub supports_long_cache_retention: bool,
    pub send_session_affinity_headers: bool,
    pub supports_cache_control_on_tools: bool,
    pub supports_temperature: bool,
    pub allow_empty_signature: bool,
    pub supports_strict_tools: bool,
    pub supports_tool_references: bool,
}

/// `getAnthropicCompat`.
pub fn get_anthropic_compat(model: &Model) -> ResolvedAnthropicCompat {
    let compat = model.compat.as_ref();
    ResolvedAnthropicCompat {
        supports_eager_tool_input_streaming: compat
            .and_then(|c| c.supports_eager_tool_input_streaming)
            .unwrap_or(true),
        supports_long_cache_retention: compat
            .and_then(|c| c.supports_long_cache_retention)
            .unwrap_or(true),
        send_session_affinity_headers: compat
            .and_then(|c| c.send_session_affinity_headers)
            .unwrap_or(false),
        supports_cache_control_on_tools: compat
            .and_then(|c| c.supports_cache_control_on_tools)
            .unwrap_or(true),
        supports_temperature: compat.and_then(|c| c.supports_temperature).unwrap_or(true),
        allow_empty_signature: compat
            .and_then(|c| c.allow_empty_signature)
            .unwrap_or(false),
        supports_strict_tools: compat
            .and_then(|c| c.supports_strict_tools)
            .unwrap_or(false),
        supports_tool_references: compat
            .and_then(|c| c.supports_tool_references)
            .unwrap_or_else(|| default_supports_tool_references(model)),
    }
}

static TOOL_REFERENCES_VERSION: LazyLock<regex::Regex> = LazyLock::new(|| {
    // invariant: literal pattern compiles
    #[allow(clippy::expect_used)]
    regex::Regex::new(r"^claude-(?:opus|sonnet|fable)-(\d+)(?:-(\d+))?(?:-|$)")
        .expect("static regex")
});

/// `defaultSupportsToolReferences`: first-party Anthropic models except Haiku
/// and models predating tool search (Claude 3.x, Opus/Sonnet 4.0, Opus 4.1).
pub fn default_supports_tool_references(model: &Model) -> bool {
    if model.provider != "anthropic" || model.id.contains("haiku") {
        return false;
    }
    let Some(captures) = TOOL_REFERENCES_VERSION.captures(&model.id) else {
        return false;
    };
    let major: u32 = captures
        .get(1)
        .and_then(|m| m.as_str().parse().ok())
        .unwrap_or(0);
    let minor: u32 = match captures.get(2) {
        Some(m) if m.as_str().len() < 8 => m.as_str().parse().unwrap_or(0),
        _ => 0,
    };
    major > 4 || (major == 4 && minor >= 5)
}

// ---------------------------------------------------------------------------
// Cache retention
// ---------------------------------------------------------------------------

/// `resolveCacheRetention`. Defaults to "short"; the `RPI_CACHE_RETENTION`
/// env var (upstream: `PI_CACHE_RETENTION`) opts into "long".
pub fn resolve_cache_retention(
    cache_retention: Option<CacheRetention>,
    env: Option<&ProviderEnv>,
) -> CacheRetention {
    if let Some(cache_retention) = cache_retention {
        return cache_retention;
    }
    if get_provider_env_value("RPI_CACHE_RETENTION", env).as_deref() == Some("long") {
        return CacheRetention::Long;
    }
    CacheRetention::Short
}

/// `getCacheControl`: the `cache_control` JSON block for the resolved
/// retention (`{"type":"ephemeral"}`, plus `"ttl":"1h"` for long retention on
/// supporting models). `None` retention yields `None`.
pub fn get_cache_control(
    model: &Model,
    cache_retention: Option<CacheRetention>,
    env: Option<&ProviderEnv>,
) -> (CacheRetention, Option<Value>) {
    let retention = resolve_cache_retention(cache_retention, env);
    if retention == CacheRetention::None {
        return (retention, None);
    }
    let long = retention == CacheRetention::Long
        && get_anthropic_compat(model).supports_long_cache_retention;
    let cache_control = if long {
        json!({"type": "ephemeral", "ttl": "1h"})
    } else {
        json!({"type": "ephemeral"})
    };
    (retention, Some(cache_control))
}

// ---------------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------------

fn has_header(headers: Option<&ProviderHeaders>, name: &str) -> bool {
    let Some(headers) = headers else { return false };
    let expected = name.to_lowercase();
    headers.iter().any(|(key, value)| {
        key.to_lowercase() == expected
            && value.as_ref().is_some_and(|value| !value.trim().is_empty())
    })
}

/// `assertRequestAuth`: an explicit API key or an auth-bearing header must be
/// present.
pub fn assert_request_auth(
    provider: &str,
    api_key: Option<&str>,
    headers: Option<&ProviderHeaders>,
) -> Result<(), String> {
    if api_key.is_some() {
        return Ok(());
    }
    if has_header(headers, "authorization")
        || has_header(headers, "x-api-key")
        || has_header(headers, "cf-aig-authorization")
    {
        return Ok(());
    }
    Err(format!("No API key for provider: {provider}"))
}

fn is_oauth_token(api_key: &str) -> bool {
    api_key.contains("sk-ant-oat")
}

// Header construction note: `merge_headers_chain` mirrors the
// Anthropic/OpenAI SDK `buildHeaders` net semantics (case-insensitive, later
// sources win, `None` suppresses). Auth headers sit in the base position
// because both SDKs apply `authHeaders` *before* `defaultHeaders` (user
// headers override key-derived auth).

/// `createClient` (header construction only): resolves the auth mode and the
/// full header set. Returns the merged headers and whether the token is an
/// OAuth token.
fn build_request_headers(
    model: &Model,
    api_key: Option<&str>,
    interleaved_thinking: bool,
    use_fine_grained_tool_streaming_beta: bool,
    options_headers: Option<&ProviderHeaders>,
    dynamic_headers: Option<HashMap<String, String>>,
    session_id: Option<&str>,
) -> (ProviderHeaders, bool) {
    let compat = get_anthropic_compat(model);
    // Adaptive-thinking models have interleaved thinking built in.
    let needs_interleaved_beta = interleaved_thinking
        && model
            .compat
            .as_ref()
            .and_then(|c| c.force_adaptive_thinking)
            != Some(true);
    let mut beta_features: Vec<&str> = Vec::new();
    if use_fine_grained_tool_streaming_beta {
        beta_features.push(FINE_GRAINED_TOOL_STREAMING_BETA);
    }
    if needs_interleaved_beta {
        beta_features.push(INTERLEAVED_THINKING_BETA);
    }

    let dynamic_headers: Option<ProviderHeaders> = dynamic_headers.map(|headers| {
        headers
            .into_iter()
            .map(|(key, value)| (key, Some(value)))
            .collect()
    });

    let mut base: ProviderHeaders = [
        ("accept".to_owned(), Some("application/json".to_owned())),
        (
            "anthropic-dangerous-direct-browser-access".to_owned(),
            Some("true".to_owned()),
        ),
        (
            "anthropic-version".to_owned(),
            Some(ANTHROPIC_VERSION.to_owned()),
        ),
    ]
    .into();

    // Copilot: Bearer auth, selective betas.
    if model.provider == "github-copilot" {
        if !beta_features.is_empty() {
            base.insert("anthropic-beta".to_owned(), Some(beta_features.join(",")));
        }
        if let Some(api_key) = api_key {
            base.insert(
                "authorization".to_owned(),
                Some(format!("Bearer {api_key}")),
            );
        }
        let headers = merge_headers_chain(&[
            Some(base),
            model_headers(model),
            dynamic_headers,
            options_headers.cloned(),
        ]);
        return (headers, false);
    }

    // OAuth: Bearer auth, Claude Code identity headers.
    if let Some(api_key) = api_key {
        if is_oauth_token(api_key) {
            let mut betas = vec!["claude-code-20250219", "oauth-2025-04-20"];
            betas.extend(beta_features);
            base.insert("anthropic-beta".to_owned(), Some(betas.join(",")));
            base.insert(
                "user-agent".to_owned(),
                Some(format!("claude-cli/{CLAUDE_CODE_VERSION}")),
            );
            base.insert("x-app".to_owned(), Some("cli".to_owned()));
            base.insert(
                "authorization".to_owned(),
                Some(format!("Bearer {api_key}")),
            );
            let headers =
                merge_headers_chain(&[Some(base), model_headers(model), options_headers.cloned()]);
            return (headers, true);
        }
    }

    // API key or header-owned auth.
    if !beta_features.is_empty() {
        base.insert("anthropic-beta".to_owned(), Some(beta_features.join(",")));
    }
    if let Some(api_key) = api_key {
        base.insert("x-api-key".to_owned(), Some(api_key.to_owned()));
    }
    let session_affinity_headers: Option<ProviderHeaders> = session_id
        .filter(|_| compat.send_session_affinity_headers)
        .map(|session_id| [("x-session-affinity".to_owned(), Some(session_id.to_owned()))].into());
    let headers = merge_headers_chain(&[
        Some(base),
        session_affinity_headers,
        model_headers(model),
        options_headers.cloned(),
    ]);
    (headers, false)
}

// ---------------------------------------------------------------------------
// Message conversion
// ---------------------------------------------------------------------------

/// `normalizeToolCallId`: Anthropic-required pattern and length. The allowed
/// set is ASCII, so `chars().take(64)` matches JS `slice(0, 64)`.
pub fn normalize_tool_call_id(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect()
}

/// `convertContentBlocks`: text-only content collapses to a joined string;
/// images force a block array, prepending a placeholder when no text exists.
fn convert_content_blocks(content: &[ToolResultContent]) -> Value {
    let has_images = content
        .iter()
        .any(|block| matches!(block, ToolResultContent::Image(_)));
    if !has_images {
        let joined = content
            .iter()
            .map(|block| match block {
                ToolResultContent::Text(text) => text.text.as_str(),
                // invariant: has_images is false here
                ToolResultContent::Image(_) => "",
            })
            .collect::<Vec<_>>()
            .join("\n");
        return json!(sanitize_surrogates(&joined));
    }

    let mut blocks: Vec<Value> = content
        .iter()
        .map(|block| match block {
            ToolResultContent::Text(text) => {
                json!({"type": "text", "text": sanitize_surrogates(&text.text)})
            }
            ToolResultContent::Image(image) => json!({
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": image.mime_type,
                    "data": image.data,
                },
            }),
        })
        .collect();
    let has_text = blocks
        .iter()
        .any(|block| block.get("type").and_then(Value::as_str) == Some("text"));
    if !has_text {
        blocks.insert(0, json!({"type": "text", "text": "(see attached image)"}));
    }
    Value::Array(blocks)
}

/// `convertToolResult`: a tool_result block plus any displaced sibling content
/// (tool references cannot mix with ordinary tool-result content).
fn convert_tool_result(
    msg: &ToolResultMessage,
    is_oauth_token: bool,
    deferred_tool_names: &HashSet<String>,
    loaded_tool_names: &mut HashSet<String>,
    normalize_tool_name: &dyn Fn(&str) -> String,
) -> (Value, Vec<Value>) {
    let mut references: Vec<Value> = Vec::new();
    for name in msg.added_tool_names.as_deref().unwrap_or(&[]) {
        let normalized_name = normalize_tool_name(name);
        if !deferred_tool_names.contains(&normalized_name)
            || loaded_tool_names.contains(&normalized_name)
        {
            continue;
        }
        loaded_tool_names.insert(normalized_name);
        references.push(json!({
            "type": "tool_reference",
            "tool_name": if is_oauth_token { to_claude_code_name(name) } else { name.clone() },
        }));
    }
    let converted_content = convert_content_blocks(&msg.content);
    let has_references = !references.is_empty();
    let tool_result = json!({
        "type": "tool_result",
        "tool_use_id": msg.tool_call_id,
        "content": if has_references { Value::Array(references) } else { converted_content.clone() },
        "is_error": msg.is_error,
    });
    let sibling_content = if !has_references {
        Vec::new()
    } else {
        match converted_content {
            Value::String(text) => vec![json!({"type": "text", "text": text})],
            Value::Array(blocks) => blocks,
            // invariant: convert_content_blocks returns only String or Array
            _ => Vec::new(),
        }
    };
    (tool_result, sibling_content)
}

/// `convertMessages`.
fn convert_messages(
    transformed_messages: &[Message],
    is_oauth_token: bool,
    cache_control: Option<&Value>,
    allow_empty_signature: bool,
    deferred_tool_names: &HashSet<String>,
    normalize_tool_name: &dyn Fn(&str) -> String,
) -> Vec<Value> {
    let mut params: Vec<Value> = Vec::new();
    let mut loaded_tool_names: HashSet<String> = HashSet::new();

    let mut i = 0;
    while i < transformed_messages.len() {
        let msg = &transformed_messages[i];
        match msg {
            Message::User(user) => match &user.content {
                UserContent::Text(text) => {
                    if !text.trim().is_empty() {
                        params.push(json!({
                            "role": "user",
                            "content": sanitize_surrogates(text),
                        }));
                    }
                }
                UserContent::Blocks(content) => {
                    let blocks: Vec<Value> = content
                        .iter()
                        .map(|item| match item {
                            UserContentBlock::Text(text) => json!({
                                "type": "text",
                                "text": sanitize_surrogates(&text.text),
                            }),
                            UserContentBlock::Image(image) => json!({
                                "type": "image",
                                "source": {
                                    "type": "base64",
                                    "media_type": image.mime_type,
                                    "data": image.data,
                                },
                            }),
                        })
                        .filter(|block| {
                            block.get("type").and_then(Value::as_str) != Some("text")
                                || !block["text"].as_str().unwrap_or("").trim().is_empty()
                        })
                        .collect();
                    if blocks.is_empty() {
                        i += 1;
                        continue;
                    }
                    params.push(json!({"role": "user", "content": blocks}));
                }
            },
            Message::Assistant(assistant) => {
                let mut blocks: Vec<Value> = Vec::new();
                for block in &assistant.content {
                    match block {
                        AssistantContent::Text(text) => {
                            if text.text.trim().is_empty() {
                                continue;
                            }
                            blocks.push(json!({
                                "type": "text",
                                "text": sanitize_surrogates(&text.text),
                            }));
                        }
                        AssistantContent::Thinking(thinking) => {
                            // Redacted thinking: pass the opaque payload back.
                            if thinking.redacted.unwrap_or(false) {
                                let mut redacted = json!({"type": "redacted_thinking"});
                                if let Some(signature) = &thinking.thinking_signature {
                                    redacted["data"] = json!(signature);
                                }
                                blocks.push(redacted);
                                continue;
                            }
                            let signature = thinking.thinking_signature.as_deref().unwrap_or("");
                            let has_signature = !signature.trim().is_empty();
                            if thinking.thinking.trim().is_empty() && !has_signature {
                                continue;
                            }
                            // Missing/empty signature (e.g. aborted stream):
                            // convert to plain text unless the model is marked
                            // as accepting empty signatures.
                            if !has_signature {
                                if allow_empty_signature {
                                    blocks.push(json!({
                                        "type": "thinking",
                                        "thinking": sanitize_surrogates(&thinking.thinking),
                                        "signature": "",
                                    }));
                                } else {
                                    blocks.push(json!({
                                        "type": "text",
                                        "text": sanitize_surrogates(&thinking.thinking),
                                    }));
                                }
                            } else {
                                blocks.push(json!({
                                    "type": "thinking",
                                    "thinking": sanitize_surrogates(&thinking.thinking),
                                    "signature": signature,
                                }));
                            }
                        }
                        AssistantContent::ToolCall(call) => {
                            blocks.push(json!({
                                "type": "tool_use",
                                "id": call.id,
                                "name": if is_oauth_token { to_claude_code_name(&call.name) } else { call.name.clone() },
                                "input": call.arguments,
                            }));
                        }
                    }
                }
                if blocks.is_empty() {
                    i += 1;
                    continue;
                }
                params.push(json!({"role": "assistant", "content": blocks}));
            }
            Message::ToolResult(_) => {
                // Collect all consecutive toolResult messages (z.ai Anthropic
                // endpoint requires them grouped in one user message).
                let mut tool_results: Vec<Value> = Vec::new();
                let mut sibling_content: Vec<Value> = Vec::new();
                let mut j = i;
                while j < transformed_messages.len() {
                    let Message::ToolResult(result) = &transformed_messages[j] else {
                        break;
                    };
                    let (tool_result, siblings) = convert_tool_result(
                        result,
                        is_oauth_token,
                        deferred_tool_names,
                        &mut loaded_tool_names,
                        normalize_tool_name,
                    );
                    tool_results.push(tool_result);
                    sibling_content.extend(siblings);
                    j += 1;
                }
                i = j - 1;
                // Displaced reference-bearing results follow every tool_result.
                let mut content = tool_results;
                content.extend(sibling_content);
                params.push(json!({"role": "user", "content": content}));
            }
        }
        i += 1;
    }

    // Add cache_control to the last user message to cache conversation history.
    if let (Some(cache_control), Some(last_message)) = (cache_control, params.last_mut()) {
        if last_message.get("role").and_then(Value::as_str) == Some("user") {
            match last_message.get_mut("content") {
                Some(Value::Array(blocks)) => {
                    if let Some(last_block) = blocks.last_mut() {
                        if matches!(
                            last_block.get("type").and_then(Value::as_str),
                            Some("text") | Some("image") | Some("tool_result")
                        ) {
                            last_block["cache_control"] = cache_control.clone();
                        }
                    }
                }
                Some(Value::String(text)) => {
                    let text = text.clone();
                    last_message["content"] = json!([{
                        "type": "text",
                        "text": text,
                        "cache_control": cache_control,
                    }]);
                }
                _ => {}
            }
        }
    }

    params
}

fn should_use_fine_grained_tool_streaming_beta(model: &Model, context: &Context) -> bool {
    context
        .tools
        .as_ref()
        .is_some_and(|tools| !tools.is_empty())
        && !get_anthropic_compat(model).supports_eager_tool_input_streaming
}

/// `convertTools`.
fn convert_tools(
    tools: &[Tool],
    is_oauth_token: bool,
    supports_eager_tool_input_streaming: bool,
    supports_strict_tools: bool,
    cache_control: Option<&Value>,
    defer_loading: bool,
) -> Result<Vec<Value>, String> {
    tools
        .iter()
        .enumerate()
        .map(|(index, tool)| {
            let strict = resolve_json_schema_strict_sampling(tool, supports_strict_tools)?;
            let schema = &tool.parameters;
            let legacy_input_schema = json!({
                "type": "object",
                "properties": schema.get("properties").cloned().unwrap_or_else(|| json!({})),
                "required": schema.get("required").cloned().unwrap_or_else(|| json!([])),
            });
            let input_schema = if strict == Some(true) {
                // { ...tool.parameters, ...legacyInputSchema } — legacy wins.
                let mut base = schema.as_object().cloned().unwrap_or_default();
                if let Value::Object(legacy) = &legacy_input_schema {
                    for (key, value) in legacy {
                        base.insert(key.clone(), value.clone());
                    }
                }
                Value::Object(base)
            } else {
                legacy_input_schema
            };

            let mut converted = json!({
                "name": if is_oauth_token { to_claude_code_name(&tool.name) } else { tool.name.clone() },
                "description": tool.description,
                "input_schema": input_schema,
            });
            if supports_eager_tool_input_streaming {
                converted["eager_input_streaming"] = json!(true);
            }
            if strict == Some(true) {
                converted["strict"] = json!(true);
            }
            if defer_loading {
                converted["defer_loading"] = json!(true);
            }
            if let (Some(cache_control), true) = (cache_control, index == tools.len() - 1) {
                converted["cache_control"] = cache_control.clone();
            }
            Ok(converted)
        })
        .collect()
}

fn text_block(text: &str, cache_control: Option<&Value>) -> Value {
    let mut block = json!({"type": "text", "text": text});
    if let Some(cache_control) = cache_control {
        block["cache_control"] = cache_control.clone();
    }
    block
}

/// `buildParams`.
fn build_params(
    model: &Model,
    context: &Context,
    is_oauth_token: bool,
    options: &AnthropicOptions,
) -> Result<Value, String> {
    let (_retention, cache_control) = get_cache_control(
        model,
        options.stream.cache_retention,
        options.stream.env.as_ref(),
    );
    let compat = get_anthropic_compat(model);
    let transformed_messages = transform_messages(
        &context.messages,
        model,
        Some(&mut |id: &str, _model: &Model, _msg: &AssistantMessage| normalize_tool_call_id(id)),
    );
    let normalize_tool_name = |name: &str| {
        if is_oauth_token {
            to_claude_code_name(name)
        } else {
            name.to_owned()
        }
    };
    let transformed_context = Context {
        system_prompt: context.system_prompt.clone(),
        messages: transformed_messages.clone(),
        tools: context.tools.clone(),
    };
    let placement = split_deferred_tools(
        &transformed_context,
        compat.supports_tool_references,
        normalize_tool_name,
    );
    let mut immediate_tools = placement.immediate;
    let mut deferred_tools: Vec<Tool> = placement
        .deferred
        .into_iter()
        .map(|(_, tool)| tool)
        .collect();
    if immediate_tools.is_empty() && !deferred_tools.is_empty() {
        immediate_tools = deferred_tools;
        deferred_tools = Vec::new();
    }
    let deferred_tool_names: HashSet<String> = deferred_tools
        .iter()
        .map(|tool| normalize_tool_name(&tool.name))
        .collect();

    let mut params = json!({
        "model": model.id,
        "messages": convert_messages(
            &transformed_messages,
            is_oauth_token,
            cache_control.as_ref(),
            compat.allow_empty_signature,
            &deferred_tool_names,
            &normalize_tool_name,
        ),
        "max_tokens": options.stream.max_tokens.unwrap_or(model.max_tokens),
        "stream": true,
    });

    // For OAuth tokens, we MUST include the Claude Code identity.
    if is_oauth_token {
        let mut system = vec![text_block(
            "You are Claude Code, Anthropic's official CLI for Claude.",
            cache_control.as_ref(),
        )];
        if let Some(system_prompt) = &context.system_prompt {
            system.push(text_block(
                sanitize_surrogates(system_prompt),
                cache_control.as_ref(),
            ));
        }
        params["system"] = Value::Array(system);
    } else if let Some(system_prompt) = &context.system_prompt {
        params["system"] = json!([text_block(
            sanitize_surrogates(system_prompt),
            cache_control.as_ref(),
        )]);
    }

    // Temperature is incompatible with extended thinking and unsupported on
    // Claude Opus 4.7+.
    if let Some(temperature) = options.stream.temperature {
        if options.thinking_enabled != Some(true) && compat.supports_temperature {
            params["temperature"] = json!(temperature);
        }
    }

    if !immediate_tools.is_empty() || !deferred_tools.is_empty() {
        let mut tools = convert_tools(
            &immediate_tools,
            is_oauth_token,
            compat.supports_eager_tool_input_streaming,
            compat.supports_strict_tools,
            if compat.supports_cache_control_on_tools {
                cache_control.as_ref()
            } else {
                None
            },
            false,
        )?;
        tools.extend(convert_tools(
            &deferred_tools,
            is_oauth_token,
            compat.supports_eager_tool_input_streaming,
            compat.supports_strict_tools,
            None,
            true,
        )?);
        params["tools"] = Value::Array(tools);
    }

    // Configure thinking mode: adaptive, budget-based, or explicitly disabled.
    if model.reasoning {
        if options.thinking_enabled == Some(true) {
            // Default to "summarized" so Opus 4.7 and Mythos Preview behave
            // like older Claude 4 models (whose API default is also
            // "summarized").
            let display = options
                .thinking_display
                .unwrap_or(AnthropicThinkingDisplay::Summarized)
                .as_str();
            if model
                .compat
                .as_ref()
                .and_then(|c| c.force_adaptive_thinking)
                == Some(true)
            {
                // Adaptive thinking: Claude decides when and how much to think.
                params["thinking"] = json!({"type": "adaptive", "display": display});
                if let Some(effort) = &options.effort {
                    params["output_config"] = json!({"effort": effort});
                }
            } else {
                // Budget-based thinking for older models.
                let budget = options
                    .thinking_budget_tokens
                    .filter(|budget| *budget > 0)
                    .unwrap_or(1024);
                params["thinking"] = json!({
                    "type": "enabled",
                    "budget_tokens": budget,
                    "display": display,
                });
            }
        } else if options.thinking_enabled == Some(false)
            && model
                .thinking_level_map
                .as_ref()
                .and_then(|map| map.get(&ModelThinkingLevel::Off))
                != Some(&None)
        {
            params["thinking"] = json!({"type": "disabled"});
        }
    }

    if let Some(metadata) = &options.stream.metadata {
        if let Some(user_id) = metadata.get("user_id").and_then(Value::as_str) {
            params["metadata"] = json!({"user_id": user_id});
        }
    }

    if let Some(tool_choice) = &options.tool_choice {
        params["tool_choice"] = match tool_choice {
            AnthropicToolChoice::Auto => json!({"type": "auto"}),
            AnthropicToolChoice::Any => json!({"type": "any"}),
            AnthropicToolChoice::None => json!({"type": "none"}),
            AnthropicToolChoice::Tool { name } => json!({"type": "tool", "name": name}),
        };
    }

    Ok(params)
}

// ---------------------------------------------------------------------------
// Stop reason mapping
// ---------------------------------------------------------------------------

/// `mapStopReason`; unknown stop reasons are an error (the API may add new
/// values).
fn map_stop_reason(
    reason: &str,
    stop_details: Option<&Value>,
) -> Result<(StopReason, Option<String>), String> {
    match reason {
        "end_turn" => Ok((StopReason::Stop, None)),
        "max_tokens" => Ok((StopReason::Length, None)),
        "tool_use" => Ok((StopReason::ToolUse, None)),
        "refusal" => Ok((
            StopReason::Error,
            Some(
                stop_details
                    .and_then(|details| details.get("explanation"))
                    .and_then(Value::as_str)
                    .unwrap_or("The model refused to complete the request")
                    .to_owned(),
            ),
        )),
        // Stop is good enough -> resubmit.
        "pause_turn" => Ok((StopReason::Stop, None)),
        // We don't supply stop sequences, so this should never happen.
        "stop_sequence" => Ok((StopReason::Stop, None)),
        // Content flagged by safety filters (not yet in SDK types).
        "sensitive" => Ok((
            StopReason::Error,
            Some("Provider stopped with: sensitive".to_owned()),
        )),
        other => Err(format!("Unhandled stop reason: {other}")),
    }
}

// ---------------------------------------------------------------------------
// Stream processing
// ---------------------------------------------------------------------------

fn json_u64(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_f64().map(|float| float as u64))
}

/// Per-content-block streaming scratch state (upstream stores `index` and
/// `partialJson` on the block itself; here they live alongside
/// `output.content`).
#[derive(Debug, Default)]
struct BlockState {
    event_index: usize,
    partial_json: String,
}

/// Consumes Anthropic SSE events and drives the [`StreamEvent`] protocol,
/// accumulating the final assistant message. Factored out of the HTTP layer
/// so tests can drive it with recorded streams.
struct StreamProcessor<'a> {
    output: &'a mut AssistantMessage,
    model: &'a Model,
    is_oauth_token: bool,
    tools: Option<&'a [Tool]>,
    saw_message_start: bool,
    saw_message_end: bool,
    block_states: Vec<BlockState>,
}

impl<'a> StreamProcessor<'a> {
    fn new(
        output: &'a mut AssistantMessage,
        model: &'a Model,
        is_oauth_token: bool,
        tools: Option<&'a [Tool]>,
    ) -> Self {
        Self {
            output,
            model,
            is_oauth_token,
            tools,
            saw_message_start: false,
            saw_message_end: false,
            block_states: Vec::new(),
        }
    }

    fn recompute_total_and_cost(&mut self) {
        // Anthropic doesn't provide total_tokens; compute from components.
        self.output.usage.total_tokens = self.output.usage.input
            + self.output.usage.output
            + self.output.usage.cache_read
            + self.output.usage.cache_write;
        calculate_cost(self.model, &mut self.output.usage);
    }

    /// `iterateAnthropicEvents` per-event body: filter, parse, flag, dispatch.
    fn handle_sse(
        &mut self,
        sse: &ServerSentEvent,
        events: &AssistantMessageEventStream,
    ) -> Result<(), String> {
        if sse.event.as_deref() == Some("error") {
            return Err(sse.data.clone());
        }
        let event_name = sse.event.as_deref().unwrap_or("");
        if !ANTHROPIC_MESSAGE_EVENTS.contains(&event_name) {
            return Ok(());
        }
        let event = parse_json_with_repair(&sse.data).map_err(|error| {
            format!(
                "Could not parse Anthropic SSE event {event_name}: {error}; data={}; raw={}",
                sse.data,
                sse.raw.join("\\n")
            )
        })?;
        let event_type = event.get("type").and_then(Value::as_str);
        if event_type == Some("message_start") {
            self.saw_message_start = true;
        } else if event_type == Some("message_stop") {
            self.saw_message_end = true;
        }
        self.handle_event(&event, events)
    }

    fn handle_event(
        &mut self,
        event: &Value,
        events: &AssistantMessageEventStream,
    ) -> Result<(), String> {
        match event.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                let message = &event["message"];
                if let Some(id) = message.get("id").and_then(Value::as_str) {
                    self.output.response_id = Some(id.to_owned());
                }
                // Capture initial token usage from message_start; this ensures
                // input token counts even if the stream is aborted early.
                let usage = &message["usage"];
                self.output.usage.input = json_u64(&usage["input_tokens"]).unwrap_or(0);
                self.output.usage.output = json_u64(&usage["output_tokens"]).unwrap_or(0);
                self.output.usage.cache_read =
                    json_u64(&usage["cache_read_input_tokens"]).unwrap_or(0);
                self.output.usage.cache_write =
                    json_u64(&usage["cache_creation_input_tokens"]).unwrap_or(0);
                self.output.usage.cache_write1h = Some(
                    json_u64(&usage["cache_creation"]["ephemeral_1h_input_tokens"]).unwrap_or(0),
                );
                self.recompute_total_and_cost();
            }
            Some("content_block_start") => {
                let event_index = json_u64(&event["index"]).unwrap_or(0) as usize;
                let block = &event["content_block"];
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        // 59ad3dead: the initial `text` of a content_block_start
                        // is part of the content, not a delta preamble — keep it.
                        let initial_text = block
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned();
                        self.output.content.push(AssistantContent::Text(
                            crate::types::TextContent {
                                text: initial_text,
                                text_signature: None,
                            },
                        ));
                        self.block_states.push(BlockState {
                            event_index,
                            ..BlockState::default()
                        });
                        events.push(StreamEvent::TextStart {
                            content_index: self.output.content.len() - 1,
                            partial: self.output.clone(),
                        });
                    }
                    Some("thinking") => {
                        // 59ad3dead: keep the initial `thinking`/`signature`
                        // carried by content_block_start.
                        self.output.content.push(AssistantContent::Thinking(
                            crate::types::ThinkingContent {
                                thinking: block
                                    .get("thinking")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_owned(),
                                thinking_signature: Some(
                                    block
                                        .get("signature")
                                        .and_then(Value::as_str)
                                        .unwrap_or_default()
                                        .to_owned(),
                                ),
                                redacted: None,
                            },
                        ));
                        self.block_states.push(BlockState {
                            event_index,
                            ..BlockState::default()
                        });
                        events.push(StreamEvent::ThinkingStart {
                            content_index: self.output.content.len() - 1,
                            partial: self.output.clone(),
                        });
                    }
                    Some("redacted_thinking") => {
                        self.output.content.push(AssistantContent::Thinking(
                            crate::types::ThinkingContent {
                                thinking: "[Reasoning redacted]".to_owned(),
                                thinking_signature: block
                                    .get("data")
                                    .and_then(Value::as_str)
                                    .map(str::to_owned),
                                redacted: Some(true),
                            },
                        ));
                        self.block_states.push(BlockState {
                            event_index,
                            ..BlockState::default()
                        });
                        events.push(StreamEvent::ThinkingStart {
                            content_index: self.output.content.len() - 1,
                            partial: self.output.clone(),
                        });
                    }
                    Some("tool_use") => {
                        let name = block
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        self.output.content.push(AssistantContent::ToolCall(
                            crate::types::ToolCall {
                                id: block
                                    .get("id")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_owned(),
                                name: if self.is_oauth_token {
                                    from_claude_code_name(name, self.tools)
                                } else {
                                    name.to_owned()
                                },
                                arguments: block
                                    .get("input")
                                    .and_then(Value::as_object)
                                    .cloned()
                                    .unwrap_or_default(),
                                thought_signature: None,
                                namespace: None,
                            },
                        ));
                        self.block_states.push(BlockState {
                            event_index,
                            ..BlockState::default()
                        });
                        events.push(StreamEvent::ToolCallStart {
                            content_index: self.output.content.len() - 1,
                            partial: self.output.clone(),
                        });
                    }
                    _ => {}
                }
            }
            Some("content_block_delta") => {
                let event_index = json_u64(&event["index"]).unwrap_or(0) as usize;
                let delta = &event["delta"];
                let Some(content_index) = self
                    .block_states
                    .iter()
                    .position(|state| state.event_index == event_index)
                else {
                    return Ok(());
                };
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => {
                        let text = delta.get("text").and_then(Value::as_str).unwrap_or("");
                        if let Some(AssistantContent::Text(block)) =
                            self.output.content.get_mut(content_index)
                        {
                            block.text.push_str(text);
                            events.push(StreamEvent::TextDelta {
                                content_index,
                                delta: text.to_owned(),
                                partial: self.output.clone(),
                            });
                        }
                    }
                    Some("thinking_delta") => {
                        let thinking = delta.get("thinking").and_then(Value::as_str).unwrap_or("");
                        if let Some(AssistantContent::Thinking(block)) =
                            self.output.content.get_mut(content_index)
                        {
                            block.thinking.push_str(thinking);
                            events.push(StreamEvent::ThinkingDelta {
                                content_index,
                                delta: thinking.to_owned(),
                                partial: self.output.clone(),
                            });
                        }
                    }
                    Some("input_json_delta") => {
                        let partial_json = delta
                            .get("partial_json")
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        self.block_states[content_index]
                            .partial_json
                            .push_str(partial_json);
                        if let Some(AssistantContent::ToolCall(block)) =
                            self.output.content.get_mut(content_index)
                        {
                            block.arguments = parse_streaming_json(Some(
                                &self.block_states[content_index].partial_json,
                            ));
                            events.push(StreamEvent::ToolCallDelta {
                                content_index,
                                delta: partial_json.to_owned(),
                                partial: self.output.clone(),
                            });
                        }
                    }
                    Some("signature_delta") => {
                        let signature =
                            delta.get("signature").and_then(Value::as_str).unwrap_or("");
                        if let Some(AssistantContent::Thinking(block)) =
                            self.output.content.get_mut(content_index)
                        {
                            block
                                .thinking_signature
                                .get_or_insert_with(String::new)
                                .push_str(signature);
                        }
                    }
                    _ => {}
                }
            }
            Some("content_block_stop") => {
                let event_index = json_u64(&event["index"]).unwrap_or(0) as usize;
                let Some(content_index) = self
                    .block_states
                    .iter()
                    .position(|state| state.event_index == event_index)
                else {
                    return Ok(());
                };
                match self.output.content.get_mut(content_index) {
                    Some(AssistantContent::Text(block)) => {
                        events.push(StreamEvent::TextEnd {
                            content_index,
                            content: block.text.clone(),
                            partial: self.output.clone(),
                        });
                    }
                    Some(AssistantContent::Thinking(block)) => {
                        events.push(StreamEvent::ThinkingEnd {
                            content_index,
                            content: block.thinking.clone(),
                            partial: self.output.clone(),
                        });
                    }
                    Some(AssistantContent::ToolCall(block)) => {
                        block.arguments = parse_streaming_json(Some(
                            &self.block_states[content_index].partial_json,
                        ));
                        events.push(StreamEvent::ToolCallEnd {
                            content_index,
                            tool_call: block.clone(),
                            partial: self.output.clone(),
                        });
                    }
                    None => {}
                }
            }
            Some("message_delta") => {
                let delta = &event["delta"];
                if let Some(reason) = delta.get("stop_reason").and_then(Value::as_str) {
                    if !reason.is_empty() {
                        // 926eb15c1: preserve the raw provider reason before
                        // mapping.
                        self.output.raw_stop_reason = Some(reason.to_owned());
                        let (stop_reason, error_message) =
                            map_stop_reason(reason, delta.get("stop_details"))?;
                        self.output.stop_reason = stop_reason;
                        if let Some(error_message) = error_message {
                            self.output.error_message = Some(error_message);
                        }
                    }
                }
                // Only update usage fields if present (not null). Preserves
                // input_tokens from message_start when proxies omit it in
                // message_delta.
                if let Some(usage) = event.get("usage") {
                    if let Some(input) = json_u64(&usage["input_tokens"]) {
                        self.output.usage.input = input;
                    }
                    if let Some(output) = json_u64(&usage["output_tokens"]) {
                        self.output.usage.output = output;
                    }
                    if let Some(cache_read) = json_u64(&usage["cache_read_input_tokens"]) {
                        self.output.usage.cache_read = cache_read;
                    }
                    if let Some(cache_write) = json_u64(&usage["cache_creation_input_tokens"]) {
                        self.output.usage.cache_write = cache_write;
                    }
                    // Reasoning tokens arrive in
                    // `output_tokens_details.thinking_tokens` on the final
                    // message_delta usage (a subset of output_tokens).
                    if let Some(thinking_tokens) =
                        json_u64(&usage["output_tokens_details"]["thinking_tokens"])
                    {
                        self.output.usage.reasoning = Some(thinking_tokens);
                    }
                }
                self.recompute_total_and_cost();
            }
            _ => {}
        }
        Ok(())
    }

    /// Post-stream validation (upstream `iterateAnthropicEvents` tail plus the
    /// stream function's final checks).
    fn finish(self, signal: Option<&CancellationToken>) -> Result<DoneReason, String> {
        if self.saw_message_start && !self.saw_message_end {
            return Err("Anthropic stream ended before message_stop".to_owned());
        }
        if signal.is_some_and(|signal| signal.is_cancelled()) {
            return Err("Request was aborted".to_owned());
        }
        match self.output.stop_reason {
            // `Deferred` shares the `Pending` arm: no rpi provider produces it
            // (lifecycle is [DEFER], R2.2.1), so it is unreachable here and
            // treated as "stream ended without a usable stop reason".
            StopReason::Pending | StopReason::Deferred => {
                Err("Anthropic stream ended without a stop reason".to_owned())
            }
            StopReason::Aborted | StopReason::Error => Err(self
                .output
                .error_message
                .clone()
                .unwrap_or_else(|| "An unknown error occurred".to_owned())),
            StopReason::Stop => Ok(DoneReason::Stop),
            StopReason::Length => Ok(DoneReason::Length),
            StopReason::ToolUse => Ok(DoneReason::ToolUse),
        }
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn initial_output(model: &Model) -> AssistantMessage {
    AssistantMessage {
        role: crate::types::AssistantRole::Assistant,
        content: vec![],
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        response_model: None,
        response_id: None,
        diagnostics: None,
        usage: Usage::default(),
        stop_reason: StopReason::Pending,
        error_message: None,
        timestamp: now_ms(),
        deferred: None,
        end_turn: None,
        raw_stop_reason: None,
    }
}

/// The streaming body: everything that runs inside upstream's async IIFE.
/// Errors return the upstream `Error.message`; `output` carries the partial
/// message either way.
async fn run(
    model: &Model,
    context: &Context,
    options: &AnthropicOptions,
    output: &mut AssistantMessage,
    events: &AssistantMessageEventStream,
) -> Result<DoneReason, String> {
    let api_key = options.stream.api_key.as_deref();
    assert_request_auth(&model.provider, api_key, options.stream.headers.as_ref())?;

    let dynamic_headers = if model.provider == "github-copilot" {
        let has_images = has_copilot_vision_input(&context.messages);
        Some(build_copilot_dynamic_headers(&context.messages, has_images))
    } else {
        None
    };

    let (retention, _cache_control) = get_cache_control(
        model,
        options.stream.cache_retention,
        options.stream.env.as_ref(),
    );
    let cache_session_id = if retention == CacheRetention::None {
        None
    } else {
        options.stream.session_id.as_deref()
    };

    let (headers, is_oauth_token) = build_request_headers(
        model,
        api_key,
        options.interleaved_thinking.unwrap_or(true),
        should_use_fine_grained_tool_streaming_beta(model, context),
        options.stream.headers.as_ref(),
        dynamic_headers,
        cache_session_id,
    );

    let mut params = build_params(model, context, is_oauth_token, options)?;
    if let Some(on_payload) = &options.stream.on_payload {
        if let Some(next_params) = on_payload(params.clone(), model).await {
            params = next_params;
        }
    }

    let url = format!("{}/v1/messages", model.base_url.trim_end_matches('/'));
    let header_map = provider_headers_to_header_map(&headers)?;
    let mut client_builder = reqwest::Client::builder();
    if let Some(timeout_ms) = options.stream.timeout_ms {
        client_builder = client_builder.timeout(std::time::Duration::from_millis(timeout_ms));
    }
    let client = client_builder.build().map_err(|error| error.to_string())?;

    let response = retry_provider_request(
        || {
            let request = client.post(&url).headers(header_map.clone()).json(&params);
            let signal = options.stream.signal.clone();
            let fetch = options.stream.fetch.clone();
            async move {
                // 027a58479 (R2.7.4): per-request custom fetch channel; `None`
                // keeps the reqwest default path unchanged.
                let result = send_provider_request(request, fetch.as_ref(), signal.as_ref()).await;
                match result {
                    Ok(response) => {
                        let status = response.status();
                        if status.is_success() {
                            Ok(response)
                        } else {
                            let status = status.as_u16();
                            let response_headers = headers_to_record(response.headers());
                            let body = response.text().await.unwrap_or_default();
                            let normalized = NormalizedProviderError::new(
                                Some(status),
                                Some(body),
                                format!("Request failed with status {status}"),
                            );
                            Err(ProviderErrorInfo {
                                status: Some(status),
                                headers: Some(response_headers),
                                message: format_provider_error(&normalized, None),
                            })
                        }
                    }
                    Err(error) => Err(error.into_provider_error_info()),
                }
            }
        },
        ProviderRetryOptions {
            max_retries: options.stream.max_retries,
            max_retry_delay_ms: options.stream.max_retry_delay_ms,
        },
        options.stream.signal.as_ref(),
    )
    .await
    .map_err(|error| error.message())?;

    if let Some(on_response) = &options.stream.on_response {
        on_response(
            ProviderResponse {
                status: response.status().as_u16(),
                headers: headers_to_record(response.headers()),
            },
            model,
        )
        .await;
    }

    events.push(StreamEvent::Start {
        partial: output.clone(),
    });

    let mut processor =
        StreamProcessor::new(output, model, is_oauth_token, context.tools.as_deref());
    let mut decoder = SseDecoder::new();
    let mut byte_stream = response.bytes_stream();
    while let Some(chunk) = byte_stream.next().await {
        if options
            .stream
            .signal
            .as_ref()
            .is_some_and(|signal| signal.is_cancelled())
        {
            return Err("Request was aborted".to_owned());
        }
        let bytes = chunk.map_err(|error| error.to_string())?;
        for sse in decoder.feed(&bytes) {
            processor.handle_sse(&sse, events)?;
        }
    }
    for sse in decoder.finish() {
        processor.handle_sse(&sse, events)?;
    }
    processor.finish(options.stream.signal.as_ref())
}

/// `stream` (anthropic-messages).
pub fn stream(
    model: &Model,
    context: &Context,
    options: AnthropicOptions,
) -> AssistantMessageEventStream {
    let event_stream = AssistantMessageEventStream::new();
    let task_stream = event_stream.clone();
    let model = model.clone();
    let context = context.clone();
    tokio::spawn(async move {
        let signal = options.stream.signal.clone();
        let mut output = initial_output(&model);
        match run(&model, &context, &options, &mut output, &task_stream).await {
            Ok(reason) => {
                task_stream.push(StreamEvent::Done {
                    reason,
                    message: output,
                });
                task_stream.end(None);
            }
            Err(message) => {
                let aborted = signal.as_ref().is_some_and(|signal| signal.is_cancelled());
                output.stop_reason = if aborted {
                    StopReason::Aborted
                } else {
                    StopReason::Error
                };
                output.error_message = Some(message);
                task_stream.push(StreamEvent::Error {
                    reason: if aborted {
                        ErrorReason::Aborted
                    } else {
                        ErrorReason::Error
                    },
                    error: output,
                });
                task_stream.end(None);
            }
        }
    });
    event_stream
}

/// `mapThinkingLevelToEffort`: ThinkingLevel → Anthropic effort for adaptive
/// thinking. Note: effort "max" is available on all adaptive-thinking Claude
/// models, while native "xhigh" is only on Opus 4.7/4.8, Sonnet 5, Fable 5.
fn map_thinking_level_to_effort(model: &Model, level: Option<ThinkingLevel>) -> String {
    if let Some(level) = level {
        let key = match level {
            ThinkingLevel::Minimal => ModelThinkingLevel::Minimal,
            ThinkingLevel::Low => ModelThinkingLevel::Low,
            ThinkingLevel::Medium => ModelThinkingLevel::Medium,
            ThinkingLevel::High => ModelThinkingLevel::High,
            ThinkingLevel::Xhigh => ModelThinkingLevel::Xhigh,
            ThinkingLevel::Max => ModelThinkingLevel::Max,
        };
        if let Some(Some(mapped)) = model
            .thinking_level_map
            .as_ref()
            .and_then(|map| map.get(&key))
        {
            return mapped.clone();
        }
    }
    match level {
        Some(ThinkingLevel::Minimal) | Some(ThinkingLevel::Low) => "low",
        Some(ThinkingLevel::Medium) => "medium",
        _ => "high",
    }
    .to_owned()
}

/// `streamSimple` (anthropic-messages).
pub fn stream_simple(
    model: &Model,
    context: &Context,
    options: Option<SimpleStreamOptions>,
) -> AssistantMessageEventStream {
    if let Err(message) = assert_request_auth(
        &model.provider,
        options.as_ref().and_then(|o| o.stream.api_key.as_deref()),
        options.as_ref().and_then(|o| o.stream.headers.as_ref()),
    ) {
        return immediate_error_stream(model, &message);
    }

    let api_key = options.as_ref().and_then(|o| o.stream.api_key.clone());
    let base = build_base_options(model, context, options.as_ref(), api_key);
    let Some(reasoning) = options.as_ref().and_then(|o| o.reasoning) else {
        return stream(
            model,
            context,
            AnthropicOptions {
                stream: base,
                thinking_enabled: Some(false),
                ..AnthropicOptions::default()
            },
        );
    };

    // Models with adaptive thinking use an effort level; older models use
    // budget-based thinking.
    if model
        .compat
        .as_ref()
        .and_then(|c| c.force_adaptive_thinking)
        == Some(true)
    {
        let effort = map_thinking_level_to_effort(model, Some(reasoning));
        return stream(
            model,
            context,
            AnthropicOptions {
                stream: base,
                thinking_enabled: Some(true),
                effort: Some(effort),
                ..AnthropicOptions::default()
            },
        );
    }

    let adjusted = adjust_max_tokens_for_thinking(
        base.max_tokens,
        model.max_tokens,
        reasoning,
        options.as_ref().and_then(|o| o.thinking_budgets.as_ref()),
    );
    let max_tokens = clamp_max_tokens_to_context(model, context, adjusted.max_tokens);

    let mut anthropic_options = AnthropicOptions {
        stream: base,
        thinking_enabled: Some(true),
        thinking_budget_tokens: Some(
            adjusted
                .thinking_budget
                .min(max_tokens.saturating_sub(1024)),
        ),
        ..AnthropicOptions::default()
    };
    anthropic_options.stream.max_tokens = Some(max_tokens);
    stream(model, context, anthropic_options)
}

/// `ProviderStreams` implementation for `ApiKind::ANTHROPIC_MESSAGES`.
///
/// The trait carries plain [`StreamOptions`]; Anthropic-specific extras
/// ([`AnthropicOptions`]) reach [`stream`] only through direct calls or via
/// [`stream_simple`] reasoning mapping (design §3.3 collapses per-API extras).
#[derive(Debug, Clone, Copy, Default)]
pub struct AnthropicMessages;

impl ProviderStreams for AnthropicMessages {
    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: Option<StreamOptions>,
    ) -> AssistantMessageEventStream {
        stream(
            model,
            context,
            AnthropicOptions {
                stream: options.unwrap_or_default(),
                ..AnthropicOptions::default()
            },
        )
    }

    fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: Option<SimpleStreamOptions>,
    ) -> AssistantMessageEventStream {
        stream_simple(model, context, options)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use futures::StreamExt;
    use serde_json::json;

    use super::*;
    use crate::types::{ApiKind, AssistantRole, Usage};

    pub(crate) fn make_model(extra: serde_json::Value) -> Model {
        let mut value = json!({
            "id": "claude-sonnet-4-5", "name": "Sonnet", "api": "anthropic-messages",
            "provider": "anthropic", "baseUrl": "https://api.anthropic.com",
            "reasoning": true, "input": ["text"],
            "cost": {"input": 3.0, "output": 15.0, "cacheRead": 0.3, "cacheWrite": 3.75},
            "contextWindow": 200000, "maxTokens": 64000
        });
        value
            .as_object_mut()
            .expect("object")
            .extend(extra.as_object().cloned().unwrap_or_default());
        serde_json::from_value(value).expect("model")
    }

    fn assistant_message(
        content: Vec<AssistantContent>,
        provider: &str,
        model_id: &str,
    ) -> Message {
        Message::Assistant(AssistantMessage {
            role: AssistantRole::Assistant,
            content,
            api: ApiKind::from(ApiKind::ANTHROPIC_MESSAGES),
            provider: provider.to_owned(),
            model: model_id.to_owned(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: 0,
            deferred: None,
            end_turn: None,
            raw_stop_reason: None,
        })
    }

    fn user_text(text: &str) -> Message {
        serde_json::from_value(json!({
            "role": "user", "content": text, "timestamp": 0
        }))
        .expect("user")
    }

    fn tool(name: &str) -> Tool {
        serde_json::from_value(json!({
            "name": name, "description": "d",
            "parameters": {"type": "object", "properties": {"x": {"type": "string"}}, "required": ["x"]}
        }))
        .expect("tool")
    }

    fn tool_result(
        tool_call_id: &str,
        content: &str,
        added_tool_names: Option<Vec<&str>>,
    ) -> Message {
        serde_json::from_value(json!({
            "role": "toolResult", "toolCallId": tool_call_id, "toolName": "t",
            "content": [{"type": "text", "text": content}],
            "addedToolNames": added_tool_names,
            "isError": false, "timestamp": 0
        }))
        .expect("toolResult")
    }

    fn context(messages: Vec<Message>, tools: Option<Vec<Tool>>) -> Context {
        Context {
            system_prompt: None,
            messages,
            tools,
        }
    }

    // -----------------------------------------------------------------------
    // Pure helpers
    // -----------------------------------------------------------------------

    #[test]
    fn test_normalize_tool_call_id() {
        assert_eq!(
            normalize_tool_call_id("call_abc|item_xyz"),
            "call_abc_item_xyz"
        );
        assert_eq!(normalize_tool_call_id("fc_🙈x"), "fc__x");
        let long = "a".repeat(100);
        assert_eq!(normalize_tool_call_id(&long).len(), 64);
    }

    #[test]
    fn test_claude_code_name_mapping() {
        assert_eq!(to_claude_code_name("bash"), "Bash");
        assert_eq!(to_claude_code_name("BASH"), "Bash");
        assert_eq!(to_claude_code_name("custom_tool"), "custom_tool");

        let tools = vec![tool("BASH")];
        assert_eq!(from_claude_code_name("Bash", Some(&tools)), "BASH");
        assert_eq!(from_claude_code_name("Bash", None), "Bash");
        assert_eq!(from_claude_code_name("unknown", Some(&tools)), "unknown");
    }

    #[test]
    fn test_resolve_cache_retention() {
        assert_eq!(resolve_cache_retention(None, None), CacheRetention::Short);
        assert_eq!(
            resolve_cache_retention(Some(CacheRetention::None), None),
            CacheRetention::None
        );
        let env: ProviderEnv = [("RPI_CACHE_RETENTION".to_owned(), "long".to_owned())].into();
        assert_eq!(
            resolve_cache_retention(None, Some(&env)),
            CacheRetention::Long
        );
        // Explicit option wins over env.
        assert_eq!(
            resolve_cache_retention(Some(CacheRetention::Short), Some(&env)),
            CacheRetention::Short
        );
    }

    #[test]
    fn test_get_cache_control() {
        let model = make_model(json!({}));
        let (_, cc) = get_cache_control(&model, None, None);
        assert_eq!(cc, Some(json!({"type": "ephemeral"})));

        let (_, cc) = get_cache_control(&model, Some(CacheRetention::Long), None);
        assert_eq!(cc, Some(json!({"type": "ephemeral", "ttl": "1h"})));

        // Long retention unsupported by the model: plain ephemeral.
        let model = make_model(json!({"compat": {"supportsLongCacheRetention": false}}));
        let (_, cc) = get_cache_control(&model, Some(CacheRetention::Long), None);
        assert_eq!(cc, Some(json!({"type": "ephemeral"})));

        let (retention, cc) = get_cache_control(&model, Some(CacheRetention::None), None);
        assert_eq!(retention, CacheRetention::None);
        assert_eq!(cc, None);
    }

    #[test]
    fn test_default_supports_tool_references() {
        let mut m = make_model(json!({}));
        assert!(default_supports_tool_references(&m)); // claude-sonnet-4-5
        m.id = "claude-opus-4-1".to_owned();
        assert!(!default_supports_tool_references(&m));
        m.id = "claude-sonnet-4-0".to_owned();
        assert!(!default_supports_tool_references(&m));
        m.id = "claude-haiku-4-5".to_owned();
        assert!(!default_supports_tool_references(&m));
        m.id = "claude-fable-5".to_owned();
        assert!(default_supports_tool_references(&m));
        m.id = "claude-sonnet-4-5-20250929".to_owned();
        assert!(default_supports_tool_references(&m));
        m.provider = "bedrock".to_owned();
        m.id = "claude-sonnet-4-5".to_owned();
        assert!(!default_supports_tool_references(&m));
    }

    #[test]
    fn test_get_anthropic_compat_defaults() {
        let compat = get_anthropic_compat(&make_model(json!({})));
        assert!(compat.supports_eager_tool_input_streaming);
        assert!(compat.supports_long_cache_retention);
        assert!(!compat.send_session_affinity_headers);
        assert!(compat.supports_cache_control_on_tools);
        assert!(compat.supports_temperature);
        assert!(!compat.allow_empty_signature);
        assert!(!compat.supports_strict_tools);
        assert!(compat.supports_tool_references); // claude-sonnet-4-5 default
    }

    #[test]
    fn test_assert_request_auth() {
        assert!(assert_request_auth("p", Some("sk"), None).is_ok());
        let headers: ProviderHeaders =
            [("Authorization".to_owned(), Some("Bearer x".to_owned()))].into();
        assert!(assert_request_auth("p", None, Some(&headers)).is_ok());
        let empty: ProviderHeaders = [("authorization".to_owned(), Some("  ".to_owned()))].into();
        assert_eq!(
            assert_request_auth("p", None, Some(&empty)),
            Err("No API key for provider: p".to_owned())
        );
        assert_eq!(
            assert_request_auth("p", None, None),
            Err("No API key for provider: p".to_owned())
        );
    }

    #[test]
    fn test_build_request_headers_api_key() {
        let model = make_model(json!({}));
        let options_headers: ProviderHeaders =
            [("X-Custom".to_owned(), Some("1".to_owned()))].into();
        let (headers, is_oauth) = build_request_headers(
            &model,
            Some("sk-ant-api03-key"),
            true,
            false,
            Some(&options_headers),
            None,
            Some("session-1"),
        );
        assert!(!is_oauth);
        assert_eq!(
            headers.get("x-api-key").and_then(|v| v.as_deref()),
            Some("sk-ant-api03-key")
        );
        assert_eq!(
            headers.get("anthropic-version").and_then(|v| v.as_deref()),
            Some(ANTHROPIC_VERSION)
        );
        // Interleaved-thinking beta is requested by default.
        assert_eq!(
            headers.get("anthropic-beta").and_then(|v| v.as_deref()),
            Some(INTERLEAVED_THINKING_BETA)
        );
        // sendSessionAffinityHeaders defaults to false.
        assert!(!headers.contains_key("x-session-affinity"));
        assert_eq!(
            headers.get("X-Custom").and_then(|v| v.as_deref()),
            Some("1")
        );
    }

    #[test]
    fn test_build_request_headers_oauth() {
        let model = make_model(json!({}));
        let (headers, is_oauth) = build_request_headers(
            &model,
            Some("sk-ant-oat01-token"),
            true,
            true,
            None,
            None,
            None,
        );
        assert!(is_oauth);
        assert_eq!(
            headers.get("authorization").and_then(|v| v.as_deref()),
            Some("Bearer sk-ant-oat01-token")
        );
        assert_eq!(
            headers.get("user-agent").and_then(|v| v.as_deref()),
            Some("claude-cli/2.1.75")
        );
        assert_eq!(headers.get("x-app").and_then(|v| v.as_deref()), Some("cli"));
        let beta = headers
            .get("anthropic-beta")
            .and_then(|v| v.as_deref())
            .expect("beta");
        assert!(beta.starts_with("claude-code-20250219,oauth-2025-04-20,"));
        assert!(beta.contains(FINE_GRAINED_TOOL_STREAMING_BETA));
        assert!(beta.contains(INTERLEAVED_THINKING_BETA));
    }

    #[test]
    fn test_build_request_headers_adaptive_skips_interleaved_beta() {
        let model = make_model(json!({"compat": {"forceAdaptiveThinking": true}}));
        let (headers, _) =
            build_request_headers(&model, Some("sk-key"), true, false, None, None, None);
        assert!(!headers.contains_key("anthropic-beta"));
    }

    #[test]
    fn test_build_request_headers_session_affinity() {
        let model = make_model(json!({"compat": {"sendSessionAffinityHeaders": true}}));
        let (headers, _) = build_request_headers(
            &model,
            Some("sk-key"),
            false,
            false,
            None,
            None,
            Some("sess-9"),
        );
        assert_eq!(
            headers.get("x-session-affinity").and_then(|v| v.as_deref()),
            Some("sess-9")
        );
    }

    // -----------------------------------------------------------------------
    // Message conversion
    // -----------------------------------------------------------------------

    #[test]
    fn test_convert_messages_basic_and_cache_control() {
        let messages = vec![
            user_text("hello"),
            assistant_message(
                vec![AssistantContent::Text(crate::types::TextContent {
                    text: "hi there".to_owned(),
                    text_signature: None,
                })],
                "anthropic",
                "claude-sonnet-4-5",
            ),
            user_text("again"),
        ];
        let cc = json!({"type": "ephemeral"});
        let params = convert_messages(&messages, false, Some(&cc), false, &HashSet::new(), &|n| {
            n.to_owned()
        });
        assert_eq!(params.len(), 3);
        assert_eq!(params[0], json!({"role": "user", "content": "hello"}));
        assert_eq!(
            params[1],
            json!({"role": "assistant", "content": [{"type": "text", "text": "hi there"}]})
        );
        // cache_control lands on the last user message, wrapping string content.
        assert_eq!(
            params[2],
            json!({"role": "user", "content": [{"type": "text", "text": "again", "cache_control": {"type": "ephemeral"}}]})
        );
    }

    #[test]
    fn test_convert_messages_skips_empty() {
        let messages = vec![
            user_text("   "),
            assistant_message(vec![], "anthropic", "claude-sonnet-4-5"),
            user_text("real"),
        ];
        let params = convert_messages(&messages, false, None, false, &HashSet::new(), &|n| {
            n.to_owned()
        });
        assert_eq!(params.len(), 1);
        assert_eq!(params[0]["content"], json!("real"));
    }

    #[test]
    fn test_convert_messages_thinking_signature() {
        let thinking = |thinking: &str, signature: Option<&str>| {
            AssistantContent::Thinking(crate::types::ThinkingContent {
                thinking: thinking.to_owned(),
                thinking_signature: signature.map(str::to_owned),
                redacted: None,
            })
        };
        let messages = vec![assistant_message(
            vec![
                thinking("signed thought", Some("sig")),
                thinking("unsigned thought", None),
                thinking("   ", None),
                thinking("", Some("")),
            ],
            "anthropic",
            "claude-sonnet-4-5",
        )];
        let params = convert_messages(&messages, false, None, false, &HashSet::new(), &|n| {
            n.to_owned()
        });
        let blocks = params[0]["content"].as_array().expect("blocks");
        assert_eq!(
            blocks[0],
            json!({"type": "thinking", "thinking": "signed thought", "signature": "sig"})
        );
        // Missing signature → plain text.
        assert_eq!(
            blocks[1],
            json!({"type": "text", "text": "unsigned thought"})
        );
        // Empty thinking without signature is dropped entirely.
        assert_eq!(blocks.len(), 2);

        // allowEmptySignature keeps the block as thinking with "".
        let params = convert_messages(&messages, false, None, true, &HashSet::new(), &|n| {
            n.to_owned()
        });
        let blocks = params[0]["content"].as_array().expect("blocks");
        assert_eq!(
            blocks[1],
            json!({"type": "thinking", "thinking": "unsigned thought", "signature": ""})
        );
    }

    #[test]
    fn test_convert_messages_redacted_thinking() {
        let messages = vec![assistant_message(
            vec![AssistantContent::Thinking(crate::types::ThinkingContent {
                thinking: "[Reasoning redacted]".to_owned(),
                thinking_signature: Some("opaque".to_owned()),
                redacted: Some(true),
            })],
            "anthropic",
            "claude-sonnet-4-5",
        )];
        let params = convert_messages(&messages, false, None, false, &HashSet::new(), &|n| {
            n.to_owned()
        });
        assert_eq!(
            params[0]["content"][0],
            json!({"type": "redacted_thinking", "data": "opaque"})
        );
    }

    #[test]
    fn test_convert_messages_tool_results_grouped() {
        let messages = vec![
            tool_result("call_1", "one", None),
            tool_result("call_2", "two", None),
            user_text("next"),
        ];
        let params = convert_messages(&messages, false, None, false, &HashSet::new(), &|n| {
            n.to_owned()
        });
        assert_eq!(params.len(), 2);
        let content = params[0]["content"].as_array().expect("content");
        assert_eq!(
            content[0],
            json!({"type": "tool_result", "tool_use_id": "call_1", "content": "one", "is_error": false})
        );
        assert_eq!(content[1]["tool_use_id"], json!("call_2"));
        assert_eq!(params[0]["role"], json!("user"));
        assert_eq!(params[1]["content"], json!("next"));
    }

    #[test]
    fn test_convert_messages_tool_references() {
        let deferred: HashSet<String> = ["search".to_owned()].into();
        let messages = vec![tool_result("call_1", "result body", Some(vec!["search"]))];
        let params = convert_messages(&messages, false, None, false, &deferred, &|n| n.to_owned());
        let content = params[0]["content"].as_array().expect("content");
        // Reference replaces the tool result content; the original content is
        // displaced into a sibling block.
        assert_eq!(
            content[0],
            json!({
                "type": "tool_result", "tool_use_id": "call_1",
                "content": [{"type": "tool_reference", "tool_name": "search"}],
                "is_error": false
            })
        );
        assert_eq!(content[1], json!({"type": "text", "text": "result body"}));
    }

    #[test]
    fn test_convert_messages_oauth_tool_names() {
        let messages = vec![assistant_message(
            vec![AssistantContent::ToolCall(crate::types::ToolCall {
                id: "toolu_1".to_owned(),
                name: "bash".to_owned(),
                arguments: serde_json::Map::new(),
                thought_signature: None,
                namespace: None,
            })],
            "anthropic",
            "claude-sonnet-4-5",
        )];
        let params = convert_messages(
            &messages,
            true,
            None,
            false,
            &HashSet::new(),
            &to_claude_code_name,
        );
        assert_eq!(params[0]["content"][0]["name"], json!("Bash"));
        let params = convert_messages(&messages, false, None, false, &HashSet::new(), &|n| {
            n.to_owned()
        });
        assert_eq!(params[0]["content"][0]["name"], json!("bash"));
    }

    #[test]
    fn test_convert_tools() {
        let tools = vec![tool("a"), tool("b")];
        let cc = json!({"type": "ephemeral"});
        let converted = convert_tools(&tools, false, true, false, Some(&cc), false).expect("tools");
        assert_eq!(converted.len(), 2);
        assert_eq!(
            converted[0]["input_schema"],
            json!({"type": "object", "properties": {"x": {"type": "string"}}, "required": ["x"]})
        );
        assert_eq!(converted[0]["eager_input_streaming"], json!(true));
        assert!(converted[0].get("cache_control").is_none());
        // cache_control only on the last tool.
        assert_eq!(converted[1]["cache_control"], cc);

        // Deferred loading; no eager streaming flag when unsupported.
        let converted = convert_tools(&tools[..1], false, false, false, None, true).expect("tools");
        assert_eq!(converted[0]["defer_loading"], json!(true));
        assert!(converted[0].get("eager_input_streaming").is_none());
    }

    #[test]
    fn test_convert_tools_strict() {
        let strict_tool: Tool = serde_json::from_value(json!({
            "name": "s", "description": "d",
            "parameters": {"type": "object", "properties": {"x": {"type": "string"}}, "required": ["x"], "additionalProperties": false},
            "constrainedSampling": {"type": "json_schema", "strict": "prefer"}
        }))
        .expect("tool");
        // Strict supported: full schema is kept and `strict: true` added.
        let converted = convert_tools(
            std::slice::from_ref(&strict_tool),
            false,
            true,
            true,
            None,
            false,
        )
        .expect("tools");
        assert_eq!(converted[0]["strict"], json!(true));
        assert_eq!(
            converted[0]["input_schema"]["additionalProperties"],
            json!(false)
        );
        // Strict unsupported with "prefer": silently degrades.
        let converted = convert_tools(
            std::slice::from_ref(&strict_tool),
            false,
            true,
            false,
            None,
            false,
        )
        .expect("tools");
        assert!(converted[0].get("strict").is_none());
        assert!(converted[0]["input_schema"]
            .get("additionalProperties")
            .is_none());

        // "require" unsupported → error.
        let require_tool: Tool = serde_json::from_value(json!({
            "name": "r", "description": "d",
            "parameters": {"type": "object", "properties": {}, "required": []},
            "constrainedSampling": {"type": "json_schema", "strict": "require"}
        }))
        .expect("tool");
        assert!(convert_tools(&[require_tool], false, true, false, None, false).is_err());
    }

    // -----------------------------------------------------------------------
    // build_params
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_params_oauth_system_and_identity() {
        let model = make_model(json!({}));
        let mut ctx = context(vec![user_text("hi")], Some(vec![tool("bash")]));
        ctx.system_prompt = Some("You are rpi.".to_owned());
        let options = AnthropicOptions::default();
        let params = build_params(&model, &ctx, true, &options).expect("params");

        let system = params["system"].as_array().expect("system");
        assert_eq!(
            system[0]["text"],
            json!("You are Claude Code, Anthropic's official CLI for Claude.")
        );
        assert_eq!(system[0]["cache_control"], json!({"type": "ephemeral"}));
        assert_eq!(system[1]["text"], json!("You are rpi."));
        // OAuth canonicalizes tool names.
        assert_eq!(params["tools"][0]["name"], json!("Bash"));
    }

    #[test]
    fn test_build_params_temperature_gating() {
        let model = make_model(json!({}));
        let ctx = context(vec![user_text("hi")], None);
        let mut options = AnthropicOptions::default();
        options.stream.temperature = Some(0.5);

        let params = build_params(&model, &ctx, false, &options).expect("params");
        assert_eq!(params["temperature"], json!(0.5));

        // Temperature is incompatible with extended thinking.
        options.thinking_enabled = Some(true);
        let params = build_params(&model, &ctx, false, &options).expect("params");
        assert!(params.get("temperature").is_none());

        // ... and unsupported on models without temperature support.
        let model = make_model(json!({"compat": {"supportsTemperature": false}}));
        options.thinking_enabled = None;
        let params = build_params(&model, &ctx, false, &options).expect("params");
        assert!(params.get("temperature").is_none());
    }

    #[test]
    fn test_build_params_thinking_modes() {
        let ctx = context(vec![user_text("hi")], None);

        // Budget-based thinking.
        let model = make_model(json!({}));
        let mut options = AnthropicOptions {
            thinking_enabled: Some(true),
            thinking_budget_tokens: Some(2048),
            ..AnthropicOptions::default()
        };
        let params = build_params(&model, &ctx, false, &options).expect("params");
        assert_eq!(
            params["thinking"],
            json!({"type": "enabled", "budget_tokens": 2048, "display": "summarized"})
        );

        // Adaptive thinking with effort.
        let model = make_model(json!({"compat": {"forceAdaptiveThinking": true}}));
        options.effort = Some("high".to_owned());
        let params = build_params(&model, &ctx, false, &options).expect("params");
        assert_eq!(
            params["thinking"],
            json!({"type": "adaptive", "display": "summarized"})
        );
        assert_eq!(params["output_config"], json!({"effort": "high"}));

        // Explicitly disabled.
        let mut options = AnthropicOptions {
            thinking_enabled: Some(false),
            ..AnthropicOptions::default()
        };
        let params = build_params(&model, &ctx, false, &options).expect("params");
        assert_eq!(params["thinking"], json!({"type": "disabled"}));

        // thinkingLevelMap.off === null keeps thinking omitted.
        let model = make_model(json!({"thinkingLevelMap": {"off": null}}));
        let params = build_params(&model, &ctx, false, &options).expect("params");
        assert!(params.get("thinking").is_none());

        // thinkingEnabled None → no thinking param at all.
        options.thinking_enabled = None;
        let params = build_params(&model, &ctx, false, &options).expect("params");
        assert!(params.get("thinking").is_none());
    }

    #[test]
    fn test_build_params_metadata_and_tool_choice() {
        let model = make_model(json!({}));
        let ctx = context(vec![user_text("hi")], None);
        let mut options = AnthropicOptions {
            tool_choice: Some(AnthropicToolChoice::Any),
            ..AnthropicOptions::default()
        };
        options
            .stream
            .metadata
            .get_or_insert_with(serde_json::Map::new)
            .insert("user_id".to_owned(), json!("u-1"));
        let params = build_params(&model, &ctx, false, &options).expect("params");
        assert_eq!(params["metadata"], json!({"user_id": "u-1"}));
        assert_eq!(params["tool_choice"], json!({"type": "any"}));

        options.tool_choice = Some(AnthropicToolChoice::Tool {
            name: "search".to_owned(),
        });
        let params = build_params(&model, &ctx, false, &options).expect("params");
        assert_eq!(
            params["tool_choice"],
            json!({"type": "tool", "name": "search"})
        );
    }

    #[test]
    fn test_build_params_normalizes_cross_model_tool_call_ids() {
        let model = make_model(json!({}));
        let messages = vec![
            // Assistant message from a different model: id normalization applies.
            assistant_message(
                vec![AssistantContent::ToolCall(crate::types::ToolCall {
                    id: "call_abc|item_xyz".to_owned(),
                    name: "bash".to_owned(),
                    arguments: serde_json::Map::new(),
                    thought_signature: None,
                    namespace: None,
                })],
                "openai",
                "gpt-5",
            ),
            tool_result("call_abc|item_xyz", "ok", None),
        ];
        let ctx = context(messages, None);
        let params =
            build_params(&model, &ctx, false, &AnthropicOptions::default()).expect("params");
        let assistant = &params["messages"][0];
        assert_eq!(assistant["content"][0]["id"], json!("call_abc_item_xyz"));
        // The tool result id is remapped to the normalized id.
        let result = &params["messages"][1];
        assert_eq!(
            result["content"][0]["tool_use_id"],
            json!("call_abc_item_xyz")
        );
    }

    #[test]
    fn test_build_params_deferred_tools() {
        let model = make_model(json!({}));
        let messages = vec![
            assistant_message(
                vec![AssistantContent::ToolCall(crate::types::ToolCall {
                    id: "toolu_1".to_owned(),
                    name: "read".to_owned(),
                    arguments: serde_json::Map::new(),
                    thought_signature: None,
                    namespace: None,
                })],
                "anthropic",
                "claude-sonnet-4-5",
            ),
            tool_result("toolu_1", "loaded search", Some(vec!["search"])),
        ];
        let ctx = context(messages, Some(vec![tool("read"), tool("search")]));
        let params =
            build_params(&model, &ctx, false, &AnthropicOptions::default()).expect("params");
        let tools = params["tools"].as_array().expect("tools");
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0]["name"], json!("read"));
        assert!(tools[0].get("defer_loading").is_none());
        assert_eq!(tools[1]["name"], json!("search"));
        assert_eq!(tools[1]["defer_loading"], json!(true));

        // The tool result carries a tool_reference, content displaced.
        let content = params["messages"][1]["content"]
            .as_array()
            .expect("content");
        assert_eq!(
            content[0]["content"],
            json!([{"type": "tool_reference", "tool_name": "search"}])
        );
        // The displaced sibling text is the last block of the last user
        // message, so cache_control lands on it (upstream convertMessages).
        assert_eq!(
            content[1],
            json!({"type": "text", "text": "loaded search", "cache_control": {"type": "ephemeral"}})
        );
    }

    #[test]
    fn test_build_params_oauth_used_name_keeps_tool_immediate() {
        // Upstream "normalizes OAuth names before checking prior tool usage":
        // an OAuth call to "Read" satisfies the "read" marker, so no tool is
        // deferred and no tool_reference is emitted.
        let model = make_model(json!({}));
        let messages = vec![
            assistant_message(
                vec![AssistantContent::ToolCall(crate::types::ToolCall {
                    id: "toolu_1".to_owned(),
                    name: "Read".to_owned(),
                    arguments: serde_json::Map::new(),
                    thought_signature: None,
                    namespace: None,
                })],
                "anthropic",
                "claude-sonnet-4-5",
            ),
            tool_result("toolu_1", "loaded", Some(vec!["read"])),
        ];
        let ctx = context(messages, Some(vec![tool("base_tool"), tool("read")]));
        let params =
            build_params(&model, &ctx, true, &AnthropicOptions::default()).expect("params");
        let tools = params["tools"].as_array().expect("tools");
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0]["name"], json!("base_tool"));
        assert_eq!(tools[1]["name"], json!("Read"));
        assert!(tools.iter().all(|t| t.get("defer_loading").is_none()));
        let body = serde_json::to_string(&params).expect("serialize");
        assert!(!body.contains("tool_reference"));
    }

    #[test]
    fn test_build_params_oauth_canonicalized_markers_defer() {
        // Upstream "matches OAuth-canonicalized markers to active tools":
        // the "Read" marker canonicalizes to the "read" tool → deferred, and
        // the tool_reference names the canonical Claude Code casing.
        let model = make_model(json!({}));
        let messages = vec![
            assistant_message(
                vec![AssistantContent::ToolCall(crate::types::ToolCall {
                    id: "toolu_1".to_owned(),
                    name: "base_tool".to_owned(),
                    arguments: serde_json::Map::new(),
                    thought_signature: None,
                    namespace: None,
                })],
                "anthropic",
                "claude-sonnet-4-5",
            ),
            tool_result("toolu_1", "loaded", Some(vec!["Read"])),
        ];
        let ctx = context(messages, Some(vec![tool("base_tool"), tool("read")]));
        let params =
            build_params(&model, &ctx, true, &AnthropicOptions::default()).expect("params");
        let tools = params["tools"].as_array().expect("tools");
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0]["name"], json!("base_tool"));
        assert!(tools[0].get("defer_loading").is_none());
        assert_eq!(tools[1]["name"], json!("Read"));
        assert_eq!(tools[1]["defer_loading"], json!(true));
        let content = params["messages"][1]["content"]
            .as_array()
            .expect("content");
        assert_eq!(
            content[0]["content"],
            json!([{"type": "tool_reference", "tool_name": "Read"}])
        );
    }

    #[test]
    fn test_build_params_all_tools_deferred_keeps_one_immediate() {
        // Upstream "keeps one immediate Anthropic tool when every current
        // tool is marked": the only tool stays immediate, no reference.
        let model = make_model(json!({}));
        let messages = vec![tool_result("toolu_1", "loaded", Some(vec!["late_tool"]))];
        let ctx = context(messages, Some(vec![tool("late_tool")]));
        let params =
            build_params(&model, &ctx, false, &AnthropicOptions::default()).expect("params");
        let tools = params["tools"].as_array().expect("tools");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], json!("late_tool"));
        assert!(tools[0].get("defer_loading").is_none());
        let body = serde_json::to_string(&params).expect("serialize");
        assert!(!body.contains("tool_reference"));
    }

    #[test]
    fn test_build_params_missing_tool_marker_no_reference() {
        // Upstream "does not resurrect a marked tool missing from
        // Context.tools": markers only defer tools actually present.
        let model = make_model(json!({}));
        let messages = vec![tool_result("toolu_1", "loaded", Some(vec!["ghost"]))];
        let ctx = context(messages, Some(vec![tool("base_tool")]));
        let params =
            build_params(&model, &ctx, false, &AnthropicOptions::default()).expect("params");
        let tools = params["tools"].as_array().expect("tools");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], json!("base_tool"));
        let body = serde_json::to_string(&params).expect("serialize");
        assert!(!body.contains("tool_reference"));
    }

    #[test]
    fn test_build_params_transport_preference_ignored() {
        // Transport is a codex-only preference; every other provider silently
        // ignores it (upstream: only openai-codex-responses.ts reads it).
        let model = make_model(json!({}));
        let ctx = context(vec![user_text("hi")], None);
        let plain =
            build_params(&model, &ctx, false, &AnthropicOptions::default()).expect("params");
        let with_transport = build_params(
            &model,
            &ctx,
            false,
            &AnthropicOptions {
                stream: StreamOptions {
                    transport: Some(crate::types::Transport::Websocket),
                    ..StreamOptions::default()
                },
                ..AnthropicOptions::default()
            },
        )
        .expect("params");
        assert_eq!(
            serde_json::to_string(&plain).expect("serialize"),
            serde_json::to_string(&with_transport).expect("serialize")
        );
    }

    // -----------------------------------------------------------------------
    // map_stop_reason
    // -----------------------------------------------------------------------

    #[test]
    fn test_map_stop_reason() {
        assert_eq!(
            map_stop_reason("end_turn", None),
            Ok((StopReason::Stop, None))
        );
        assert_eq!(
            map_stop_reason("max_tokens", None),
            Ok((StopReason::Length, None))
        );
        assert_eq!(
            map_stop_reason("tool_use", None),
            Ok((StopReason::ToolUse, None))
        );
        assert_eq!(
            map_stop_reason("pause_turn", None),
            Ok((StopReason::Stop, None))
        );
        assert_eq!(
            map_stop_reason("stop_sequence", None),
            Ok((StopReason::Stop, None))
        );
        assert_eq!(
            map_stop_reason("sensitive", None),
            Ok((
                StopReason::Error,
                Some("Provider stopped with: sensitive".to_owned())
            ))
        );
        assert_eq!(
            map_stop_reason("refusal", None),
            Ok((
                StopReason::Error,
                Some("The model refused to complete the request".to_owned())
            ))
        );
        let details = json!({"explanation": "custom reason"});
        assert_eq!(
            map_stop_reason("refusal", Some(&details)),
            Ok((StopReason::Error, Some("custom reason".to_owned())))
        );
        assert_eq!(
            map_stop_reason("brand_new", None),
            Err("Unhandled stop reason: brand_new".to_owned())
        );
    }

    // -----------------------------------------------------------------------
    // SSE stream processing
    // -----------------------------------------------------------------------

    fn drive_sse(
        model: &Model,
        bytes: &[u8],
    ) -> (
        Vec<StreamEvent>,
        Result<DoneReason, String>,
        AssistantMessage,
    ) {
        let events = AssistantMessageEventStream::new();
        let mut output = initial_output(model);
        let reason = {
            let mut processor = StreamProcessor::new(&mut output, model, false, None);
            let mut decoder = SseDecoder::new();
            let mut result = Ok(());
            for sse in decoder.feed(bytes) {
                if let Err(error) = processor.handle_sse(&sse, &events) {
                    result = Err(error);
                    break;
                }
            }
            if result.is_ok() {
                for sse in decoder.finish() {
                    if let Err(error) = processor.handle_sse(&sse, &events) {
                        result = Err(error);
                        break;
                    }
                }
            }
            match result {
                Ok(()) => processor.finish(None),
                Err(error) => Err(error),
            }
        };
        events.end(None);
        let collected: Vec<StreamEvent> = futures::executor::block_on(events.collect());
        (collected, reason, output)
    }

    const RECORDED_STREAM: &str = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_123\",\"usage\":{\"input_tokens\":10,\"output_tokens\":1,\"cache_read_input_tokens\":5,\"cache_creation_input_tokens\":2,\"cache_creation\":{\"ephemeral_1h_input_tokens\":1}}}}\n",
        "\n",
        "event: ping\n",
        "data: {\"type\":\"ping\"}\n",
        "\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n",
        "\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"let me \"}}\n",
        "\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig1\"}}\n",
        "\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n",
        "\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n",
        "\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n",
        "\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":1}\n",
        "\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"bash\",\"input\":{}}}\n",
        "\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"command\\\":\"}}\n",
        "\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"ls\\\"}\"}}\n",
        "\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":2}\n",
        "\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":25,\"output_tokens_details\":{\"thinking_tokens\":10}}}\n",
        "\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n",
        "\n",
    );

    #[test]
    fn test_stream_processor_recorded_flow() {
        let model = make_model(json!({}));
        let (events, reason, output) = drive_sse(&model, RECORDED_STREAM.as_bytes());

        assert_eq!(reason, Ok(DoneReason::ToolUse));
        assert_eq!(output.response_id.as_deref(), Some("msg_123"));
        assert_eq!(output.stop_reason, StopReason::ToolUse);
        assert_eq!(output.usage.input, 10);
        assert_eq!(output.usage.output, 25);
        assert_eq!(output.usage.cache_read, 5);
        assert_eq!(output.usage.cache_write, 2);
        assert_eq!(output.usage.cache_write1h, Some(1));
        assert_eq!(output.usage.reasoning, Some(10));
        assert_eq!(output.usage.total_tokens, 42);

        // Content: thinking (with signature), text, tool call.
        assert_eq!(output.content.len(), 3);
        let AssistantContent::Thinking(thinking) = &output.content[0] else {
            panic!("expected thinking");
        };
        assert_eq!(thinking.thinking, "let me ");
        assert_eq!(thinking.thinking_signature.as_deref(), Some("sig1"));
        let AssistantContent::ToolCall(call) = &output.content[2] else {
            panic!("expected tool call");
        };
        assert_eq!(call.id, "toolu_1");
        assert_eq!(call.name, "bash");
        assert_eq!(call.arguments.get("command"), Some(&json!("ls")));

        // Event sequence (ping is filtered out).
        let kinds: Vec<&str> = events
            .iter()
            .map(|event| match event {
                StreamEvent::ThinkingStart { .. } => "thinking_start",
                StreamEvent::ThinkingDelta { .. } => "thinking_delta",
                StreamEvent::ThinkingEnd { .. } => "thinking_end",
                StreamEvent::TextStart { .. } => "text_start",
                StreamEvent::TextDelta { .. } => "text_delta",
                StreamEvent::TextEnd { .. } => "text_end",
                StreamEvent::ToolCallStart { .. } => "toolcall_start",
                StreamEvent::ToolCallDelta { .. } => "toolcall_delta",
                StreamEvent::ToolCallEnd { .. } => "toolcall_end",
                _ => "other",
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                "thinking_start",
                "thinking_delta",
                "thinking_end",
                "text_start",
                "text_delta",
                "text_end",
                "toolcall_start",
                "toolcall_delta",
                "toolcall_delta",
                "toolcall_end",
            ]
        );
    }

    /// Ported intent of `anthropic-sse-parsing.test.ts` @ 4181f66:
    /// "preserves content from content_block_start events" (59ad3dead,
    /// #7358). The initial `text`/`thinking`/`signature` carried by
    /// content_block_start must survive into the final message.
    #[test]
    fn test_stream_processor_preserves_content_block_start_content() {
        let model = make_model(json!({}));
        let bytes = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_initial_content\",\"usage\":{\"input_tokens\":12,\"output_tokens\":0,\"cache_read_input_tokens\":0,\"cache_creation_input_tokens\":0}}}\n",
            "\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"Initial text\"}}\n",
            "\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" plus delta\"}}\n",
            "\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n",
            "\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"Initial thinking\",\"signature\":\"initial signature\"}}\n",
            "\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\" plus delta\"}}\n",
            "\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"signature_delta\",\"signature\":\" plus delta\"}}\n",
            "\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":1}\n",
            "\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":12,\"output_tokens\":5,\"cache_read_input_tokens\":0,\"cache_creation_input_tokens\":0}}\n",
            "\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n",
            "\n",
        );
        let (_, reason, output) = drive_sse(&model, bytes.as_bytes());
        assert_eq!(reason, Ok(DoneReason::Stop));

        assert_eq!(output.content.len(), 2);
        let AssistantContent::Text(text) = &output.content[0] else {
            panic!("expected text block, got {:?}", output.content[0]);
        };
        assert_eq!(text.text, "Initial text plus delta");
        let AssistantContent::Thinking(thinking) = &output.content[1] else {
            panic!("expected thinking block, got {:?}", output.content[1]);
        };
        assert_eq!(thinking.thinking, "Initial thinking plus delta");
        assert_eq!(
            thinking.thinking_signature.as_deref(),
            Some("initial signature plus delta")
        );
    }

    #[test]
    fn test_stream_processor_sse_error_event() {
        let model = make_model(json!({}));
        let (_, reason, _) = drive_sse(
            &model,
            b"event: error\ndata: {\"type\":\"error\",\"error\":{\"message\":\"overloaded\"}}\n\n",
        );
        assert_eq!(
            reason,
            Err("{\"type\":\"error\",\"error\":{\"message\":\"overloaded\"}}".to_owned())
        );
    }

    #[test]
    fn test_stream_processor_ended_before_message_stop() {
        let model = make_model(json!({}));
        let bytes = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"usage\":{\"input_tokens\":1}}}\n",
            "\n"
        );
        let (_, reason, _) = drive_sse(&model, bytes.as_bytes());
        assert_eq!(
            reason,
            Err("Anthropic stream ended before message_stop".to_owned())
        );
    }

    #[test]
    fn test_stream_processor_missing_stop_reason() {
        let model = make_model(json!({}));
        let (_, reason, _) = drive_sse(&model, b"");
        assert_eq!(
            reason,
            Err("Anthropic stream ended without a stop reason".to_owned())
        );
    }

    #[test]
    fn test_stream_processor_refusal_maps_to_error() {
        let model = make_model(json!({}));
        let bytes = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"usage\":{\"input_tokens\":1}}}\n",
            "\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"refusal\",\"stop_details\":{\"explanation\":\"cannot help\"}}}\n",
            "\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n",
            "\n"
        );
        let (_, reason, output) = drive_sse(&model, bytes.as_bytes());
        assert_eq!(reason, Err("cannot help".to_owned()));
        assert_eq!(output.stop_reason, StopReason::Error);
        assert_eq!(output.error_message.as_deref(), Some("cannot help"));
        // 926eb15c1: the raw provider reason is preserved.
        assert_eq!(output.raw_stop_reason.as_deref(), Some("refusal"));
    }

    /// anthropic-sse-parsing.test.ts: "preserves sensitive stop reasons with a
    /// descriptive error message" (926eb15c1; text unified by 5a2539a7b @
    /// 4181f66).
    #[test]
    fn preserves_sensitive_stop_reasons_with_a_descriptive_error_message() {
        let model = make_model(json!({}));
        let bytes = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_sensitive\",\"usage\":{\"input_tokens\":12,\"output_tokens\":0,\"cache_read_input_tokens\":0,\"cache_creation_input_tokens\":0}}}\n",
            "\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"sensitive\"},\"usage\":{\"input_tokens\":12,\"output_tokens\":0,\"cache_read_input_tokens\":0,\"cache_creation_input_tokens\":0}}\n",
            "\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n",
            "\n"
        );
        let (_, reason, output) = drive_sse(&model, bytes.as_bytes());
        assert_eq!(reason, Err("Provider stopped with: sensitive".to_owned()));
        assert_eq!(output.stop_reason, StopReason::Error);
        assert_eq!(output.raw_stop_reason.as_deref(), Some("sensitive"));
        assert_eq!(
            output.error_message.as_deref(),
            Some("Provider stopped with: sensitive")
        );
    }

    // -----------------------------------------------------------------------
    // stream_simple
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_stream_simple_missing_auth_is_stream_error() {
        let model = make_model(json!({}));
        let events: Vec<StreamEvent> =
            stream_simple(&model, &context(vec![user_text("hi")], None), None)
                .collect()
                .await;
        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::Error { error, .. } => {
                assert_eq!(error.stop_reason, StopReason::Error);
                assert_eq!(
                    error.error_message.as_deref(),
                    Some("No API key for provider: anthropic")
                );
            }
            other => panic!("expected error event, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod header_semantics_tests {
    use serde_json::json;

    use super::tests::make_model;
    use super::*;

    #[test]
    fn test_user_headers_override_key_derived_auth() {
        // SDK semantics: defaultHeaders are applied AFTER authHeaders, so a
        // user-supplied x-api-key/authorization overrides the key-derived one
        // (case-insensitive).
        let model = make_model(json!({}));
        let options_headers: ProviderHeaders =
            [("X-API-Key".to_owned(), Some("user-key".to_owned()))].into();
        let (headers, _) = build_request_headers(
            &model,
            Some("sk-key"),
            false,
            false,
            Some(&options_headers),
            None,
            None,
        );
        assert!(!headers.contains_key("x-api-key"));
        assert_eq!(
            headers.get("X-API-Key").and_then(|v| v.as_deref()),
            Some("user-key")
        );
    }

    #[test]
    fn test_none_value_suppresses_key_derived_auth() {
        let model = make_model(json!({}));
        let options_headers: ProviderHeaders = [("x-api-key".to_owned(), None)].into();
        let (headers, _) = build_request_headers(
            &model,
            Some("sk-key"),
            false,
            false,
            Some(&options_headers),
            None,
            None,
        );
        // The suppression marker replaces the key-derived header; it is
        // dropped at the HTTP boundary.
        assert_eq!(headers.get("x-api-key"), Some(&None));
        let map = provider_headers_to_header_map(&headers).expect("header map");
        assert!(map.get("x-api-key").is_none());
    }
}
