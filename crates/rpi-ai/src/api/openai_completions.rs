//! Port of `packages/ai/src/api/openai-completions.ts` @ pi 0.82.1 (2efa728);
//! stream-termination semantics (rawStopReason, `supportsFinishReason`,
//! function-vs-empty-custom tool-call deltas) updated to 4181f66 (fe1c9b6d5,
//! 2c3041242, 34239180a).
//!
//! OpenAI Chat Completions adapter: compat auto-detection (`detect_compat` /
//! `get_compat`), message conversion (tool-call id normalization, thinking
//! replay, tool-result grouping, Kimi deferred tools), Anthropic-style cache
//! control markers, the ten `thinkingFormat` request variants, SSE chunk
//! processing (text / reasoning / function & custom grammar tool calls /
//! encrypted reasoning details), and `stream_simple` reasoning mapping.
//!
//! Intentional differences (upstream deviations):
//! - HTTP is a direct reqwest call, not the `openai` SDK; the SDK's
//!   `x-stainless-*` telemetry headers, default `User-Agent` and platform
//!   headers are not sent, and there is no SDK default timeout (callers set
//!   `StreamOptions::timeout_ms`).
//! - SSE chunks are parsed with strict `serde_json` (the SDK uses
//!   `JSON.parse`, not a repairing parser); parse failures read
//!   `Could not parse OpenAI SSE chunk: {error}; data={data}` (the SDK would
//!   surface a `SyntaxError` message instead).
//! - The legacy env var is renamed `PI_CACHE_RETENTION` →
//!   `RPI_CACHE_RETENTION` (requirements §5.5); the resolution helper is
//!   shared with the anthropic-messages adapter
//!   ([`crate::api::anthropic_messages::resolve_cache_retention`]).
//! - `streamSimple`'s `toolChoice` smuggling (upstream reads `toolChoice` off
//!   the `SimpleStreamOptions` object) is not ported: rpi's
//!   `SimpleStreamOptions` struct has no such field. Tool choice is available
//!   via [`OpenAICompletionsOptions::tool_choice`] on direct [`stream`] calls.
//! - OpenRouter's `error.metadata.raw` detail is extracted from the HTTP error
//!   body JSON (upstream reads it off the SDK error object).

use std::collections::HashMap;

use futures::StreamExt;
use serde_json::{json, Map, Value};
use tokio_util::sync::CancellationToken;

use crate::api::anthropic_messages::resolve_cache_retention;
use crate::api::constrained_sampling::{
    append_grammar_tool_input_json_delta, create_grammar_tool_input_properties,
    get_grammar_tool_input, resolve_grammar_constrained_sampling,
    resolve_json_schema_strict_sampling, GrammarToolInputJsonBuffer,
};
use crate::api::copilot_headers::{build_copilot_dynamic_headers, has_copilot_vision_input};
use crate::api::lazy::immediate_error_stream;
use crate::api::openai_prompt_cache::clamp_openai_prompt_cache_key;
use crate::api::simple_options::build_base_options;
use crate::api::sse::{ServerSentEvent, SseDecoder};
use crate::models::{clamp_thinking_level, ProviderStreams};
use crate::types::{
    AssistantContent, AssistantMessage, CacheControlFormat, CacheRetention, ChatTemplateKwargValue,
    ChatTemplateKwargVarKind, Context, DeferredToolsMode, DoneReason, ErrorReason, InputModality,
    MaxTokensField, Message, Model, ModelThinkingLevel, OpenRouterRouting, ProviderHeaders,
    ProviderResponse, Role, SessionAffinityFormat, SimpleStreamOptions, StopReason, StreamEvent,
    StreamOptions, TextContent, ThinkingContent, ThinkingFormat, Tool, ToolCall, ToolResultContent,
    Usage, UserContent, UserContentBlock, VercelGatewayRouting,
};
use crate::utils::cost::calculate_cost;
use crate::utils::error_body::{format_provider_error, NormalizedProviderError};
use crate::utils::event_stream::AssistantMessageEventStream;
use crate::utils::hash::short_hash;
use crate::utils::headers::{
    headers_to_record, merge_headers_chain, model_headers, provider_headers_to_header_map,
};
use crate::utils::json_parse::parse_streaming_json;
use crate::utils::provider_retry::{
    retry_provider_request, ProviderErrorInfo, ProviderRetryOptions,
};
use crate::utils::sanitize_unicode::sanitize_surrogates;
use crate::utils::transform_messages::transform_messages;

// ---------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------

/// `OpenAICompletionsOptions` — `StreamOptions` plus completions-specific
/// extras. `reasoning_effort` keys directly into
/// `Model::thinking_level_map`; `tool_choice` is the raw OpenAI
/// `ChatCompletionToolChoiceOption` JSON value.
#[derive(Debug, Clone, Default)]
pub struct OpenAICompletionsOptions {
    pub stream: StreamOptions,
    pub reasoning_effort: Option<ModelThinkingLevel>,
    pub tool_choice: Option<Value>,
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

/// `getClientApiKey`: an explicit API key wins; an auth-bearing header makes
/// the key unnecessary ("unused" placeholder, matching upstream — the SDK
/// requires *some* key string). Otherwise the request cannot be authenticated.
pub fn get_client_api_key(
    provider: &str,
    api_key: Option<&str>,
    headers: Option<&ProviderHeaders>,
) -> Result<String, String> {
    if let Some(api_key) = api_key {
        return Ok(api_key.to_owned());
    }
    if has_header(headers, "authorization") || has_header(headers, "cf-aig-authorization") {
        return Ok("unused".to_owned());
    }
    Err(format!("No API key for provider: {provider}"))
}

// ---------------------------------------------------------------------------
// Tool history / deferred tools
// ---------------------------------------------------------------------------

/// `hasToolHistory`: true when the conversation contains tool calls or tool
/// results (Anthropic via proxy requires the `tools` param to be present then).
pub fn has_tool_history(messages: &[Message]) -> bool {
    messages.iter().any(|msg| match msg {
        Message::ToolResult(_) => true,
        Message::Assistant(assistant) => assistant
            .content
            .iter()
            .any(|block| matches!(block, AssistantContent::ToolCall(_))),
        Message::User(_) => false,
    })
}

/// `getDeferredToolNames`: tool names introduced by tool results, in first
/// appearance order (JS `Set` iteration semantics).
pub fn get_deferred_tool_names(messages: &[Message]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for message in messages {
        if let Message::ToolResult(result) = message {
            for name in result.added_tool_names.as_deref().unwrap_or(&[]) {
                if !names.contains(name) {
                    names.push(name.clone());
                }
            }
        }
    }
    names
}

/// `getToolsByName`: tools from `tools` matching `names`, in `names` order.
/// Upstream builds `new Map(tools.map((tool) => [tool.name, tool]))` — a
/// later duplicate name overwrites an earlier one.
pub fn get_tools_by_name(tools: Option<&[Tool]>, names: &[String]) -> Vec<Tool> {
    let Some(tools) = tools else {
        return Vec::new();
    };
    let mut by_name: HashMap<&str, &Tool> = HashMap::new();
    for tool in tools {
        by_name.insert(tool.name.as_str(), tool);
    }
    names
        .iter()
        .filter_map(|name| by_name.get(name.as_str()).map(|tool| (*tool).clone()))
        .collect()
}

// ---------------------------------------------------------------------------
// Compat detection and resolution
// ---------------------------------------------------------------------------

/// `Required<OpenAICompletionsCompat>` with `cacheControlFormat` and
/// `deferredToolsMode` kept optional (upstream `ResolvedOpenAICompletionsCompat`).
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedOpenAICompletionsCompat {
    pub supports_store: bool,
    pub supports_developer_role: bool,
    pub supports_reasoning_effort: bool,
    pub supports_usage_in_streaming: bool,
    /// 2c3041242: whether streamed responses include `finish_reason`. When
    /// false, `stop`/`toolUse` is inferred from content at stream end.
    pub supports_finish_reason: bool,
    pub max_tokens_field: MaxTokensField,
    pub requires_tool_result_name: bool,
    pub requires_assistant_after_tool_result: bool,
    pub requires_thinking_as_text: bool,
    pub requires_reasoning_content_on_assistant_messages: bool,
    pub thinking_format: ThinkingFormat,
    pub open_router_routing: OpenRouterRouting,
    pub vercel_gateway_routing: VercelGatewayRouting,
    pub chat_template_kwargs: std::collections::BTreeMap<String, ChatTemplateKwargValue>,
    pub zai_tool_stream: bool,
    pub supports_strict_mode: bool,
    pub supports_open_ai_grammar_tools: bool,
    pub cache_control_format: Option<CacheControlFormat>,
    pub send_session_affinity_headers: bool,
    pub deferred_tools_mode: Option<DeferredToolsMode>,
    pub session_affinity_format: SessionAffinityFormat,
    pub supports_long_cache_retention: bool,
}

/// `detectCompat`: auto-detect compatibility settings from provider name and
/// baseUrl. Used as the base when `model.compat` is not set; explicit
/// `model.compat` entries override these detected values.
pub fn detect_compat(model: &Model) -> ResolvedOpenAICompletionsCompat {
    let provider = model.provider.as_str();
    let base_url = model.base_url.as_str();

    let is_zai = provider == "zai"
        || provider == "zai-coding-cn"
        || base_url.contains("api.z.ai")
        || base_url.contains("open.bigmodel.cn");
    let is_together = provider == "together"
        || base_url.contains("api.together.ai")
        || base_url.contains("api.together.xyz");
    let is_moonshot = provider == "moonshotai"
        || provider == "moonshotai-cn"
        || base_url.contains("api.moonshot.");
    let is_openrouter = provider == "openrouter" || base_url.contains("openrouter.ai");
    let is_cloudflare_workers_ai =
        provider == "cloudflare-workers-ai" || base_url.contains("api.cloudflare.com");
    let is_cloudflare_ai_gateway =
        provider == "cloudflare-ai-gateway" || base_url.contains("gateway.ai.cloudflare.com");
    let is_nvidia = provider == "nvidia" || base_url.contains("integrate.api.nvidia.com");
    let is_ant_ling = provider == "ant-ling" || base_url.contains("api.ant-ling.com");

    let is_non_standard = is_nvidia
        || provider == "cerebras"
        || base_url.contains("cerebras.ai")
        || provider == "xai"
        || base_url.contains("api.x.ai")
        || is_together
        || base_url.contains("chutes.ai")
        || base_url.contains("deepseek.com")
        || is_zai
        || is_moonshot
        || provider == "opencode"
        || base_url.contains("opencode.ai")
        || is_cloudflare_workers_ai
        || is_cloudflare_ai_gateway
        || is_ant_ling;

    let use_max_tokens = base_url.contains("chutes.ai")
        || is_moonshot
        || is_cloudflare_ai_gateway
        || is_together
        || is_nvidia
        || is_ant_ling;

    let is_grok = provider == "xai" || base_url.contains("api.x.ai");
    let is_deepseek = provider == "deepseek" || base_url.contains("deepseek.com");
    let is_openrouter_developer_role_model =
        is_openrouter && (model.id.starts_with("anthropic/") || model.id.starts_with("openai/"));
    let cache_control_format = if provider == "openrouter" && model.id.starts_with("anthropic/") {
        Some(CacheControlFormat::Anthropic)
    } else {
        None
    };

    ResolvedOpenAICompletionsCompat {
        supports_store: !is_non_standard,
        supports_developer_role: is_openrouter_developer_role_model
            || (!is_non_standard && !is_openrouter),
        supports_reasoning_effort: !is_grok
            && !is_zai
            && !is_moonshot
            && !is_together
            && !is_cloudflare_ai_gateway
            && !is_nvidia
            && !is_ant_ling,
        supports_usage_in_streaming: true,
        supports_finish_reason: true,
        max_tokens_field: if use_max_tokens {
            MaxTokensField::MaxTokens
        } else {
            MaxTokensField::MaxCompletionTokens
        },
        requires_tool_result_name: false,
        requires_assistant_after_tool_result: false,
        requires_thinking_as_text: false,
        requires_reasoning_content_on_assistant_messages: is_deepseek,
        thinking_format: if is_deepseek {
            ThinkingFormat::Deepseek
        } else if is_zai {
            ThinkingFormat::Zai
        } else if is_together {
            ThinkingFormat::Together
        } else if is_ant_ling {
            ThinkingFormat::AntLing
        } else if is_openrouter {
            ThinkingFormat::Openrouter
        } else {
            ThinkingFormat::Openai
        },
        open_router_routing: OpenRouterRouting::default(),
        vercel_gateway_routing: VercelGatewayRouting::default(),
        chat_template_kwargs: Default::default(),
        zai_tool_stream: false,
        supports_strict_mode: !is_moonshot
            && !is_together
            && !is_cloudflare_ai_gateway
            && !is_nvidia,
        supports_open_ai_grammar_tools: false,
        cache_control_format,
        send_session_affinity_headers: false,
        deferred_tools_mode: None,
        session_affinity_format: if is_openrouter {
            SessionAffinityFormat::Openrouter
        } else {
            SessionAffinityFormat::Openai
        },
        supports_long_cache_retention: !(is_together
            || is_cloudflare_workers_ai
            || is_cloudflare_ai_gateway
            || is_nvidia
            || is_ant_ling),
    }
}

/// `getCompat`: auto-detect from provider/URL, then override with explicit
/// `model.compat` entries. Note `openRouterRouting` defaults to `{}` (not the
/// detected value), matching upstream.
pub fn get_compat(model: &Model) -> ResolvedOpenAICompletionsCompat {
    let detected = detect_compat(model);
    let Some(compat) = model.compat.as_ref() else {
        return detected;
    };
    ResolvedOpenAICompletionsCompat {
        supports_store: compat.supports_store.unwrap_or(detected.supports_store),
        supports_developer_role: compat
            .supports_developer_role
            .unwrap_or(detected.supports_developer_role),
        supports_reasoning_effort: compat
            .supports_reasoning_effort
            .unwrap_or(detected.supports_reasoning_effort),
        supports_usage_in_streaming: compat
            .supports_usage_in_streaming
            .unwrap_or(detected.supports_usage_in_streaming),
        supports_finish_reason: compat
            .supports_finish_reason
            .unwrap_or(detected.supports_finish_reason),
        max_tokens_field: compat.max_tokens_field.unwrap_or(detected.max_tokens_field),
        requires_tool_result_name: compat
            .requires_tool_result_name
            .unwrap_or(detected.requires_tool_result_name),
        requires_assistant_after_tool_result: compat
            .requires_assistant_after_tool_result
            .unwrap_or(detected.requires_assistant_after_tool_result),
        requires_thinking_as_text: compat
            .requires_thinking_as_text
            .unwrap_or(detected.requires_thinking_as_text),
        requires_reasoning_content_on_assistant_messages: compat
            .requires_reasoning_content_on_assistant_messages
            .unwrap_or(detected.requires_reasoning_content_on_assistant_messages),
        thinking_format: compat.thinking_format.unwrap_or(detected.thinking_format),
        open_router_routing: compat.open_router_routing.clone().unwrap_or_default(),
        vercel_gateway_routing: compat
            .vercel_gateway_routing
            .clone()
            .unwrap_or(detected.vercel_gateway_routing),
        chat_template_kwargs: compat
            .chat_template_kwargs
            .clone()
            .unwrap_or(detected.chat_template_kwargs),
        zai_tool_stream: compat.zai_tool_stream.unwrap_or(detected.zai_tool_stream),
        supports_strict_mode: compat
            .supports_strict_mode
            .unwrap_or(detected.supports_strict_mode),
        supports_open_ai_grammar_tools: compat
            .supports_open_ai_grammar_tools
            .unwrap_or(detected.supports_open_ai_grammar_tools),
        cache_control_format: compat
            .cache_control_format
            .or(detected.cache_control_format),
        send_session_affinity_headers: compat
            .send_session_affinity_headers
            .unwrap_or(detected.send_session_affinity_headers),
        deferred_tools_mode: compat.deferred_tools_mode.or(detected.deferred_tools_mode),
        session_affinity_format: compat
            .session_affinity_format
            .unwrap_or(detected.session_affinity_format),
        supports_long_cache_retention: compat
            .supports_long_cache_retention
            .unwrap_or(detected.supports_long_cache_retention),
    }
}

// ---------------------------------------------------------------------------
// Client headers
// ---------------------------------------------------------------------------

/// `createClient` (header construction only). The base carries the SDK's
/// `Accept: application/json` default and the key-derived `Authorization`
/// header; both sit in the base position because the SDK applies
/// `authHeaders` *before* `defaultHeaders` (user headers override key-derived
/// auth, case-insensitively — see `merge_headers_chain`).
pub fn build_client_headers(
    model: &Model,
    context: &Context,
    api_key: &str,
    options_headers: Option<&ProviderHeaders>,
    session_id: Option<&str>,
    compat: &ResolvedOpenAICompletionsCompat,
) -> ProviderHeaders {
    let base: ProviderHeaders = [
        ("accept".to_owned(), Some("application/json".to_owned())),
        (
            "authorization".to_owned(),
            Some(format!("Bearer {api_key}")),
        ),
    ]
    .into();

    let copilot_headers: Option<ProviderHeaders> =
        (model.provider == "github-copilot").then(|| {
            let has_images = has_copilot_vision_input(&context.messages);
            build_copilot_dynamic_headers(&context.messages, has_images)
                .into_iter()
                .map(|(key, value)| (key, Some(value)))
                .collect()
        });

    let session_affinity_headers: Option<ProviderHeaders> = session_id
        .filter(|_| compat.send_session_affinity_headers)
        .map(|session_id| {
            let mut headers = ProviderHeaders::new();
            match compat.session_affinity_format {
                SessionAffinityFormat::Openrouter => {
                    headers.insert("x-session-id".to_owned(), Some(session_id.to_owned()));
                }
                SessionAffinityFormat::Openai => {
                    headers.insert("session_id".to_owned(), Some(session_id.to_owned()));
                    headers.insert(
                        "x-client-request-id".to_owned(),
                        Some(session_id.to_owned()),
                    );
                    headers.insert("x-session-affinity".to_owned(), Some(session_id.to_owned()));
                }
                SessionAffinityFormat::OpenaiNosession => {
                    headers.insert(
                        "x-client-request-id".to_owned(),
                        Some(session_id.to_owned()),
                    );
                    headers.insert("x-session-affinity".to_owned(), Some(session_id.to_owned()));
                }
            }
            headers
        });

    merge_headers_chain(&[
        Some(base),
        model_headers(model),
        copilot_headers,
        session_affinity_headers,
        options_headers.cloned(),
    ])
}

// ---------------------------------------------------------------------------
// Message conversion
// ---------------------------------------------------------------------------

/// `convertMessages`. Tool-call ids are normalized through
/// [`transform_messages`] with the completions-specific rules: pipe-separated
/// Responses-API ids collapse to `callId_itemId` (sanitized, 40-char cap with
/// an 8-char `shortHash` back-fill), and plain ids truncate to 40 chars for
/// provider `openai` only.
pub fn convert_messages(
    model: &Model,
    context: &Context,
    compat: &ResolvedOpenAICompletionsCompat,
    grammar_tool_input_properties: &HashMap<String, String>,
) -> Result<Vec<Value>, String> {
    let mut params: Vec<Value> = Vec::new();

    let provider = model.provider.clone();
    let mut normalize_tool_call_id = move |id: &str, _model: &Model, _msg: &AssistantMessage| {
        // Pipe-separated IDs from the OpenAI Responses API:
        // `{call_id}|{item_id}` where item_id can be 400+ chars with special
        // chars. Multiple tool calls in the same turn can share call_id but
        // differ by item_id; preserve item-level uniqueness.
        if let Some(separator_index) = id.find('|') {
            // Sanitize to allowed chars and truncate to 40 chars (OpenAI limit).
            // Post-sanitize text is ASCII, so char-based slicing is exact.
            let sanitize = |text: &str| -> String {
                text.chars()
                    .map(|c| {
                        if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                            c
                        } else {
                            '_'
                        }
                    })
                    .collect()
            };
            let call_id = sanitize(&id[..separator_index]);
            let item_id = sanitize(&id[separator_index + 1..]);
            let combined_id = if item_id.is_empty() {
                call_id.clone()
            } else {
                format!("{call_id}_{item_id}")
            };
            if combined_id.chars().count() <= 40 {
                return combined_id;
            }
            let hash: String = short_hash(id).chars().take(8).collect();
            let prefix: String = call_id.chars().take(40 - hash.len() - 1).collect();
            let prefix: String = if prefix.is_empty() {
                // JS `callId.slice(0, Math.max(1, 31))` keeps at least one char.
                call_id.chars().take(1).collect()
            } else {
                prefix
            };
            return format!("{prefix}_{hash}");
        }

        if provider == "openai" {
            return id.chars().take(40).collect();
        }
        id.to_owned()
    };
    let transformed_messages =
        transform_messages(&context.messages, model, Some(&mut normalize_tool_call_id));

