//! Port of `packages/ai/src/api/openai-responses.ts` @ pi 0.82.1 (2efa728);
//! error tail carries `output.errorMessage` since 4181f66 (32850ef7c).
//!
//! OpenAI Responses adapter: compat resolution (`get_compat`), session-affinity
//! client headers, `build_params` (prompt-cache keys/retention, the 16-token
//! minimum on `max_output_tokens`, reasoning effort/summary, service tier),
//! SSE event processing through the shared [`ResponsesStreamProcessor`], and
//! `stream_simple` reasoning mapping.
//!
//! Intentional differences (upstream deviations):
//! - HTTP is a direct reqwest call, not the `openai` SDK; the SDK's
//!   `x-stainless-*` telemetry headers, default `User-Agent` and platform
//!   headers are not sent, and there is no SDK default timeout (callers set
//!   `StreamOptions::timeout_ms`).
//! - SSE events are parsed with strict `serde_json` (the SDK uses
//!   `JSON.parse`, not a repairing parser); parse failures read
//!   `Could not parse OpenAI Responses SSE event: {error}; data={data}` (the
//!   SDK would surface a `SyntaxError` message instead).
//! - The legacy env var is renamed `PI_CACHE_RETENTION` →
//!   `RPI_CACHE_RETENTION` (requirements §5.5); the resolution helper is
//!   shared with the anthropic-messages adapter
//!   ([`crate::api::anthropic_messages::resolve_cache_retention`]).
//! - HTTP error status/body are extracted from the response at the call site
//!   (upstream reads them off the SDK error object in the catch block). Only
//!   HTTP failures carry a status, so the `OpenAI API error` prefix applies to
//!   exactly the same errors as upstream's
//!   `formatProviderError(normalizeProviderError(error), "OpenAI API error")`.
//! - Upstream scrubs `partialJson`/`customInput` scratch off content blocks in
//!   its catch block; rpi keeps streaming scratch in the processor (never in
//!   content blocks), so there is nothing to scrub.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use futures::StreamExt;
use serde_json::{json, Value};

use crate::api::anthropic_messages::resolve_cache_retention;
use crate::api::constrained_sampling::create_grammar_tool_input_properties;
use crate::api::copilot_headers::{build_copilot_dynamic_headers, has_copilot_vision_input};
use crate::api::lazy::immediate_error_stream;
use crate::api::openai_completions::{
    get_client_api_key, mapped_or_level_name, off_is_not_null, off_value,
};
use crate::api::openai_prompt_cache::clamp_openai_prompt_cache_key;
use crate::api::openai_responses_shared::{
    convert_responses_messages, convert_responses_tools, ConvertResponsesMessagesOptions,
    ConvertResponsesToolsOptions, ResponsesDeferredToolsMode, ResponsesStreamOptions,
    ResponsesStreamProcessor,
};
use crate::api::simple_options::build_base_options;
use crate::api::sse::SseDecoder;
use crate::models::{clamp_thinking_level, ProviderStreams};
use crate::types::{
    AssistantMessage, CacheRetention, Context, DoneReason, ErrorReason, Model, ModelThinkingLevel,
    ProviderHeaders, ProviderResponse, SessionAffinityFormat, SimpleStreamOptions, StopReason,
    StreamEvent, StreamOptions, Usage,
};
use crate::utils::custom_fetch::send_provider_request;
use crate::utils::deferred_tools::split_deferred_tools_identity;
use crate::utils::error_body::{format_provider_error, NormalizedProviderError};
use crate::utils::event_stream::AssistantMessageEventStream;
use crate::utils::headers::{
    headers_to_record, merge_headers_chain, model_headers, provider_headers_to_header_map,
};
use crate::utils::provider_retry::{
    retry_provider_request, ProviderErrorInfo, ProviderRetryOptions,
};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// `OPENAI_TOOL_CALL_PROVIDERS`: providers whose tool-call ids use the
/// Responses `{call_id}|{item_id}` composite form.
pub static OPENAI_TOOL_CALL_PROVIDERS: LazyLock<HashSet<&'static str>> =
    LazyLock::new(|| HashSet::from(["openai", "openai-codex", "opencode"]));

/// `OPENAI_RESPONSES_MIN_OUTPUT_TOKENS` — OpenAI Responses rejects
/// `max_output_tokens` below 16 (<https://github.com/earendil-works/pi/issues/6265>).
pub const OPENAI_RESPONSES_MIN_OUTPUT_TOKENS: u32 = 16;

// ---------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------

/// `OpenAIResponsesOptions` — `StreamOptions` plus responses-specific extras.
/// `reasoning_effort` keys directly into `Model::thinking_level_map`;
/// `reasoning_summary` is `"auto" | "detailed" | "concise"`; `tool_choice` is
/// the raw OpenAI `tool_choice` JSON value.
#[derive(Debug, Clone, Default)]
pub struct OpenAIResponsesOptions {
    pub stream: StreamOptions,
    pub reasoning_effort: Option<ModelThinkingLevel>,
    pub reasoning_summary: Option<String>,
    pub service_tier: Option<String>,
    pub tool_choice: Option<Value>,
}

