//! Port of `packages/ai/src/api/azure-openai-responses.ts` @ pi 0.82.1 (2efa728).
//! (`azure-openai-responses.lazy.ts` is the upstream dynamic-import wrapper;
//! rpi adapters are linked statically, so there is no lazy counterpart.)
//! Error tail carries `output.errorMessage` since 4181f66 (32850ef7c).
//!
//! Azure OpenAI Responses adapter: deployment-name resolution (option →
//! `AZURE_OPENAI_DEPLOYMENT_NAME_MAP` → model id), Azure base-URL
//! normalization (the three Azure host suffixes collapse root-ish paths to
//! `/openai/v1`), config resolution (API version defaulting to `v1`),
//! `build_params` (deployment name as `model`, always-clamped prompt-cache
//! key, the 16-token minimum on `max_output_tokens`, reasoning
//! effort/summary, strict tools supported by default), and SSE event
//! processing through the shared [`ResponsesStreamProcessor`].
//!
//! Intentional differences (upstream deviations, D-024):
//! - HTTP is a direct reqwest call, not the `openai` SDK's `AzureOpenAI`
//!   client; the SDK's `x-stainless-*` telemetry headers, `User-Agent`,
//!   platform headers and default timeout are not sent (callers set
//!   `StreamOptions::timeout_ms`). The wire shape was verified against the
//!   pinned openai@6.26.0 SDK: `POST {baseUrl}/responses?api-version={v}`
//!   with the deployment name in the body's `model` field (the SDK's
//!   `/deployments/{model}` rewrite does not cover `/responses`), the
//!   `api-key` auth header (no `Authorization: Bearer`) and
//!   `Accept: application/json`.
//! - `on_payload` sees the wire (snake_case) JSON body, not the SDK's
//!   camelCase `ResponseCreateParamsStreaming` object (consistent with the
//!   other rpi adapters).
//! - SSE events are parsed with strict `serde_json` (the SDK uses
//!   `JSON.parse`); parse failures read
//!   `Could not parse Azure OpenAI Responses SSE event: {error}; data={data}`
//!   (the SDK would surface a `SyntaxError` message instead).
//! - HTTP error status/body are extracted from the response at the call site
//!   (upstream reads them off the SDK error object in the catch block). Only
//!   HTTP failures carry a status, so the `Azure OpenAI API error` prefix
//!   applies to exactly the same errors as upstream's
//!   `formatProviderError(normalizeProviderError(error), "Azure OpenAI API error")`.
//! - `stream_simple` reports a missing API key as a stream error event
//!   (upstream `streamSimple` throws synchronously), consistent with the
//!   other rpi adapters.
//! - Upstream scrubs `partialJson`/`customInput` scratch off content blocks in
//!   its catch block; rpi keeps streaming scratch in the processor (never in
//!   content blocks), so there is nothing to scrub.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use futures::StreamExt;
use serde_json::{json, Value};
use url::Url;

use crate::api::constrained_sampling::create_grammar_tool_input_properties;
use crate::api::lazy::immediate_error_stream;
use crate::api::openai_completions::{mapped_or_level_name, off_is_not_null, off_value};
use crate::api::openai_prompt_cache::clamp_openai_prompt_cache_key;
use crate::api::openai_responses::OPENAI_RESPONSES_MIN_OUTPUT_TOKENS;
use crate::api::openai_responses_shared::{
    convert_responses_messages, convert_responses_tools, ConvertResponsesMessagesOptions,
    ConvertResponsesToolsOptions, ResponsesStreamOptions, ResponsesStreamProcessor,
};
use crate::api::simple_options::build_base_options;
use crate::api::sse::SseDecoder;
use crate::models::{clamp_thinking_level, ProviderStreams};
use crate::types::{
    AssistantMessage, Context, DoneReason, ErrorReason, Model, ModelThinkingLevel, ProviderHeaders,
    ProviderResponse, SimpleStreamOptions, StopReason, StreamEvent, StreamOptions, Usage,
};
use crate::utils::custom_fetch::send_provider_request;
use crate::utils::error_body::{format_provider_error, NormalizedProviderError};
use crate::utils::event_stream::AssistantMessageEventStream;
use crate::utils::headers::{
    headers_to_record, merge_headers_chain, model_headers, provider_headers_to_header_map,
};
use crate::utils::provider_env::get_provider_env_value;
use crate::utils::provider_retry::{
    retry_provider_request, ProviderErrorInfo, ProviderRetryOptions,
};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// `DEFAULT_AZURE_API_VERSION`.
pub const DEFAULT_AZURE_API_VERSION: &str = "v1";