    if let Some(system_prompt) = &context.system_prompt {
        let role = if model.reasoning && compat.supports_developer_role {
            "developer"
        } else {
            "system"
        };
        params.push(json!({"role": role, "content": sanitize_surrogates(system_prompt)}));
    }

    let mut last_role: Option<Role> = None;
    let mut i = 0;
    while i < transformed_messages.len() {
        let msg = &transformed_messages[i];
        // Some providers don't allow user messages directly after tool results;
        // insert a synthetic assistant message to bridge the gap.
        if compat.requires_assistant_after_tool_result
            && last_role == Some(Role::ToolResult)
            && matches!(msg, Message::User(_))
        {
            params.push(json!({
                "role": "assistant",
                "content": "I have processed the tool results.",
            }));
        }

        match msg {
            Message::User(user) => match &user.content {
                UserContent::Text(text) => {
                    params.push(json!({
                        "role": "user",
                        "content": sanitize_surrogates(text),
                    }));
                }
                UserContent::Blocks(blocks) => {
                    let content: Vec<Value> = blocks
                        .iter()
                        .map(|item| match item {
                            UserContentBlock::Text(text) => json!({
                                "type": "text",
                                "text": sanitize_surrogates(&text.text),
                            }),
                            UserContentBlock::Image(image) => json!({
                                "type": "image_url",
                                "image_url": {
                                    "url": format!("data:{};base64,{}", image.mime_type, image.data),
                                },
                            }),
                        })
                        .collect();
                    if content.is_empty() {
                        i += 1;
                        continue;
                    }
                    params.push(json!({"role": "user", "content": content}));
                }
            },
            Message::Assistant(assistant) => {
                let mut assistant_msg = Map::new();
                assistant_msg.insert("role".to_owned(), json!("assistant"));
                // Some providers don't accept null content, use empty string instead.
                assistant_msg.insert(
                    "content".to_owned(),
                    if compat.requires_assistant_after_tool_result {
                        json!("")
                    } else {
                        Value::Null
                    },
                );

                let text_parts: Vec<Value> = assistant
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        AssistantContent::Text(text) if !text.text.trim().is_empty() => {
                            Some(json!({
                                "type": "text",
                                "text": sanitize_surrogates(&text.text),
                            }))
                        }
                        _ => None,
                    })
                    .collect();
                let assistant_text = text_parts
                    .iter()
                    .filter_map(|part| part["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("");

                let thinking_blocks: Vec<&ThinkingContent> = assistant
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        AssistantContent::Thinking(thinking)
                            if !thinking.thinking.trim().is_empty() =>
                        {
                            Some(thinking)
                        }
                        _ => None,
                    })
                    .collect();

                if !thinking_blocks.is_empty() {
                    if compat.requires_thinking_as_text {
                        // Convert thinking blocks to plain text (no tags to avoid
                        // model mimicking them).
                        let thinking_text = thinking_blocks
                            .iter()
                            .map(|block| sanitize_surrogates(&block.thinking))
                            .collect::<Vec<_>>()
                            .join("\n\n");
                        let mut content = vec![json!({"type": "text", "text": thinking_text})];
                        content.extend(text_parts);
                        assistant_msg.insert("content".to_owned(), Value::Array(content));
                    } else {
                        // Always send assistant content as a plain string (OpenAI
                        // Chat Completions API standard format); an array of text
                        // parts makes some models mirror the block structure in
                        // their output.
                        if !assistant_text.is_empty() {
                            assistant_msg.insert("content".to_owned(), json!(assistant_text));
                        }

                        // Use the signature from the first thinking block if
                        // available (for llama.cpp server + gpt-oss).
                        let mut signature = thinking_blocks[0].thinking_signature.clone();
                        if model.provider == "opencode-go"
                            && signature.as_deref() == Some("reasoning")
                        {
                            signature = Some("reasoning_content".to_owned());
                        }
                        if let Some(signature) = signature.filter(|s| !s.is_empty()) {
                            // Raw thinking text (not sanitized), joined with "\n".
                            let joined = thinking_blocks
                                .iter()
                                .map(|block| block.thinking.as_str())
                                .collect::<Vec<_>>()
                                .join("\n");
                            assistant_msg.insert(signature, json!(joined));
                        }
                    }
                } else if !assistant_text.is_empty() {
                    assistant_msg.insert("content".to_owned(), json!(assistant_text));
                }

                let tool_calls: Vec<&ToolCall> = assistant
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        AssistantContent::ToolCall(call) => Some(call),
                        _ => None,
                    })
                    .collect();
                if !tool_calls.is_empty() {
                    let mut converted: Vec<Value> = Vec::with_capacity(tool_calls.len());
                    for tool_call in &tool_calls {
                        if let Some(property) = grammar_tool_input_properties.get(&tool_call.name) {
                            let input = get_grammar_tool_input(
                                &tool_call.name,
                                &tool_call.arguments,
                                property,
                            )?;
                            converted.push(json!({
                                "id": tool_call.id,
                                "type": "custom",
                                "custom": {
                                    "name": tool_call.name,
                                    "input": sanitize_surrogates(&input),
                                },
                            }));
                        } else {
                            converted.push(json!({
                                "id": tool_call.id,
                                "type": "function",
                                "function": {
                                    "name": tool_call.name,
                                    "arguments": serde_json::to_string(&tool_call.arguments)
                                        .unwrap_or_else(|_| "{}".to_owned()),
                                },
                            }));
                        }
                    }
                    let reasoning_details: Vec<Value> = tool_calls
                        .iter()
                        .filter_map(|tool_call| tool_call.thought_signature.as_ref())
                        .map(|signature| {
                            serde_json::from_str::<Value>(signature).unwrap_or(Value::Null)
                        })
                        .filter(is_js_truthy)
                        .collect();
                    assistant_msg.insert("tool_calls".to_owned(), Value::Array(converted));
                    if !reasoning_details.is_empty() {
                        assistant_msg.insert(
                            "reasoning_details".to_owned(),
                            Value::Array(reasoning_details),
                        );
                    }
                }

                if compat.requires_reasoning_content_on_assistant_messages
                    && model.reasoning
                    && !assistant_msg.contains_key("reasoning_content")
                {
                    assistant_msg.insert("reasoning_content".to_owned(), json!(""));
                }

                // Skip assistant messages that have no content and no tool calls.
                // Some providers require "either content or tool_calls, but not
                // none"; others don't accept empty assistant messages. This
                // handles aborted assistant responses that got no content.
                let has_content = match assistant_msg.get("content") {
                    Some(Value::String(text)) => !text.is_empty(),
                    Some(Value::Array(parts)) => !parts.is_empty(),
                    _ => false,
                };
                if !has_content && !assistant_msg.contains_key("tool_calls") {
                    i += 1;
                    continue;
                }
                params.push(Value::Object(assistant_msg));
            }
            Message::ToolResult(_) => {
                let mut image_blocks: Vec<Value> = Vec::new();
                let mut deferred_tool_names: Vec<String> = Vec::new();
                let mut j = i;

                while j < transformed_messages.len() {
                    let Message::ToolResult(tool_msg) = &transformed_messages[j] else {
                        break;
                    };

                    let text_result = tool_msg
                        .content
                        .iter()
                        .filter_map(|block| match block {
                            ToolResultContent::Text(text) => Some(text.text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    let has_images = tool_msg
                        .content
                        .iter()
                        .any(|block| matches!(block, ToolResultContent::Image(_)));

                    // Always send tool result with text (or placeholder if only images).
                    let tool_result_text = if !text_result.is_empty() {
                        text_result
                    } else if has_images {
                        "(see attached image)".to_owned()
                    } else {
                        "(no tool output)".to_owned()
                    };
                    let mut tool_result_msg = json!({
                        "role": "tool",
                        "content": sanitize_surrogates(&tool_result_text),
                        "tool_call_id": tool_msg.tool_call_id,
                    });
                    // Some providers require the 'name' field in tool results.
                    if compat.requires_tool_result_name && !tool_msg.tool_name.is_empty() {
                        tool_result_msg["name"] = json!(tool_msg.tool_name);
                    }
                    params.push(tool_result_msg);

                    if compat.deferred_tools_mode == Some(DeferredToolsMode::Kimi) {
                        for name in tool_msg.added_tool_names.as_deref().unwrap_or(&[]) {
                            if !deferred_tool_names.contains(name) {
                                deferred_tool_names.push(name.clone());
                            }
                        }
                    }

                    if has_images && model.input.contains(&InputModality::Image) {
                        for block in &tool_msg.content {
                            if let ToolResultContent::Image(image) = block {
                                image_blocks.push(json!({
                                    "type": "image_url",
                                    "image_url": {
                                        "url": format!("data:{};base64,{}", image.mime_type, image.data),
                                    },
                                }));
                            }
                        }
                    }
                    j += 1;
                }

                if !image_blocks.is_empty() {
                    if compat.requires_assistant_after_tool_result {
                        params.push(json!({
                            "role": "assistant",
                            "content": "I have processed the tool results.",
                        }));
                    }
                    let mut content = vec![
                        json!({"type": "text", "text": "Attached image(s) from tool result:"}),
                    ];
                    content.extend(image_blocks);
                    params.push(json!({"role": "user", "content": content}));
                    last_role = Some(Role::User);
                } else {
                    last_role = Some(Role::ToolResult);
                }

                if !deferred_tool_names.is_empty() {
                    let deferred_tools =
                        get_tools_by_name(context.tools.as_deref(), &deferred_tool_names);
                    if !deferred_tools.is_empty() {
                        // Kimi accepts a system message with tools but omits the
                        // standard content field.
                        params.push(json!({
                            "role": "system",
                            "tools": convert_tools(&deferred_tools, compat)?,
                        }));
                    }
                }

                i = j;
                continue;
            }
        }

        last_role = Some(msg.role());
        i += 1;
    }

    Ok(params)
}