// ---------------------------------------------------------------------------
// Compat
// ---------------------------------------------------------------------------

/// `detectSessionAffinityFormat`: openrouter (provider id or base URL) vs
/// openai.
pub fn detect_session_affinity_format(model: &Model) -> SessionAffinityFormat {
    if model.provider == "openrouter" || model.base_url.contains("openrouter.ai") {
        SessionAffinityFormat::Openrouter
    } else {
        SessionAffinityFormat::Openai
    }
}

/// `Required<OpenAIResponsesCompat>` — every flag resolved to a concrete
/// value. Defaults differ from chat completions: strict mode and the grammar
/// / tool-search / explicit-prompt-cache features default to **false**.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedOpenAIResponsesCompat {
    pub supports_developer_role: bool,
    pub session_affinity_format: SessionAffinityFormat,
    pub supports_long_cache_retention: bool,
    pub supports_strict_mode: bool,
    pub supports_open_ai_grammar_tools: bool,
    /// e47b8e37a (#7709): message-anchored `additional_tools` input items.
    pub supports_additional_tools: bool,
    pub supports_tool_search: bool,
    pub supports_explicit_prompt_cache_mode: bool,
}

/// `getCompat` (openai-responses).
pub fn get_compat(model: &Model) -> ResolvedOpenAIResponsesCompat {
    let compat = model.compat.as_ref();
    ResolvedOpenAIResponsesCompat {
        supports_developer_role: compat
            .and_then(|compat| compat.supports_developer_role)
            .unwrap_or(true),
        session_affinity_format: compat
            .and_then(|compat| compat.session_affinity_format)
            .unwrap_or_else(|| detect_session_affinity_format(model)),
        supports_long_cache_retention: compat
            .and_then(|compat| compat.supports_long_cache_retention)
            .unwrap_or(true),
        supports_strict_mode: compat
            .and_then(|compat| compat.supports_strict_mode)
            .unwrap_or(false),
        supports_open_ai_grammar_tools: compat
            .and_then(|compat| compat.supports_open_ai_grammar_tools)
            .unwrap_or(false),
        supports_additional_tools: compat
            .and_then(|compat| compat.supports_additional_tools)
            .unwrap_or(false),
        supports_tool_search: compat
            .and_then(|compat| compat.supports_tool_search)
            .unwrap_or(false),
        supports_explicit_prompt_cache_mode: compat
            .and_then(|compat| compat.supports_explicit_prompt_cache_mode)
            .unwrap_or(false),
    }
}

/// `getPromptCacheRetention`: long retention opts into the 24h window when
/// the provider supports it.
fn get_prompt_cache_retention(
    compat: &ResolvedOpenAIResponsesCompat,
    cache_retention: CacheRetention,
) -> Option<&'static str> {
    (cache_retention == CacheRetention::Long && compat.supports_long_cache_retention)
        .then_some("24h")
}

// ---------------------------------------------------------------------------
// Client headers
// ---------------------------------------------------------------------------