/// `AZURE_TOOL_CALL_PROVIDERS`: providers whose tool-call ids keep the
/// Responses `{call_id}|{item_id}` composite form (the OpenAI family plus
/// this API itself).
pub static AZURE_TOOL_CALL_PROVIDERS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    HashSet::from([
        "openai",
        "openai-codex",
        "opencode",
        "azure-openai-responses",
    ])
});

// ---------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------

/// `AzureOpenAIResponsesOptions` — `StreamOptions` plus Azure-specific
/// extras. `reasoning_effort` keys directly into `Model::thinking_level_map`;
/// `reasoning_summary` is `"auto" | "detailed" | "concise"` (upstream also
/// allows explicit `null`, which `None` covers).
#[derive(Debug, Clone, Default)]
pub struct AzureOpenAIResponsesOptions {
    pub stream: StreamOptions,
    pub reasoning_effort: Option<ModelThinkingLevel>,
    pub reasoning_summary: Option<String>,
    pub azure_api_version: Option<String>,
    pub azure_resource_name: Option<String>,
    pub azure_base_url: Option<String>,
    pub azure_deployment_name: Option<String>,
}

// ---------------------------------------------------------------------------
// Deployment name resolution
// ---------------------------------------------------------------------------

/// `parseDeploymentNameMap`: `"modelA=depA, modelB=depB"` → map. Malformed
/// entries (no `=`, empty side) are skipped; both sides are trimmed on
/// insert. Entries are trimmed before splitting, so a whitespace-only value
/// (`"a= "`) collapses to `"a="` and is skipped like an empty one.
pub fn parse_deployment_name_map(value: Option<&str>) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let Some(value) = value else { return map };
    for entry in value.split(',') {
        let trimmed = entry.trim();
        if trimmed.is_empty() {
            continue;
        }
        // JS `trimmed.split("=", 2)` keeps only the first two parts.
        let mut parts = trimmed.splitn(2, '=');
        let model_id = parts.next().unwrap_or("");
        let Some(deployment_name) = parts.next() else {
            continue;
        };
        if model_id.is_empty() || deployment_name.is_empty() {
            continue;
        }
        map.insert(
            model_id.trim().to_owned(),
            deployment_name.trim().to_owned(),
        );
    }
    map
}

/// `resolveDeploymentName`: the option wins, then the env map keyed by model
/// id, then the model id itself. JS `||` semantics: an empty option value or
/// an empty mapped value falls through.
pub fn resolve_deployment_name(
    model: &Model,
    options: Option<&AzureOpenAIResponsesOptions>,
) -> String {
    if let Some(name) = options
        .and_then(|options| options.azure_deployment_name.as_deref())
        .filter(|name| !name.is_empty())
    {
        return name.to_owned();
    }
    let env = options.and_then(|options| options.stream.env.as_ref());
    let map = parse_deployment_name_map(
        get_provider_env_value("AZURE_OPENAI_DEPLOYMENT_NAME_MAP", env).as_deref(),
    );
    match map.get(&model.id) {
        Some(mapped) if !mapped.is_empty() => mapped.clone(),
        _ => model.id.clone(),
    }
}

// ---------------------------------------------------------------------------
// Base URL / config resolution
// ---------------------------------------------------------------------------

/// `normalizeAzureBaseUrl`: trims trailing slashes, then — only for the
/// three Azure host suffixes — collapses root-ish paths (`""`, `"/"`,
/// `"/openai"`, `"/openai/v1/responses"`) to `/openai/v1` and drops the
/// query string, so the client can append `/responses` and
/// `?api-version=v1` correctly (azure-openai-responses.ts:190-209).
/// Non-Azure proxy URLs pass through untouched.
pub fn normalize_azure_base_url(base_url: &str) -> Result<String, String> {
    let trimmed = base_url.trim().trim_end_matches('/');
    let mut url =
        Url::parse(trimmed).map_err(|_| format!("Invalid Azure OpenAI base URL: {base_url}"))?;

    let hostname = url.host_str().unwrap_or("");
    let is_azure_host = hostname.ends_with(".openai.azure.com")
        || hostname.ends_with(".cognitiveservices.azure.com")
        || hostname.ends_with(".ai.azure.com");
    // For absolute http(s) URLs the path is at least "/"; trimming trailing
    // slashes folds JS's "" and "/" cases into "".
    let normalized_path = url.path().trim_end_matches('/');

    if is_azure_host
        && (normalized_path.is_empty()
            || normalized_path == "/openai"
            || normalized_path == "/openai/v1/responses")
    {
        url.set_path("/openai/v1");
        url.set_query(None);
    }

    Ok(url.as_str().trim_end_matches('/').to_owned())
}