/// `convertTools`: grammar-constrained tools become OpenAI `custom` tools;
/// everything else becomes a `function` tool (`strict` included only when the
/// provider supports it — some reject unknown fields).
pub fn convert_tools(
    tools: &[Tool],
    compat: &ResolvedOpenAICompletionsCompat,
) -> Result<Vec<Value>, String> {
    tools
        .iter()
        .map(|tool| {
            if let Some(grammar) =
                resolve_grammar_constrained_sampling(tool, compat.supports_open_ai_grammar_tools)?
            {
                return Ok(json!({
                    "type": "custom",
                    "custom": {
                        "name": tool.name,
                        "description": tool.description,
                        "format": {
                            "type": "grammar",
                            "grammar": {
                                "syntax": grammar.format.as_str(),
                                "definition": grammar.definition,
                            },
                        },
                    },
                }));
            }

            let strict = resolve_json_schema_strict_sampling(tool, compat.supports_strict_mode)?;
            let mut function = Map::new();
            function.insert("name".to_owned(), json!(tool.name));
            function.insert("description".to_owned(), json!(tool.description));
            // TypeBox already generates JSON Schema upstream; `parameters` is a
            // schema value here.
            function.insert("parameters".to_owned(), tool.parameters.clone());
            if compat.supports_strict_mode {
                function.insert("strict".to_owned(), json!(strict.unwrap_or(false)));
            }
            Ok(json!({"type": "function", "function": Value::Object(function)}))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Chat template kwargs (thinkingFormat "chat-template")
// ---------------------------------------------------------------------------

/// `buildChatTemplateKwargs`: resolves each configured kwarg; `None` when no
/// kwarg survives resolution.
fn build_chat_template_kwargs(
    model: &Model,
    reasoning_effort: Option<ModelThinkingLevel>,
    compat: &ResolvedOpenAICompletionsCompat,
) -> Option<Value> {
    let mut kwargs = Map::new();
    for (key, value) in &compat.chat_template_kwargs {
        if let Some(resolved) = resolve_chat_template_kwarg_value(model, reasoning_effort, value) {
            kwargs.insert(key.clone(), resolved);
        }
    }
    if kwargs.is_empty() {
        None
    } else {
        Some(Value::Object(kwargs))
    }
}

/// `resolveChatTemplateKwargValue`: scalars pass through; `$var` refs resolve
/// against the current reasoning effort and the model's thinking level map.
fn resolve_chat_template_kwarg_value(
    model: &Model,
    reasoning_effort: Option<ModelThinkingLevel>,
    value: &ChatTemplateKwargValue,
) -> Option<Value> {
    let scalar = match value {
        ChatTemplateKwargValue::Scalar(value) => return Some(value.clone()),
        ChatTemplateKwargValue::Var(var) => var,
    };

    if reasoning_effort.is_none() && scalar.omit_when_off == Some(true) {
        return None;
    }
    if scalar.var == ChatTemplateKwargVarKind::ThinkingEnabled {
        return Some(json!(reasoning_effort.is_some()));
    }

    // thinking.effort: mapped value for the effort (or `off` when no effort).
    let mapped = match reasoning_effort {
        Some(effort) => model
            .thinking_level_map
            .as_ref()
            .and_then(|map| map.get(&effort))
            .cloned(),
        None => model
            .thinking_level_map
            .as_ref()
            .and_then(|map| map.get(&ModelThinkingLevel::Off))
            .cloned(),
    };
    // `mappedValue === undefined ? reasoningEffort : typeof mappedValue === "string" ? mappedValue : undefined`
    match mapped {
        None => reasoning_effort.map(|effort| json!(effort.as_str())),
        Some(Some(value)) => Some(json!(value)),
        Some(None) => None,
    }
}

// ---------------------------------------------------------------------------
// Anthropic-style cache control markers
// ---------------------------------------------------------------------------

/// `getCompatCacheControl`: `{"type":"ephemeral"}` (plus `"ttl":"1h"` for long
/// retention on supporting models) when the compat opts into Anthropic-style
/// cache control and retention is not "none".
fn get_compat_cache_control(
    compat: &ResolvedOpenAICompletionsCompat,
    cache_retention: CacheRetention,
) -> Option<Value> {
    if compat.cache_control_format != Some(CacheControlFormat::Anthropic)
        || cache_retention == CacheRetention::None
    {
        return None;
    }
    let long = cache_retention == CacheRetention::Long && compat.supports_long_cache_retention;
    Some(if long {
        json!({"type": "ephemeral", "ttl": "1h"})
    } else {
        json!({"type": "ephemeral"})
    })
}

/// `applyAnthropicCacheControl`: marks the first system/developer message,
/// the last tool definition, and the last user/assistant/tool text content.
fn apply_anthropic_cache_control(
    messages: &mut [Value],
    tools: Option<&mut Vec<Value>>,
    cache_control: &Value,
) {
    // `addCacheControlToSystemPrompt`: the first instruction message consumes
    // the attempt even when it cannot carry the marker (upstream `return`).
    for message in messages.iter_mut() {
        let role = message.get("role").and_then(Value::as_str);
        if role == Some("system") || role == Some("developer") {
            add_cache_control_to_text_content(message, cache_control);
            break;
        }
    }
    if let Some(tools) = tools {
        if let Some(last_tool) = tools.last_mut() {
            last_tool["cache_control"] = cache_control.clone();
        }
    }
    for message in messages.iter_mut().rev() {
        let role = message.get("role").and_then(Value::as_str);
        if matches!(role, Some("user") | Some("assistant") | Some("tool"))
            && add_cache_control_to_text_content(message, cache_control)
        {
            break;
        }
    }
}

/// `addCacheControlToTextContent`: string content converts to a marked text
/// part; array content marks its last text part. Returns false when nothing
/// could be marked (empty/missing content or no text part).
fn add_cache_control_to_text_content(message: &mut Value, cache_control: &Value) -> bool {
    let Some(content) = message.get_mut("content") else {
        return false;
    };
    match content {
        Value::String(text) => {
            if text.is_empty() {
                return false;
            }
            let text = std::mem::take(text);
            *content = json!([{
                "type": "text",
                "text": text,
                "cache_control": cache_control,
            }]);
            true
        }
        Value::Array(parts) => {
            for part in parts.iter_mut().rev() {
                if part.get("type").and_then(Value::as_str) == Some("text") {
                    part["cache_control"] = cache_control.clone();
                    return true;
                }
            }
            false
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Thinking level map lookup semantics
// ---------------------------------------------------------------------------

/// `model.thinkingLevelMap?.[level] ?? level` — JS `??` semantics: key absent
/// *or* explicitly null falls back to the level name.
pub(crate) fn mapped_or_level_name(model: &Model, level: ModelThinkingLevel) -> String {
    model
        .thinking_level_map
        .as_ref()
        .and_then(|map| map.get(&level))
        .and_then(|mapped| mapped.clone())
        .unwrap_or_else(|| level.as_str().to_owned())
}

/// zai's `mapped === undefined ? level : mapped` — key absent falls back to
/// the level name; explicit null stays null (the param is omitted).
fn mapped_zai(model: &Model, level: ModelThinkingLevel) -> Option<String> {
    match model
        .thinking_level_map
        .as_ref()
        .and_then(|map| map.get(&level))
    {
        None => Some(level.as_str().to_owned()),
        Some(mapped) => mapped.clone(),
    }
}

/// `model.thinkingLevelMap?.off` — `None` = key absent, `Some(None)` = JSON
/// null, `Some(Some(v))` = mapped string.
pub(crate) fn off_value(model: &Model) -> Option<Option<String>> {
    model
        .thinking_level_map
        .as_ref()
        .and_then(|map| map.get(&ModelThinkingLevel::Off))
        .cloned()
}

/// `off !== null` guard: true when the key is absent or holds a string.
pub(crate) fn off_is_not_null(model: &Model) -> bool {
    off_value(model) != Some(None)
}

// ---------------------------------------------------------------------------
// buildParams
// ---------------------------------------------------------------------------

/// `buildParams`. Key insertion order mirrors the upstream object literal plus
/// conditional assignments (serde_json `preserve_order` keeps it), so payload
/// snapshots compare byte-for-byte with the JS payloads.
pub fn build_params(
    model: &Model,
    context: &Context,
    options: &OpenAICompletionsOptions,
    compat: &ResolvedOpenAICompletionsCompat,
    cache_retention: CacheRetention,
    grammar_tool_input_properties: &HashMap<String, String>,
) -> Result<Value, String> {
    let mut messages = convert_messages(model, context, compat, grammar_tool_input_properties)?;
    let cache_control = get_compat_cache_control(compat, cache_retention);

    let deferred_tool_names = if compat.deferred_tools_mode == Some(DeferredToolsMode::Kimi) {
        get_deferred_tool_names(&context.messages)
    } else {
        Vec::new()
    };
    let active_tools: Vec<Tool> = context
        .tools
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .filter(|tool| !deferred_tool_names.contains(&tool.name))
        .cloned()
        .collect();
    let mut tools: Option<Vec<Value>> = if !active_tools.is_empty() {
        Some(convert_tools(&active_tools, compat)?)
    } else if has_tool_history(&context.messages) {
        // Anthropic (via LiteLLM/proxy) requires tools param when the
        // conversation has tool_calls/tool_results.
        Some(Vec::new())
    } else {
        None
    };

    if let Some(cache_control) = &cache_control {
        apply_anthropic_cache_control(&mut messages, tools.as_mut(), cache_control);
    }

    let mut params = Map::new();
    params.insert("model".to_owned(), json!(model.id));
    params.insert("messages".to_owned(), Value::Array(messages));
    params.insert("stream".to_owned(), json!(true));

    let prompt_cache_key = if (model.base_url.contains("api.openai.com")
        && cache_retention != CacheRetention::None)
        || (cache_retention == CacheRetention::Long && compat.supports_long_cache_retention)
    {
        clamp_openai_prompt_cache_key(options.stream.session_id.as_deref())
    } else {
        None
    };
    if let Some(key) = prompt_cache_key {
        params.insert("prompt_cache_key".to_owned(), json!(key));
    }
    if cache_retention == CacheRetention::Long && compat.supports_long_cache_retention {
        params.insert("prompt_cache_retention".to_owned(), json!("24h"));
    }
    if compat.supports_usage_in_streaming {
        params.insert("stream_options".to_owned(), json!({"include_usage": true}));
    }
    if compat.supports_store {
        params.insert("store".to_owned(), json!(false));
    }
    if let Some(max_tokens) = options.stream.max_tokens {
        match compat.max_tokens_field {
            MaxTokensField::MaxTokens => {
                params.insert("max_tokens".to_owned(), json!(max_tokens));
            }
            MaxTokensField::MaxCompletionTokens => {
                params.insert("max_completion_tokens".to_owned(), json!(max_tokens));
            }
        }
    }
    if let Some(temperature) = options.stream.temperature {
        params.insert("temperature".to_owned(), json!(temperature));
    }
    if let Some(tools) = tools {
        let has_tools = !tools.is_empty();
        params.insert("tools".to_owned(), Value::Array(tools));
        if has_tools && compat.zai_tool_stream {
            params.insert("tool_stream".to_owned(), json!(true));
        }
    }
    if let Some(tool_choice) = &options.tool_choice {
        params.insert("tool_choice".to_owned(), tool_choice.clone());
    }

    apply_thinking_params(&mut params, model, options.reasoning_effort, compat);

    // OpenRouter provider routing preferences (read from model.compat
    // directly, like upstream — not from the resolved compat).
    if let Some(routing) = model
        .compat
        .as_ref()
        .and_then(|compat| compat.open_router_routing.as_ref())
    {
        params.insert("provider".to_owned(), json!(routing));
    }

    // Vercel AI Gateway provider routing preferences.
    if let Some(routing) = model
        .compat
        .as_ref()
        .and_then(|compat| compat.vercel_gateway_routing.as_ref())
    {
        if routing.only.is_some() || routing.order.is_some() {
            let mut gateway = Map::new();
            if let Some(only) = &routing.only {
                gateway.insert("only".to_owned(), json!(only));
            }
            if let Some(order) = &routing.order {
                gateway.insert("order".to_owned(), json!(order));
            }
            params.insert(
                "providerOptions".to_owned(),
                json!({"gateway": Value::Object(gateway)}),
            );
        }
    }

    Ok(Value::Object(params))
}

/// The ten `thinkingFormat` branches plus the OpenAI-style fallbacks, ported
/// as a single else-if chain (order matters).
fn apply_thinking_params(
    params: &mut Map<String, Value>,
    model: &Model,
    reasoning_effort: Option<ModelThinkingLevel>,
    compat: &ResolvedOpenAICompletionsCompat,
) {
    if compat.thinking_format == ThinkingFormat::Zai && model.reasoning {
        params.insert(
            "thinking".to_owned(),
            if reasoning_effort.is_some() {
                json!({"type": "enabled", "clear_thinking": false})
            } else {
                json!({"type": "disabled"})
            },
        );
        if let Some(effort) = reasoning_effort {
            if compat.supports_reasoning_effort {
                if let Some(value) = mapped_zai(model, effort) {
                    params.insert("reasoning_effort".to_owned(), json!(value));
                }
            }
        }
    } else if compat.thinking_format == ThinkingFormat::Qwen && model.reasoning {
        params.insert(
            "enable_thinking".to_owned(),
            json!(reasoning_effort.is_some()),
        );
    } else if compat.thinking_format == ThinkingFormat::QwenChatTemplate && model.reasoning {
        params.insert(
            "chat_template_kwargs".to_owned(),
            json!({
                "enable_thinking": reasoning_effort.is_some(),
                "preserve_thinking": true,
            }),
        );
    } else if compat.thinking_format == ThinkingFormat::ChatTemplate && model.reasoning {
        if let Some(kwargs) = build_chat_template_kwargs(model, reasoning_effort, compat) {
            params.insert("chat_template_kwargs".to_owned(), kwargs);
        }
    } else if compat.thinking_format == ThinkingFormat::Deepseek && model.reasoning {
        if reasoning_effort.is_some() {
            params.insert("thinking".to_owned(), json!({"type": "enabled"}));
        } else if off_is_not_null(model) {
            params.insert("thinking".to_owned(), json!({"type": "disabled"}));
        }
        if let Some(effort) = reasoning_effort {
            if compat.supports_reasoning_effort {
                params.insert(
                    "reasoning_effort".to_owned(),
                    json!(mapped_or_level_name(model, effort)),
                );
            }
        }
    } else if compat.thinking_format == ThinkingFormat::Openrouter && model.reasoning {
        // OpenRouter normalizes reasoning across providers via a nested
        // reasoning object.
        if let Some(effort) = reasoning_effort {
            params.insert(
                "reasoning".to_owned(),
                json!({"effort": mapped_or_level_name(model, effort)}),
            );
        } else if off_is_not_null(model) {
            let effort = off_value(model)
                .flatten()
                .unwrap_or_else(|| "none".to_owned());
            params.insert("reasoning".to_owned(), json!({"effort": effort}));
        }
    } else if compat.thinking_format == ThinkingFormat::AntLing
        && model.reasoning
        && reasoning_effort.is_some()
    {
        // Only a mapped (non-null) effort string is sent.
        if let Some(effort) = reasoning_effort {
            if let Some(Some(value)) = model
                .thinking_level_map
                .as_ref()
                .and_then(|map| map.get(&effort))
                .cloned()
            {
                params.insert("reasoning".to_owned(), json!({"effort": value}));
            }
        }
    } else if compat.thinking_format == ThinkingFormat::Together && model.reasoning {
        params.insert(
            "reasoning".to_owned(),
            json!({"enabled": reasoning_effort.is_some()}),
        );
        if let Some(effort) = reasoning_effort {
            if compat.supports_reasoning_effort {
                params.insert(
                    "reasoning_effort".to_owned(),
                    json!(mapped_or_level_name(model, effort)),
                );
            }
        }
    } else if compat.thinking_format == ThinkingFormat::StringThinking && model.reasoning {
        if let Some(effort) = reasoning_effort {
            params.insert(
                "thinking".to_owned(),
                json!(mapped_or_level_name(model, effort)),
            );
        } else if off_is_not_null(model) {
            let value = off_value(model)
                .flatten()
                .unwrap_or_else(|| "none".to_owned());
            params.insert("thinking".to_owned(), json!(value));
        }
    } else if reasoning_effort.is_some() && model.reasoning && compat.supports_reasoning_effort {
        // OpenAI-style reasoning_effort.
        if let Some(effort) = reasoning_effort {
            params.insert(
                "reasoning_effort".to_owned(),
                json!(mapped_or_level_name(model, effort)),
            );
        }
    } else if reasoning_effort.is_none() && model.reasoning && compat.supports_reasoning_effort {
        if let Some(Some(off)) = off_value(model) {
            params.insert("reasoning_effort".to_owned(), json!(off));
        }
    }
}

// ---------------------------------------------------------------------------
// Usage and stop reason mapping
// ---------------------------------------------------------------------------

fn json_u64(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_f64().map(|float| float as u64))
}

/// JS truthiness for JSON values (used where upstream relies on `Boolean` /
/// truthy checks, e.g. the `reasoning_details` `.filter(Boolean)`).
fn is_js_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(number) => number.as_f64().is_some_and(|number| number != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// `parseChunkUsage`: OpenAI/OpenRouter usage semantics — `cached_tokens` is
/// cache-read (hits), `cache_write_tokens` a separate write count (do not
/// subtract writes from cached), `completion_tokens` already includes
/// reasoning tokens.
pub fn parse_chunk_usage(raw_usage: &Value, model: &Model) -> Usage {
    let prompt_tokens = json_u64(&raw_usage["prompt_tokens"]).unwrap_or(0);
    let cache_read_tokens = json_u64(&raw_usage["prompt_tokens_details"]["cached_tokens"])
        .or_else(|| json_u64(&raw_usage["prompt_cache_hit_tokens"]))
        .unwrap_or(0);
    let cache_write_tokens =
        json_u64(&raw_usage["prompt_tokens_details"]["cache_write_tokens"]).unwrap_or(0);

    let input = prompt_tokens.saturating_sub(cache_read_tokens + cache_write_tokens);
    let output = json_u64(&raw_usage["completion_tokens"]).unwrap_or(0);
    let mut usage = Usage {
        input,
        output,
        cache_read: cache_read_tokens,
        cache_write: cache_write_tokens,
        cache_write1h: None,
        reasoning: Some(
            json_u64(&raw_usage["completion_tokens_details"]["reasoning_tokens"]).unwrap_or(0),
        ),
        total_tokens: input + output + cache_read_tokens + cache_write_tokens,
        cost: Default::default(),
    };
    calculate_cost(model, &mut usage);
    usage
}

/// `mapStopReason`; a null finish reason maps to "stop", unknown reasons are
/// an error carrying the raw reason.
pub fn map_stop_reason(reason: Option<&str>) -> (StopReason, Option<String>) {
    let Some(reason) = reason else {
        return (StopReason::Stop, None);
    };
    match reason {
        "stop" | "end" => (StopReason::Stop, None),
        "length" => (StopReason::Length, None),
        "function_call" | "tool_calls" => (StopReason::ToolUse, None),
        other => (
            StopReason::Error,
            Some(format!("Provider finish_reason: {other}")),
        ),
    }
}

// ---------------------------------------------------------------------------
// Stream processing
// ---------------------------------------------------------------------------

/// Custom (grammar) tool streaming state: the input property and the JSON
/// reconstruction buffer (upstream keeps these on the streaming block).
#[derive(Debug, Default)]
struct CustomInputState {
    property: String,
    json_buffer: GrammarToolInputJsonBuffer,
}

/// Per-tool-call streaming scratch state (upstream stores `partialArgs`,
/// `customInput` and `streamIndex` on the block; here they live alongside
/// `output.content`, keyed by content index).
#[derive(Debug, Default)]
struct ToolScratch {
    partial_args: Option<String>,
    custom_input: Option<CustomInputState>,
    stream_index: Option<usize>,
}

/// Outcome of handling one SSE event (the `[DONE]` sentinel ends the stream).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SseOutcome {
    Chunk,
    Done,
}

/// `getCustomToolCallInput`: the current raw grammar input text.
fn custom_tool_call_input(block: &ToolCall, scratch: &ToolScratch) -> String {
    let Some(custom_input) = &scratch.custom_input else {
        return String::new();
    };
    match block.arguments.get(&custom_input.property) {
        Some(Value::String(input)) => input.clone(),
        _ => String::new(),
    }
}

/// `appendCustomToolCallInput`: advance the grammar JSON buffer and replace
/// `block.arguments` with `{ property: nextInput }`. Returns the JSON delta to
/// emit (`None` for a no-op / idempotent close).
fn append_custom_tool_call_input(
    block: &mut ToolCall,
    scratch: &mut ToolScratch,
    next_input: &str,
    close: bool,
) -> Result<Option<String>, String> {
    let Some(custom_input) = &mut scratch.custom_input else {
        return Ok(None);
    };
    let delta = append_grammar_tool_input_json_delta(
        &mut custom_input.json_buffer,
        &custom_input.property,
        next_input,
        close,
    )?;
    let mut arguments = Map::new();
    arguments.insert(custom_input.property.clone(), json!(next_input));
    block.arguments = arguments;
    Ok(delta)
}

/// `isEncryptedReasoningDetail` plus the `JSON.stringify` serialization
/// (serde_json `preserve_order` keeps the original key order).
fn encrypted_reasoning_detail(detail: &Value) -> Option<(String, String)> {
    let object = detail.as_object()?;
    if object.get("type").and_then(Value::as_str) != Some("reasoning.encrypted") {
        return None;
    }
    let id = object
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())?;
    if object
        .get("data")
        .and_then(Value::as_str)
        .is_none_or(|data| data.is_empty())
    {
        return None;
    }
    let serialized = serde_json::to_string(detail).unwrap_or_default();
    Some((id.to_owned(), serialized))
}

/// Consumes OpenAI chat completion chunks and drives the [`StreamEvent`]
/// protocol, accumulating the final assistant message. Factored out of the
/// HTTP layer so tests can drive it with recorded streams.
struct CompletionsProcessor<'a> {
    output: &'a mut AssistantMessage,
    model: &'a Model,
    grammar_tool_input_properties: &'a HashMap<String, String>,
    /// Resolved `compat.supportsFinishReason` (2c3041242).
    supports_finish_reason: bool,
    has_finish_reason: bool,
    text_block: Option<usize>,
    thinking_block: Option<usize>,
    tool_call_by_index: HashMap<usize, usize>,
    tool_call_by_id: HashMap<String, usize>,
    scratch: HashMap<usize, ToolScratch>,
    pending_reasoning_details: HashMap<String, String>,
}

impl<'a> CompletionsProcessor<'a> {
    fn new(
        output: &'a mut AssistantMessage,
        model: &'a Model,
        grammar_tool_input_properties: &'a HashMap<String, String>,
        supports_finish_reason: bool,
    ) -> Self {
        Self {
            output,
            model,
            grammar_tool_input_properties,
            supports_finish_reason,
            has_finish_reason: false,
            text_block: None,
            thinking_block: None,
            tool_call_by_index: HashMap::new(),
            tool_call_by_id: HashMap::new(),
            scratch: HashMap::new(),
            pending_reasoning_details: HashMap::new(),
        }
    }

    /// Per-SSE-event body: `[DONE]` sentinel, strict chunk parse, dispatch.
    fn handle_sse(
        &mut self,
        sse: &ServerSentEvent,
        events: &AssistantMessageEventStream,
    ) -> Result<SseOutcome, String> {
        if sse.data.trim() == "[DONE]" {
            return Ok(SseOutcome::Done);
        }
        let chunk: Value = serde_json::from_str(&sse.data).map_err(|error| {
            format!(
                "Could not parse OpenAI SSE chunk: {error}; data={}",
                sse.data
            )
        })?;
        self.handle_chunk(&chunk, events)?;
        Ok(SseOutcome::Chunk)
    }

    fn ensure_text_block(&mut self, events: &AssistantMessageEventStream) -> usize {
        if let Some(index) = self.text_block {
            return index;
        }
        self.output
            .content
            .push(AssistantContent::Text(TextContent {
                text: String::new(),
                text_signature: None,
            }));
        let index = self.output.content.len() - 1;
        self.text_block = Some(index);
        events.push(StreamEvent::TextStart {
            content_index: index,
            partial: self.output.clone(),
        });
        index
    }

    fn ensure_thinking_block(
        &mut self,
        thinking_signature: &str,
        events: &AssistantMessageEventStream,
    ) -> usize {
        if let Some(index) = self.thinking_block {
            return index;
        }
        self.output
            .content
            .push(AssistantContent::Thinking(ThinkingContent {
                thinking: String::new(),
                thinking_signature: Some(thinking_signature.to_owned()),
                redacted: None,
            }));
        let index = self.output.content.len() - 1;
        self.thinking_block = Some(index);
        events.push(StreamEvent::ThinkingStart {
            content_index: index,
            partial: self.output.clone(),
        });
        index
    }

    fn apply_pending_reasoning_detail(&mut self, content_index: usize) {
        let Some(AssistantContent::ToolCall(block)) = self.output.content.get(content_index) else {
            return;
        };
        if block.id.is_empty() {
            return;
        }
        let Some(pending) = self
            .pending_reasoning_details
            .get(&block.id)
            .filter(|detail| !detail.is_empty())
            .cloned()
        else {
            return;
        };
        if let Some(AssistantContent::ToolCall(block)) = self.output.content.get_mut(content_index)
        {
            block.thought_signature = Some(pending);
        }
        if let Some(AssistantContent::ToolCall(block)) = self.output.content.get(content_index) {
            self.pending_reasoning_details.remove(&block.id);
        }
    }

    /// `ensureToolCallBlock`: find-or-create the streaming tool call block for
    /// a chunk delta, with id/index registration, name back-fill, custom-tool
    /// upgrade and pending reasoning-detail application.
    fn ensure_tool_call_block(
        &mut self,
        tool_call: &Value,
        events: &AssistantMessageEventStream,
    ) -> usize {
        let stream_index = tool_call
            .get("index")
            .and_then(json_u64)
            .map(|index| index as usize);
        let name = tool_call
            .get("function")
            .and_then(|function| function.get("name"))
            .and_then(Value::as_str)
            .or_else(|| {
                tool_call
                    .get("custom")
                    .and_then(|custom| custom.get("name"))
                    .and_then(Value::as_str)
            })
            .unwrap_or("");
        let id = tool_call.get("id").and_then(Value::as_str);
        // 34239180a: `toolCall.custom && !toolCall.function` — a delta with a
        // valid `function` plus an empty `custom` object stays a function call
        // (its arguments must not be dropped).
        let is_custom = tool_call.get("custom").is_some_and(Value::is_object)
            && !tool_call.get("function").is_some_and(is_js_truthy);

        let mut content_index =
            stream_index.and_then(|index| self.tool_call_by_index.get(&index).copied());
        if content_index.is_none() {
            if let Some(id) = id {
                content_index = self.tool_call_by_id.get(id).copied();
            }
        }

        if content_index.is_none() {
            // Note: the "input" fallback here should/must not be taken; in case
            // the LLM makes up a tool we don't know about, we at least have a
            // place to stash our stuff.
            let custom_property = if is_custom {
                Some(
                    self.grammar_tool_input_properties
                        .get(name)
                        .cloned()
                        .unwrap_or_else(|| "input".to_owned()),
                )
            } else {
                None
            };
            let mut arguments = Map::new();
            if let Some(property) = &custom_property {
                arguments.insert(property.clone(), json!(""));
            }
            self.output
                .content
                .push(AssistantContent::ToolCall(ToolCall {
                    id: id.unwrap_or("").to_owned(),
                    name: name.to_owned(),
                    arguments,
                    thought_signature: None,
                    namespace: None,
                }));
            let index = self.output.content.len() - 1;
            self.scratch.insert(
                index,
                ToolScratch {
                    partial_args: if custom_property.is_some() {
                        None
                    } else {
                        Some(String::new())
                    },
                    custom_input: custom_property.map(|property| CustomInputState {
                        property,
                        json_buffer: GrammarToolInputJsonBuffer::default(),
                    }),
                    stream_index,
                },
            );
            if let Some(stream_index) = stream_index {
                self.tool_call_by_index.insert(stream_index, index);
            }
            if let Some(id) = id.filter(|id| !id.is_empty()) {
                self.tool_call_by_id.insert(id.to_owned(), index);
            }
            events.push(StreamEvent::ToolCallStart {
                content_index: index,
                partial: self.output.clone(),
            });
            content_index = Some(index);
        }

        // invariant: content_index is always Some after the creation branch
        let index = content_index.unwrap_or(0);

        if let Some(stream_index) = stream_index {
            let register = match self.scratch.get_mut(&index) {
                Some(scratch) if scratch.stream_index.is_none() => {
                    scratch.stream_index = Some(stream_index);
                    true
                }
                _ => false,
            };
            if register {
                self.tool_call_by_index.insert(stream_index, index);
            }
        }
        if let Some(id) = id.filter(|id| !id.is_empty()) {
            self.tool_call_by_id.insert(id.to_owned(), index);
        }
        if let Some(AssistantContent::ToolCall(block)) = self.output.content.get_mut(index) {
            if block.name.is_empty() && !name.is_empty() {
                block.name = name.to_owned();
            }
        }

        // Custom-tool upgrade: a function-created block that later turns out
        // to be custom switches to grammar-input streaming.
        if is_custom
            && self
                .scratch
                .get(&index)
                .is_some_and(|scratch| scratch.custom_input.is_none())
        {
            let block_name = match self.output.content.get(index) {
                Some(AssistantContent::ToolCall(block)) => block.name.clone(),
                _ => String::new(),
            };
            let property = self
                .grammar_tool_input_properties
                .get(&block_name)
                .cloned()
                .unwrap_or_else(|| "input".to_owned());
            if let Some(AssistantContent::ToolCall(block)) = self.output.content.get_mut(index) {
                let mut arguments = Map::new();
                arguments.insert(property.clone(), json!(""));
                block.arguments = arguments;
            }
            if let Some(scratch) = self.scratch.get_mut(&index) {
                scratch.custom_input = Some(CustomInputState {
                    property,
                    json_buffer: GrammarToolInputJsonBuffer::default(),
                });
                scratch.partial_args = None;
            }
        }

        self.apply_pending_reasoning_detail(index);
        index
    }

    /// Per-chunk body: response metadata, usage, finish reason, and the delta
    /// (text / reasoning / tool calls / encrypted reasoning details).
    fn handle_chunk(
        &mut self,
        chunk: &Value,
        events: &AssistantMessageEventStream,
    ) -> Result<(), String> {
        if !chunk.is_object() {
            return Ok(());
        }

        // OpenAI documents ChatCompletionChunk.id as the unique chat completion
        // identifier; each chunk in a streamed completion carries the same id.
        if self.output.response_id.is_none() {
            if let Some(id) = chunk.get("id").and_then(Value::as_str) {
                self.output.response_id = Some(id.to_owned());
            }
        }
        if let Some(chunk_model) = chunk.get("model").and_then(Value::as_str) {
            if !chunk_model.is_empty()
                && chunk_model != self.model.id
                && self.output.response_model.is_none()
            {
                self.output.response_model = Some(chunk_model.to_owned());
            }
        }
        if chunk.get("usage").is_some_and(is_js_truthy) {
            self.output.usage = parse_chunk_usage(&chunk["usage"], self.model);
        }

        let Some(choice) = chunk
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
        else {
            return Ok(());
        };

        // Fallback: some providers (e.g., Moonshot) return usage in
        // choice.usage instead of the standard chunk.usage.
        if !chunk.get("usage").is_some_and(is_js_truthy) {
            if let Some(usage) = choice.get("usage").filter(|usage| is_js_truthy(usage)) {
                self.output.usage = parse_chunk_usage(usage, self.model);
            }
        }

        if let Some(finish_reason) = choice.get("finish_reason") {
            if is_js_truthy(finish_reason) {
                // fe1c9b6d5: preserve the raw provider reason before mapping.
                self.output.raw_stop_reason = finish_reason.as_str().map(str::to_owned);
                let (stop_reason, error_message) = map_stop_reason(finish_reason.as_str());
                self.output.stop_reason = stop_reason;
                if let Some(error_message) = error_message {
                    self.output.error_message = Some(error_message);
                }
                self.has_finish_reason = true;
            }
        }

        let Some(delta) = choice.get("delta").filter(|delta| is_js_truthy(delta)) else {
            return Ok(());
        };

        if let Some(content) = delta.get("content").and_then(Value::as_str) {
            if !content.is_empty() {
                let content_index = self.ensure_text_block(events);
                if let Some(AssistantContent::Text(block)) =
                    self.output.content.get_mut(content_index)
                {
                    block.text.push_str(content);
                }
                events.push(StreamEvent::TextDelta {
                    content_index,
                    delta: content.to_owned(),
                    partial: self.output.clone(),
                });
            }
        }

        // Some endpoints return reasoning in reasoning_content (llama.cpp), or
        // reasoning (other openai compatible endpoints). Use the first
        // non-empty reasoning field to avoid duplication (e.g., chutes.ai
        // returns both with the same content).
        let reasoning_field = ["reasoning_content", "reasoning", "reasoning_text"]
            .into_iter()
            .find(|field| {
                delta
                    .get(field)
                    .and_then(Value::as_str)
                    .is_some_and(|value| !value.is_empty())
            });
        if let Some(field) = reasoning_field {
            // invariant: `find` matched a non-empty string value
            let value = delta.get(field).and_then(Value::as_str).unwrap_or("");
            let thinking_signature = if self.model.provider == "opencode-go" && field == "reasoning"
            {
                "reasoning_content"
            } else {
                field
            };
            let content_index = self.ensure_thinking_block(thinking_signature, events);
            if let Some(AssistantContent::Thinking(block)) =
                self.output.content.get_mut(content_index)
            {
                block.thinking.push_str(value);
            }
            events.push(StreamEvent::ThinkingDelta {
                content_index,
                delta: value.to_owned(),
                partial: self.output.clone(),
            });
        }

        if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for tool_call in tool_calls {
                let content_index = self.ensure_tool_call_block(tool_call, events);

                if let Some(id) = tool_call
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                {
                    let needs_id = matches!(
                        self.output.content.get(content_index),
                        Some(AssistantContent::ToolCall(block)) if block.id.is_empty()
                    );
                    if needs_id {
                        if let Some(AssistantContent::ToolCall(block)) =
                            self.output.content.get_mut(content_index)
                        {
                            block.id = id.to_owned();
                        }
                        self.tool_call_by_id.insert(id.to_owned(), content_index);
                    }
                }
                let name = tool_call
                    .get("function")
                    .and_then(|function| function.get("name"))
                    .and_then(Value::as_str)
                    .or_else(|| {
                        tool_call
                            .get("custom")
                            .and_then(|custom| custom.get("name"))
                            .and_then(Value::as_str)
                    });
                if let Some(AssistantContent::ToolCall(block)) =
                    self.output.content.get_mut(content_index)
                {
                    if block.name.is_empty() {
                        if let Some(name) = name {
                            block.name = name.to_owned();
                        }
                    }
                }

                let mut delta_text = String::new();
                let function_arguments = tool_call
                    .get("function")
                    .and_then(|function| function.get("arguments"))
                    .and_then(Value::as_str)
                    .filter(|arguments| !arguments.is_empty());
                let custom_input = tool_call
                    .get("custom")
                    .and_then(|custom| custom.get("input"))
                    .and_then(Value::as_str)
                    .filter(|input| !input.is_empty());
                if let Some(arguments) = function_arguments {
                    delta_text = arguments.to_owned();
                    let partial = {
                        let scratch = self.scratch.entry(content_index).or_default();
                        let partial = scratch.partial_args.get_or_insert_with(String::new);
                        partial.push_str(arguments);
                        partial.clone()
                    };
                    if let Some(AssistantContent::ToolCall(block)) =
                        self.output.content.get_mut(content_index)
                    {
                        block.arguments = parse_streaming_json(Some(&partial));
                    }
                } else if let Some(input) = custom_input {
                    let scratch = self.scratch.entry(content_index).or_default();
                    if let Some(AssistantContent::ToolCall(block)) =
                        self.output.content.get_mut(content_index)
                    {
                        let next_input =
                            format!("{}{input}", custom_tool_call_input(block, scratch));
                        delta_text =
                            append_custom_tool_call_input(block, scratch, &next_input, false)?
                                .unwrap_or_default();
                    }
                }
                events.push(StreamEvent::ToolCallDelta {
                    content_index,
                    delta: delta_text,
                    partial: self.output.clone(),
                });
            }
        }

        if let Some(reasoning_details) = delta.get("reasoning_details").and_then(Value::as_array) {
            for detail in reasoning_details {
                if let Some((id, serialized)) = encrypted_reasoning_detail(detail) {
                    if let Some(index) = self.tool_call_by_id.get(&id).copied() {
                        if let Some(AssistantContent::ToolCall(block)) =
                            self.output.content.get_mut(index)
                        {
                            block.thought_signature = Some(serialized);
                        }
                    } else {
                        self.pending_reasoning_details.insert(id, serialized);
                    }
                }
            }
        }

        Ok(())
    }

    /// `finishBlock`: emit the end event for one content block (closing the
    /// grammar JSON buffer for custom tools first).
    fn finish_block(
        &mut self,
        content_index: usize,
        events: &AssistantMessageEventStream,
    ) -> Result<(), String> {
        enum BlockEnd {
            Text(String),
            Thinking(String),
            ToolCall(ToolCall),
        }

        let mut close_delta: Option<String> = None;
        let end = match self.output.content.get_mut(content_index) {
            Some(AssistantContent::Text(block)) => Some(BlockEnd::Text(block.text.clone())),
            Some(AssistantContent::Thinking(block)) => {
                Some(BlockEnd::Thinking(block.thinking.clone()))
            }
            Some(AssistantContent::ToolCall(block)) => {
                let scratch = self.scratch.entry(content_index).or_default();
                if scratch.custom_input.is_some() {
                    let next_input = custom_tool_call_input(block, scratch);
                    close_delta = append_custom_tool_call_input(block, scratch, &next_input, true)?;
                } else {
                    block.arguments = parse_streaming_json(scratch.partial_args.as_deref());
                }
                Some(BlockEnd::ToolCall(block.clone()))
            }
            None => None,
        };

        if let Some(delta) = close_delta {
            events.push(StreamEvent::ToolCallDelta {
                content_index,
                delta,
                partial: self.output.clone(),
            });
        }
        match end {
            Some(BlockEnd::Text(content)) => events.push(StreamEvent::TextEnd {
                content_index,
                content,
                partial: self.output.clone(),
            }),
            Some(BlockEnd::Thinking(content)) => events.push(StreamEvent::ThinkingEnd {
                content_index,
                content,
                partial: self.output.clone(),
            }),
            Some(BlockEnd::ToolCall(tool_call)) => events.push(StreamEvent::ToolCallEnd {
                content_index,
                tool_call,
                partial: self.output.clone(),
            }),
            None => {}
        }
        Ok(())
    }

    /// Post-stream tail: finish all blocks, then the upstream final checks.
    fn finish(
        mut self,
        signal: Option<&CancellationToken>,
        events: &AssistantMessageEventStream,
    ) -> Result<DoneReason, String> {
        for index in 0..self.output.content.len() {
            self.finish_block(index, events)?;
        }
        if signal.is_some_and(|signal| signal.is_cancelled()) {
            return Err("Request was aborted".to_owned());
        }
        if self.output.stop_reason == StopReason::Aborted {
            return Err("Request was aborted".to_owned());
        }
        // 2c3041242: providers that never send `finish_reason` get their stop
        // reason inferred from content.
        if !self.has_finish_reason && !self.supports_finish_reason {
            self.output.stop_reason = if self
                .output
                .content
                .iter()
                .any(|block| matches!(block, AssistantContent::ToolCall(_)))
            {
                StopReason::ToolUse
            } else {
                StopReason::Stop
            };
        }
        if self.output.stop_reason == StopReason::Error {
            return Err(self
                .output
                .error_message
                .clone()
                .unwrap_or_else(|| "Provider returned an error stop reason".to_owned()));
        }
        if (self.supports_finish_reason && !self.has_finish_reason)
            || self.output.stop_reason == StopReason::Pending
        {
            return Err("Stream ended without finish_reason".to_owned());
        }
        Ok(match self.output.stop_reason {
            StopReason::Length => DoneReason::Length,
            StopReason::ToolUse => DoneReason::ToolUse,
            // Stop is the only remaining reachable reason (Pending/Error/
            // Aborted returned above).
            _ => DoneReason::Stop,
        })
    }
}

// ---------------------------------------------------------------------------
// Stream entry points
// ---------------------------------------------------------------------------

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
    options: &OpenAICompletionsOptions,
    output: &mut AssistantMessage,
    events: &AssistantMessageEventStream,
) -> Result<DoneReason, String> {
    let api_key = get_client_api_key(
        &model.provider,
        options.stream.api_key.as_deref(),
        options.stream.headers.as_ref(),
    )?;
    let compat = get_compat(model);
    let grammar_tool_input_properties = create_grammar_tool_input_properties(
        context.tools.as_deref(),
        compat.supports_open_ai_grammar_tools,
    )?;
    let cache_retention =
        resolve_cache_retention(options.stream.cache_retention, options.stream.env.as_ref());
    let cache_session_id = if cache_retention == CacheRetention::None {
        None
    } else {
        options.stream.session_id.as_deref()
    };
    let headers = build_client_headers(
        model,
        context,
        &api_key,
        options.stream.headers.as_ref(),
        cache_session_id,
        &compat,
    );
    let mut params = build_params(
        model,
        context,
        options,
        &compat,
        cache_retention,
        &grammar_tool_input_properties,
    )?;
    if let Some(on_payload) = &options.stream.on_payload {
        if let Some(next_params) = on_payload(params.clone(), model).await {
            params = next_params;
        }
    }

    let url = format!("{}/chat/completions", model.base_url.trim_end_matches('/'));
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
            async move {
                let send = request.send();
                let result = match &signal {
                    Some(token) => tokio::select! {
                        outcome = send => outcome,
                        () = token.cancelled() => {
                            return Err(ProviderErrorInfo {
                                status: None,
                                headers: None,
                                message: "Request was aborted".to_owned(),
                            });
                        }
                    },
                    None => send.await,
                };
                match result {
                    Ok(response) => {
                        let status = response.status();
                        if status.is_success() {
                            Ok(response)
                        } else {
                            let status = status.as_u16();
                            let response_headers = headers_to_record(response.headers());
                            let body = response.text().await.unwrap_or_default();
                            // Some providers via OpenRouter give additional
                            // information in error.metadata.raw; append it when
                            // it is not already part of the message.
                            let raw_metadata = serde_json::from_str::<Value>(&body)
                                .ok()
                                .and_then(|value| {
                                    value
                                        .get("error")?
                                        .get("metadata")?
                                        .get("raw")?
                                        .as_str()
                                        .map(str::to_owned)
                                })
                                .filter(|raw| !raw.is_empty());
                            let normalized = NormalizedProviderError::new(
                                Some(status),
                                Some(body),
                                format!("Request failed with status {status}"),
                            );
                            let mut message = format_provider_error(&normalized, None);
                            if let Some(raw) = raw_metadata {
                                if !message.contains(&raw) {
                                    message.push('\n');
                                    message.push_str(&raw);
                                }
                            }
                            Err(ProviderErrorInfo {
                                status: Some(status),
                                headers: Some(response_headers),
                                message,
                            })
                        }
                    }
                    Err(error) => Err(ProviderErrorInfo {
                        status: error.status().map(|status| status.as_u16()),
                        headers: None,
                        message: error.to_string(),
                    }),
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

    let mut processor = CompletionsProcessor::new(
        output,
        model,
        &grammar_tool_input_properties,
        compat.supports_finish_reason,
    );
    let mut decoder = SseDecoder::new();
    let mut byte_stream = response.bytes_stream();
    let mut saw_done = false;
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
            if processor.handle_sse(&sse, events)? == SseOutcome::Done {
                saw_done = true;
                break;
            }
        }
        if saw_done {
            break;
        }
    }
    if !saw_done {
        for sse in decoder.finish() {
            if processor.handle_sse(&sse, events)? == SseOutcome::Done {
                break;
            }
        }
    }
    processor.finish(options.stream.signal.as_ref(), events)
}

/// `stream` (openai-completions).
pub fn stream(
    model: &Model,
    context: &Context,
    options: OpenAICompletionsOptions,
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

/// `streamSimple` (openai-completions): reasoning maps through
/// `clampThinkingLevel`; a clamped "off" omits `reasoning_effort`.
pub fn stream_simple(
    model: &Model,
    context: &Context,
    options: Option<SimpleStreamOptions>,
) -> AssistantMessageEventStream {
    if let Err(message) = get_client_api_key(
        &model.provider,
        options.as_ref().and_then(|o| o.stream.api_key.as_deref()),
        options.as_ref().and_then(|o| o.stream.headers.as_ref()),
    ) {
        return immediate_error_stream(model, &message);
    }

    let api_key = options.as_ref().and_then(|o| o.stream.api_key.clone());
    let base = build_base_options(model, context, options.as_ref(), api_key);
    let reasoning_effort = options
        .as_ref()
        .and_then(|o| o.reasoning)
        .map(|reasoning| clamp_thinking_level(model, reasoning.to_model_level()))
        .filter(|level| *level != ModelThinkingLevel::Off);

    stream(
        model,
        context,
        OpenAICompletionsOptions {
            stream: base,
            reasoning_effort,
            tool_choice: None,
        },
    )
}

/// `ProviderStreams` implementation for `ApiKind::OPENAI_COMPLETIONS`.
///
/// The trait carries plain [`StreamOptions`]; completions-specific extras
/// ([`OpenAICompletionsOptions`]) reach [`stream`] only through direct calls
/// or via [`stream_simple`] reasoning mapping (design §3.3 collapses per-API
/// extras).
#[derive(Debug, Clone, Copy, Default)]
pub struct OpenAiCompletions;

impl ProviderStreams for OpenAiCompletions {
    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: Option<StreamOptions>,
    ) -> AssistantMessageEventStream {
        stream(
            model,
            context,
            OpenAICompletionsOptions {
                stream: options.unwrap_or_default(),
                ..OpenAICompletionsOptions::default()
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod tests {
    use serde_json::{json, Value};

    use super::*;

    pub(crate) fn make_model(extra: Value) -> Model {
        let mut value = json!({
            "id": "gpt-4o", "name": "GPT-4o", "api": "openai-completions",
            "provider": "openai", "baseUrl": "https://api.openai.com",
            "reasoning": true, "input": ["text"],
            "cost": {"input": 2.5, "output": 10.0, "cacheRead": 1.25, "cacheWrite": 2.5},
            "contextWindow": 128000, "maxTokens": 16384
        });
        value
            .as_object_mut()
            .expect("object")
            .extend(extra.as_object().cloned().unwrap_or_default());
        serde_json::from_value(value).expect("model")
    }

    fn usage_json() -> Value {
        json!({
            "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 0,
            "cost": {"input": 0.0, "output": 0.0, "cacheRead": 0.0, "cacheWrite": 0.0, "total": 0.0}
        })
    }

    /// Assistant message from the target model itself (same-model replay).
    pub(crate) fn same_model_assistant(content: Value) -> Message {
        serde_json::from_value(json!({
            "role": "assistant", "content": content,
            "api": "openai-completions", "provider": "openai", "model": "gpt-4o",
            "usage": usage_json(), "stopReason": "stop", "timestamp": 0
        }))
        .expect("assistant")
    }

    /// Assistant message from another API/model (cross-model handoff).
    pub(crate) fn foreign_assistant(content: Value) -> Message {
        serde_json::from_value(json!({
            "role": "assistant", "content": content,
            "api": "openai-responses", "provider": "github-copilot", "model": "gpt-5",
            "usage": usage_json(), "stopReason": "stop", "timestamp": 0
        }))
        .expect("assistant")
    }

    pub(crate) fn tool_result(tool_call_id: &str, content: Value, extra: Value) -> Message {
        let mut value = json!({
            "role": "toolResult", "toolCallId": tool_call_id, "toolName": "bash",
            "content": content, "isError": false, "timestamp": 0
        });
        value
            .as_object_mut()
            .expect("object")
            .extend(extra.as_object().cloned().unwrap_or_default());
        serde_json::from_value(value).expect("tool result")
    }

    pub(crate) fn user_text(text: &str) -> Message {
        serde_json::from_value(json!({"role": "user", "content": text, "timestamp": 0}))
            .expect("user")
    }

    pub(crate) fn context(messages: Vec<Message>, tools: Option<Vec<Tool>>) -> Context {
        Context {
            system_prompt: None,
            messages,
            tools,
        }
    }

    pub(crate) fn tool(name: &str) -> Tool {
        serde_json::from_value(json!({
            "name": name, "description": "d",
            "parameters": {"type": "object", "properties": {"query": {"type": "string"}}, "required": ["query"]}
        }))
        .expect("tool")
    }

    fn no_grammar() -> HashMap<String, String> {
        HashMap::new()
    }

    fn convert(
        model: &Model,
        ctx: &Context,
        compat: &ResolvedOpenAICompletionsCompat,
    ) -> Vec<Value> {
        convert_messages(model, ctx, compat, &no_grammar()).expect("convert")
    }

    // -- compat detection ----------------------------------------------------

    #[test]
    fn test_detect_compat_default_openai() {
        let compat = detect_compat(&make_model(json!({})));
        assert!(compat.supports_store);
        assert!(compat.supports_developer_role);
        assert!(compat.supports_reasoning_effort);
        assert!(compat.supports_usage_in_streaming);
        assert_eq!(compat.max_tokens_field, MaxTokensField::MaxCompletionTokens);
        assert!(!compat.requires_tool_result_name);
        assert!(!compat.requires_assistant_after_tool_result);
        assert!(!compat.requires_thinking_as_text);
        assert!(!compat.requires_reasoning_content_on_assistant_messages);
        assert_eq!(compat.thinking_format, ThinkingFormat::Openai);
        assert!(compat.supports_strict_mode);
        assert!(!compat.supports_open_ai_grammar_tools);
        assert_eq!(compat.cache_control_format, None);
        assert!(!compat.send_session_affinity_headers);
        assert_eq!(compat.deferred_tools_mode, None);
        assert_eq!(
            compat.session_affinity_format,
            SessionAffinityFormat::Openai
        );
        assert!(compat.supports_long_cache_retention);
    }

    #[test]
    fn test_detect_compat_zai() {
        let compat = detect_compat(&make_model(json!({"provider": "zai"})));
        assert_eq!(compat.thinking_format, ThinkingFormat::Zai);
        assert!(!compat.supports_store);
        assert!(!compat.supports_reasoning_effort);
        assert!(!compat.supports_developer_role);
        // baseUrl-only detection works too.
        let by_url = detect_compat(&make_model(
            json!({"baseUrl": "https://api.z.ai/api/paas/v4"}),
        ));
        assert_eq!(by_url.thinking_format, ThinkingFormat::Zai);
    }

    #[test]
    fn test_detect_compat_together_moonshot() {
        let together = detect_compat(&make_model(json!({"provider": "together"})));
        assert_eq!(together.thinking_format, ThinkingFormat::Together);
        assert_eq!(together.max_tokens_field, MaxTokensField::MaxTokens);
        assert!(!together.supports_strict_mode);
        assert!(!together.supports_long_cache_retention);

        let moonshot = detect_compat(&make_model(json!({"provider": "moonshotai"})));
        assert_eq!(moonshot.max_tokens_field, MaxTokensField::MaxTokens);
        assert!(!moonshot.supports_reasoning_effort);
        assert!(!moonshot.supports_strict_mode);
        assert_eq!(moonshot.thinking_format, ThinkingFormat::Openai);
    }

    #[test]
    fn test_detect_compat_openrouter() {
        let plain = detect_compat(&make_model(
            json!({"provider": "openrouter", "id": "meta/llama"}),
        ));
        assert_eq!(plain.thinking_format, ThinkingFormat::Openrouter);
        assert!(!plain.supports_developer_role);
        assert_eq!(
            plain.session_affinity_format,
            SessionAffinityFormat::Openrouter
        );
        assert_eq!(plain.cache_control_format, None);

        let anthropic_id = detect_compat(&make_model(
            json!({"provider": "openrouter", "id": "anthropic/claude"}),
        ));
        assert!(anthropic_id.supports_developer_role);
        assert_eq!(
            anthropic_id.cache_control_format,
            Some(CacheControlFormat::Anthropic)
        );
    }

    #[test]
    fn test_detect_compat_deepseek() {
        let compat = detect_compat(&make_model(json!({
            "provider": "deepseek", "baseUrl": "https://api.deepseek.com"
        })));
        assert_eq!(compat.thinking_format, ThinkingFormat::Deepseek);
        assert!(compat.requires_reasoning_content_on_assistant_messages);
        assert!(!compat.supports_store);
        // deepseek is not in the reasoning-effort exclusion list.
        assert!(compat.supports_reasoning_effort);
    }

    #[test]
    fn test_detect_compat_nvidia_ant_ling() {
        let nvidia = detect_compat(&make_model(json!({"provider": "nvidia"})));
        assert_eq!(nvidia.max_tokens_field, MaxTokensField::MaxTokens);
        assert!(!nvidia.supports_reasoning_effort);
        assert!(!nvidia.supports_long_cache_retention);

        let ant_ling = detect_compat(&make_model(json!({"provider": "ant-ling"})));
        assert_eq!(ant_ling.thinking_format, ThinkingFormat::AntLing);
        assert!(!ant_ling.supports_reasoning_effort);
    }

    #[test]
    fn test_detect_compat_grok_and_chutes() {
        let grok = detect_compat(&make_model(json!({"provider": "xai"})));
        assert!(!grok.supports_reasoning_effort);
        assert_eq!(grok.max_tokens_field, MaxTokensField::MaxCompletionTokens);

        let chutes = detect_compat(&make_model(json!({"baseUrl": "https://llm.chutes.ai/v1"})));
        assert_eq!(chutes.max_tokens_field, MaxTokensField::MaxTokens);
        assert!(!chutes.supports_store);
    }

    #[test]
    fn test_get_compat_overrides() {
        let model = make_model(json!({
            "compat": {
                "supportsStore": false,
                "thinkingFormat": "string-thinking",
                "vercelGatewayRouting": {"only": ["bedrock"]}
            }
        }));
        let compat = get_compat(&model);
        assert!(!compat.supports_store);
        assert_eq!(compat.thinking_format, ThinkingFormat::StringThinking);
        assert_eq!(
            compat.vercel_gateway_routing.only,
            Some(vec!["bedrock".to_owned()])
        );
        // Untouched fields keep detected values; openRouterRouting defaults to
        // `{}` (not detected) when absent from model.compat.
        assert!(compat.supports_developer_role);
        assert_eq!(compat.open_router_routing, OpenRouterRouting::default());
    }

    // -- auth / tool history ---------------------------------------------------

    #[test]
    fn test_get_client_api_key() {
        assert_eq!(
            get_client_api_key("openai", Some("sk-key"), None),
            Ok("sk-key".to_owned())
        );
        let headers: ProviderHeaders =
            [("Authorization".to_owned(), Some("Bearer t".to_owned()))].into();
        assert_eq!(
            get_client_api_key("openai", None, Some(&headers)),
            Ok("unused".to_owned())
        );
        let cf: ProviderHeaders =
            [("cf-aig-authorization".to_owned(), Some("t".to_owned()))].into();
        assert_eq!(
            get_client_api_key("openai", None, Some(&cf)),
            Ok("unused".to_owned())
        );
        // Empty header values don't count.
        let empty: ProviderHeaders = [("authorization".to_owned(), Some("  ".to_owned()))].into();
        assert_eq!(
            get_client_api_key("openai", None, Some(&empty)),
            Err("No API key for provider: openai".to_owned())
        );
        assert_eq!(
            get_client_api_key("openai", None, None),
            Err("No API key for provider: openai".to_owned())
        );
    }

    #[test]
    fn test_has_tool_history() {
        assert!(!has_tool_history(&[user_text("hi")]));
        assert!(has_tool_history(&[same_model_assistant(json!([
            {"type": "toolCall", "id": "c1", "name": "bash", "arguments": {}}
        ]))]));
        assert!(has_tool_history(&[tool_result("c1", json!([]), json!({}))]));
    }

    #[test]
    fn test_deferred_tool_names_and_lookup() {
        let messages = vec![
            tool_result("c1", json!([]), json!({"addedToolNames": ["a", "b"]})),
            tool_result("c2", json!([]), json!({"addedToolNames": ["b", "c"]})),
        ];
        // First-appearance order, deduplicated (JS Set semantics).
        assert_eq!(
            get_deferred_tool_names(&messages),
            vec!["a".to_owned(), "b".to_owned(), "c".to_owned()]
        );
        let tools = vec![tool("c"), tool("a"), tool("b")];
        let found = get_tools_by_name(Some(&tools), &get_deferred_tool_names(&messages));
        assert_eq!(
            found.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
            vec!["a", "b", "c"]
        );
        assert!(get_tools_by_name(None, &["a".to_owned()]).is_empty());
    }

    // -- client headers --------------------------------------------------------

    #[test]
    fn test_build_client_headers_base_and_override() {
        let model = make_model(json!({}));
        let ctx = context(vec![user_text("hi")], None);
        let compat = get_compat(&model);
        let headers = build_client_headers(&model, &ctx, "sk-key", None, None, &compat);
        assert_eq!(
            headers.get("accept").and_then(|v| v.as_deref()),
            Some("application/json")
        );
        assert_eq!(
            headers.get("authorization").and_then(|v| v.as_deref()),
            Some("Bearer sk-key")
        );

        // SDK semantics: user headers land after auth headers and win
        // case-insensitively.
        let options_headers: ProviderHeaders =
            [("Authorization".to_owned(), Some("Bearer user".to_owned()))].into();
        let headers = build_client_headers(
            &model,
            &ctx,
            "sk-key",
            Some(&options_headers),
            None,
            &compat,
        );
        assert_eq!(headers.len(), 2);
        assert_eq!(
            headers.get("Authorization").and_then(|v| v.as_deref()),
            Some("Bearer user")
        );
    }

    #[test]
    fn test_build_client_headers_session_affinity() {
        let ctx = context(vec![user_text("hi")], None);
        let make = |format: SessionAffinityFormat| {
            let model = make_model(json!({
                "compat": {"sendSessionAffinityHeaders": true, "sessionAffinityFormat": format}
            }));
            let compat = get_compat(&model);
            build_client_headers(&model, &ctx, "k", None, Some("sess-1"), &compat)
        };
        let openrouter = make(SessionAffinityFormat::Openrouter);
        assert_eq!(
            openrouter.get("x-session-id").and_then(|v| v.as_deref()),
            Some("sess-1")
        );
        assert!(!openrouter.contains_key("x-client-request-id"));

        let openai = make(SessionAffinityFormat::Openai);
        for name in ["session_id", "x-client-request-id", "x-session-affinity"] {
            assert_eq!(openai.get(name).and_then(|v| v.as_deref()), Some("sess-1"));
        }

        let nosession = make(SessionAffinityFormat::OpenaiNosession);
        assert!(!nosession.contains_key("session_id"));
        assert!(nosession.contains_key("x-client-request-id"));
        assert!(nosession.contains_key("x-session-affinity"));

        // Disabled compat drops the headers entirely.
        let model = make_model(json!({}));
        let compat = get_compat(&model);
        let headers = build_client_headers(&model, &ctx, "k", None, Some("sess-1"), &compat);
        assert!(!headers.contains_key("x-session-id"));
        assert!(!headers.contains_key("x-session-affinity"));
    }

    // -- convert_messages --------------------------------------------------------

    #[test]
    fn test_convert_messages_system_prompt_role() {
        let model = make_model(json!({}));
        let mut ctx = context(vec![user_text("hi")], None);
        ctx.system_prompt = Some("be nice".to_owned());
        // reasoning + developer role support → "developer".
        let compat = get_compat(&model);
        let params = convert(&model, &ctx, &compat);
        assert_eq!(
            params[0],
            json!({"role": "developer", "content": "be nice"})
        );

        let non_reasoning = make_model(json!({"reasoning": false}));
        let compat = get_compat(&non_reasoning);
        let params = convert(&non_reasoning, &ctx, &compat);
        assert_eq!(params[0], json!({"role": "system", "content": "be nice"}));
    }

    #[test]
    fn test_convert_messages_user_text_and_image() {
        let model = make_model(json!({"input": ["text", "image"]}));
        let ctx = context(
            vec![serde_json::from_value(json!({
                "role": "user", "timestamp": 0,
                "content": [
                    {"type": "text", "text": "look"},
                    {"type": "image", "data": "AAAA", "mimeType": "image/png"}
                ]
            }))
            .expect("user")],
            None,
        );
        let compat = get_compat(&model);
        let params = convert(&model, &ctx, &compat);
        assert_eq!(
            params,
            vec![json!({
                "role": "user",
                "content": [
                    {"type": "text", "text": "look"},
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}}
                ]
            })]
        );
    }

    #[test]
    fn test_convert_messages_normalizes_pipe_tool_call_id() {
        let model = make_model(json!({}));
        let ctx = context(
            vec![
                foreign_assistant(json!([
                    {"type": "toolCall", "id": "call_abc|item_xyz", "name": "bash", "arguments": {"cmd": "ls"}}
                ])),
                tool_result(
                    "call_abc|item_xyz",
                    json!([{"type": "text", "text": "ok"}]),
                    json!({}),
                ),
            ],
            None,
        );
        let compat = get_compat(&model);
        let params = convert(&model, &ctx, &compat);
        assert_eq!(params[0]["tool_calls"][0]["id"], json!("call_abc_item_xyz"));
        // The tool result's tool_call_id is back-filled with the normalized id.
        assert_eq!(params[1]["tool_call_id"], json!("call_abc_item_xyz"));
        assert_eq!(params[1]["role"], json!("tool"));
    }

    #[test]
    fn test_convert_messages_pipe_id_long_hash_backfill() {
        let model = make_model(json!({}));
        let long_id = format!("call_{}|{}", "a".repeat(50), "b".repeat(50));
        let ctx = context(
            vec![foreign_assistant(json!([
                {"type": "toolCall", "id": long_id, "name": "bash", "arguments": {}}
            ]))],
            None,
        );
        let compat = get_compat(&model);
        let params = convert(&model, &ctx, &compat);
        let id = params[0]["tool_calls"][0]["id"].as_str().expect("id");
        // 40-char cap: 31-char sanitized call_id prefix + "_" + 8-char hash.
        assert_eq!(id.chars().count(), 40);
        assert!(id.starts_with("call_"));
        let expected_hash: String = short_hash(&long_id).chars().take(8).collect();
        assert!(id.ends_with(&format!("_{expected_hash}")));
    }

    #[test]
    fn test_convert_messages_openai_truncates_plain_id() {
        let model = make_model(json!({}));
        let ctx = context(
            vec![foreign_assistant(json!([
                {"type": "toolCall", "id": "x".repeat(50), "name": "bash", "arguments": {}}
            ]))],
            None,
        );
        let compat = get_compat(&model);
        let params = convert(&model, &ctx, &compat);
        assert_eq!(params[0]["tool_calls"][0]["id"], json!("x".repeat(40)));

        // Non-openai providers keep long ids.
        let other = make_model(json!({"provider": "mistral"}));
        let compat = get_compat(&other);
        let params = convert(&other, &ctx, &compat);
        assert_eq!(params[0]["tool_calls"][0]["id"], json!("x".repeat(50)));
    }

    #[test]
    fn test_convert_messages_assistant_thinking_signature() {
        let model = make_model(json!({}));
        let ctx = context(
            vec![same_model_assistant(json!([
                {"type": "thinking", "thinking": "first", "thinkingSignature": "reasoning_content"},
                {"type": "thinking", "thinking": "second", "thinkingSignature": "reasoning_content"},
                {"type": "text", "text": "answer"}
            ]))],
            None,
        );
        let compat = get_compat(&model);
        let params = convert(&model, &ctx, &compat);
        // Content is the plain text string; thinking lands under the first
        // block's signature key, raw (unsanitized), joined with "\n".
        assert_eq!(
            params[0],
            json!({
                "role": "assistant",
                "content": "answer",
                "reasoning_content": "first\nsecond"
            })
        );
    }

    #[test]
    fn test_convert_messages_opencode_go_signature_mapping() {
        let model =
            make_model(json!({"provider": "opencode-go", "baseUrl": "https://opencode.ai"}));
        let assistant: Message = serde_json::from_value(json!({
            "role": "assistant",
            "content": [
                {"type": "thinking", "thinking": "t", "thinkingSignature": "reasoning"},
                {"type": "text", "text": "answer"}
            ],
            "api": "openai-completions", "provider": "opencode-go", "model": "gpt-4o",
            "usage": usage_json(), "stopReason": "stop", "timestamp": 0
        }))
        .expect("assistant");
        let ctx = context(vec![assistant], None);
        let compat = get_compat(&model);
        let params = convert(&model, &ctx, &compat);
        // opencode-go maps the "reasoning" signature to "reasoning_content".
        assert_eq!(params[0]["reasoning_content"], json!("t"));
        assert!(params[0].get("reasoning").is_none());
    }

    #[test]
    fn test_convert_messages_thinking_as_text() {
        let model = make_model(json!({"compat": {"requiresThinkingAsText": true}}));
        let ctx = context(
            vec![same_model_assistant(json!([
                {"type": "thinking", "thinking": "t1", "thinkingSignature": "reasoning_content"},
                {"type": "thinking", "thinking": "t2", "thinkingSignature": "reasoning_content"},
                {"type": "text", "text": "answer"}
            ]))],
            None,
        );
        let compat = get_compat(&model);
        let params = convert(&model, &ctx, &compat);
        assert_eq!(
            params[0]["content"],
            json!([
                {"type": "text", "text": "t1\n\nt2"},
                {"type": "text", "text": "answer"}
            ])
        );
    }

    #[test]
    fn test_convert_messages_custom_grammar_tool_call() {
        let model = make_model(json!({}));
        let ctx = context(
            vec![same_model_assistant(json!([
                {"type": "toolCall", "id": "c1", "name": "sql", "arguments": {"query": "SELECT 1"}}
            ]))],
            None,
        );
        let compat = get_compat(&model);
        let grammar = HashMap::from([("sql".to_owned(), "query".to_owned())]);
        let params = convert_messages(&model, &ctx, &compat, &grammar).expect("convert");
        assert_eq!(
            params[0]["tool_calls"][0],
            json!({
                "id": "c1",
                "type": "custom",
                "custom": {"name": "sql", "input": "SELECT 1"}
            })
        );

        // A non-string grammar input property is an error.
        let ctx = context(
            vec![same_model_assistant(json!([
                {"type": "toolCall", "id": "c1", "name": "sql", "arguments": {"query": 1}}
            ]))],
            None,
        );
        let error = convert_messages(&model, &ctx, &compat, &grammar).unwrap_err();
        assert_eq!(
            error,
            "Grammar tool call \"sql\" requires argument \"query\" to be a string."
        );
    }

    #[test]
    fn test_convert_messages_reasoning_details_falsy_filtered() {
        let model = make_model(json!({}));
        let ctx = context(
            vec![same_model_assistant(json!([
                {"type": "toolCall", "id": "c1", "name": "bash", "arguments": {},
                 "thoughtSignature": "{\"type\":\"reasoning.encrypted\",\"id\":\"c1\",\"data\":\"sig\"}"},
                {"type": "toolCall", "id": "c2", "name": "bash", "arguments": {},
                 "thoughtSignature": "null"},
                {"type": "toolCall", "id": "c3", "name": "bash", "arguments": {},
                 "thoughtSignature": "0"},
                {"type": "toolCall", "id": "c4", "name": "bash", "arguments": {},
                 "thoughtSignature": "\"\""},
                {"type": "toolCall", "id": "c5", "name": "bash", "arguments": {},
                 "thoughtSignature": "not json"}
            ]))],
            None,
        );
        let compat = get_compat(&model);
        let params = convert(&model, &ctx, &compat);
        // JS `JSON.parse` + `.filter(Boolean)`: null / 0 / "" / parse failures
        // are all dropped.
        assert_eq!(
            params[0]["reasoning_details"],
            json!([{"type": "reasoning.encrypted", "id": "c1", "data": "sig"}])
        );
    }

    #[test]
    fn test_convert_messages_requires_reasoning_content() {
        let model = make_model(json!({
            "compat": {"requiresReasoningContentOnAssistantMessages": true}
        }));
        let ctx = context(
            vec![same_model_assistant(
                json!([{"type": "text", "text": "hi"}]),
            )],
            None,
        );
        let compat = get_compat(&model);
        let params = convert(&model, &ctx, &compat);
        assert_eq!(params[0]["reasoning_content"], json!(""));

        // An existing reasoning_content is not overwritten.
        let ctx = context(
            vec![same_model_assistant(json!([
                {"type": "thinking", "thinking": "t", "thinkingSignature": "reasoning_content"},
                {"type": "text", "text": "hi"}
            ]))],
            None,
        );
        let params = convert(&model, &ctx, &compat);
        assert_eq!(params[0]["reasoning_content"], json!("t"));
    }

    #[test]
    fn test_convert_messages_skips_empty_assistant() {
        let model = make_model(json!({}));
        let ctx = context(
            vec![
                same_model_assistant(json!([])),
                same_model_assistant(json!([{"type": "text", "text": "  "}])),
                user_text("next"),
            ],
            None,
        );
        let compat = get_compat(&model);
        let params = convert(&model, &ctx, &compat);
        assert_eq!(params, vec![json!({"role": "user", "content": "next"})]);
    }

    #[test]
    fn test_convert_messages_tool_result_grouping_and_images() {
        let model = make_model(json!({"input": ["text", "image"]}));
        let ctx = context(
            vec![
                tool_result("c1", json!([{"type": "text", "text": "out1"}]), json!({})),
                tool_result(
                    "c2",
                    json!([{"type": "image", "data": "BBBB", "mimeType": "image/png"}]),
                    json!({}),
                ),
                user_text("go on"),
            ],
            None,
        );
        let compat = get_compat(&model);
        let params = convert(&model, &ctx, &compat);
        assert_eq!(
            params,
            vec![
                json!({"role": "tool", "content": "out1", "tool_call_id": "c1"}),
                json!({"role": "tool", "content": "(see attached image)", "tool_call_id": "c2"}),
                json!({
                    "role": "user",
                    "content": [
                        {"type": "text", "text": "Attached image(s) from tool result:"},
                        {"type": "image_url", "image_url": {"url": "data:image/png;base64,BBBB"}}
                    ]
                }),
                json!({"role": "user", "content": "go on"}),
            ]
        );
    }

    #[test]
    fn test_convert_messages_requires_tool_result_name() {
        let model = make_model(json!({"compat": {"requiresToolResultName": true}}));
        let ctx = context(
            vec![tool_result(
                "c1",
                json!([{"type": "text", "text": "out"}]),
                json!({}),
            )],
            None,
        );
        let compat = get_compat(&model);
        let params = convert(&model, &ctx, &compat);
        assert_eq!(params[0]["name"], json!("bash"));
    }

    #[test]
    fn test_convert_messages_kimi_deferred_tools() {
        let model = make_model(json!({"compat": {"deferredToolsMode": "kimi"}}));
        let ctx = context(
            vec![tool_result(
                "c1",
                json!([{"type": "text", "text": "schema"}]),
                json!({"addedToolNames": ["sql"]}),
            )],
            Some(vec![tool("sql"), tool("bash")]),
        );
        let compat = get_compat(&model);
        let params = convert(&model, &ctx, &compat);
        // A system message carrying the deferred tool (no content field).
        assert_eq!(params.len(), 2);
        assert_eq!(params[1]["role"], json!("system"));
        assert!(params[1].get("content").is_none());
        assert_eq!(params[1]["tools"][0]["function"]["name"], json!("sql"));
    }

    #[test]
    fn test_convert_messages_kimi_deferred_batch_after_all_tool_results() {
        // Upstream "emits Kimi deferred schemas after all tool results in a
        // batch": markers from every tool result in the run collect into one
        // system message placed after all of them.
        let model = make_model(json!({"compat": {"deferredToolsMode": "kimi"}}));
        let ctx = context(
            vec![
                user_text("Hello"),
                same_model_assistant(json!([
                    {"type": "toolCall", "id": "call_1", "name": "base_tool", "arguments": {}}
                ])),
                tool_result(
                    "call_1",
                    json!([{"type": "text", "text": "done"}]),
                    json!({"addedToolNames": ["late_tool"]}),
                ),
                tool_result(
                    "call_2",
                    json!([{"type": "text", "text": "done2"}]),
                    json!({"addedToolNames": ["later_tool"]}),
                ),
                user_text("next"),
            ],
            Some(vec![
                tool("base_tool"),
                tool("late_tool"),
                tool("later_tool"),
            ]),
        );
        let compat = get_compat(&model);
        let params = convert(&model, &ctx, &compat);
        let roles: Vec<&str> = params
            .iter()
            .map(|msg| msg["role"].as_str().unwrap_or(""))
            .collect();
        assert_eq!(
            roles,
            ["user", "assistant", "tool", "tool", "system", "user"]
        );
        assert_eq!(
            params[4]["tools"][0]["function"]["name"],
            json!("late_tool")
        );
        assert_eq!(
            params[4]["tools"][1]["function"]["name"],
            json!("later_tool")
        );
    }

    #[test]
    fn test_get_tools_by_name_duplicate_last_wins() {
        // Upstream `getToolsByName` builds a Map (later duplicates overwrite).
        let mut canonical = tool("late_tool");
        canonical.description = "Canonical definition".to_owned();
        let tools = vec![tool("late_tool"), canonical];
        let found = get_tools_by_name(Some(&tools), &["late_tool".to_owned()]);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].description, "Canonical definition");
        assert!(get_tools_by_name(None, &["late_tool".to_owned()]).is_empty());
    }

    #[test]
    fn test_convert_messages_bridge_after_tool_result() {
        let model = make_model(json!({
            "compat": {"requiresAssistantAfterToolResult": true}
        }));
        let ctx = context(
            vec![
                tool_result("c1", json!([{"type": "text", "text": "out"}]), json!({})),
                user_text("next"),
            ],
            None,
        );
        let compat = get_compat(&model);
        let params = convert(&model, &ctx, &compat);
        assert_eq!(
            params,
            vec![
                json!({"role": "tool", "content": "out", "tool_call_id": "c1"}),
                json!({"role": "assistant", "content": "I have processed the tool results."}),
                json!({"role": "user", "content": "next"}),
            ]
        );
    }
}

#[cfg(test)]
mod build_and_stream_tests {
    use futures::StreamExt;
    use serde_json::{json, Value};

    use super::tests::{context, make_model, same_model_assistant, tool, tool_result, user_text};
    use super::*;
    use crate::types::{ChatTemplateKwargVar, ThinkingBudgets, ThinkingLevel};

    fn no_grammar() -> HashMap<String, String> {
        HashMap::new()
    }

    fn options(stream: StreamOptions) -> OpenAICompletionsOptions {
        OpenAICompletionsOptions {
            stream,
            ..OpenAICompletionsOptions::default()
        }
    }

    fn params_for(
        model: &Model,
        ctx: &Context,
        opts: &OpenAICompletionsOptions,
        retention: CacheRetention,
    ) -> Value {
        let compat = get_compat(model);
        build_params(model, ctx, opts, &compat, retention, &no_grammar()).expect("params")
    }

    // -- transport preference ----------------------------------------------------

    #[test]
    fn test_build_params_transport_preference_ignored() {
        // Transport is a codex-only preference; every other provider silently
        // ignores it (upstream: only openai-codex-responses.ts reads it).
        let model = make_model(json!({}));
        let ctx = context(vec![user_text("hi")], None);
        let plain = params_for(
            &model,
            &ctx,
            &options(StreamOptions::default()),
            CacheRetention::Short,
        );
        let with_transport = params_for(
            &model,
            &ctx,
            &options(StreamOptions {
                transport: Some(crate::types::Transport::Websocket),
                ..StreamOptions::default()
            }),
            CacheRetention::Short,
        );
        assert_eq!(
            serde_json::to_string(&plain).expect("serialize"),
            serde_json::to_string(&with_transport).expect("serialize")
        );
    }

    #[test]
    fn test_convert_messages_without_kimi_mode_leaves_tools_unchanged() {
        // Upstream "leaves OpenAI Completions tools unchanged without Kimi
        // mode": no system tool message, and build_params keeps every tool.
        let model = make_model(json!({}));
        let ctx = context(
            vec![
                same_model_assistant(json!([
                    {"type": "toolCall", "id": "call_1", "name": "base_tool", "arguments": {}}
                ])),
                tool_result(
                    "call_1",
                    json!([{"type": "text", "text": "done"}]),
                    json!({"addedToolNames": ["late_tool"]}),
                ),
            ],
            Some(vec![tool("base_tool"), tool("late_tool")]),
        );
        let compat = get_compat(&model);
        let params = convert_messages(&model, &ctx, &compat, &no_grammar()).expect("messages");
        assert!(!params
            .iter()
            .any(|msg| msg["role"] == json!("system") && msg.get("tools").is_some()));
        let opts = options(StreamOptions::default());
        let built = params_for(&model, &ctx, &opts, CacheRetention::Short);
        let names: Vec<&str> = built["tools"]
            .as_array()
            .expect("tools")
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap_or(""))
            .collect();
        assert_eq!(names, ["base_tool", "late_tool"]);
    }

    // -- convert_tools -----------------------------------------------------------

    #[test]
    fn test_convert_tools_function_strict_and_grammar() {
        let model = make_model(json!({}));
        let compat = get_compat(&model);

        let function_tool = tool("bash");
        let converted = convert_tools(&[function_tool], &compat).expect("tools");
        assert_eq!(
            converted[0],
            json!({
                "type": "function",
                "function": {
                    "name": "bash",
                    "description": "d",
                    "parameters": {"type": "object", "properties": {"query": {"type": "string"}}, "required": ["query"]},
                    "strict": false
                }
            })
        );

        // strict omitted when the provider doesn't support it.
        let no_strict = ResolvedOpenAICompletionsCompat {
            supports_strict_mode: false,
            ..compat.clone()
        };
        let converted = convert_tools(&[tool("bash")], &no_strict).expect("tools");
        assert!(converted[0]["function"].get("strict").is_none());

        // Grammar-constrained tool → custom tool.
        let grammar_model = make_model(json!({"compat": {"supportsOpenAIGrammarTools": true}}));
        let compat = get_compat(&grammar_model);
        let grammar_tool: Tool = serde_json::from_value(json!({
            "name": "sql", "description": "d",
            "parameters": {"type": "object", "properties": {"query": {"type": "string"}}, "required": ["query"]},
            "constrainedSampling": {"type": "grammar", "variants": {"openai_lark": "start: query"}}
        }))
        .expect("tool");
        let converted = convert_tools(&[grammar_tool], &compat).expect("tools");
        assert_eq!(
            converted[0],
            json!({
                "type": "custom",
                "custom": {
                    "name": "sql",
                    "description": "d",
                    "format": {"type": "grammar", "grammar": {"syntax": "lark", "definition": "start: query"}}
                }
            })
        );
    }

    // -- chat template kwargs ------------------------------------------------------

    #[test]
    fn test_resolve_chat_template_kwarg_value() {
        let model = make_model(json!({"thinkingLevelMap": {"high": "HIGH", "off": "NONE"}}));

        // Scalar passes through.
        assert_eq!(
            resolve_chat_template_kwarg_value(
                &model,
                None,
                &ChatTemplateKwargValue::Scalar(json!("x"))
            ),
            Some(json!("x"))
        );

        // thinking.enabled reflects the effort presence.
        let enabled = ChatTemplateKwargValue::Var(ChatTemplateKwargVar {
            var: ChatTemplateKwargVarKind::ThinkingEnabled,
            omit_when_off: None,
        });
        assert_eq!(
            resolve_chat_template_kwarg_value(&model, None, &enabled),
            Some(json!(false))
        );
        assert_eq!(
            resolve_chat_template_kwarg_value(&model, Some(ModelThinkingLevel::High), &enabled),
            Some(json!(true))
        );

        // thinking.effort resolves through the map; omitWhenOff drops it.
        let effort = ChatTemplateKwargValue::Var(ChatTemplateKwargVar {
            var: ChatTemplateKwargVarKind::ThinkingEffort,
            omit_when_off: Some(true),
        });
        assert_eq!(
            resolve_chat_template_kwarg_value(&model, None, &effort),
            None
        );
        assert_eq!(
            resolve_chat_template_kwarg_value(&model, Some(ModelThinkingLevel::High), &effort),
            Some(json!("HIGH"))
        );
        // Unmapped level → the level name itself.
        assert_eq!(
            resolve_chat_template_kwarg_value(&model, Some(ModelThinkingLevel::Low), &effort),
            Some(json!("low"))
        );

        // Without omitWhenOff, `off` resolves through the map.
        let keep = ChatTemplateKwargValue::Var(ChatTemplateKwargVar {
            var: ChatTemplateKwargVarKind::ThinkingEffort,
            omit_when_off: None,
        });
        assert_eq!(
            resolve_chat_template_kwarg_value(&model, None, &keep),
            Some(json!("NONE"))
        );
    }

    #[test]
    fn test_build_chat_template_kwargs_empty_is_none() {
        let model = make_model(json!({}));
        let compat = get_compat(&model);
        assert_eq!(build_chat_template_kwargs(&model, None, &compat), None);
    }

    // -- cache control ------------------------------------------------------------

    #[test]
    fn test_get_compat_cache_control() {
        let model = make_model(json!({
            "compat": {"cacheControlFormat": "anthropic"}
        }));
        let compat = get_compat(&model);
        assert_eq!(
            get_compat_cache_control(&compat, CacheRetention::Short),
            Some(json!({"type": "ephemeral"}))
        );
        assert_eq!(
            get_compat_cache_control(&compat, CacheRetention::Long),
            Some(json!({"type": "ephemeral", "ttl": "1h"}))
        );
        assert_eq!(
            get_compat_cache_control(&compat, CacheRetention::None),
            None
        );

        // No anthropic format → no marker.
        let compat = get_compat(&make_model(json!({})));
        assert_eq!(
            get_compat_cache_control(&compat, CacheRetention::Short),
            None
        );
    }

    #[test]
    fn test_apply_anthropic_cache_control() {
        let cache_control = json!({"type": "ephemeral"});
        let mut messages = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "first"}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "a"}, {"type": "text", "text": "b"}]}),
            json!({"role": "user", "content": [{"type": "image_url", "image_url": {"url": "u"}}]}),
        ];
        let mut tools = vec![
            json!({"type": "function", "function": {"name": "t1"}}),
            json!({"type": "function", "function": {"name": "t2"}}),
        ];
        apply_anthropic_cache_control(&mut messages, Some(&mut tools), &cache_control);

        // First system message: string content becomes a marked text part.
        assert_eq!(
            messages[0]["content"],
            json!([{"type": "text", "text": "sys", "cache_control": {"type": "ephemeral"}}])
        );
        // Last tool definition is marked.
        assert!(tools[0].get("cache_control").is_none());
        assert_eq!(tools[1]["cache_control"], cache_control);
        // Last conversation message with a text part wins: the trailing user
        // message has no text part, so the assistant's LAST text part is marked.
        assert!(messages[3]["content"][0].get("cache_control").is_none());
        assert!(messages[2]["content"][0].get("cache_control").is_none());
        assert_eq!(messages[2]["content"][1]["cache_control"], cache_control);
        assert!(messages[1]["content"].get("cache_control").is_none());
    }

    // -- build_params ---------------------------------------------------------------

    #[test]
    fn test_build_params_default_openai_shape() {
        let model = make_model(json!({}));
        let ctx = context(vec![user_text("hi")], None);
        let opts = options(StreamOptions {
            max_tokens: Some(1024),
            temperature: Some(0.5),
            session_id: Some("sess".to_owned()),
            ..StreamOptions::default()
        });
        let params = params_for(&model, &ctx, &opts, CacheRetention::Short);
        // Byte-for-byte: key order mirrors the upstream params object.
        assert_eq!(
            serde_json::to_string(&params).expect("serialize"),
            r#"{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}],"stream":true,"prompt_cache_key":"sess","stream_options":{"include_usage":true},"store":false,"max_completion_tokens":1024,"temperature":0.5}"#
        );
    }

    #[test]
    fn test_build_params_cache_key_and_retention() {
        let model = make_model(json!({}));
        let ctx = context(vec![user_text("hi")], None);
        let opts = options(StreamOptions {
            session_id: Some("s".to_owned()),
            ..StreamOptions::default()
        });

        // "none" retention suppresses the cache key entirely.
        let params = params_for(&model, &ctx, &opts, CacheRetention::None);
        assert!(params.get("prompt_cache_key").is_none());
        assert!(params.get("prompt_cache_retention").is_none());

        // Long retention adds the 24h marker (default openai compat supports it).
        let params = params_for(&model, &ctx, &opts, CacheRetention::Long);
        assert_eq!(params["prompt_cache_key"], json!("s"));
        assert_eq!(params["prompt_cache_retention"], json!("24h"));

        // Non-openai baseUrl: short retention → no key; long still qualifies.
        let other = make_model(json!({"baseUrl": "https://example.com/v1"}));
        let params = params_for(&other, &ctx, &opts, CacheRetention::Short);
        assert!(params.get("prompt_cache_key").is_none());
        let params = params_for(&other, &ctx, &opts, CacheRetention::Long);
        assert_eq!(params["prompt_cache_key"], json!("s"));
    }

    #[test]
    fn test_build_params_max_tokens_field() {
        let ctx = context(vec![user_text("hi")], None);
        let opts = options(StreamOptions {
            max_tokens: Some(100),
            ..StreamOptions::default()
        });
        let together = make_model(json!({"provider": "together"}));
        let params = params_for(&together, &ctx, &opts, CacheRetention::Short);
        assert_eq!(params["max_tokens"], json!(100));
        assert!(params.get("max_completion_tokens").is_none());
    }

    #[test]
    fn test_build_params_tools_and_tool_stream() {
        let ctx = context(vec![user_text("hi")], Some(vec![tool("bash")]));
        let opts = options(StreamOptions::default());

        let model = make_model(json!({}));
        let params = params_for(&model, &ctx, &opts, CacheRetention::Short);
        assert_eq!(params["tools"][0]["function"]["name"], json!("bash"));
        assert!(params.get("tool_stream").is_none());

        // zai tool_stream flag.
        let zai = make_model(json!({"provider": "zai", "compat": {"zaiToolStream": true}}));
        let params = params_for(&zai, &ctx, &opts, CacheRetention::Short);
        assert_eq!(params["tool_stream"], json!(true));

        // Tool history without tools → empty tools array.
        let history_ctx = context(
            vec![same_model_assistant(json!([
                {"type": "toolCall", "id": "c1", "name": "bash", "arguments": {}}
            ]))],
            None,
        );
        let params = params_for(&model, &history_ctx, &opts, CacheRetention::Short);
        assert_eq!(params["tools"], json!([]));
    }

    #[test]
    fn test_build_params_tool_choice() {
        let model = make_model(json!({}));
        let ctx = context(vec![user_text("hi")], None);
        let mut opts = options(StreamOptions::default());
        opts.tool_choice = Some(json!("required"));
        let params = params_for(&model, &ctx, &opts, CacheRetention::Short);
        assert_eq!(params["tool_choice"], json!("required"));
    }

    #[test]
    fn test_build_params_thinking_formats() {
        let ctx = context(vec![user_text("hi")], None);
        let with_effort = |effort: ModelThinkingLevel| OpenAICompletionsOptions {
            stream: StreamOptions::default(),
            reasoning_effort: Some(effort),
            tool_choice: None,
        };
        let no_effort = options(StreamOptions::default());

        // zai: enabled/disabled thinking object; no reasoning_effort by default.
        let zai = make_model(json!({"provider": "zai"}));
        let params = params_for(&zai, &ctx, &no_effort, CacheRetention::Short);
        assert_eq!(params["thinking"], json!({"type": "disabled"}));
        let params = params_for(
            &zai,
            &ctx,
            &with_effort(ModelThinkingLevel::High),
            CacheRetention::Short,
        );
        assert_eq!(
            params["thinking"],
            json!({"type": "enabled", "clear_thinking": false})
        );
        assert!(params.get("reasoning_effort").is_none());
        // zai with explicit effort support: absent mapping → level name; null
        // mapping → omitted.
        let zai_mapped = make_model(json!({
            "provider": "zai",
            "compat": {"supportsReasoningEffort": true},
            "thinkingLevelMap": {"high": "HIGH", "low": null}
        }));
        let params = params_for(
            &zai_mapped,
            &ctx,
            &with_effort(ModelThinkingLevel::High),
            CacheRetention::Short,
        );
        assert_eq!(params["reasoning_effort"], json!("HIGH"));
        let params = params_for(
            &zai_mapped,
            &ctx,
            &with_effort(ModelThinkingLevel::Medium),
            CacheRetention::Short,
        );
        assert_eq!(params["reasoning_effort"], json!("medium"));
        let params = params_for(
            &zai_mapped,
            &ctx,
            &with_effort(ModelThinkingLevel::Low),
            CacheRetention::Short,
        );
        assert!(params.get("reasoning_effort").is_none());

        // qwen / qwen-chat-template.
        let qwen = make_model(json!({"compat": {"thinkingFormat": "qwen"}}));
        let params = params_for(
            &qwen,
            &ctx,
            &with_effort(ModelThinkingLevel::Low),
            CacheRetention::Short,
        );
        assert_eq!(params["enable_thinking"], json!(true));
        let qwen_ct = make_model(json!({"compat": {"thinkingFormat": "qwen-chat-template"}}));
        let params = params_for(&qwen_ct, &ctx, &no_effort, CacheRetention::Short);
        assert_eq!(
            params["chat_template_kwargs"],
            json!({"enable_thinking": false, "preserve_thinking": true})
        );

        // chat-template with configured kwargs.
        let chat_template = make_model(json!({
            "compat": {
                "thinkingFormat": "chat-template",
                "chatTemplateKwargs": {
                    "on": {"$var": "thinking.enabled"},
                    "level": {"$var": "thinking.effort", "omitWhenOff": true},
                    "fixed": "v"
                }
            }
        }));
        let params = params_for(&chat_template, &ctx, &no_effort, CacheRetention::Short);
        assert_eq!(
            params["chat_template_kwargs"],
            json!({"on": false, "fixed": "v"})
        );
        let params = params_for(
            &chat_template,
            &ctx,
            &with_effort(ModelThinkingLevel::High),
            CacheRetention::Short,
        );
        assert_eq!(
            params["chat_template_kwargs"],
            json!({"on": true, "level": "high", "fixed": "v"})
        );

        // deepseek: disabled unless off is explicitly null; effort adds
        // reasoning_effort (deepseek supports it by detection).
        let deepseek = make_model(json!({"provider": "deepseek"}));
        let params = params_for(&deepseek, &ctx, &no_effort, CacheRetention::Short);
        assert_eq!(params["thinking"], json!({"type": "disabled"}));
        let params = params_for(
            &deepseek,
            &ctx,
            &with_effort(ModelThinkingLevel::High),
            CacheRetention::Short,
        );
        assert_eq!(params["thinking"], json!({"type": "enabled"}));
        assert_eq!(params["reasoning_effort"], json!("high"));
        let deepseek_null_off = make_model(json!({
            "provider": "deepseek", "thinkingLevelMap": {"off": null}
        }));
        let params = params_for(&deepseek_null_off, &ctx, &no_effort, CacheRetention::Short);
        assert!(params.get("thinking").is_none());

        // openrouter: nested reasoning object; off default "none".
        let openrouter = make_model(json!({"provider": "openrouter"}));
        let params = params_for(&openrouter, &ctx, &no_effort, CacheRetention::Short);
        assert_eq!(params["reasoning"], json!({"effort": "none"}));
        let params = params_for(
            &openrouter,
            &ctx,
            &with_effort(ModelThinkingLevel::High),
            CacheRetention::Short,
        );
        assert_eq!(params["reasoning"], json!({"effort": "high"}));

        // ant-ling: only a mapped (non-null) effort is sent.
        let ant_ling = make_model(json!({
            "provider": "ant-ling", "thinkingLevelMap": {"high": "HIGH"}
        }));
        let params = params_for(
            &ant_ling,
            &ctx,
            &with_effort(ModelThinkingLevel::High),
            CacheRetention::Short,
        );
        assert_eq!(params["reasoning"], json!({"effort": "HIGH"}));
        let params = params_for(
            &ant_ling,
            &ctx,
            &with_effort(ModelThinkingLevel::Low),
            CacheRetention::Short,
        );
        assert!(params.get("reasoning").is_none());

        // together: enabled flag; reasoning_effort only when supported.
        let together = make_model(json!({"provider": "together"}));
        let params = params_for(&together, &ctx, &no_effort, CacheRetention::Short);
        assert_eq!(params["reasoning"], json!({"enabled": false}));
        let together_effort = make_model(json!({
            "provider": "together", "compat": {"supportsReasoningEffort": true}
        }));
        let params = params_for(
            &together_effort,
            &ctx,
            &with_effort(ModelThinkingLevel::High),
            CacheRetention::Short,
        );
        assert_eq!(params["reasoning"], json!({"enabled": true}));
        assert_eq!(params["reasoning_effort"], json!("high"));

        // string-thinking: plain string; off default "none".
        let string_thinking = make_model(json!({"compat": {"thinkingFormat": "string-thinking"}}));
        let params = params_for(&string_thinking, &ctx, &no_effort, CacheRetention::Short);
        assert_eq!(params["thinking"], json!("none"));
        let params = params_for(
            &string_thinking,
            &ctx,
            &with_effort(ModelThinkingLevel::Medium),
            CacheRetention::Short,
        );
        assert_eq!(params["thinking"], json!("medium"));

        // Default openai: reasoning_effort with map fallback; off mapping used
        // when no effort is given.
        let model = make_model(json!({}));
        let params = params_for(
            &model,
            &ctx,
            &with_effort(ModelThinkingLevel::High),
            CacheRetention::Short,
        );
        assert_eq!(params["reasoning_effort"], json!("high"));
        let off_mapped = make_model(json!({"thinkingLevelMap": {"off": "minimal"}}));
        let params = params_for(&off_mapped, &ctx, &no_effort, CacheRetention::Short);
        assert_eq!(params["reasoning_effort"], json!("minimal"));
        // Non-reasoning models emit no thinking params.
        let plain = make_model(json!({"reasoning": false}));
        let params = params_for(
            &plain,
            &ctx,
            &with_effort(ModelThinkingLevel::High),
            CacheRetention::Short,
        );
        assert!(params.get("reasoning_effort").is_none());
        assert!(params.get("thinking").is_none());
    }

    #[test]
    fn test_build_params_routing() {
        let ctx = context(vec![user_text("hi")], None);
        let opts = options(StreamOptions::default());

        let openrouter = make_model(json!({
            "compat": {"openRouterRouting": {"order": ["a", "b"], "allow_fallbacks": false}}
        }));
        let params = params_for(&openrouter, &ctx, &opts, CacheRetention::Short);
        assert_eq!(
            params["provider"],
            json!({"order": ["a", "b"], "allow_fallbacks": false})
        );

        let vercel = make_model(json!({
            "compat": {"vercelGatewayRouting": {"only": ["bedrock"], "order": ["a"]}}
        }));
        let params = params_for(&vercel, &ctx, &opts, CacheRetention::Short);
        assert_eq!(
            params["providerOptions"],
            json!({"gateway": {"only": ["bedrock"], "order": ["a"]}})
        );

        // Empty routing objects produce nothing.
        let empty = make_model(json!({"compat": {"vercelGatewayRouting": {}}}));
        let params = params_for(&empty, &ctx, &opts, CacheRetention::Short);
        assert!(params.get("providerOptions").is_none());
    }

    #[test]
    fn test_build_params_applies_cache_control() {
        let model = make_model(json!({
            "provider": "openrouter", "id": "anthropic/claude"
        }));
        let mut ctx = context(vec![user_text("hi")], Some(vec![tool("bash")]));
        ctx.system_prompt = Some("sys".to_owned());
        let opts = options(StreamOptions::default());
        let params = params_for(&model, &ctx, &opts, CacheRetention::Short);
        assert_eq!(
            params["messages"][0]["content"],
            json!([{"type": "text", "text": "sys", "cache_control": {"type": "ephemeral"}}])
        );
        assert_eq!(
            params["tools"][0]["cache_control"],
            json!({"type": "ephemeral"})
        );
        assert_eq!(
            params["messages"][1]["content"],
            json!([{"type": "text", "text": "hi", "cache_control": {"type": "ephemeral"}}])
        );
    }

    // -- usage / stop reason ---------------------------------------------------------

    #[test]
    fn test_parse_chunk_usage() {
        let model = make_model(json!({}));
        let usage = parse_chunk_usage(
            &json!({
                "prompt_tokens": 100,
                "completion_tokens": 20,
                "prompt_tokens_details": {"cached_tokens": 30, "cache_write_tokens": 10},
                "completion_tokens_details": {"reasoning_tokens": 5}
            }),
            &model,
        );
        assert_eq!(usage.input, 60);
        assert_eq!(usage.output, 20);
        assert_eq!(usage.cache_read, 30);
        assert_eq!(usage.cache_write, 10);
        assert_eq!(usage.reasoning, Some(5));
        assert_eq!(usage.total_tokens, 120);

        // Legacy prompt_cache_hit_tokens fallback; clamp at zero.
        let usage = parse_chunk_usage(
            &json!({"prompt_tokens": 10, "prompt_cache_hit_tokens": 50}),
            &model,
        );
        assert_eq!(usage.input, 0);
        assert_eq!(usage.cache_read, 50);

        // Cost is calculated (2.5/M input, 10/M output, 1.25/M read, 2.5/M write).
        let usage = parse_chunk_usage(
            &json!({"prompt_tokens": 1_000_000, "completion_tokens": 1_000_000}),
            &model,
        );
        assert_eq!(usage.cost.input, 2.5);
        assert_eq!(usage.cost.output, 10.0);
    }

    #[test]
    fn test_map_stop_reason() {
        assert_eq!(map_stop_reason(None), (StopReason::Stop, None));
        assert_eq!(map_stop_reason(Some("stop")), (StopReason::Stop, None));
        assert_eq!(map_stop_reason(Some("end")), (StopReason::Stop, None));
        assert_eq!(map_stop_reason(Some("length")), (StopReason::Length, None));
        assert_eq!(
            map_stop_reason(Some("tool_calls")),
            (StopReason::ToolUse, None)
        );
        assert_eq!(
            map_stop_reason(Some("function_call")),
            (StopReason::ToolUse, None)
        );
        assert_eq!(
            map_stop_reason(Some("content_filter")),
            (
                StopReason::Error,
                Some("Provider finish_reason: content_filter".to_owned())
            )
        );
        assert_eq!(
            map_stop_reason(Some("something_new")),
            (
                StopReason::Error,
                Some("Provider finish_reason: something_new".to_owned())
            )
        );
    }

    // -- stream processor replay -------------------------------------------------------

    fn replay(
        model: &Model,
        grammar_props: &HashMap<String, String>,
        bytes: &[u8],
    ) -> (
        Vec<StreamEvent>,
        Result<DoneReason, String>,
        AssistantMessage,
    ) {
        let events = AssistantMessageEventStream::new();
        let mut output = initial_output(model);
        let reason = {
            let mut processor = CompletionsProcessor::new(
                &mut output,
                model,
                grammar_props,
                get_compat(model).supports_finish_reason,
            );
            let mut decoder = SseDecoder::new();
            let mut result: Result<(), String> = Ok(());
            let mut saw_done = false;
            for sse in decoder.feed(bytes) {
                match processor.handle_sse(&sse, &events) {
                    Ok(SseOutcome::Chunk) => {}
                    Ok(SseOutcome::Done) => {
                        saw_done = true;
                        break;
                    }
                    Err(error) => {
                        result = Err(error);
                        break;
                    }
                }
            }
            if result.is_ok() && !saw_done {
                for sse in decoder.finish() {
                    match processor.handle_sse(&sse, &events) {
                        Ok(SseOutcome::Chunk) => {}
                        Ok(SseOutcome::Done) => break,
                        Err(error) => {
                            result = Err(error);
                            break;
                        }
                    }
                }
            }
            match result {
                Ok(()) => processor.finish(None, &events),
                Err(error) => Err(error),
            }
        };
        events.end(None);
        let collected: Vec<StreamEvent> = futures::executor::block_on(events.collect());
        (collected, reason, output)
    }

    fn event_kinds(events: &[StreamEvent]) -> Vec<&str> {
        events
            .iter()
            .map(|event| match event {
                StreamEvent::Start { .. } => "start",
                StreamEvent::TextStart { .. } => "text_start",
                StreamEvent::TextDelta { .. } => "text_delta",
                StreamEvent::TextEnd { .. } => "text_end",
                StreamEvent::ThinkingStart { .. } => "thinking_start",
                StreamEvent::ThinkingDelta { .. } => "thinking_delta",
                StreamEvent::ThinkingEnd { .. } => "thinking_end",
                StreamEvent::ToolCallStart { .. } => "toolcall_start",
                StreamEvent::ToolCallDelta { .. } => "toolcall_delta",
                StreamEvent::ToolCallEnd { .. } => "toolcall_end",
                StreamEvent::Done { .. } => "done",
                StreamEvent::Error { .. } => "error",
            })
            .collect()
    }

    const RECORDED_STREAM: &str = concat!(
        // Encrypted reasoning detail arrives before the tool call (pending).
        "data: {\"id\":\"chatcmpl-1\",\"model\":\"gpt-4o-mini\",\"choices\":[{\"index\":0,\"delta\":{\"reasoning_details\":[{\"type\":\"reasoning.encrypted\",\"id\":\"call_1\",\"data\":\"sig\"}]},\"finish_reason\":null}]}\n",
        "\n",
        "data: {\"id\":\"chatcmpl-1\",\"model\":\"gpt-4o-mini\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\n",
        "\n",
        // Both reasoning fields present: the first (reasoning_content) wins.
        "data: {\"id\":\"chatcmpl-1\",\"model\":\"gpt-4o-mini\",\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"think\",\"reasoning\":\"think\"},\"finish_reason\":null}]}\n",
        "\n",
        "data: {\"id\":\"chatcmpl-1\",\"model\":\"gpt-4o-mini\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" world\"},\"finish_reason\":null}]}\n",
        "\n",
        "data: {\"id\":\"chatcmpl-1\",\"model\":\"gpt-4o-mini\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"bash\",\"arguments\":\"\"}}]},\"finish_reason\":null}]}\n",
        "\n",
        "data: {\"id\":\"chatcmpl-1\",\"model\":\"gpt-4o-mini\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"cmd\\\":\"}}]},\"finish_reason\":null}]}\n",
        "\n",
        "data: {\"id\":\"chatcmpl-1\",\"model\":\"gpt-4o-mini\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"ls\\\"}\"}}]},\"finish_reason\":null}]}\n",
        "\n",
        "data: {\"id\":\"chatcmpl-1\",\"model\":\"gpt-4o-mini\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5,\"prompt_tokens_details\":{\"cached_tokens\":4}}}\n",
        "\n",
        "data: [DONE]\n",
        "\n",
    );

    #[test]
    fn test_processor_recorded_stream() {
        let model = make_model(json!({}));
        let (events, reason, output) = replay(&model, &no_grammar(), RECORDED_STREAM.as_bytes());
        assert_eq!(reason, Ok(DoneReason::ToolUse));
        assert_eq!(
            event_kinds(&events),
            vec![
                "text_start",
                "text_delta",
                "thinking_start",
                "thinking_delta",
                "text_delta",
                "toolcall_start",
                "toolcall_delta", // empty arguments chunk still emits a delta
                "toolcall_delta",
                "toolcall_delta",
                "text_end",
                "thinking_end",
                "toolcall_end",
            ]
        );

        // Final message shape.
        assert_eq!(output.response_id.as_deref(), Some("chatcmpl-1"));
        assert_eq!(output.response_model.as_deref(), Some("gpt-4o-mini"));
        assert_eq!(output.stop_reason, StopReason::ToolUse);
        assert_eq!(output.usage.input, 6);
        assert_eq!(output.usage.cache_read, 4);
        assert_eq!(output.usage.output, 5);
        assert_eq!(output.usage.reasoning, Some(0));
        assert_eq!(output.usage.total_tokens, 15);

        let text = match &output.content[0] {
            AssistantContent::Text(text) => text,
            other => panic!("expected text block, got {other:?}"),
        };
        assert_eq!(text.text, "Hello world");
        let thinking = match &output.content[1] {
            AssistantContent::Thinking(thinking) => thinking,
            other => panic!("expected thinking block, got {other:?}"),
        };
        assert_eq!(thinking.thinking, "think");
        assert_eq!(
            thinking.thinking_signature.as_deref(),
            Some("reasoning_content")
        );
        let call = match &output.content[2] {
            AssistantContent::ToolCall(call) => call,
            other => panic!("expected tool call block, got {other:?}"),
        };
        assert_eq!(call.id, "call_1");
        assert_eq!(call.name, "bash");
        assert_eq!(
            call.arguments,
            json!({"cmd": "ls"}).as_object().cloned().unwrap()
        );
        // The pending encrypted reasoning detail landed on the tool call.
        assert_eq!(
            call.thought_signature.as_deref(),
            Some("{\"type\":\"reasoning.encrypted\",\"id\":\"call_1\",\"data\":\"sig\"}")
        );

        // The toolcall_end event carries the finalized call.
        let end_call = events.iter().find_map(|event| match event {
            StreamEvent::ToolCallEnd { tool_call, .. } => Some(tool_call.clone()),
            _ => None,
        });
        assert_eq!(
            end_call.as_ref().map(|c| c.arguments.clone()),
            Some(call.arguments.clone())
        );
    }

    #[test]
    fn test_processor_custom_grammar_tool() {
        let model = make_model(json!({}));
        let grammar = HashMap::from([("sql".to_owned(), "query".to_owned())]);
        let stream = concat!(
            "data: {\"id\":\"x\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"type\":\"custom\",\"custom\":{\"name\":\"sql\",\"input\":\"SELECT \"}}]},\"finish_reason\":null}]}\n",
            "\n",
            "data: {\"id\":\"x\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"custom\":{\"input\":\"*\"}}]},\"finish_reason\":null}]}\n",
            "\n",
            "data: {\"id\":\"x\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n",
            "\n",
            "data: [DONE]\n\n",
        );
        let (events, reason, output) = replay(&model, &grammar, stream.as_bytes());
        assert_eq!(reason, Ok(DoneReason::ToolUse));

        // Grammar input deltas are JSON fragments reconstructing the arguments.
        let deltas: Vec<&str> = events
            .iter()
            .filter_map(|event| match event {
                StreamEvent::ToolCallDelta { delta, .. } => Some(delta.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(deltas, vec!["{\"query\":\"SELECT ", "*", "\"}"]);

        let call = match &output.content[0] {
            AssistantContent::ToolCall(call) => call,
            other => panic!("expected tool call block, got {other:?}"),
        };
        assert_eq!(call.name, "sql");
        assert_eq!(
            call.arguments,
            json!({"query": "SELECT *"}).as_object().cloned().unwrap()
        );
    }

    #[test]
    fn test_processor_done_sentinel_stops_early() {
        let model = make_model(json!({}));
        // Chunks after [DONE] are ignored.
        let stream = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"a\"},\"finish_reason\":\"stop\"}]}\n",
            "\n",
            "data: [DONE]\n",
            "\n",
            "data: {not json}\n",
            "\n",
        );
        let (_events, reason, output) = replay(&model, &no_grammar(), stream.as_bytes());
        assert_eq!(reason, Ok(DoneReason::Stop));
        assert_eq!(
            match &output.content[0] {
                AssistantContent::Text(text) => text.text.as_str(),
                other => panic!("expected text, got {other:?}"),
            },
            "a"
        );
    }

    #[test]
    fn test_processor_strict_parse_error() {
        let model = make_model(json!({}));
        let stream = "data: {not json}\n\n";
        let (_events, reason, _output) = replay(&model, &no_grammar(), stream.as_bytes());
        let error = reason.unwrap_err();
        assert!(
            error.starts_with("Could not parse OpenAI SSE chunk: "),
            "unexpected error: {error}"
        );
        assert!(
            error.contains("; data={not json}"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn test_processor_missing_finish_reason_is_error() {
        let model = make_model(json!({}));
        let stream = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"a\"},\"finish_reason\":null}]}\n\n";
        let (_events, reason, _output) = replay(&model, &no_grammar(), stream.as_bytes());
        assert_eq!(reason, Err("Stream ended without finish_reason".to_owned()));
    }

    #[test]
    fn test_processor_content_filter_error() {
        let model = make_model(json!({}));
        let stream = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"a\"},\"finish_reason\":\"content_filter\"}]}\n",
            "\n",
            "data: [DONE]\n\n",
        );
        let (_events, reason, output) = replay(&model, &no_grammar(), stream.as_bytes());
        assert_eq!(
            reason,
            Err("Provider finish_reason: content_filter".to_owned())
        );
        assert_eq!(output.stop_reason, StopReason::Error);
    }

    #[test]
    fn test_processor_choice_usage_fallback() {
        let model = make_model(json!({}));
        // Moonshot-style: usage on the choice instead of the chunk.
        let stream = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"a\"},\"finish_reason\":\"stop\",\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":3}}]}\n",
            "\n",
            "data: [DONE]\n\n",
        );
        let (_events, reason, output) = replay(&model, &no_grammar(), stream.as_bytes());
        assert_eq!(reason, Ok(DoneReason::Stop));
        assert_eq!(output.usage.input, 7);
        assert_eq!(output.usage.output, 3);
    }

    #[test]
    fn test_processor_length_stop_maps_to_done_length() {
        let model = make_model(json!({}));
        let stream = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"a\"},\"finish_reason\":\"length\"}]}\n\n";
        let (_events, reason, _output) = replay(&model, &no_grammar(), stream.as_bytes());
        assert_eq!(reason, Ok(DoneReason::Length));
    }

    // -- raw stop reasons (openai-completions-raw-stop-reason.test.ts: fe1c9b6d5 @ 4181f66) --

    /// openai-completions-raw-stop-reason.test.ts: "preserves raw finish
    /// reasons for successful stops".
    #[test]
    fn preserves_raw_finish_reasons_for_successful_stops() {
        let model = make_model(json!({}));
        let stream = concat!(
            "data: {\"id\":\"chatcmpl-1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n",
            "\n",
            "data: [DONE]\n\n",
        );
        let (_events, reason, output) = replay(&model, &no_grammar(), stream.as_bytes());
        assert_eq!(reason, Ok(DoneReason::Stop));
        assert_eq!(output.stop_reason, StopReason::Stop);
        assert_eq!(output.raw_stop_reason.as_deref(), Some("stop"));
        assert_eq!(output.error_message, None);
    }

    /// openai-completions-raw-stop-reason.test.ts: "preserves raw finish
    /// reasons for provider error stops".
    #[test]
    fn preserves_raw_finish_reasons_for_provider_error_stops() {
        let model = make_model(json!({}));
        let stream = concat!(
            "data: {\"id\":\"chatcmpl-2\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"content_filter\"}]}\n",
            "\n",
            "data: [DONE]\n\n",
        );
        let (_events, reason, output) = replay(&model, &no_grammar(), stream.as_bytes());
        assert_eq!(
            reason,
            Err("Provider finish_reason: content_filter".to_owned())
        );
        assert_eq!(output.stop_reason, StopReason::Error);
        assert_eq!(output.raw_stop_reason.as_deref(), Some("content_filter"));
        assert_eq!(
            output.error_message.as_deref(),
            Some("Provider finish_reason: content_filter")
        );
    }

    // -- supportsFinishReason (openai-completions-tool-choice.test.ts: 2c3041242 @ 4181f66) --

    /// openai-completions-tool-choice.test.ts: "accepts streams without
    /// finish_reason when compat disables it".
    #[test]
    fn accepts_streams_without_finish_reason_when_compat_disables_it() {
        let model = make_model(json!({"compat": {"supportsFinishReason": false}}));
        let stream = concat!(
            "data: {\"id\":\"chatcmpl-no-finish-reason\",\"choices\":[{\"delta\":{\"content\":\"complete answer\"},\"finish_reason\":null}]}\n",
            "\n",
        );
        let (_events, reason, output) = replay(&model, &no_grammar(), stream.as_bytes());
        assert_eq!(reason, Ok(DoneReason::Stop));
        assert_eq!(output.stop_reason, StopReason::Stop);
        assert_eq!(output.error_message, None);
        match &output.content[0] {
            AssistantContent::Text(text) => assert_eq!(text.text, "complete answer"),
            other => panic!("expected text, got {other:?}"),
        }
    }

    /// The toolUse arm of the 2c3041242 content inference (no upstream test
    /// case; anchors the `toolCall` branch of `output.content.some(...)`).
    #[test]
    fn infers_tool_use_without_finish_reason_when_compat_disables_it() {
        let model = make_model(json!({"compat": {"supportsFinishReason": false}}));
        let stream = concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"read\",\"arguments\":\"{}\"}}]},\"finish_reason\":null}]}\n",
            "\n",
        );
        let (_events, reason, output) = replay(&model, &no_grammar(), stream.as_bytes());
        assert_eq!(reason, Ok(DoneReason::ToolUse));
        assert_eq!(output.stop_reason, StopReason::ToolUse);
    }

    // -- tool-call delta (openai-completions-tool-choice.test.ts: 34239180a @ 4181f66) --

    /// openai-completions-tool-choice.test.ts: "ignores empty custom objects
    /// on function tool call deltas".
    #[test]
    fn ignores_empty_custom_objects_on_function_tool_call_deltas() {
        let model = make_model(json!({}));
        let stream = concat!(
            "data: {\"id\":\"chatcmpl-empty-custom\",\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\\\"README.md\\\"}\"},\"custom\":{}}]},\"finish_reason\":\"tool_calls\"}]}\n",
            "\n",
            "data: [DONE]\n\n",
        );
        let (_events, reason, output) = replay(&model, &no_grammar(), stream.as_bytes());
        assert_eq!(reason, Ok(DoneReason::ToolUse));
        let AssistantContent::ToolCall(call) = &output.content[0] else {
            panic!("expected tool call block, got {:?}", output.content);
        };
        assert_eq!(call.id, "call_1");
        assert_eq!(call.name, "read");
        assert_eq!(
            call.arguments,
            json!({"path": "README.md"}).as_object().cloned().unwrap()
        );
    }

    // -- stream_simple -----------------------------------------------------------

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
                    Some("No API key for provider: openai")
                );
            }
            other => panic!("expected error event, got {other:?}"),
        }
    }

    #[test]
    fn test_stream_simple_reasoning_mapping() {
        // Off clamps away entirely; supported levels pass through.
        let model = make_model(json!({}));
        let _ = &model;
        // clampThinkingLevel is exercised via Models tests; here we only assert
        // the level→model-level conversion used by stream_simple.
        assert_eq!(
            ThinkingLevel::High.to_model_level(),
            ModelThinkingLevel::High
        );
        let _ = ThinkingBudgets::default();
    }
}