/// `createClient`'s header assembly (openai-responses variant). Unlike chat
/// completions there is no `sendSessionAffinityHeaders` gate and no
/// `x-session-affinity` header: a session id is always sent when present —
/// openrouter gets `x-session-id`; every other format gets
/// `x-client-request-id`, plus `session_id` for the openai format.
pub fn build_client_headers(
    model: &Model,
    context: &Context,
    api_key: &str,
    options_headers: Option<&ProviderHeaders>,
    session_id: Option<&str>,
    compat: &ResolvedOpenAIResponsesCompat,
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

    let session_headers: Option<ProviderHeaders> = session_id.map(|session_id| {
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
            }
            SessionAffinityFormat::OpenaiNosession => {
                headers.insert(
                    "x-client-request-id".to_owned(),
                    Some(session_id.to_owned()),
                );
            }
        }
        headers
    });

    merge_headers_chain(&[
        Some(base),
        model_headers(model),
        copilot_headers,
        session_headers,
        options_headers.cloned(),
    ])
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
    options: &OpenAIResponsesOptions,
    compat: &ResolvedOpenAIResponsesCompat,
    grammar_tool_input_properties: &HashMap<String, String>,
) -> Result<Value, String> {
    // e47b8e37a (#7709): additional_tools-capable models (GPT-5.6 family)
    // prefer message-anchored input items over client tool search.
    let deferred_tools_mode = if compat.supports_additional_tools {
        Some(ResponsesDeferredToolsMode::AdditionalTools)
    } else if compat.supports_tool_search {
        Some(ResponsesDeferredToolsMode::ToolSearch)
    } else {
        None
    };
    let tool_placement = split_deferred_tools_identity(context, deferred_tools_mode.is_some());
    let tool_options = ConvertResponsesToolsOptions {
        supports_strict_mode: Some(compat.supports_strict_mode),
        supports_open_ai_grammar_tools: Some(compat.supports_open_ai_grammar_tools),
        ..ConvertResponsesToolsOptions::default()
    };
    let messages = convert_responses_messages(
        model,
        context,
        &OPENAI_TOOL_CALL_PROVIDERS,
        &ConvertResponsesMessagesOptions {
            grammar_tool_input_properties: Some(grammar_tool_input_properties),
            deferred_tools: Some(&tool_placement.deferred),
            deferred_tools_mode,
            tool_options: tool_options.clone(),
            ..ConvertResponsesMessagesOptions::default()
        },
    )?;

    let cache_retention =
        resolve_cache_retention(options.stream.cache_retention, options.stream.env.as_ref());
    let disable_implicit_prompt_cache =
        cache_retention == CacheRetention::None && compat.supports_explicit_prompt_cache_mode;

    let mut params = json!({
        "model": model.id,
        "input": messages,
        "stream": true,
    });
    if cache_retention != CacheRetention::None {
        if let Some(key) = clamp_openai_prompt_cache_key(options.stream.session_id.as_deref()) {
            params["prompt_cache_key"] = json!(key);
        }
    }
    if let Some(retention) = get_prompt_cache_retention(compat, cache_retention) {
        params["prompt_cache_retention"] = json!(retention);
    }
    if disable_implicit_prompt_cache {
        params["prompt_cache_options"] = json!({ "mode": "explicit" });
    }
    params["store"] = json!(false);

    if let Some(max_tokens) = options.stream.max_tokens {
        params["max_output_tokens"] = json!(max_tokens.max(OPENAI_RESPONSES_MIN_OUTPUT_TOKENS));
    }
    if let Some(temperature) = options.stream.temperature {
        params["temperature"] = json!(temperature);
    }
    if let Some(service_tier) = &options.service_tier {
        params["service_tier"] = json!(service_tier);
    }
    if !tool_placement.immediate.is_empty() {
        params["tools"] = json!(convert_responses_tools(
            &tool_placement.immediate,
            &tool_options,
        )?);
    }
    if let Some(tool_choice) = &options.tool_choice {
        params["tool_choice"] = tool_choice.clone();
    }

    if model.reasoning {
        if options.reasoning_effort.is_some() || options.reasoning_summary.is_some() {
            let effort = options
                .reasoning_effort
                .map(|level| mapped_or_level_name(model, level))
                .unwrap_or_else(|| "medium".to_owned());
            // JS `options?.reasoningSummary || "auto"`: empty string counts
            // as unset.
            let summary = options
                .reasoning_summary
                .as_deref()
                .filter(|summary| !summary.is_empty())
                .unwrap_or("auto");
            params["reasoning"] = json!({ "effort": effort, "summary": summary });
            params["include"] = json!(["reasoning.encrypted_content"]);
        } else if model.provider != "github-copilot" && off_is_not_null(model) {
            // `model.thinkingLevelMap?.off ?? "none"`; the `!== null` guard
            // above means `off` is absent (→ "none") or a mapped string.
            let effort = off_value(model)
                .flatten()
                .unwrap_or_else(|| "none".to_owned());
            params["reasoning"] = json!({ "effort": effort });
        }
        if model.provider == "xai" {
            params["include"] = json!(["reasoning.encrypted_content"]);
        }
    }

    // 25a2c8dcf (#7568): merged last so custom keys override the named
    // request fields.
    if let Some(sampling_params) = &options.stream.sampling_params {
        for (key, value) in sampling_params {
            params[key.clone()] = value.clone();
        }
    }

    Ok(params)
}

// ---------------------------------------------------------------------------
// Service-tier pricing
// ---------------------------------------------------------------------------

/// `getServiceTierCostMultiplier`.
pub fn get_service_tier_cost_multiplier(model_id: &str, service_tier: Option<&str>) -> f64 {
    match service_tier {
        Some("flex") => 0.5,
        Some("priority") => {
            if model_id == "gpt-5.5" {
                2.5
            } else {
                2.0
            }
        }
        _ => 1.0,
    }
}