/// `buildDefaultBaseUrl`.
pub fn build_default_base_url(resource_name: &str) -> String {
    format!("https://{resource_name}.openai.azure.com/openai/v1")
}

/// `resolveAzureConfig` result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AzureConfig {
    pub base_url: String,
    pub api_version: String,
}

/// `resolveAzureConfig`: API version from option → `AZURE_OPENAI_API_VERSION`
/// → `v1`; base URL from option → `AZURE_OPENAI_BASE_URL` → resource-name
/// default (`AZURE_OPENAI_RESOURCE_NAME` / `azureResourceName`) →
/// `model.base_url`. Empty strings are falsy upstream and fall through at
/// every step.
pub fn resolve_azure_config(
    model: &Model,
    options: Option<&AzureOpenAIResponsesOptions>,
) -> Result<AzureConfig, String> {
    let env = options.and_then(|options| options.stream.env.as_ref());

    let api_version = options
        .and_then(|options| options.azure_api_version.as_deref())
        .filter(|version| !version.is_empty())
        .map(str::to_owned)
        .or_else(|| get_provider_env_value("AZURE_OPENAI_API_VERSION", env))
        .unwrap_or_else(|| DEFAULT_AZURE_API_VERSION.to_owned());

    // JS `options?.azureBaseUrl?.trim() || env?.trim() || undefined`.
    let base_url = options
        .and_then(|options| options.azure_base_url.as_deref())
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            get_provider_env_value("AZURE_OPENAI_BASE_URL", env)
                .map(|url| url.trim().to_owned())
                .filter(|url| !url.is_empty())
        });
    let resource_name = options
        .and_then(|options| options.azure_resource_name.as_deref())
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .or_else(|| get_provider_env_value("AZURE_OPENAI_RESOURCE_NAME", env));

    let mut resolved = base_url;
    if resolved.is_none() {
        if let Some(resource_name) = resource_name {
            resolved = Some(build_default_base_url(&resource_name));
        }
    }
    if resolved.is_none() && !model.base_url.is_empty() {
        resolved = Some(model.base_url.clone());
    }

    let Some(resolved) = resolved else {
        return Err(
            "Azure OpenAI base URL is required. Set AZURE_OPENAI_BASE_URL or AZURE_OPENAI_RESOURCE_NAME, or pass azureBaseUrl, azureResourceName, or model.baseUrl."
                .to_owned(),
        );
    };

    Ok(AzureConfig {
        base_url: normalize_azure_base_url(&resolved)?,
        api_version,
    })
}

// ---------------------------------------------------------------------------
// Client headers
// ---------------------------------------------------------------------------

/// `createClient`'s header assembly: the SDK sends `api-key` (its Azure
/// `authHeaders`) and the default `Accept: application/json`, then
/// `defaultHeaders` (`model.headers` then `options.headers`) merge last and
/// may override both.
pub fn build_client_headers(
    model: &Model,
    api_key: &str,
    options_headers: Option<&ProviderHeaders>,
) -> ProviderHeaders {
    let base: ProviderHeaders = [
        ("accept".to_owned(), Some("application/json".to_owned())),
        ("api-key".to_owned(), Some(api_key.to_owned())),
    ]
    .into();
    merge_headers_chain(&[Some(base), model_headers(model), options_headers.cloned()])
}

// ---------------------------------------------------------------------------
// buildParams
// ---------------------------------------------------------------------------