/// `applyServiceTierPricing`: scales all four cost components and recomputes
/// the total.
pub fn apply_service_tier_pricing(usage: &mut Usage, service_tier: Option<&str>, model_id: &str) {
    let multiplier = get_service_tier_cost_multiplier(model_id, service_tier);
    if multiplier == 1.0 {
        return;
    }
    usage.cost.input *= multiplier;
    usage.cost.output *= multiplier;
    usage.cost.cache_read *= multiplier;
    usage.cost.cache_write *= multiplier;
    usage.cost.total =
        usage.cost.input + usage.cost.output + usage.cost.cache_read + usage.cost.cache_write;
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
    options: &OpenAIResponsesOptions,
    output: &mut AssistantMessage,
    events: &AssistantMessageEventStream,
) -> Result<DoneReason, String> {
    let api_key = get_client_api_key(
        &model.provider,
        options.stream.api_key.as_deref(),
        options.stream.headers.as_ref(),
    )?;
    let cache_retention =
        resolve_cache_retention(options.stream.cache_retention, options.stream.env.as_ref());
    let cache_session_id = if cache_retention == CacheRetention::None {
        None
    } else {
        options.stream.session_id.as_deref()
    };
    let compat = get_compat(model);
    let grammar_tool_input_properties = create_grammar_tool_input_properties(
        context.tools.as_deref(),
        compat.supports_open_ai_grammar_tools,
    )?;
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
        &grammar_tool_input_properties,
    )?;
    if let Some(on_payload) = &options.stream.on_payload {
        if let Some(next_params) = on_payload(params.clone(), model).await {
            params = next_params;
        }
    }

    let url = format!("{}/responses", model.base_url.trim_end_matches('/'));
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
                                message: format_provider_error(
                                    &normalized,
                                    Some("OpenAI API error"),
                                ),
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

    let model_id = model.id.clone();
    let mut processor = ResponsesStreamProcessor::new(
        output,
        model,
        ResponsesStreamOptions {
            service_tier: options.service_tier.as_deref(),
            grammar_tool_input_properties: &grammar_tool_input_properties,
            apply_service_tier_pricing: Some(Box::new(move |usage: &mut Usage, tier| {
                apply_service_tier_pricing(usage, tier, &model_id);
            })),
            resolve_service_tier: None,
        },
    );
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
            // No `[DONE]` sentinel on the Responses API: a terminal
            // `response.completed`/`response.failed` event ends the stream.
            let event: Value = serde_json::from_str(&sse.data).map_err(|error| {
                format!(
                    "Could not parse OpenAI Responses SSE event: {error}; data={}",
                    sse.data
                )
            })?;
            processor.handle_event(&event, events)?;
        }
    }
    for sse in decoder.finish() {
        let event: Value = serde_json::from_str(&sse.data).map_err(|error| {
            format!(
                "Could not parse OpenAI Responses SSE event: {error}; data={}",
                sse.data
            )
        })?;
        processor.handle_event(&event, events)?;
    }
    processor.finish()?;

    if options
        .stream
        .signal
        .as_ref()
        .is_some_and(|signal| signal.is_cancelled())
    {
        return Err("Request was aborted".to_owned());
    }
    match output.stop_reason {
        StopReason::Pending => {
            Err("OpenAI Responses stream ended without a stop reason".to_owned())
        }
        // 32850ef7c (R2.3.3): carry the mapped provider error message.
        StopReason::Aborted | StopReason::Error => Err(output
            .error_message
            .clone()
            .unwrap_or_else(|| "An unknown error occurred".to_owned())),
        StopReason::Length => Ok(DoneReason::Length),
        StopReason::ToolUse => Ok(DoneReason::ToolUse),
        _ => Ok(DoneReason::Stop),
    }
}

/// `stream` (openai-responses).
pub fn stream(
    model: &Model,
    context: &Context,
    options: OpenAIResponsesOptions,
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

/// `streamSimple` (openai-responses): reasoning maps through
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
        OpenAIResponsesOptions {
            stream: base,
            reasoning_effort,
            ..OpenAIResponsesOptions::default()
        },
    )
}

/// `ProviderStreams` implementation for `ApiKind::OPENAI_RESPONSES`.
///
/// The trait carries plain [`StreamOptions`]; responses-specific extras
/// ([`OpenAIResponsesOptions`]) reach [`stream`] only through direct calls
/// or via [`stream_simple`] reasoning mapping (design §3.3 collapses per-API
/// extras).
#[derive(Debug, Clone, Copy, Default)]
pub struct OpenAiResponses;