/// `buildParams`. Key insertion order mirrors the upstream object literal
/// plus conditional assignments (serde_json `preserve_order` keeps it), so
/// payload snapshots compare byte-for-byte with the JS payloads.
pub fn build_params(
    model: &Model,
    context: &Context,
    options: &AzureOpenAIResponsesOptions,
    deployment_name: &str,
    grammar_tool_input_properties: &HashMap<String, String>,
) -> Result<Value, String> {
    let messages = convert_responses_messages(
        model,
        context,
        &AZURE_TOOL_CALL_PROVIDERS,
        &ConvertResponsesMessagesOptions {
            grammar_tool_input_properties: Some(grammar_tool_input_properties),
            ..ConvertResponsesMessagesOptions::default()
        },
    )?;

    let mut params = json!({
        "model": deployment_name,
        "input": messages,
        "stream": true,
    });
    // Upstream sets `prompt_cache_key` unconditionally in the literal; an
    // undefined value is dropped by JSON.stringify, so only Some lands here.
    if let Some(key) = clamp_openai_prompt_cache_key(options.stream.session_id.as_deref()) {
        params["prompt_cache_key"] = json!(key);
    }
    params["store"] = json!(false);

    if let Some(max_tokens) = options.stream.max_tokens {
        params["max_output_tokens"] = json!(max_tokens.max(OPENAI_RESPONSES_MIN_OUTPUT_TOKENS));
    }
    if let Some(temperature) = options.stream.temperature {
        params["temperature"] = json!(temperature);
    }
    if let Some(tools) = context.tools.as_deref().filter(|tools| !tools.is_empty()) {
        // Unlike openai-responses (default false), the Azure adapter defaults
        // `supportsStrictMode` to true.
        params["tools"] = json!(convert_responses_tools(
            tools,
            &ConvertResponsesToolsOptions {
                supports_strict_mode: Some(
                    model
                        .compat
                        .as_ref()
                        .and_then(|compat| compat.supports_strict_mode)
                        .unwrap_or(true),
                ),
                supports_open_ai_grammar_tools: Some(
                    model
                        .compat
                        .as_ref()
                        .and_then(|compat| compat.supports_open_ai_grammar_tools)
                        .unwrap_or(false),
                ),
                ..ConvertResponsesToolsOptions::default()
            },
        )?);
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
        } else if off_is_not_null(model) {
            // `model.thinkingLevelMap?.off ?? "none"`; the `!== null` guard
            // above means `off` is absent (→ "none") or a mapped string.
            let effort = off_value(model)
                .flatten()
                .unwrap_or_else(|| "none".to_owned());
            params["reasoning"] = json!({ "effort": effort });
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
    options: &AzureOpenAIResponsesOptions,
    output: &mut AssistantMessage,
    events: &AssistantMessageEventStream,
) -> Result<DoneReason, String> {
    let deployment_name = resolve_deployment_name(model, Some(options));

    // Upstream checks `options?.apiKey` only — no header fallback (JS `||`
    // makes an empty key count as missing).
    let api_key = options
        .stream
        .api_key
        .as_deref()
        .filter(|key| !key.is_empty())
        .ok_or_else(|| format!("No API key for provider: {}", model.provider))?;
    let config = resolve_azure_config(model, Some(options))?;
    let grammar_tool_input_properties = create_grammar_tool_input_properties(
        context.tools.as_deref(),
        model
            .compat
            .as_ref()
            .and_then(|compat| compat.supports_open_ai_grammar_tools)
            .unwrap_or(false),
    )?;
    let headers = build_client_headers(model, api_key, options.stream.headers.as_ref());
    let mut params = build_params(
        model,
        context,
        options,
        &deployment_name,
        &grammar_tool_input_properties,
    )?;
    if let Some(on_payload) = &options.stream.on_payload {
        if let Some(next_params) = on_payload(params.clone(), model).await {
            params = next_params;
        }
    }

    let url = format!("{}/responses", config.base_url);
    let header_map = provider_headers_to_header_map(&headers)?;
    let mut client_builder = reqwest::Client::builder();
    if let Some(timeout_ms) = options.stream.timeout_ms {
        client_builder = client_builder.timeout(std::time::Duration::from_millis(timeout_ms));
    }
    let client = client_builder.build().map_err(|error| error.to_string())?;

    let response = retry_provider_request(
        || {
            let request = client
                .post(&url)
                .query(&[("api-version", config.api_version.as_str())])
                .headers(header_map.clone())
                .json(&params);
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
                                    Some("Azure OpenAI API error"),
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

    let mut processor = ResponsesStreamProcessor::new(
        output,
        model,
        ResponsesStreamOptions {
            service_tier: None,
            grammar_tool_input_properties: &grammar_tool_input_properties,
            apply_service_tier_pricing: None,
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
                    "Could not parse Azure OpenAI Responses SSE event: {error}; data={}",
                    sse.data
                )
            })?;
            processor.handle_event(&event, events)?;
        }
    }
    for sse in decoder.finish() {
        let event: Value = serde_json::from_str(&sse.data).map_err(|error| {
            format!(
                "Could not parse Azure OpenAI Responses SSE event: {error}; data={}",
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
            Err("Azure OpenAI Responses stream ended without a stop reason".to_owned())
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

/// `stream` (azure-openai-responses).
pub fn stream(
    model: &Model,
    context: &Context,
    options: AzureOpenAIResponsesOptions,
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

/// `streamSimple` (azure-openai-responses): reasoning maps through
/// `clampThinkingLevel`; a clamped "off" omits `reasoning_effort`.
pub fn stream_simple(
    model: &Model,
    context: &Context,
    options: Option<SimpleStreamOptions>,
) -> AssistantMessageEventStream {
    let api_key_valid = options
        .as_ref()
        .and_then(|o| o.stream.api_key.as_deref())
        .is_some_and(|key| !key.is_empty());
    if !api_key_valid {
        return immediate_error_stream(
            model,
            &format!("No API key for provider: {}", model.provider),
        );
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
        AzureOpenAIResponsesOptions {
            stream: base,
            reasoning_effort,
            ..AzureOpenAIResponsesOptions::default()
        },
    )
}

/// `ProviderStreams` implementation for `ApiKind::AZURE_OPENAI_RESPONSES`.
///
/// The trait carries plain [`StreamOptions`]; Azure-specific extras
/// ([`AzureOpenAIResponsesOptions`]) reach [`stream`] only through direct
/// calls or via [`stream_simple`] reasoning mapping (design §3.3 collapses
/// per-API extras; `StreamOptions::env` still carries the `AZURE_OPENAI_*`
/// overrides through the trait).
#[derive(Debug, Clone, Copy, Default)]
pub struct AzureOpenAiResponses;

impl ProviderStreams for AzureOpenAiResponses {
    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: Option<StreamOptions>,
    ) -> AssistantMessageEventStream {
        stream(
            model,
            context,
            AzureOpenAIResponsesOptions {
                stream: options.unwrap_or_default(),
                ..AzureOpenAIResponsesOptions::default()
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
    use crate::types::ProviderEnv;

    fn model(extra: Value) -> Model {
        let mut overrides =
            json!({"api": "azure-openai-responses", "provider": "azure-openai-responses"});
        overrides
            .as_object_mut()
            .expect("object")
            .extend(extra.as_object().cloned().unwrap_or_default());
        common::make_model(overrides)
    }

    fn no_grammar() -> HashMap<String, String> {
        HashMap::new()
    }

    fn params_for(model: &Model, ctx: &Context, options: &AzureOpenAIResponsesOptions) -> Value {
        build_params(model, ctx, options, "deployment-x", &no_grammar()).expect("params")
    }

    fn param_keys(params: &Value) -> Vec<String> {
        params
            .as_object()
            .expect("object")
            .keys()
            .cloned()
            .collect()
    }

    // -- parseDeploymentNameMap -------------------------------------------------

    #[test]
    fn test_parse_deployment_name_map() {
        assert!(parse_deployment_name_map(None).is_empty());
        assert!(parse_deployment_name_map(Some("")).is_empty());
        let map =
            parse_deployment_name_map(Some("gpt-4o=dep-a, gpt-5 = dep-b ,,bad-entry,=noval,x="));
        assert_eq!(map.get("gpt-4o"), Some(&"dep-a".to_owned()));
        assert_eq!(map.get("gpt-5"), Some(&"dep-b".to_owned()));
        assert_eq!(map.len(), 2);
        // Whitespace-only values collapse to empty after the entry trim and
        // are skipped; values with inner spaces survive trimmed.
        let map = parse_deployment_name_map(Some("a= ,b=,c= dep c "));
        assert!(!map.contains_key("a"));
        assert!(!map.contains_key("b"));
        assert_eq!(map.get("c"), Some(&"dep c".to_owned()));
    }

    // -- resolveDeploymentName ---------------------------------------------------

    #[test]
    fn test_resolve_deployment_name() {
        let m = model(json!({"id": "gpt-4o"}));
        // No option, no env → model id.
        assert_eq!(resolve_deployment_name(&m, None), "gpt-4o");

        // Env map (scoped override, not process env).
        let env: ProviderEnv = [(
            "AZURE_OPENAI_DEPLOYMENT_NAME_MAP".to_owned(),
            "gpt-4o=my-dep".to_owned(),
        )]
        .into();
        let options = AzureOpenAIResponsesOptions {
            stream: StreamOptions {
                request: crate::types::ProviderRequestOptions {
                    env: Some(env),
                    ..Default::default()
                },
                ..StreamOptions::default()
            },
            ..AzureOpenAIResponsesOptions::default()
        };
        assert_eq!(resolve_deployment_name(&m, Some(&options)), "my-dep");

        // Explicit option wins over the env map; empty option falls through.
        let options = AzureOpenAIResponsesOptions {
            azure_deployment_name: Some("explicit-dep".to_owned()),
            ..options.clone()
        };
        assert_eq!(resolve_deployment_name(&m, Some(&options)), "explicit-dep");
        let options = AzureOpenAIResponsesOptions {
            azure_deployment_name: Some(String::new()),
            ..options.clone()
        };
        assert_eq!(resolve_deployment_name(&m, Some(&options)), "my-dep");

        // Unmapped model falls back to its id.
        let other = model(json!({"id": "gpt-5"}));
        assert_eq!(resolve_deployment_name(&other, Some(&options)), "gpt-5");
    }

    // -- normalizeAzureBaseUrl (azure-openai-base-url.test.ts cases) -------------

    #[test]
    fn test_normalize_azure_base_url_host_suffixes() {
        for (input, expected) in [
            (
                "https://marc-quicktests-resource.cognitiveservices.azure.com",
                "https://marc-quicktests-resource.cognitiveservices.azure.com/openai/v1",
            ),
            (
                "https://marc-quicktests-resource.ai.azure.com",
                "https://marc-quicktests-resource.ai.azure.com/openai/v1",
            ),
            (
                "https://my-resource.openai.azure.com",
                "https://my-resource.openai.azure.com/openai/v1",
            ),
        ] {
            assert_eq!(normalize_azure_base_url(input).expect("url"), expected);
        }
    }

    #[test]
    fn test_normalize_azure_base_url_path_variants() {
        for (input, expected) in [
            (
                "https://my-resource.cognitiveservices.azure.com/openai",
                "https://my-resource.cognitiveservices.azure.com/openai/v1",
            ),
            (
                "https://my-resource.cognitiveservices.azure.com/openai/v1",
                "https://my-resource.cognitiveservices.azure.com/openai/v1",
            ),
            (
                "https://my-resource.services.ai.azure.com/openai/v1/responses",
                "https://my-resource.services.ai.azure.com/openai/v1",
            ),
            // Trailing slashes are trimmed before the path comparison.
            (
                "https://my-resource.openai.azure.com/openai/",
                "https://my-resource.openai.azure.com/openai/v1",
            ),
        ] {
            assert_eq!(normalize_azure_base_url(input).expect("url"), expected);
        }
    }

    #[test]
    fn test_normalize_azure_base_url_query_handling() {
        // Azure host normalization drops the query string.
        assert_eq!(
            normalize_azure_base_url(
                "https://my-resource.openai.azure.com/openai?api-version=2024-12-01"
            )
            .expect("url"),
            "https://my-resource.openai.azure.com/openai/v1"
        );
        // Non-Azure proxy URLs keep path and query untouched.
        assert_eq!(
            normalize_azure_base_url("https://my-proxy.example.com/v1?custom=true").expect("url"),
            "https://my-proxy.example.com/v1?custom=true"
        );
        assert_eq!(
            normalize_azure_base_url("https://my-proxy.example.com/v1").expect("url"),
            "https://my-proxy.example.com/v1"
        );
    }

    #[test]
    fn test_normalize_azure_base_url_invalid() {
        let error = normalize_azure_base_url("not-a-url").expect_err("invalid");
        assert_eq!(error, "Invalid Azure OpenAI base URL: not-a-url");
    }

    // -- resolveAzureConfig -------------------------------------------------------

    #[test]
    fn test_resolve_azure_config_precedence() {
        let m = model(json!({"baseUrl": "https://model-level.example.com/v1"}));

        // Nothing set anywhere: model.baseUrl wins; api version defaults v1.
        let config = resolve_azure_config(&m, None).expect("config");
        assert_eq!(config.base_url, "https://model-level.example.com/v1");
        assert_eq!(config.api_version, "v1");

        // Resource name builds the default URL when no base URL is set.
        let m_empty = model(json!({"baseUrl": ""}));
        let options = AzureOpenAIResponsesOptions {
            azure_resource_name: Some("my-resource".to_owned()),
            ..AzureOpenAIResponsesOptions::default()
        };
        let config = resolve_azure_config(&m_empty, Some(&options)).expect("config");
        assert_eq!(
            config.base_url,
            "https://my-resource.openai.azure.com/openai/v1"
        );

        // Explicit azureBaseUrl wins over resource name and model.baseUrl;
        // Azure hosts are normalized.
        let options = AzureOpenAIResponsesOptions {
            azure_base_url: Some("https://res.openai.azure.com".to_owned()),
            azure_resource_name: Some("ignored".to_owned()),
            azure_api_version: Some("2024-12-01".to_owned()),
            ..AzureOpenAIResponsesOptions::default()
        };
        let config = resolve_azure_config(&m, Some(&options)).expect("config");
        assert_eq!(config.base_url, "https://res.openai.azure.com/openai/v1");
        assert_eq!(config.api_version, "2024-12-01");

        // Scoped env overrides: base URL + api version from env.
        let env: ProviderEnv = [
            (
                "AZURE_OPENAI_BASE_URL".to_owned(),
                "https://env-proxy.example.com/v1/".to_owned(),
            ),
            ("AZURE_OPENAI_API_VERSION".to_owned(), "preview".to_owned()),
        ]
        .into();
        let options = AzureOpenAIResponsesOptions {
            stream: StreamOptions {
                request: crate::types::ProviderRequestOptions {
                    env: Some(env),
                    ..Default::default()
                },
                ..StreamOptions::default()
            },
            ..AzureOpenAIResponsesOptions::default()
        };
        let config = resolve_azure_config(&m, Some(&options)).expect("config");
        assert_eq!(config.base_url, "https://env-proxy.example.com/v1");
        assert_eq!(config.api_version, "preview");

        // AZURE_OPENAI_RESOURCE_NAME from env builds the default URL.
        let env: ProviderEnv = [(
            "AZURE_OPENAI_RESOURCE_NAME".to_owned(),
            "env-res".to_owned(),
        )]
        .into();
        let options = AzureOpenAIResponsesOptions {
            stream: StreamOptions {
                request: crate::types::ProviderRequestOptions {
                    env: Some(env),
                    ..Default::default()
                },
                ..StreamOptions::default()
            },
            ..AzureOpenAIResponsesOptions::default()
        };
        let config = resolve_azure_config(&m_empty, Some(&options)).expect("config");
        assert_eq!(
            config.base_url,
            "https://env-res.openai.azure.com/openai/v1"
        );
    }

    #[test]
    fn test_resolve_azure_config_missing_base_url() {
        let m = model(json!({"baseUrl": ""}));
        let error = resolve_azure_config(&m, None).expect_err("missing");
        assert!(error.starts_with("Azure OpenAI base URL is required."));
    }

    // -- build_client_headers ------------------------------------------------------

    #[test]
    fn test_build_client_headers() {
        let m = model(json!({}));
        let headers = build_client_headers(&m, "key", None);
        // Azure auth header, not Authorization: Bearer.
        assert_eq!(headers.get("api-key"), Some(&Some("key".to_owned())));
        assert_eq!(
            headers.get("accept"),
            Some(&Some("application/json".to_owned()))
        );
        assert!(!headers.contains_key("authorization"));

        // Options headers merge last and override.
        let options_headers: ProviderHeaders =
            [("api-key".to_owned(), Some("custom".to_owned()))].into();
        let headers = build_client_headers(&m, "key", Some(&options_headers));
        assert_eq!(headers.get("api-key"), Some(&Some("custom".to_owned())));
    }

    // -- build_params ----------------------------------------------------------------

    /// Upstream sampling-options.test.ts (25a2c8dcf @ 4181f66, #7568),
    /// azure-openai-responses leg: sampling params merge last, overriding
    /// named request fields.
    #[test]
    fn test_build_params_sampling_params_merged_last() {
        let m = model(json!({"reasoning": false}));
        let ctx = common::context(vec![common::user_text("hi")], None);
        let options = AzureOpenAIResponsesOptions {
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
            ..AzureOpenAIResponsesOptions::default()
        };
        let params = params_for(&m, &ctx, &options);
        assert_eq!(params["top_p"], json!(0.95));
        assert_eq!(params["top_k"], json!(0));
        assert_eq!(params["min_p"], json!(0));
        assert_eq!(params["temperature"], json!(1.0));

        let plain = params_for(&m, &ctx, &AzureOpenAIResponsesOptions::default());
        assert!(plain.get("top_p").is_none());
        assert!(plain.get("temperature").is_none());
    }

    #[test]
    fn test_build_params_minimal_key_order() {
        let m = model(json!({"reasoning": false}));
        let ctx = common::context(vec![common::user_text("hi")], None);
        let params = params_for(&m, &ctx, &AzureOpenAIResponsesOptions::default());
        // The deployment name replaces the model id.
        assert_eq!(
            param_keys(&params),
            vec!["model", "input", "stream", "store"]
        );
        assert_eq!(params["model"], json!("deployment-x"));
        assert_eq!(params["stream"], json!(true));
        assert_eq!(params["store"], json!(false));
    }

    #[test]
    fn test_build_params_prompt_cache_key_clamped() {
        let m = model(json!({"reasoning": false}));
        let ctx = common::context(vec![common::user_text("hi")], None);
        let options = AzureOpenAIResponsesOptions {
            stream: StreamOptions {
                session_id: Some("x".repeat(67)),
                ..StreamOptions::default()
            },
            ..AzureOpenAIResponsesOptions::default()
        };
        let params = params_for(&m, &ctx, &options);
        // OpenAI's 64-character prompt_cache_key limit (base-url test intent).
        assert_eq!(params["prompt_cache_key"], json!("x".repeat(64)));

        // No session id: the key is dropped (JS undefined property).
        let params = params_for(&m, &ctx, &AzureOpenAIResponsesOptions::default());
        assert!(params.get("prompt_cache_key").is_none());
    }

    #[test]
    fn test_build_params_min_output_tokens() {
        let m = model(json!({"reasoning": false}));
        let ctx = common::context(vec![common::user_text("hi")], None);
        let options = AzureOpenAIResponsesOptions {
            stream: StreamOptions {
                max_tokens: Some(4),
                ..StreamOptions::default()
            },
            ..AzureOpenAIResponsesOptions::default()
        };
        let params = params_for(&m, &ctx, &options);
        assert_eq!(params["max_output_tokens"], json!(16));
    }

    #[test]
    fn test_build_params_reasoning_branches() {
        let ctx = common::context(vec![common::user_text("hi")], None);

        // Effort + default summary, mapped through thinkingLevelMap.
        let m = model(json!({"thinkingLevelMap": {"high": "high-mapped"}}));
        let options = AzureOpenAIResponsesOptions {
            reasoning_effort: Some(ModelThinkingLevel::High),
            ..AzureOpenAIResponsesOptions::default()
        };
        let params = params_for(&m, &ctx, &options);
        assert_eq!(
            params["reasoning"],
            json!({"effort": "high-mapped", "summary": "auto"})
        );
        assert_eq!(params["include"], json!(["reasoning.encrypted_content"]));

        // Summary only: effort defaults to "medium"; empty summary → "auto".
        let options = AzureOpenAIResponsesOptions {
            reasoning_summary: Some("detailed".to_owned()),
            ..AzureOpenAIResponsesOptions::default()
        };
        let params = params_for(&m, &ctx, &options);
        assert_eq!(
            params["reasoning"],
            json!({"effort": "medium", "summary": "detailed"})
        );
        let options = AzureOpenAIResponsesOptions {
            reasoning_summary: Some(String::new()),
            ..AzureOpenAIResponsesOptions::default()
        };
        let params = params_for(&m, &ctx, &options);
        assert_eq!(params["reasoning"]["summary"], json!("auto"));

        // No effort/summary: effort from thinkingLevelMap.off, else "none";
        // off: null disables the branch entirely.
        let m = model(json!({"thinkingLevelMap": {"off": "reasoning-off"}}));
        let params = params_for(&m, &ctx, &AzureOpenAIResponsesOptions::default());
        assert_eq!(params["reasoning"], json!({"effort": "reasoning-off"}));
        assert!(params.get("include").is_none());

        let m = model(json!({}));
        let params = params_for(&m, &ctx, &AzureOpenAIResponsesOptions::default());
        assert_eq!(params["reasoning"], json!({"effort": "none"}));

        let m = model(json!({"thinkingLevelMap": {"off": null}}));
        let params = params_for(&m, &ctx, &AzureOpenAIResponsesOptions::default());
        assert!(params.get("reasoning").is_none());
    }

    #[test]
    fn test_build_params_tools_strict_default_true() {
        let m = model(json!({"reasoning": false}));
        let ctx = common::context(
            vec![common::user_text("hi")],
            Some(vec![common::tool("bash")]),
        );
        let params = params_for(&m, &ctx, &AzureOpenAIResponsesOptions::default());
        // Azure default: supportsStrictMode true → the strict key is present.
        assert_eq!(params["tools"][0]["type"], json!("function"));
        assert_eq!(params["tools"][0]["strict"], json!(false));

        // supportsStrictMode: false → no strict key (base-url test intent).
        let m = model(json!({
            "reasoning": false,
            "compat": {"supportsStrictMode": false}
        }));
        let params = params_for(&m, &ctx, &AzureOpenAIResponsesOptions::default());
        assert!(params["tools"][0].get("strict").is_none());

        // No tools → no tools key.
        let ctx = common::context(vec![common::user_text("hi")], None);
        let params = params_for(&m, &ctx, &AzureOpenAIResponsesOptions::default());
        assert!(params.get("tools").is_none());
    }

    // -- stream_simple ------------------------------------------------------------

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
                    Some("No API key for provider: azure-openai-responses")
                );
            }
            other => panic!("expected error event, got {other:?}"),
        }
    }
}