impl ProviderStreams for OpenAiResponses {
    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: Option<StreamOptions>,
    ) -> AssistantMessageEventStream {
        stream(
            model,
            context,
            OpenAIResponsesOptions {
                stream: options.unwrap_or_default(),
                ..OpenAIResponsesOptions::default()
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
mod tests {
    use serde_json::{json, Value};

    use super::*;
    use crate::api::openai_completions::tests as common;
    use crate::types::UsageCost;

    fn model(extra: Value) -> Model {
        let mut overrides = json!({"api": "openai-responses"});
        overrides
            .as_object_mut()
            .expect("object")
            .extend(extra.as_object().cloned().unwrap_or_default());
        common::make_model(overrides)
    }

    fn no_grammar() -> HashMap<String, String> {
        HashMap::new()
    }

    fn params_for(model: &Model, ctx: &Context, options: &OpenAIResponsesOptions) -> Value {
        build_params(model, ctx, options, &get_compat(model), &no_grammar()).expect("params")
    }

    fn param_keys(params: &Value) -> Vec<String> {
        params
            .as_object()
            .expect("object")
            .keys()
            .cloned()
            .collect()
    }

    // -- compat ---------------------------------------------------------------

    #[test]
    fn test_detect_session_affinity_format() {
        assert_eq!(
            detect_session_affinity_format(&model(json!({"provider": "openrouter"}))),
            SessionAffinityFormat::Openrouter
        );
        assert_eq!(
            detect_session_affinity_format(&model(
                json!({"provider": "custom", "baseUrl": "https://openrouter.ai/api/v1"})
            )),
            SessionAffinityFormat::Openrouter
        );
        assert_eq!(
            detect_session_affinity_format(&model(json!({}))),
            SessionAffinityFormat::Openai
        );
    }

    #[test]
    fn test_get_compat_defaults() {
        let compat = get_compat(&model(json!({})));
        assert!(compat.supports_developer_role);
        assert_eq!(
            compat.session_affinity_format,
            SessionAffinityFormat::Openai
        );
        assert!(compat.supports_long_cache_retention);
        // Responses defaults differ from chat completions: all feature flags
        // default to false.
        assert!(!compat.supports_strict_mode);
        assert!(!compat.supports_open_ai_grammar_tools);
        assert!(!compat.supports_tool_search);
        assert!(!compat.supports_explicit_prompt_cache_mode);
    }

    #[test]
    fn test_get_compat_explicit_overrides() {
        let compat = get_compat(&model(json!({
            "compat": {
                "supportsDeveloperRole": false,
                "sessionAffinityFormat": "openai-nosession",
                "supportsLongCacheRetention": false,
                "supportsStrictMode": true,
                "supportsOpenAIGrammarTools": true,
                "supportsToolSearch": true,
                "supportsExplicitPromptCacheMode": true
            }
        })));
        assert!(!compat.supports_developer_role);
        assert_eq!(
            compat.session_affinity_format,
            SessionAffinityFormat::OpenaiNosession
        );
        assert!(!compat.supports_long_cache_retention);
        assert!(compat.supports_strict_mode);
        assert!(compat.supports_open_ai_grammar_tools);
        assert!(compat.supports_tool_search);
        assert!(compat.supports_explicit_prompt_cache_mode);
    }

    // -- sampling params (25a2c8dcf @ 4181f66, #7568) -------------------------

    /// Upstream sampling-options.test.ts, openai-responses leg: sampling
    /// params merge into the request body last, overriding named fields.
    #[test]
    fn test_build_params_sampling_params_merged_last() {
        let m = model(json!({}));
        let ctx = common::context(vec![common::user_text("hi")], None);
        let options = OpenAIResponsesOptions {
            stream: StreamOptions {
                temperature: Some(0.0),
                sampling_params: Some(
                    [
                        ("top_p".to_owned(), json!(0.95)),
                        ("top_k".to_owned(), json!(0)),
                        ("min_p".to_owned(), json!(0)),
                        ("temperature".to_owned(), json!(1.0)),
                    ]
                    .into_iter()
                    .collect(),
                ),
                ..StreamOptions::default()
            },
            ..OpenAIResponsesOptions::default()
        };
        let params = params_for(&m, &ctx, &options);
        assert_eq!(params["top_p"], json!(0.95));
        assert_eq!(params["top_k"], json!(0));
        assert_eq!(params["min_p"], json!(0));
        assert_eq!(params["temperature"], json!(1.0));

        let plain = params_for(&m, &ctx, &OpenAIResponsesOptions::default());
        assert!(plain.get("top_p").is_none());
        assert!(plain.get("temperature").is_none());
    }

    // -- deferred tools mode (e47b8e37a @ 4181f66, #7709) ----------------------

    /// One base_tool call whose result marks late_tool as transcript-loaded;
    /// both tools are in `Context.tools`.
    fn deferred_tools_context() -> Context {
        common::context(
            vec![
                common::same_model_assistant(json!([
                    {"type": "toolCall", "id": "call_1|fc_1", "name": "base_tool", "arguments": {}}
                ])),
                common::tool_result(
                    "call_1|fc_1",
                    json!([{"type": "text", "text": "ok"}]),
                    json!({"addedToolNames": ["late_tool"]}),
                ),
            ],
            Some(vec![common::tool("base_tool"), common::tool("late_tool")]),
        )
    }

    fn top_level_tool_names(params: &Value) -> Vec<&str> {
        params["tools"]
            .as_array()
            .expect("tools")
            .iter()
            .filter_map(|tool| tool["name"].as_str())
            .collect()
    }

    /// Upstream deferred-tools.test.ts: "loads an OpenAI Responses tool
    /// through additional_tools" + "falls back to client tool search when
    /// additional_tools is unsupported" + "leaves providers without deferred
    /// loading unchanged" (adapter-level mode selection).
    #[test]
    fn test_build_params_deferred_tools_mode_selection() {
        let ctx = deferred_tools_context();

        // supportsAdditionalTools: the deferred tool rides a message-anchored
        // additional_tools item (no defer_loading, no tool-search pair).
        let m = model(json!({"compat": {"supportsAdditionalTools": true}}));
        let params = params_for(&m, &ctx, &OpenAIResponsesOptions::default());
        assert_eq!(top_level_tool_names(&params), vec!["base_tool"]);
        let input = params["input"].as_array().expect("input");
        let additional = input
            .iter()
            .find(|item| item["type"] == json!("additional_tools"))
            .expect("additional_tools");
        assert_eq!(additional["role"], json!("developer"));
        assert_eq!(additional["tools"][0]["type"], json!("function"));
        assert_eq!(additional["tools"][0]["name"], json!("late_tool"));
        assert!(additional["tools"][0].get("defer_loading").is_none());
        assert!(input
            .iter()
            .all(|item| item["type"] != json!("tool_search_call")));
        assert!(input
            .iter()
            .all(|item| item["type"] != json!("tool_search_output")));

        // additional_tools unsupported + tool search supported: client
        // tool-search fallback.
        let m = model(
            json!({"compat": {"supportsAdditionalTools": false, "supportsToolSearch": true}}),
        );
        let params = params_for(&m, &ctx, &OpenAIResponsesOptions::default());
        assert_eq!(top_level_tool_names(&params), vec!["base_tool"]);
        let input = params["input"].as_array().expect("input");
        let search_call = input
            .iter()
            .find(|item| item["type"] == json!("tool_search_call"))
            .expect("tool_search_call");
        let search_output = input
            .iter()
            .find(|item| item["type"] == json!("tool_search_output"))
            .expect("tool_search_output");
        assert_eq!(search_call["execution"], json!("client"));
        assert_eq!(search_call["status"], json!("completed"));
        assert_eq!(search_output["call_id"], search_call["call_id"]);
        assert_eq!(search_output["tools"][0]["name"], json!("late_tool"));
        assert_eq!(search_output["tools"][0]["defer_loading"], json!(true));
        assert!(input
            .iter()
            .all(|item| item["type"] != json!("additional_tools")));

        // Neither flag: every tool stays top-level, no replay items.
        let m = model(json!({}));
        let params = params_for(&m, &ctx, &OpenAIResponsesOptions::default());
        assert_eq!(
            top_level_tool_names(&params),
            vec!["base_tool", "late_tool"]
        );
        let input = params["input"].as_array().expect("input");
        assert!(input
            .iter()
            .all(|item| item["type"] != json!("additional_tools")));
        assert!(input
            .iter()
            .all(|item| item["type"] != json!("tool_search_output")));
    }

    // -- client headers ---------------------------------------------------------

    #[test]
    fn test_build_client_headers_openai_session() {
        let m = model(json!({}));
        let ctx = common::context(vec![common::user_text("hi")], None);
        let headers = build_client_headers(&m, &ctx, "key", None, Some("sess"), &get_compat(&m));
        assert_eq!(
            headers.get("authorization"),
            Some(&Some("Bearer key".to_owned()))
        );
        assert_eq!(
            headers.get("accept"),
            Some(&Some("application/json".to_owned()))
        );
        // Responses variant: no sendSessionAffinityHeaders gate, no
        // x-session-affinity.
        assert_eq!(headers.get("session_id"), Some(&Some("sess".to_owned())));
        assert_eq!(
            headers.get("x-client-request-id"),
            Some(&Some("sess".to_owned()))
        );
        assert!(!headers.contains_key("x-session-affinity"));
        assert!(!headers.contains_key("x-session-id"));
    }

    #[test]
    fn test_build_client_headers_openrouter_session() {
        let m = model(json!({"provider": "openrouter"}));
        let ctx = common::context(vec![common::user_text("hi")], None);
        let headers = build_client_headers(&m, &ctx, "key", None, Some("sess"), &get_compat(&m));
        assert_eq!(headers.get("x-session-id"), Some(&Some("sess".to_owned())));
        assert!(!headers.contains_key("session_id"));
        assert!(!headers.contains_key("x-client-request-id"));
    }

    #[test]
    fn test_build_client_headers_nosession_and_override() {
        let m = model(json!({"compat": {"sessionAffinityFormat": "openai-nosession"}}));
        let ctx = common::context(vec![common::user_text("hi")], None);
        let headers = build_client_headers(&m, &ctx, "key", None, Some("sess"), &get_compat(&m));
        assert!(!headers.contains_key("session_id"));
        assert_eq!(
            headers.get("x-client-request-id"),
            Some(&Some("sess".to_owned()))
        );

        // Options headers merge last and override defaults.
        let options_headers: ProviderHeaders =
            [("authorization".to_owned(), Some("Bearer custom".to_owned()))].into();
        let headers = build_client_headers(
            &m,
            &ctx,
            "key",
            Some(&options_headers),
            None,
            &get_compat(&m),
        );
        assert_eq!(
            headers.get("authorization"),
            Some(&Some("Bearer custom".to_owned()))
        );
    }

    // -- build_params -----------------------------------------------------------

    #[test]
    fn test_build_params_minimal_key_order() {
        let m = model(json!({"reasoning": false}));
        let ctx = common::context(vec![common::user_text("hi")], None);
        let params = params_for(&m, &ctx, &OpenAIResponsesOptions::default());
        assert_eq!(
            param_keys(&params),
            vec!["model", "input", "stream", "store"]
        );
        assert_eq!(params["model"], json!("gpt-4o"));
        assert_eq!(params["stream"], json!(true));
        assert_eq!(params["store"], json!(false));
    }

    #[test]
    fn test_build_params_prompt_cache_states() {
        let m = model(json!({"reasoning": false}));
        let ctx = common::context(vec![common::user_text("hi")], None);

        // Long retention + session id → key and 24h retention.
        let options = OpenAIResponsesOptions {
            stream: StreamOptions {
                session_id: Some("sess".to_owned()),
                cache_retention: Some(CacheRetention::Long),
                ..StreamOptions::default()
            },
            ..OpenAIResponsesOptions::default()
        };
        let params = params_for(&m, &ctx, &options);
        assert_eq!(
            param_keys(&params),
            vec![
                "model",
                "input",
                "stream",
                "prompt_cache_key",
                "prompt_cache_retention",
                "store"
            ]
        );
        assert_eq!(params["prompt_cache_key"], json!("sess"));
        assert_eq!(params["prompt_cache_retention"], json!("24h"));

        // Default (short) retention: no retention key even with a session id.
        let options = OpenAIResponsesOptions {
            stream: StreamOptions {
                session_id: Some("sess".to_owned()),
                ..StreamOptions::default()
            },
            ..OpenAIResponsesOptions::default()
        };
        let params = params_for(&m, &ctx, &options);
        assert_eq!(params["prompt_cache_key"], json!("sess"));
        assert!(params.get("prompt_cache_retention").is_none());

        // Retention none + explicit-mode support → prompt_cache_options, no key.
        let m = model(json!({
            "reasoning": false,
            "compat": {"supportsExplicitPromptCacheMode": true}
        }));
        let options = OpenAIResponsesOptions {
            stream: StreamOptions {
                session_id: Some("sess".to_owned()),
                cache_retention: Some(CacheRetention::None),
                ..StreamOptions::default()
            },
            ..OpenAIResponsesOptions::default()
        };
        let params = params_for(&m, &ctx, &options);
        assert!(params.get("prompt_cache_key").is_none());
        assert_eq!(params["prompt_cache_options"], json!({"mode": "explicit"}));
    }

    #[test]
    fn test_build_params_min_output_tokens() {
        let m = model(json!({"reasoning": false}));
        let ctx = common::context(vec![common::user_text("hi")], None);
        let options = OpenAIResponsesOptions {
            stream: StreamOptions {
                max_tokens: Some(4),
                ..StreamOptions::default()
            },
            ..OpenAIResponsesOptions::default()
        };
        let params = params_for(&m, &ctx, &options);
        assert_eq!(params["max_output_tokens"], json!(16));

        let options = OpenAIResponsesOptions {
            stream: StreamOptions {
                max_tokens: Some(100),
                ..StreamOptions::default()
            },
            ..OpenAIResponsesOptions::default()
        };
        let params = params_for(&m, &ctx, &options);
        assert_eq!(params["max_output_tokens"], json!(100));
    }

    #[test]
    fn test_build_params_reasoning_effort_and_summary() {
        let m = model(json!({"thinkingLevelMap": {"high": "high-mapped"}}));
        let ctx = common::context(vec![common::user_text("hi")], None);
        let options = OpenAIResponsesOptions {
            reasoning_effort: Some(ModelThinkingLevel::High),
            ..OpenAIResponsesOptions::default()
        };
        let params = params_for(&m, &ctx, &options);
        assert_eq!(
            params["reasoning"],
            json!({"effort": "high-mapped", "summary": "auto"})
        );
        assert_eq!(params["include"], json!(["reasoning.encrypted_content"]));

        // Explicit summary wins; empty string falls back to "auto".
        let options = OpenAIResponsesOptions {
            reasoning_summary: Some("detailed".to_owned()),
            ..OpenAIResponsesOptions::default()
        };
        let params = params_for(&m, &ctx, &options);
        assert_eq!(params["reasoning"]["summary"], json!("detailed"));
        assert_eq!(params["reasoning"]["effort"], json!("medium"));

        let options = OpenAIResponsesOptions {
            reasoning_summary: Some(String::new()),
            ..OpenAIResponsesOptions::default()
        };
        let params = params_for(&m, &ctx, &options);
        assert_eq!(params["reasoning"]["summary"], json!("auto"));
    }

    #[test]
    fn test_build_params_reasoning_off_branches() {
        let ctx = common::context(vec![common::user_text("hi")], None);

        // No effort/summary: effort from thinkingLevelMap.off, else "none".
        let m = model(json!({"thinkingLevelMap": {"off": "reasoning-off"}}));
        let params = params_for(&m, &ctx, &OpenAIResponsesOptions::default());
        assert_eq!(params["reasoning"], json!({"effort": "reasoning-off"}));
        assert!(params.get("include").is_none());

        let m = model(json!({}));
        let params = params_for(&m, &ctx, &OpenAIResponsesOptions::default());
        assert_eq!(params["reasoning"], json!({"effort": "none"}));

        // off: null disables the off branch entirely.
        let m = model(json!({"thinkingLevelMap": {"off": null}}));
        let params = params_for(&m, &ctx, &OpenAIResponsesOptions::default());
        assert!(params.get("reasoning").is_none());

        // github-copilot never gets the off branch.
        let m = model(json!({"provider": "github-copilot"}));
        let params = params_for(&m, &ctx, &OpenAIResponsesOptions::default());
        assert!(params.get("reasoning").is_none());

        // xai always includes encrypted reasoning content.
        let m = model(json!({"provider": "xai"}));
        let params = params_for(&m, &ctx, &OpenAIResponsesOptions::default());
        assert_eq!(params["reasoning"], json!({"effort": "none"}));
        assert_eq!(params["include"], json!(["reasoning.encrypted_content"]));
    }

    #[test]
    fn test_build_params_tools_choice_and_service_tier() {
        let m = model(json!({"reasoning": false}));
        let ctx = common::context(
            vec![common::user_text("hi")],
            Some(vec![common::tool("bash")]),
        );
        let options = OpenAIResponsesOptions {
            service_tier: Some("flex".to_owned()),
            tool_choice: Some(json!("auto")),
            ..OpenAIResponsesOptions::default()
        };
        let params = params_for(&m, &ctx, &options);
        assert_eq!(params["service_tier"], json!("flex"));
        assert_eq!(params["tool_choice"], json!("auto"));
        // Default compat: strict mode unsupported → no strict key on tools.
        assert_eq!(params["tools"][0]["type"], json!("function"));
        assert_eq!(params["tools"][0]["name"], json!("bash"));
        assert!(params["tools"][0].get("strict").is_none());

        // No tools → no tools key.
        let ctx = common::context(vec![common::user_text("hi")], None);
        let params = params_for(&m, &ctx, &OpenAIResponsesOptions::default());
        assert!(params.get("tools").is_none());
    }

    // -- service tier pricing -------------------------------------------------

    #[test]
    fn test_service_tier_cost_multiplier() {
        assert_eq!(
            get_service_tier_cost_multiplier("gpt-4o", Some("flex")),
            0.5
        );
        assert_eq!(
            get_service_tier_cost_multiplier("gpt-4o", Some("priority")),
            2.0
        );
        assert_eq!(
            get_service_tier_cost_multiplier("gpt-5.5", Some("priority")),
            2.5
        );
        assert_eq!(
            get_service_tier_cost_multiplier("gpt-4o", Some("auto")),
            1.0
        );
        assert_eq!(get_service_tier_cost_multiplier("gpt-4o", None), 1.0);
    }

    #[test]
    fn test_apply_service_tier_pricing() {
        let mut usage = Usage {
            cost: UsageCost {
                input: 4.0,
                output: 8.0,
                cache_read: 2.0,
                cache_write: 6.0,
                total: 20.0,
            },
            ..Usage::default()
        };
        apply_service_tier_pricing(&mut usage, Some("flex"), "gpt-4o");
        assert_eq!(usage.cost.input, 2.0);
        assert_eq!(usage.cost.output, 4.0);
        assert_eq!(usage.cost.cache_read, 1.0);
        assert_eq!(usage.cost.cache_write, 3.0);
        assert_eq!(usage.cost.total, 10.0);

        // Multiplier 1 leaves everything untouched.
        let mut usage = Usage {
            cost: UsageCost {
                input: 4.0,
                output: 8.0,
                cache_read: 2.0,
                cache_write: 6.0,
                total: 20.0,
            },
            ..Usage::default()
        };
        apply_service_tier_pricing(&mut usage, None, "gpt-4o");
        assert_eq!(usage.cost.total, 20.0);
    }

    // -- stream_simple ----------------------------------------------------------

    #[tokio::test]
    async fn test_stream_simple_missing_auth_is_stream_error() {
        let m = model(json!({}));
        let events: Vec<StreamEvent> = stream_simple(
            &m,
            &common::context(vec![common::user_text("hi")], None),
            None,
        )
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
}
