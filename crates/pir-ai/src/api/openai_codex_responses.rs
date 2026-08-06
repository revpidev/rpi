//! Port of `packages/ai/src/api/openai-codex-responses.ts` @ pi 0.82.1 (2efa728).
//!
//! OpenAI Codex Responses adapter: ChatGPT-backend transport with a WebSocket
//! subsystem (connection cache with 5min/55min dual TTL, per-session permanent
//! SSE fallback, two one-shot retry rules, cached-context continuation with
//! input deltas) and an SSE fallback path with zstd-compressed request bodies.
//! Stream events flow through the shared [`ResponsesStreamProcessor`]; the
//! WebSocket subsystem lives in [`crate::api::codex_ws`] (state machine
//! documented there, design §13).
//!
//! Intentional differences (upstream deviations, D-027 family):
//! - HTTP is a direct reqwest call, not `fetch`; the SSE body timeout surfaces
//!   as `Request was aborted` (upstream surfaces Node's `AbortError` message)
//!   and there is no `combinedSignal` machinery — the deadline and the user
//!   cancellation token are selected directly.
//! - zstd compression always applies (upstream falls back to the uncompressed
//!   body only on browser runtimes without `node:zlib`).
//! - The JWT payload is decoded trying standard and URL-safe base64 alphabets
//!   (upstream `atob` only accepts the standard alphabet; real ChatGPT tokens
//!   are URL-safe).
//! - `User-Agent` uses `std::env::consts` plus `libc::uname` for the kernel
//!   release (upstream `node:os`).
//! - SSE `data:` lines keep the shared [`SseDecoder`] semantics (one leading
//!   space stripped), like every other pir adapter (D-005 family).
//! - `stream_simple` reports a missing API key as a stream error event instead
//!   of throwing synchronously (matches the other pir adapters).
//! - `openai-codex-responses.lazy.ts` has no Rust counterpart: pir-ai is a
//!   statically linked crate, so `lazyApi` dynamic imports do not exist
//!   (consistent with D-021..D-026).

use std::collections::{BTreeMap, HashMap};
use std::sync::LazyLock;
use std::time::{Duration, SystemTime};

use futures::StreamExt;
use regex::Regex;
use serde_json::{json, Map, Value};

use crate::api::codex_ws::cache as ws_cache;
use crate::api::codex_ws::{self, CodexError};
use crate::api::constrained_sampling::create_grammar_tool_input_properties;
use crate::api::lazy::immediate_error_stream;
use crate::api::openai_prompt_cache::clamp_openai_prompt_cache_key;
use crate::api::openai_responses::{apply_service_tier_pricing, OPENAI_TOOL_CALL_PROVIDERS};
use crate::api::openai_responses_shared::{
    convert_responses_messages, convert_responses_tools, ConvertResponsesMessagesOptions,
    ConvertResponsesToolsOptions, ResponsesStreamOptions, ResponsesStreamProcessor,
};
use crate::api::simple_options::build_base_options;
use crate::api::sse::SseDecoder;
use crate::models::{clamp_thinking_level, ProviderStreams};
use crate::types::{
    AssistantMessage, AssistantMessageDiagnostic, CacheRetention, Context, DiagnosticErrorInfo,
    DoneReason, ErrorReason, Message, Model, ModelThinkingLevel, NumberOrString, ProviderHeaders,
    ProviderResponse, SimpleStreamOptions, StopReason, StreamEvent, StreamOptions, Transport,
    Usage,
};
use crate::utils::deferred_tools::split_deferred_tools_identity;
use crate::utils::event_stream::AssistantMessageEventStream;
use crate::utils::headers::headers_to_record;
use crate::utils::uuid::uuidv7;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// `DEFAULT_CODEX_BASE_URL`.
pub const DEFAULT_CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api";
/// `JWT_CLAIM_PATH`.
const JWT_CLAIM_PATH: &str = "https://api.openai.com/auth";
/// `DEFAULT_MAX_RETRIES`.
const DEFAULT_MAX_RETRIES: u32 = 0;
/// `BASE_DELAY_MS`.
const BASE_DELAY_MS: u64 = 1000;
/// `DEFAULT_MAX_RETRY_DELAY_MS`.
const DEFAULT_MAX_RETRY_DELAY_MS: u64 = 60_000;
/// `REQUEST_COMPRESSION_ZSTD_LEVEL` — the Codex backend accepts zstd-compressed
/// request bodies on the SSE responses endpoint (the same endpoint the
/// official Codex client compresses against).
const REQUEST_COMPRESSION_ZSTD_LEVEL: i32 = 3;
/// `OPENAI_BETA_RESPONSES_WEBSOCKETS`.
const OPENAI_BETA_RESPONSES_WEBSOCKETS: &str = "responses_websockets=2026-02-06";

/// `CODEX_RESPONSE_STATUSES`. (`CODEX_TOOL_CALL_PROVIDERS` has membership
/// identical to [`OPENAI_TOOL_CALL_PROVIDERS`] and reuses it.)
const CODEX_RESPONSE_STATUSES: [&str; 6] = [
    "completed",
    "incomplete",
    "failed",
    "cancelled",
    "queued",
    "in_progress",
];

// ---------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------

/// `OpenAICodexResponsesOptions` — `StreamOptions` plus codex-specific extras.
/// `reasoning_effort` keeps the upstream literal union (`"none" | "minimal" |
/// "low" | "medium" | "high" | "xhigh" | "max"`); `tool_choice` is
/// `"auto" | "none" | "required"`.
#[derive(Debug, Clone, Default)]
pub struct OpenAiCodexResponsesOptions {
    pub stream: StreamOptions,
    pub reasoning_effort: Option<String>,
    pub reasoning_summary: Option<String>,
    pub service_tier: Option<String>,
    pub text_verbosity: Option<String>,
    pub tool_choice: Option<String>,
}

// ---------------------------------------------------------------------------
// Retry helpers
// ---------------------------------------------------------------------------

static TERMINAL_RATE_LIMIT_RE: LazyLock<Regex> = LazyLock::new(|| {
    // invariant: literal pattern compiles
    Regex::new(
        r"(?i)GoUsageLimitError|FreeUsageLimitError|Monthly usage limit reached|available balance|insufficient_quota|out of budget|quota exceeded|billing",
    )
    .unwrap()
});
static RETRYABLE_TEXT_RE: LazyLock<Regex> = LazyLock::new(|| {
    // invariant: literal pattern compiles
    Regex::new(
        r"(?i)rate.?limit|overloaded|service.?unavailable|upstream.?connect|connection.?refused",
    )
    .unwrap()
});

/// `isTerminalRateLimitError`.
fn is_terminal_rate_limit_error(error_text: &str) -> bool {
    TERMINAL_RATE_LIMIT_RE.is_match(error_text)
}

/// `isRetryableError`.
fn is_retryable_error(status: u16, error_text: &str) -> bool {
    if status == 429 && is_terminal_rate_limit_error(error_text) {
        return false;
    }
    if matches!(status, 429 | 500 | 502 | 503 | 504) {
        return true;
    }
    RETRYABLE_TEXT_RE.is_match(error_text)
}

/// `getRetryAfterDelayMs`.
fn get_retry_after_delay_ms(headers: &HashMap<String, String>) -> Option<u64> {
    if let Some(retry_after_ms) = headers.get("retry-after-ms") {
        if let Ok(millis) = retry_after_ms.parse::<f64>() {
            if millis.is_finite() {
                return Some(millis.max(0.0) as u64);
            }
        }
    }

    let retry_after = headers.get("retry-after")?;
    if let Ok(seconds) = retry_after.parse::<f64>() {
        if seconds.is_finite() {
            return Some((seconds * 1000.0).max(0.0) as u64);
        }
    }

    if let Ok(date) = httpdate::parse_http_date(retry_after) {
        let delay = date
            .duration_since(SystemTime::now())
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or(0);
        return Some(delay);
    }

    None
}

/// `validateRetryDelayMs`.
fn validate_retry_delay_ms(delay_ms: u64, options: &StreamOptions) -> Result<u64, CodexError> {
    let max_retry_delay_ms = options
        .max_retry_delay_ms
        .unwrap_or(DEFAULT_MAX_RETRY_DELAY_MS);
    if max_retry_delay_ms > 0 && delay_ms > max_retry_delay_ms {
        return Err(CodexError::RetryDelayExceeded(format!(
            "Server requested {}s retry delay (max: {}s)",
            delay_ms.div_ceil(1000),
            max_retry_delay_ms.div_ceil(1000)
        )));
    }
    Ok(delay_ms)
}

/// `sleep(ms, signal)`.
async fn sleep_abortable(
    ms: u64,
    signal: Option<&tokio_util::sync::CancellationToken>,
) -> Result<(), CodexError> {
    let Some(signal) = signal else {
        tokio::time::sleep(Duration::from_millis(ms)).await;
        return Ok(());
    };
    if signal.is_cancelled() {
        return Err(CodexError::Aborted);
    }
    tokio::select! {
        () = tokio::time::sleep(Duration::from_millis(ms)) => Ok(()),
        () = signal.cancelled() => Err(CodexError::Aborted),
    }
}

// ---------------------------------------------------------------------------
// Request compression
// ---------------------------------------------------------------------------

/// `compressRequestBodyZstd`. Always available in Rust; `Option` kept to
/// mirror the upstream shape (returns `None` only on compression failure).
fn compress_request_body_zstd(body_json: &str) -> Option<Vec<u8>> {
    zstd::bulk::compress(body_json.as_bytes(), REQUEST_COMPRESSION_ZSTD_LEVEL).ok()
}

// ---------------------------------------------------------------------------
// URL resolution
// ---------------------------------------------------------------------------

/// `resolveCodexUrl`.
pub fn resolve_codex_url(base_url: Option<&str>) -> String {
    let raw = base_url
        .filter(|url| !url.trim().is_empty())
        .unwrap_or(DEFAULT_CODEX_BASE_URL);
    let normalized = raw.trim_end_matches('/');
    if normalized.ends_with("/codex/responses") {
        return normalized.to_owned();
    }
    if let Some(prefix) = normalized.strip_suffix("/codex") {
        return format!("{prefix}/codex/responses");
    }
    format!("{normalized}/codex/responses")
}

/// `resolveCodexWebSocketUrl`.
pub fn resolve_codex_websocket_url(base_url: Option<&str>) -> String {
    let url = resolve_codex_url(base_url);
    if let Some(rest) = url.strip_prefix("https:") {
        format!("wss:{rest}")
    } else if let Some(rest) = url.strip_prefix("http:") {
        format!("ws:{rest}")
    } else {
        url
    }
}

// ---------------------------------------------------------------------------
// Auth & headers
// ---------------------------------------------------------------------------

/// Decodes a JWT payload segment. Upstream uses `atob` (standard alphabet);
/// real ChatGPT tokens are base64url, so both alphabets are tried (padded and
/// unpadded).
fn decode_jwt_payload(segment: &str) -> Option<Vec<u8>> {
    use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
    use base64::Engine;
    for engine in [&STANDARD, &STANDARD_NO_PAD, &URL_SAFE, &URL_SAFE_NO_PAD] {
        if let Ok(bytes) = engine.decode(segment) {
            return Some(bytes);
        }
    }
    None
}

/// `extractAccountId`: every failure surfaces as
/// `Failed to extract accountId from token`.
fn extract_account_id(token: &str) -> Result<String, CodexError> {
    fn fail() -> CodexError {
        CodexError::Other("Failed to extract accountId from token".to_owned())
    }
    let mut parts = token.split('.');
    let (Some(_header), Some(payload), Some(_signature), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(fail());
    };
    let bytes = decode_jwt_payload(payload).ok_or_else(fail)?;
    let json: Value = serde_json::from_slice(&bytes).map_err(|_| fail())?;
    let account_id = json
        .get(JWT_CLAIM_PATH)
        .and_then(|claim| claim.get("chatgpt_account_id"))
        .and_then(Value::as_str)
        .filter(|account_id| !account_id.is_empty())
        .ok_or_else(fail)?;
    Ok(account_id.to_owned())
}

/// `pi (${os.platform()} ${os.release()}; ${os.arch()})`.
fn user_agent() -> String {
    format!(
        "pi ({} {}; {})",
        std::env::consts::OS,
        os_release(),
        std::env::consts::ARCH
    )
}

#[cfg(unix)]
fn os_release() -> String {
    // SAFETY: `utsname` is a plain C struct; zeroed is a valid initial state
    // and `uname` writes NUL-terminated arrays on success.
    unsafe {
        let mut uts: libc::utsname = std::mem::zeroed();
        if libc::uname(&mut uts) == 0 {
            return std::ffi::CStr::from_ptr(uts.release.as_ptr())
                .to_string_lossy()
                .into_owned();
        }
        String::new()
    }
}

#[cfg(not(unix))]
fn os_release() -> String {
    String::new()
}

/// Case-insensitive header record (JS `Headers` lowercases all names).
type HeaderRecord = BTreeMap<String, String>;

/// `buildBaseCodexHeaders`: model headers, then option headers (`None`
/// deletes), then the auth/account/originator/UA headers always win.
fn build_base_codex_headers(
    model: &Model,
    additional_headers: Option<&ProviderHeaders>,
    account_id: &str,
    token: &str,
) -> HeaderRecord {
    let mut headers = HeaderRecord::new();
    if let Some(model_headers) = &model.headers {
        for (key, value) in model_headers {
            headers.insert(key.to_lowercase(), value.clone());
        }
    }
    for (key, value) in additional_headers.into_iter().flatten() {
        match value {
            Some(value) => headers.insert(key.to_lowercase(), value.clone()),
            None => headers.remove(&key.to_lowercase()),
        };
    }
    headers.insert("authorization".to_owned(), format!("Bearer {token}"));
    headers.insert("chatgpt-account-id".to_owned(), account_id.to_owned());
    // Literal "pi" (upstream openai-codex-responses.ts:1593); do not rename.
    headers.insert("originator".to_owned(), "pi".to_owned());
    headers.insert("user-agent".to_owned(), user_agent());
    headers
}

/// `buildSSEHeaders`.
fn build_sse_headers(
    model: &Model,
    additional_headers: Option<&ProviderHeaders>,
    account_id: &str,
    token: &str,
    session_id: Option<&str>,
) -> HeaderRecord {
    let mut headers = build_base_codex_headers(model, additional_headers, account_id, token);
    headers.insert(
        "openai-beta".to_owned(),
        "responses=experimental".to_owned(),
    );
    headers.insert("accept".to_owned(), "text/event-stream".to_owned());
    headers.insert("content-type".to_owned(), "application/json".to_owned());
    if let Some(session_id) = session_id {
        headers.insert("session-id".to_owned(), session_id.to_owned());
        headers.insert("x-client-request-id".to_owned(), session_id.to_owned());
    }
    headers
}

/// `buildWebSocketHeaders`. Upstream's `delete wsHeaders["OpenAI-Beta"]` in
/// `connectWebSocket` is a case-mismatched no-op (`headersToRecord` lowercases
/// keys), so the `openai-beta` header below **is** sent in the handshake —
/// kept bug-compatible.
fn build_websocket_headers(
    model: &Model,
    additional_headers: Option<&ProviderHeaders>,
    account_id: &str,
    token: &str,
    request_id: &str,
) -> HeaderRecord {
    let mut headers = build_base_codex_headers(model, additional_headers, account_id, token);
    headers.remove("accept");
    headers.remove("content-type");
    headers.remove("openai-beta");
    headers.insert(
        "openai-beta".to_owned(),
        OPENAI_BETA_RESPONSES_WEBSOCKETS.to_owned(),
    );
    headers.insert("x-client-request-id".to_owned(), request_id.to_owned());
    headers.insert("session-id".to_owned(), request_id.to_owned());
    headers
}

fn header_record_to_map(record: &HeaderRecord) -> Result<reqwest::header::HeaderMap, String> {
    let mut map = reqwest::header::HeaderMap::new();
    for (name, value) in record {
        let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
            .map_err(|error| format!("Invalid header name {name:?}: {error}"))?;
        let value = reqwest::header::HeaderValue::from_str(value)
            .map_err(|error| format!("Invalid header value for {name}: {error}"))?;
        map.append(name, value);
    }
    Ok(map)
}

// ---------------------------------------------------------------------------
// Request building
// ---------------------------------------------------------------------------

/// `buildRequestBody`. Key insertion order mirrors the upstream object literal
/// plus conditional assignments (serde_json `preserve_order`).
pub fn build_request_body(
    model: &Model,
    context: &Context,
    options: &OpenAiCodexResponsesOptions,
    cache_session_id: Option<&str>,
    grammar_tool_input_properties: &HashMap<String, String>,
) -> Result<Value, String> {
    let compat = model.compat.as_ref();
    let supports_strict_mode = compat
        .and_then(|compat| compat.supports_strict_mode)
        .unwrap_or(true);
    let supports_open_ai_grammar_tools = compat
        .and_then(|compat| compat.supports_open_ai_grammar_tools)
        .unwrap_or(false);
    let supports_tool_search = compat
        .and_then(|compat| compat.supports_tool_search)
        .unwrap_or(false);
    let tool_placement = split_deferred_tools_identity(context, supports_tool_search);
    let tool_options = ConvertResponsesToolsOptions {
        strict: None,
        supports_strict_mode: Some(supports_strict_mode),
        supports_open_ai_grammar_tools: Some(supports_open_ai_grammar_tools),
        defer_loading: false,
    };
    let messages = convert_responses_messages(
        model,
        context,
        &OPENAI_TOOL_CALL_PROVIDERS,
        &ConvertResponsesMessagesOptions {
            include_system_prompt: false,
            grammar_tool_input_properties: Some(grammar_tool_input_properties),
            deferred_tools: Some(&tool_placement.deferred),
            tool_options: tool_options.clone(),
        },
    )?;

    // JS `context.systemPrompt || "You are a helpful assistant."`.
    let instructions = context
        .system_prompt
        .as_deref()
        .filter(|prompt| !prompt.is_empty())
        .unwrap_or("You are a helpful assistant.");
    // JS `options?.textVerbosity || "low"`.
    let verbosity = options
        .text_verbosity
        .as_deref()
        .filter(|verbosity| !verbosity.is_empty())
        .unwrap_or("low");

    let mut body = Map::new();
    body.insert("model".to_owned(), json!(model.id));
    body.insert("store".to_owned(), json!(false));
    body.insert("stream".to_owned(), json!(true));
    body.insert("instructions".to_owned(), json!(instructions));
    body.insert("input".to_owned(), json!(messages));
    body.insert("text".to_owned(), json!({ "verbosity": verbosity }));
    body.insert("include".to_owned(), json!(["reasoning.encrypted_content"]));
    if let Some(cache_session_id) = cache_session_id {
        body.insert("prompt_cache_key".to_owned(), json!(cache_session_id));
    }
    body.insert(
        "tool_choice".to_owned(),
        json!(options.tool_choice.as_deref().unwrap_or("auto")),
    );
    body.insert("parallel_tool_calls".to_owned(), json!(true));
    let mut body = Value::Object(body);

    if let Some(temperature) = options.stream.temperature {
        body["temperature"] = json!(temperature);
    }
    if let Some(service_tier) = &options.service_tier {
        body["service_tier"] = json!(service_tier);
    }
    if !tool_placement.immediate.is_empty() {
        body["tools"] = json!(convert_responses_tools(
            &tool_placement.immediate,
            &tool_options,
        )?);
    }

    if let Some(reasoning_effort) = &options.reasoning_effort {
        let effort = if reasoning_effort == "none" {
            // `model.thinkingLevelMap?.off ?? "none"`
            model
                .thinking_level_map
                .as_ref()
                .and_then(|map| map.get(&ModelThinkingLevel::Off))
                .cloned()
                .flatten()
                .unwrap_or_else(|| "none".to_owned())
        } else {
            // `model.thinkingLevelMap?.[effort] ?? effort` (a null mapped value
            // falls back to the level name, matching JS `??`).
            let level = serde_json::from_value::<ModelThinkingLevel>(json!(reasoning_effort)).ok();
            level
                .and_then(|level| {
                    model
                        .thinking_level_map
                        .as_ref()
                        .and_then(|map| map.get(&level))
                        .cloned()
                        .flatten()
                })
                .unwrap_or_else(|| reasoning_effort.clone())
        };
        body["reasoning"] = json!({
            "effort": effort,
            "summary": options.reasoning_summary.as_deref().unwrap_or("auto"),
        });
    }

    Ok(body)
}

// ---------------------------------------------------------------------------
// Service tier
// ---------------------------------------------------------------------------

/// `resolveCodexServiceTier`: a `"default"` response tier resolves to the
/// requested flex/priority tier.
pub fn resolve_codex_service_tier(
    response_service_tier: Option<&str>,
    request_service_tier: Option<&str>,
) -> Option<String> {
    if response_service_tier == Some("default")
        && matches!(request_service_tier, Some("flex") | Some("priority"))
    {
        return request_service_tier.map(str::to_owned);
    }
    response_service_tier
        .or(request_service_tier)
        .map(str::to_owned)
}

// ---------------------------------------------------------------------------
// Codex event mapping
// ---------------------------------------------------------------------------

/// `extractCodexEventError`.
fn extract_codex_event_error(event: &Value) -> (Option<String>, Option<String>) {
    let nested = event.get("error").filter(|error| error.is_object());
    let code = event.get("code").and_then(Value::as_str).or_else(|| {
        nested
            .and_then(|error| error.get("code"))
            .and_then(Value::as_str)
    });
    let message = event.get("message").and_then(Value::as_str).or_else(|| {
        nested
            .and_then(|error| error.get("message"))
            .and_then(Value::as_str)
    });
    (code.map(str::to_owned), message.map(str::to_owned))
}

/// Terminal response events (`response.done` / `.completed` / `.incomplete`)
/// are normalized to `response.completed`; [`is_terminal_event`] reports them.
fn is_terminal_event(event: &Value) -> bool {
    matches!(
        event.get("type").and_then(Value::as_str),
        Some("response.completed")
    )
}

/// One iteration of the upstream `mapCodexEvents` generator: `Ok(Some(event))`
/// to forward (terminal events normalized to `response.completed`),
/// `Ok(None)` to skip, `Err` for `error` / `response.failed` events.
fn map_codex_event(event: &Value) -> Result<Option<Value>, CodexError> {
    let Some(event_type) = event.get("type").and_then(Value::as_str) else {
        return Ok(None);
    };

    if event_type == "error" {
        let (code, message) = extract_codex_event_error(event);
        return Err(CodexError::Api {
            message: format!(
                "Codex error: {}",
                message
                    .or(code.clone())
                    .unwrap_or_else(|| event.to_string())
            ),
            code,
        });
    }

    if event_type == "response.failed" {
        let error = event
            .get("response")
            .and_then(|response| response.get("error"));
        let code = error
            .and_then(|error| error.get("code"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        let message = error
            .and_then(|error| error.get("message"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        return Err(CodexError::Api {
            message: message.unwrap_or_else(|| "Codex response failed".to_owned()),
            code,
        });
    }

    if matches!(
        event_type,
        "response.done" | "response.completed" | "response.incomplete"
    ) {
        let mut mapped = event.clone();
        if let Some(object) = mapped.as_object_mut() {
            object.insert("type".to_owned(), json!("response.completed"));
            if let Some(response) = object.get_mut("response").and_then(Value::as_object_mut) {
                // `normalizeCodexStatus`: unknown statuses become undefined
                // (the key disappears), valid ones pass through.
                let status = response.get("status").and_then(Value::as_str);
                match status {
                    Some(status) if CODEX_RESPONSE_STATUSES.contains(&status) => {}
                    _ => {
                        response.shift_remove("status");
                    }
                }
            }
        }
        return Ok(Some(mapped));
    }

    Ok(Some(event.clone()))
}

// ---------------------------------------------------------------------------
// Cached WebSocket input deltas
// ---------------------------------------------------------------------------

/// `requestBodyWithoutInput`.
fn request_body_without_input(body: &Value) -> Value {
    let mut rest = body.as_object().cloned().unwrap_or_default();
    rest.shift_remove("input");
    rest.shift_remove("previous_response_id");
    Value::Object(rest)
}

/// `responseInputsEqual`: JSON.stringify comparison. The string comparison is
/// load-bearing: serde_json `Value` equality is map-order-insensitive, while
/// `JSON.stringify` is insertion-order-sensitive (serde_json `preserve_order`
/// serializes like JS objects).
#[allow(clippy::cmp_owned)]
fn response_inputs_equal(a: Option<&Value>, b: Option<&Value>) -> bool {
    let default = Value::Array(Vec::new());
    let a = a.unwrap_or(&default);
    let b = b.unwrap_or(&default);
    a.to_string() == b.to_string()
}

/// `requestBodiesMatchExceptInput`.
#[allow(clippy::cmp_owned)]
fn request_bodies_match_except_input(a: &Value, b: &Value) -> bool {
    request_body_without_input(a).to_string() == request_body_without_input(b).to_string()
}

/// `getCachedWebSocketInputDelta`: baseline prefix check, then the delta.
fn get_cached_websocket_input_delta(
    body: &Value,
    continuation: &ws_cache::CachedWebSocketContinuationState,
) -> Option<Vec<Value>> {
    if !request_bodies_match_except_input(body, &continuation.last_request_body) {
        return None;
    }

    let empty = Vec::new();
    let current_input = body
        .get("input")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    let mut baseline: Vec<Value> = continuation
        .last_request_body
        .get("input")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    baseline.extend(continuation.last_response_items.iter().cloned());
    if current_input.len() < baseline.len() {
        return None;
    }

    let prefix = &current_input[..baseline.len()];
    if !response_inputs_equal(
        Some(&Value::Array(prefix.to_vec())),
        Some(&Value::Array(baseline.clone())),
    ) {
        return None;
    }

    Some(current_input[baseline.len()..].to_vec())
}

/// `buildCachedWebSocketRequestBody`: on baseline mismatch or a missing
/// response id the continuation is cleared and the full body is sent.
fn build_cached_websocket_request_body(session_id: &str, body: &Value) -> Value {
    let Some(continuation) = ws_cache::continuation_for(session_id) else {
        return body.clone();
    };
    let delta = get_cached_websocket_input_delta(body, &continuation);
    match delta {
        Some(delta) if !continuation.last_response_id.is_empty() => {
            let mut request = body.as_object().cloned().unwrap_or_default();
            // JS `{...body, previous_response_id, input}`: `input` keeps its
            // position, `previous_response_id` is appended.
            request.insert("input".to_owned(), json!(delta));
            request.insert(
                "previous_response_id".to_owned(),
                json!(continuation.last_response_id),
            );
            Value::Object(request)
        }
        _ => {
            ws_cache::set_continuation(session_id, None);
            body.clone()
        }
    }
}

// ---------------------------------------------------------------------------
// Error parsing
// ---------------------------------------------------------------------------

/// `parseErrorResponse` result.
struct ParsedErrorResponse {
    message: String,
    friendly_message: Option<String>,
}

/// `parseErrorResponse`.
fn parse_error_response(status: u16, status_text: Option<&str>, raw: &str) -> ParsedErrorResponse {
    let mut message = if raw.is_empty() {
        status_text.unwrap_or("Request failed").to_owned()
    } else {
        raw.to_owned()
    };
    if raw.is_empty() && status_text.is_none() {
        message = "Request failed".to_owned();
    }
    let mut friendly_message: Option<String> = None;

    if let Ok(parsed) = serde_json::from_str::<Value>(raw) {
        if let Some(error) = parsed.get("error").filter(|error| error.is_object()) {
            let code = error
                .get("code")
                .and_then(Value::as_str)
                .or_else(|| error.get("type").and_then(Value::as_str))
                .unwrap_or("");
            static USAGE_LIMIT_RE: LazyLock<Regex> = LazyLock::new(|| {
                // invariant: literal pattern compiles
                Regex::new(r"(?i)usage_limit_reached|usage_not_included|rate_limit_exceeded")
                    .unwrap()
            });
            if USAGE_LIMIT_RE.is_match(code) || status == 429 {
                let plan = error
                    .get("plan_type")
                    .and_then(Value::as_str)
                    .map(|plan| format!(" ({} plan)", plan.to_lowercase()))
                    .unwrap_or_default();
                let minutes = error
                    .get("resets_at")
                    .and_then(Value::as_f64)
                    .map(|resets_at| {
                        let now_ms = SystemTime::now()
                            .duration_since(SystemTime::UNIX_EPOCH)
                            .map(|duration| duration.as_millis() as f64)
                            .unwrap_or(0.0);
                        ((resets_at * 1000.0 - now_ms) / 60000.0).round().max(0.0) as u64
                    });
                let when = minutes
                    .map(|minutes| format!(" Try again in ~{minutes} min."))
                    .unwrap_or_default();
                friendly_message = Some(
                    format!("You have hit your ChatGPT usage limit{plan}.{when}")
                        .trim()
                        .to_owned(),
                );
            }
            if let Some(error_message) = error.get("message").and_then(Value::as_str) {
                message = error_message.to_owned();
            } else if let Some(friendly) = &friendly_message {
                message = friendly.clone();
            }
        }
    }

    ParsedErrorResponse {
        message,
        friendly_message,
    }
}

// ---------------------------------------------------------------------------
// Output helpers
// ---------------------------------------------------------------------------

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
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
    }
}

/// `assertSuccessfulOutput`.
fn assert_successful_output(output: &AssistantMessage) -> Result<(), CodexError> {
    match output.stop_reason {
        StopReason::Pending => Err(CodexError::Other(
            "Codex stream ended without a stop reason".to_owned(),
        )),
        StopReason::Error | StopReason::Aborted => Err(CodexError::Other(
            output
                .error_message
                .clone()
                .unwrap_or_else(|| "An unknown error occurred".to_owned()),
        )),
        _ => Ok(()),
    }
}

fn done_reason(output: &AssistantMessage) -> DoneReason {
    match output.stop_reason {
        StopReason::Length => DoneReason::Length,
        StopReason::ToolUse => DoneReason::ToolUse,
        _ => DoneReason::Stop,
    }
}

fn transport_name(transport: Transport) -> &'static str {
    match transport {
        Transport::Sse => "sse",
        Transport::Websocket => "websocket",
        Transport::WebsocketCached => "websocket-cached",
        Transport::Auto => "auto",
    }
}

/// `createAssistantMessageDiagnostic("provider_transport_failure", ...)` +
/// `appendAssistantMessageDiagnostic`.
fn append_transport_failure_diagnostic(
    output: &mut AssistantMessage,
    error: &CodexError,
    transport: Transport,
    websocket_started: bool,
    request_bytes: usize,
) {
    let mut details = Map::new();
    details.insert(
        "configuredTransport".to_owned(),
        json!(transport_name(transport)),
    );
    if !websocket_started {
        details.insert("fallbackTransport".to_owned(), json!("sse"));
    }
    details.insert("eventsEmitted".to_owned(), json!(websocket_started));
    details.insert(
        "phase".to_owned(),
        json!(if websocket_started {
            "after_message_stream_start"
        } else {
            "before_message_stream_start"
        }),
    );
    details.insert("requestBytes".to_owned(), json!(request_bytes));
    let code = match error {
        CodexError::Api {
            code: Some(code), ..
        } => Some(NumberOrString::String(code.clone())),
        _ => None,
    };
    let diagnostic = AssistantMessageDiagnostic {
        kind: "provider_transport_failure".to_owned(),
        timestamp: now_ms(),
        error: Some(DiagnosticErrorInfo {
            name: Some(error.error_name().to_owned()),
            message: error.message(),
            stack: None,
            code,
        }),
        details: Some(details),
    };
    output
        .diagnostics
        .get_or_insert_with(Vec::new)
        .push(diagnostic);
}

// ---------------------------------------------------------------------------
// WebSocket request processing
// ---------------------------------------------------------------------------

/// `processWebSocketStream`. `websocket_started` / `start_emitted` replace the
/// upstream `onStart` closure (a closure cannot borrow `output` while the
/// processor holds it mutably); the start event pushes a snapshot of `output`
/// taken before the processor starts, which is exactly what upstream's
/// `onStart` observes (it runs before the first `handle_event`).
#[allow(clippy::too_many_arguments)]
async fn process_websocket_stream(
    url: &str,
    body: &Value,
    headers: &reqwest::header::HeaderMap,
    output: &mut AssistantMessage,
    events: &AssistantMessageEventStream,
    model: &Model,
    websocket_started: &mut bool,
    start_emitted: &mut bool,
    idle_timeout_ms: Option<u64>,
    websocket_connect_timeout_ms: Option<u64>,
    cache_session_id: Option<&str>,
    grammar_tool_input_properties: &HashMap<String, String>,
    options: &OpenAiCodexResponsesOptions,
) -> Result<(), CodexError> {
    let ws_cache::AcquiredWebSocket {
        mut socket,
        pending,
        reused,
        cache_key,
    } = ws_cache::acquire(
        cache_session_id,
        url,
        headers,
        websocket_connect_timeout_ms,
        options.stream.signal.as_ref(),
    )
    .await?;
    let use_cached_context = matches!(
        options.stream.transport,
        Some(Transport::WebsocketCached) | Some(Transport::Auto)
    );
    // ChatGPT Codex Responses rejects `store: true`; WebSocket continuation
    // still works via connection-scoped previous_response_id state.
    let full_body = body.clone();
    let request_body = match (use_cached_context, &cache_key) {
        (true, Some(session_id)) => build_cached_websocket_request_body(session_id, &full_body),
        _ => full_body.clone(),
    };

    if let Some(session_id) = cache_session_id {
        ws_cache::with_debug_stats(session_id, |stats| {
            stats.requests += 1;
            if reused {
                stats.connections_reused += 1;
            } else {
                stats.connections_created += 1;
            }
            if use_cached_context {
                stats.cached_context_requests += 1;
            }
            if request_body.get("store") == Some(&Value::Bool(true)) {
                stats.store_true_requests += 1;
            }
            let input_items = request_body
                .get("input")
                .and_then(Value::as_array)
                .map(|input| input.len() as u64)
                .unwrap_or(0);
            stats.last_input_items = input_items;
            if let Some(previous_response_id) = request_body
                .get("previous_response_id")
                .and_then(Value::as_str)
            {
                stats.delta_requests += 1;
                stats.last_delta_input_items = Some(input_items);
                stats.last_previous_response_id = Some(previous_response_id.to_owned());
            } else {
                stats.full_context_requests += 1;
                stats.last_delta_input_items = None;
                stats.last_previous_response_id = None;
            }
        });
    }

    let mut keep_connection = true;

    let result: Result<(), CodexError> = async {
        codex_ws::send_request(&mut socket, &request_body).await?;

        let model_id = model.id.clone();
        let mut started = false;
        let start_partial = output.clone();
        {
            let mut processor = ResponsesStreamProcessor::new(
                &mut *output,
                model,
                ResponsesStreamOptions {
                    service_tier: options.service_tier.as_deref(),
                    grammar_tool_input_properties,
                    apply_service_tier_pricing: Some(Box::new(move |usage: &mut Usage, tier| {
                        apply_service_tier_pricing(usage, tier, &model_id);
                    })),
                    resolve_service_tier: Some(resolve_codex_service_tier),
                },
            );
            let mut on_event = |event: Value| -> Result<bool, CodexError> {
                let Some(mapped) = map_codex_event(&event)? else {
                    return Ok(false);
                };
                // `startWebSocketOutputOnFirstEvent`: the start event fires on
                // the first mapped event, before the processor sees it.
                if !started {
                    started = true;
                    *websocket_started = true;
                    if !*start_emitted {
                        *start_emitted = true;
                        events.push(StreamEvent::Start {
                            partial: start_partial.clone(),
                        });
                    }
                }
                let terminal = is_terminal_event(&mapped);
                processor
                    .handle_event(&mapped, events)
                    .map_err(CodexError::Other)?;
                Ok(terminal)
            };
            codex_ws::read_events(
                &mut socket,
                pending,
                options.stream.signal.as_ref(),
                idle_timeout_ms,
                &mut on_event,
            )
            .await?;
        }

        if options
            .stream
            .signal
            .as_ref()
            .is_some_and(|signal| signal.is_cancelled())
        {
            keep_connection = false;
        } else if use_cached_context && cache_key.is_some() && output.response_id.is_some() {
            // invariant: checked is_some() above
            let session_id = cache_key.clone().unwrap();
            let response_context = Context {
                system_prompt: None,
                messages: vec![Message::Assistant(output.clone())],
                tools: None,
            };
            let response_items = convert_responses_messages(
                model,
                &response_context,
                &OPENAI_TOOL_CALL_PROVIDERS,
                &ConvertResponsesMessagesOptions {
                    include_system_prompt: false,
                    grammar_tool_input_properties: Some(grammar_tool_input_properties),
                    ..ConvertResponsesMessagesOptions::default()
                },
            )
            .map_err(CodexError::Other)?
            .into_iter()
            .filter(|item| {
                !matches!(
                    item.get("type").and_then(Value::as_str),
                    Some("function_call_output") | Some("custom_tool_call_output")
                )
            })
            .collect();
            ws_cache::set_continuation(
                &session_id,
                Some(ws_cache::CachedWebSocketContinuationState {
                    last_request_body: full_body.clone(),
                    // invariant: checked is_some() above
                    last_response_id: output.response_id.clone().unwrap(),
                    last_response_items: response_items,
                }),
            );
        }
        Ok(())
    }
    .await;

    if result.is_err() {
        if let Some(session_id) = &cache_key {
            ws_cache::set_continuation(session_id, None);
        }
        keep_connection = false;
    }
    ws_cache::release(cache_key, socket, None, keep_connection).await;
    result
}

// ---------------------------------------------------------------------------
// Main stream flow
// ---------------------------------------------------------------------------

/// The streaming body: everything that runs inside upstream's async IIFE.
async fn run(
    model: &Model,
    context: &Context,
    options: &OpenAiCodexResponsesOptions,
    output: &mut AssistantMessage,
    events: &AssistantMessageEventStream,
) -> Result<DoneReason, CodexError> {
    let signal = options.stream.signal.clone();
    let aborted = || signal.as_ref().is_some_and(|signal| signal.is_cancelled());

    let api_key =
        options.stream.api_key.clone().ok_or_else(|| {
            CodexError::Other(format!("No API key for provider: {}", model.provider))
        })?;
    let account_id = extract_account_id(&api_key)?;
    let compat = model.compat.as_ref();
    let grammar_tool_input_properties = create_grammar_tool_input_properties(
        context.tools.as_deref(),
        compat
            .and_then(|compat| compat.supports_open_ai_grammar_tools)
            .unwrap_or(false),
    )
    .map_err(CodexError::Other)?;
    let cache_session_id = if options.stream.cache_retention == Some(CacheRetention::None) {
        None
    } else {
        options.stream.session_id.as_deref()
    };
    let codex_session_id = clamp_openai_prompt_cache_key(cache_session_id);
    let mut body = build_request_body(
        model,
        context,
        options,
        codex_session_id.as_deref(),
        &grammar_tool_input_properties,
    )
    .map_err(CodexError::Other)?;
    if let Some(on_payload) = &options.stream.on_payload {
        if let Some(next_body) = on_payload(body.clone(), model).await {
            body = next_body;
        }
    }
    let websocket_request_id = codex_session_id.clone().unwrap_or_else(uuidv7);
    let sse_headers = build_sse_headers(
        model,
        options.stream.headers.as_ref(),
        &account_id,
        &api_key,
        codex_session_id.as_deref(),
    );
    let websocket_headers = build_websocket_headers(
        model,
        options.stream.headers.as_ref(),
        &account_id,
        &api_key,
        &websocket_request_id,
    );
    let body_json = body.to_string();
    let http_timeout_ms = options.stream.timeout_ms;
    let websocket_connect_timeout_ms = options.stream.websocket_connect_timeout_ms;
    let transport = options.stream.transport.unwrap_or(Transport::Auto);
    let mut start_emitted = false;
    let websocket_disabled_for_session =
        transport != Transport::Sse && ws_cache::is_sse_fallback_active(cache_session_id);
    if websocket_disabled_for_session {
        ws_cache::record_websocket_sse_fallback(cache_session_id);
    }

    if transport != Transport::Sse && !websocket_disabled_for_session {
        let ws_url = resolve_codex_websocket_url(Some(&model.base_url));
        let ws_header_map = header_record_to_map(&websocket_headers).map_err(CodexError::Other)?;
        let mut retried_connection_limit = false;
        let mut retried_missing_continuation = false;
        loop {
            let mut websocket_started = false;
            let result = process_websocket_stream(
                &ws_url,
                &body,
                &ws_header_map,
                &mut *output,
                events,
                model,
                &mut websocket_started,
                &mut start_emitted,
                http_timeout_ms,
                websocket_connect_timeout_ms,
                cache_session_id,
                &grammar_tool_input_properties,
                options,
            )
            .await;

            match result {
                Ok(()) => {
                    if aborted() {
                        return Err(CodexError::Aborted);
                    }
                    assert_successful_output(output)?;
                    return Ok(done_reason(output));
                }
                Err(error) => {
                    let is_aborted = aborted();
                    let connection_limit_before_start =
                        !websocket_started && error.is_connection_limit_reached();
                    if !is_aborted
                        && error.is_previous_response_not_found()
                        && !retried_missing_continuation
                    {
                        retried_missing_continuation = true;
                        continue;
                    }
                    if !is_aborted && connection_limit_before_start && !retried_connection_limit {
                        retried_connection_limit = true;
                        continue;
                    }
                    if is_aborted || (error.is_non_transport() && !connection_limit_before_start) {
                        return Err(error);
                    }
                    append_transport_failure_diagnostic(
                        output,
                        &error,
                        transport,
                        websocket_started,
                        body_json.len(),
                    );
                    ws_cache::record_websocket_failure(cache_session_id, &error);
                    if websocket_started {
                        return Err(error);
                    }
                    ws_cache::record_websocket_sse_fallback(cache_session_id);
                    break;
                }
            }
        }
    }

    // Compress the request body once for the SSE path; the WebSocket transport
    // above sends the uncompressed JSON frame, matching the official Codex
    // client.
    let mut sse_headers = sse_headers;
    let sse_body = match compress_request_body_zstd(&body_json) {
        Some(compressed) => {
            sse_headers.insert("content-encoding".to_owned(), "zstd".to_owned());
            compressed
        }
        None => body_json.clone().into_bytes(),
    };
    let sse_header_map = header_record_to_map(&sse_headers).map_err(CodexError::Other)?;
    let url = resolve_codex_url(Some(&model.base_url));

    // Fetch with retry logic for rate limits and transient errors.
    let max_retries = options.stream.max_retries.unwrap_or(DEFAULT_MAX_RETRIES);
    let client = reqwest::Client::new();
    let mut response: Option<reqwest::Response> = None;
    let mut deadline: Option<tokio::time::Instant> = None;

    for attempt in 0..=max_retries {
        if aborted() {
            return Err(CodexError::Aborted);
        }

        let attempt_result: Result<reqwest::Response, CodexError> = async {
            let header_timeout = http_timeout_ms.filter(|timeout| *timeout > 0);
            deadline = header_timeout
                .map(|timeout| tokio::time::Instant::now() + Duration::from_millis(timeout));
            let send = client
                .post(&url)
                .headers(sse_header_map.clone())
                .body(sse_body.clone())
                .send();
            let send_future = async {
                match &signal {
                    Some(signal) => {
                        tokio::select! {
                            outcome = send => outcome.map_err(|error| CodexError::Transport(error.to_string())),
                            () = signal.cancelled() => Err(CodexError::Aborted),
                        }
                    }
                    None => send
                        .await
                        .map_err(|error| CodexError::Transport(error.to_string())),
                }
            };
            let http_response = match header_timeout {
                Some(timeout) => {
                    match tokio::time::timeout(Duration::from_millis(timeout), send_future).await {
                        Ok(outcome) => outcome?,
                        Err(_) => {
                            return Err(CodexError::Transport(format!(
                                "Codex SSE response headers timed out after {timeout}ms"
                            )));
                        }
                    }
                }
                None => send_future.await?,
            };

            if let Some(on_response) = &options.stream.on_response {
                on_response(
                    ProviderResponse {
                        status: http_response.status().as_u16(),
                        headers: headers_to_record(http_response.headers()),
                    },
                    model,
                )
                .await;
            }

            if http_response.status().is_success() {
                return Ok(http_response);
            }

            let status = http_response.status();
            let status_text = status.canonical_reason().map(str::to_owned);
            let status = status.as_u16();
            let response_headers = headers_to_record(http_response.headers());
            let error_text = http_response.text().await.unwrap_or_default();

            if attempt < max_retries && is_retryable_error(status, &error_text) {
                let delay_ms = match get_retry_after_delay_ms(&response_headers) {
                    Some(delay) => validate_retry_delay_ms(delay, &options.stream)?,
                    None => BASE_DELAY_MS * 2u64.pow(attempt),
                };
                sleep_abortable(delay_ms, signal.as_ref()).await?;
                return Err(CodexError::RetryScheduled);
            }

            let info = parse_error_response(status, status_text.as_deref(), &error_text);
            Err(CodexError::Other(
                info.friendly_message.unwrap_or(info.message),
            ))
        }
        .await;

        match attempt_result {
            Ok(http_response) => {
                response = Some(http_response);
                break;
            }
            Err(CodexError::RetryScheduled) => continue,
            Err(error) => {
                if aborted() || matches!(error, CodexError::Aborted) {
                    return Err(CodexError::Aborted);
                }
                let message = error.message();
                // Network (and friendly-message) errors are retryable, except
                // delay-cap violations and usage-limit failures.
                if attempt < max_retries
                    && !matches!(error, CodexError::RetryDelayExceeded(_))
                    && !message.contains("usage limit")
                {
                    sleep_abortable(BASE_DELAY_MS * 2u64.pow(attempt), signal.as_ref()).await?;
                    continue;
                }
                return Err(error);
            }
        }
    }

    let Some(response) = response else {
        // Unreachable: the loop either breaks with a response or returns an
        // error; keep the upstream `Failed after retries` fallback.
        return Err(CodexError::Other("Failed after retries".to_owned()));
    };

    if !start_emitted {
        events.push(StreamEvent::Start {
            partial: output.clone(),
        });
    }

    // SSE body processing (upstream `processStream`).
    let model_id = model.id.clone();
    let mut processor = ResponsesStreamProcessor::new(
        &mut *output,
        model,
        ResponsesStreamOptions {
            service_tier: options.service_tier.as_deref(),
            grammar_tool_input_properties: &grammar_tool_input_properties,
            apply_service_tier_pricing: Some(Box::new(move |usage: &mut Usage, tier| {
                apply_service_tier_pricing(usage, tier, &model_id);
            })),
            resolve_service_tier: Some(resolve_codex_service_tier),
        },
    );
    let mut decoder = SseDecoder::new();
    let mut byte_stream = response.bytes_stream();
    let mut terminal_seen = false;
    'body: loop {
        if aborted() {
            return Err(CodexError::Aborted);
        }
        let next = byte_stream.next();
        let chunk = match deadline {
            Some(deadline) => match tokio::time::timeout_at(deadline, next).await {
                Ok(chunk) => chunk,
                // The internal header-timeout signal aborting mid-body
                // surfaces upstream as an AbortError.
                Err(_) => return Err(CodexError::Other("Request was aborted".to_owned())),
            },
            None => next.await,
        };
        let Some(chunk) = chunk else { break };
        let bytes = chunk.map_err(|error| CodexError::Transport(error.to_string()))?;
        for sse in decoder.feed(&bytes) {
            let data = sse.data.trim();
            if data.is_empty() || data == "[DONE]" {
                continue;
            }
            let event: Value = serde_json::from_str(data).map_err(|error| {
                CodexError::Protocol(format!("Invalid Codex SSE JSON: {error}"))
            })?;
            let Some(mapped) = map_codex_event(&event)? else {
                continue;
            };
            terminal_seen = is_terminal_event(&mapped);
            processor
                .handle_event(&mapped, events)
                .map_err(CodexError::Other)?;
            if terminal_seen {
                // `mapCodexEvents` returns after the terminal event; the SSE
                // body may legitimately stay open.
                break 'body;
            }
        }
    }
    if !terminal_seen {
        for sse in decoder.finish() {
            let data = sse.data.trim();
            if data.is_empty() || data == "[DONE]" {
                continue;
            }
            let event: Value = serde_json::from_str(data).map_err(|error| {
                CodexError::Protocol(format!("Invalid Codex SSE JSON: {error}"))
            })?;
            let Some(mapped) = map_codex_event(&event)? else {
                continue;
            };
            terminal_seen = is_terminal_event(&mapped);
            processor
                .handle_event(&mapped, events)
                .map_err(CodexError::Other)?;
            if terminal_seen {
                break;
            }
        }
    }
    // Release the `&mut output` borrow before the final assertions. The
    // processor's terminal-event check (`finish`) is intentionally not called:
    // upstream Codex relies on `assertSuccessfulOutput` instead.
    drop(processor);

    if aborted() {
        return Err(CodexError::Aborted);
    }
    assert_successful_output(output)?;
    Ok(done_reason(output))
}

/// `stream` (openai-codex-responses).
pub fn stream(
    model: &Model,
    context: &Context,
    options: OpenAiCodexResponsesOptions,
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
            Err(error) => {
                let aborted = signal.as_ref().is_some_and(|signal| signal.is_cancelled());
                output.stop_reason = if aborted {
                    StopReason::Aborted
                } else {
                    StopReason::Error
                };
                output.error_message = Some(error.message());
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

/// `streamSimple` (openai-codex-responses): reasoning maps through
/// `clampThinkingLevel`; a clamped "off" omits `reasoning_effort`.
pub fn stream_simple(
    model: &Model,
    context: &Context,
    options: Option<SimpleStreamOptions>,
) -> AssistantMessageEventStream {
    let api_key = options.as_ref().and_then(|o| o.stream.api_key.clone());
    if api_key.is_none() {
        return immediate_error_stream(
            model,
            &format!("No API key for provider: {}", model.provider),
        );
    }

    let base = build_base_options(model, context, options.as_ref(), api_key);
    let reasoning_effort = options
        .as_ref()
        .and_then(|o| o.reasoning)
        .map(|reasoning| clamp_thinking_level(model, reasoning.to_model_level()))
        .filter(|level| *level != ModelThinkingLevel::Off)
        .map(|level| level.as_str().to_owned());

    stream(
        model,
        context,
        OpenAiCodexResponsesOptions {
            stream: base,
            reasoning_effort,
            ..OpenAiCodexResponsesOptions::default()
        },
    )
}

/// `ProviderStreams` implementation for `ApiKind::OPENAI_CODEX_RESPONSES`
/// (`openAICodexResponsesApi` in `openai-codex-responses.lazy.ts`).
#[derive(Debug, Clone, Copy, Default)]
pub struct OpenAiCodexResponses;

impl ProviderStreams for OpenAiCodexResponses {
    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: Option<StreamOptions>,
    ) -> AssistantMessageEventStream {
        stream(
            model,
            context,
            OpenAiCodexResponsesOptions {
                stream: options.unwrap_or_default(),
                ..OpenAiCodexResponsesOptions::default()
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

    fn model(extra: Value) -> Model {
        let mut overrides = json!({
            "api": "openai-codex-responses",
            "provider": "openai-codex",
            "baseUrl": "https://chatgpt.com/backend-api"
        });
        overrides
            .as_object_mut()
            .expect("object")
            .extend(extra.as_object().cloned().unwrap_or_default());
        common::make_model(overrides)
    }

    fn mock_token() -> String {
        use base64::Engine;
        let payload = base64::engine::general_purpose::STANDARD.encode(
            serde_json::to_string(&json!({
                "https://api.openai.com/auth": {"chatgpt_account_id": "acc_test"}
            }))
            .expect("json"),
        );
        format!("aaa.{payload}.bbb")
    }

    // -- JWT -------------------------------------------------------------------

    #[test]
    fn test_extract_account_id_standard_and_urlsafe() {
        assert_eq!(
            extract_account_id(&mock_token()).expect("account id"),
            "acc_test"
        );
        // URL-safe unpadded encoding of the same payload (real ChatGPT shape).
        use base64::Engine;
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
            serde_json::to_string(&json!({
                "https://api.openai.com/auth": {"chatgpt_account_id": "acc_url"}
            }))
            .expect("json"),
        );
        let token = format!("aaa.{payload}.bbb");
        assert_eq!(extract_account_id(&token).expect("account id"), "acc_url");
    }

    #[test]
    fn test_extract_account_id_failures() {
        for token in [
            "not-a-jwt",
            "aaa.bbb",
            "aaa.not-base64!!!.bbb",
            // Valid JWT shape, no account claim.
            &format!(
                "aaa.{}.bbb",
                base64::Engine::encode(
                    &base64::engine::general_purpose::STANDARD,
                    br#"{"sub":"x"}"#
                )
            ),
        ] {
            let error = extract_account_id(token);
            assert!(
                matches!(&error, Err(CodexError::Other(message)) if message == "Failed to extract accountId from token"),
                "{token}: {error:?}"
            );
        }
    }

    // -- URLs --------------------------------------------------------------------

    #[test]
    fn test_resolve_codex_url() {
        assert_eq!(
            resolve_codex_url(None),
            "https://chatgpt.com/backend-api/codex/responses"
        );
        assert_eq!(
            resolve_codex_url(Some("https://example.com/")),
            "https://example.com/codex/responses"
        );
        assert_eq!(
            resolve_codex_url(Some("https://example.com/codex")),
            "https://example.com/codex/responses"
        );
        assert_eq!(
            resolve_codex_url(Some("https://example.com/codex/responses")),
            "https://example.com/codex/responses"
        );
    }

    #[test]
    fn test_resolve_codex_websocket_url() {
        assert_eq!(
            resolve_codex_websocket_url(Some("https://example.com")),
            "wss://example.com/codex/responses"
        );
        assert_eq!(
            resolve_codex_websocket_url(Some("http://127.0.0.1:8080")),
            "ws://127.0.0.1:8080/codex/responses"
        );
    }

    // -- Service tier ------------------------------------------------------------

    #[test]
    fn test_resolve_codex_service_tier() {
        assert_eq!(
            resolve_codex_service_tier(Some("default"), Some("flex")),
            Some("flex".to_owned())
        );
        assert_eq!(
            resolve_codex_service_tier(Some("default"), Some("priority")),
            Some("priority".to_owned())
        );
        assert_eq!(
            resolve_codex_service_tier(Some("default"), Some("auto")),
            Some("default".to_owned())
        );
        assert_eq!(
            resolve_codex_service_tier(Some("flex"), Some("priority")),
            Some("flex".to_owned())
        );
        assert_eq!(
            resolve_codex_service_tier(None, Some("flex")),
            Some("flex".to_owned())
        );
        assert_eq!(resolve_codex_service_tier(None, None), None);
    }

    // -- Retry helpers -----------------------------------------------------------

    #[test]
    fn test_is_retryable_error() {
        assert!(!is_retryable_error(429, "insufficient_quota"));
        assert!(!is_retryable_error(429, "Monthly usage limit reached"));
        assert!(is_retryable_error(429, "slow down"));
        assert!(is_retryable_error(503, ""));
        assert!(is_retryable_error(400, "Rate Limit hit"));
        assert!(!is_retryable_error(400, "bad request"));
    }

    #[test]
    fn test_get_retry_after_delay_ms() {
        let headers: HashMap<String, String> =
            [("retry-after-ms".to_owned(), "250".to_owned())].into();
        assert_eq!(get_retry_after_delay_ms(&headers), Some(250));

        let headers: HashMap<String, String> = [("retry-after".to_owned(), "2".to_owned())].into();
        assert_eq!(get_retry_after_delay_ms(&headers), Some(2000));

        let future = SystemTime::now() + Duration::from_secs(30);
        let headers: HashMap<String, String> =
            [("retry-after".to_owned(), httpdate::fmt_http_date(future))].into();
        let delay = get_retry_after_delay_ms(&headers).expect("delay");
        assert!(delay <= 30_000 && delay > 20_000, "delay {delay}");

        let headers = HashMap::new();
        assert_eq!(get_retry_after_delay_ms(&headers), None);
    }

    #[test]
    fn test_validate_retry_delay_ms() {
        let options = StreamOptions::default();
        assert_eq!(validate_retry_delay_ms(1000, &options).expect("ok"), 1000);
        let error = validate_retry_delay_ms(120_000, &options);
        assert!(
            matches!(&error, Err(CodexError::RetryDelayExceeded(message)) if message == "Server requested 120s retry delay (max: 60s)"),
            "{error:?}"
        );
        // 0 disables the cap.
        let options = StreamOptions {
            max_retry_delay_ms: Some(0),
            ..StreamOptions::default()
        };
        assert_eq!(
            validate_retry_delay_ms(120_000, &options).expect("ok"),
            120_000
        );
    }

    // -- Event mapping -----------------------------------------------------------

    #[test]
    fn test_map_codex_event_error_and_failed() {
        let error = map_codex_event(&json!({
            "type": "error",
            "error": {"code": "some_code", "message": "boom"}
        }));
        assert!(
            matches!(&error, Err(CodexError::Api { message, code }) if message == "Codex error: boom" && code.as_deref() == Some("some_code")),
            "{error:?}"
        );

        let error = map_codex_event(&json!({
            "type": "response.failed",
            "response": {"error": {"code": "dead", "message": "it failed"}}
        }));
        assert!(
            matches!(&error, Err(CodexError::Api { message, code }) if message == "it failed" && code.as_deref() == Some("dead")),
            "{error:?}"
        );

        let error = map_codex_event(&json!({"type": "response.failed", "response": {}}));
        assert!(
            matches!(&error, Err(CodexError::Api { message, .. }) if message == "Codex response failed"),
            "{error:?}"
        );
    }

    #[test]
    fn test_map_codex_event_terminal_normalization() {
        for event_type in ["response.done", "response.completed", "response.incomplete"] {
            let mapped = map_codex_event(&json!({
                "type": event_type,
                "response": {"id": "resp_1", "status": "completed"}
            }))
            .expect("ok")
            .expect("event");
            assert_eq!(mapped["type"], json!("response.completed"));
            assert_eq!(mapped["response"]["status"], json!("completed"));
            assert!(is_terminal_event(&mapped));
        }
        // Unknown status is dropped (upstream `normalizeCodexStatus` → undefined).
        let mapped = map_codex_event(&json!({
            "type": "response.completed",
            "response": {"id": "resp_1", "status": "bogus"}
        }))
        .expect("ok")
        .expect("event");
        assert!(mapped["response"].get("status").is_none());

        // Non-terminal events pass through; typeless events are skipped.
        let passthrough = map_codex_event(&json!({"type": "codex.rate_limits", "x": 1}))
            .expect("ok")
            .expect("event");
        assert_eq!(passthrough["type"], json!("codex.rate_limits"));
        assert!(map_codex_event(&json!({"x": 1})).expect("ok").is_none());
    }

    // -- Cached input deltas -------------------------------------------------------

    fn continuation(
        last_body: Value,
        response_id: &str,
        response_items: Vec<Value>,
    ) -> ws_cache::CachedWebSocketContinuationState {
        ws_cache::CachedWebSocketContinuationState {
            last_request_body: last_body,
            last_response_id: response_id.to_owned(),
            last_response_items: response_items,
        }
    }

    #[test]
    fn test_get_cached_websocket_input_delta() {
        let last_body = json!({
            "model": "m", "store": false, "stream": true, "instructions": "i",
            "input": [{"role": "user", "content": [{"type": "input_text", "text": "hi"}]}]
        });
        let response_items = vec![json!({"type": "message", "role": "assistant"})];
        let continuation = continuation(last_body.clone(), "resp_1", response_items.clone());

        // Baseline prefix (last input + response items) + one new item.
        let body = json!({
            "model": "m", "store": false, "stream": true, "instructions": "i",
            "input": [
                {"role": "user", "content": [{"type": "input_text", "text": "hi"}]},
                {"type": "message", "role": "assistant"},
                {"role": "user", "content": [{"type": "input_text", "text": "next"}]}
            ]
        });
        let delta = get_cached_websocket_input_delta(&body, &continuation).expect("delta");
        assert_eq!(
            delta,
            vec![json!({"role": "user", "content": [{"type": "input_text", "text": "next"}]})]
        );

        // Body mismatch outside input → no delta.
        let mut other = body.clone();
        other["instructions"] = json!("changed");
        assert!(get_cached_websocket_input_delta(&other, &continuation).is_none());

        // Shorter than the baseline → no delta.
        let short = json!({
            "model": "m", "store": false, "stream": true, "instructions": "i",
            "input": [{"role": "user", "content": [{"type": "input_text", "text": "hi"}]}]
        });
        assert!(get_cached_websocket_input_delta(&short, &continuation).is_none());

        // Prefix mismatch → no delta.
        let mismatched = json!({
            "model": "m", "store": false, "stream": true, "instructions": "i",
            "input": [
                {"role": "user", "content": [{"type": "input_text", "text": "EDITED"}]},
                {"type": "message", "role": "assistant"},
                {"role": "user", "content": [{"type": "input_text", "text": "next"}]}
            ]
        });
        assert!(get_cached_websocket_input_delta(&mismatched, &continuation).is_none());
    }

    // -- Request body --------------------------------------------------------------

    #[test]
    fn test_build_request_body_shape_and_key_order() {
        let m = model(json!({"reasoning": false}));
        let ctx = Context {
            system_prompt: Some(String::new()),
            ..common::context(vec![common::user_text("hi")], None)
        };
        let options = OpenAiCodexResponsesOptions::default();
        let body =
            build_request_body(&m, &ctx, &options, Some("sess"), &HashMap::new()).expect("body");
        let keys: Vec<&String> = body.as_object().expect("object").keys().collect();
        assert_eq!(
            keys,
            vec![
                "model",
                "store",
                "stream",
                "instructions",
                "input",
                "text",
                "include",
                "prompt_cache_key",
                "tool_choice",
                "parallel_tool_calls"
            ]
        );
        assert_eq!(body["store"], json!(false));
        assert_eq!(body["stream"], json!(true));
        // Empty system prompt falls back to the default instructions.
        assert_eq!(body["instructions"], json!("You are a helpful assistant."));
        assert_eq!(body["text"], json!({"verbosity": "low"}));
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
        assert_eq!(body["tool_choice"], json!("auto"));
        assert_eq!(body["parallel_tool_calls"], json!(true));
        assert_eq!(body["prompt_cache_key"], json!("sess"));
    }

    #[test]
    fn test_build_request_body_reasoning_effort_mapping() {
        let m = model(json!({
            "thinkingLevelMap": {"high": "high-mapped", "off": null}
        }));
        let ctx = common::context(vec![common::user_text("hi")], None);
        let options = OpenAiCodexResponsesOptions {
            reasoning_effort: Some("high".to_owned()),
            ..OpenAiCodexResponsesOptions::default()
        };
        let body = build_request_body(&m, &ctx, &options, None, &HashMap::new()).expect("body");
        assert_eq!(
            body["reasoning"],
            json!({"effort": "high-mapped", "summary": "auto"})
        );

        // "none" resolves through thinkingLevelMap.off; a JSON null falls back
        // to the literal "none" (JS `?? "none"`).
        let options = OpenAiCodexResponsesOptions {
            reasoning_effort: Some("none".to_owned()),
            reasoning_summary: Some("detailed".to_owned()),
            ..OpenAiCodexResponsesOptions::default()
        };
        let body = build_request_body(&m, &ctx, &options, None, &HashMap::new()).expect("body");
        assert_eq!(
            body["reasoning"],
            json!({"effort": "none", "summary": "detailed"})
        );
    }

    // -- Error parsing -------------------------------------------------------------

    #[test]
    fn test_parse_error_response_usage_limit() {
        let raw = r#"{"error":{"code":"usage_limit_reached","plan_type":"Plus"}}"#;
        let parsed = parse_error_response(429, Some("Too Many Requests"), raw);
        assert_eq!(
            parsed.friendly_message.as_deref(),
            Some("You have hit your ChatGPT usage limit (plus plan).")
        );
        assert_eq!(
            parsed.message,
            "You have hit your ChatGPT usage limit (plus plan)."
        );

        // Error message wins over the friendly message.
        let raw = r#"{"error":{"code":"usage_limit_reached","message":"quota gone"}}"#;
        let parsed = parse_error_response(429, None, raw);
        assert_eq!(parsed.message, "quota gone");
    }

    #[test]
    fn test_parse_error_response_fallbacks() {
        let parsed = parse_error_response(400, Some("Bad Request"), "");
        assert_eq!(parsed.message, "Bad Request");
        let parsed = parse_error_response(400, None, "plain text");
        assert_eq!(parsed.message, "plain text");
    }

    // -- Compression ---------------------------------------------------------------

    #[test]
    fn test_compress_request_body_zstd_roundtrip() {
        let body = r#"{"model":"m","input":"compress me compress me"}"#;
        let compressed = compress_request_body_zstd(body).expect("compressed");
        let decompressed = zstd::bulk::decompress(&compressed, 4096).expect("decompress");
        assert_eq!(decompressed, body.as_bytes());
    }

    // -- Headers ---------------------------------------------------------------------

    #[test]
    fn test_build_sse_headers() {
        let m = model(json!({}));
        let headers = build_sse_headers(&m, None, "acc_test", "token", Some("sess"));
        assert_eq!(
            headers.get("authorization"),
            Some(&"Bearer token".to_owned())
        );
        assert_eq!(
            headers.get("chatgpt-account-id"),
            Some(&"acc_test".to_owned())
        );
        // originator is the literal "pi" (upstream :1593).
        assert_eq!(headers.get("originator"), Some(&"pi".to_owned()));
        assert_eq!(
            headers.get("openai-beta"),
            Some(&"responses=experimental".to_owned())
        );
        assert_eq!(headers.get("accept"), Some(&"text/event-stream".to_owned()));
        assert_eq!(
            headers.get("content-type"),
            Some(&"application/json".to_owned())
        );
        assert_eq!(headers.get("session-id"), Some(&"sess".to_owned()));
        assert_eq!(headers.get("x-client-request-id"), Some(&"sess".to_owned()));
        assert!(headers
            .get("user-agent")
            .is_some_and(|ua| ua.starts_with("pi (")));
    }

    #[test]
    fn test_build_websocket_headers() {
        let m = model(json!({}));
        let headers = build_websocket_headers(&m, None, "acc_test", "token", "req_1");
        assert_eq!(
            headers.get("openai-beta"),
            Some(&"responses_websockets=2026-02-06".to_owned())
        );
        assert!(!headers.contains_key("accept"));
        assert!(!headers.contains_key("content-type"));
        assert_eq!(
            headers.get("x-client-request-id"),
            Some(&"req_1".to_owned())
        );
        assert_eq!(headers.get("session-id"), Some(&"req_1".to_owned()));
        assert_eq!(headers.get("originator"), Some(&"pi".to_owned()));
    }

    #[test]
    fn test_build_headers_model_and_option_merging() {
        let m = model(json!({"headers": {"X-Custom": "yes", "Originator": "model"}}));
        let options_headers: ProviderHeaders = [
            ("x-custom".to_owned(), None),
            ("x-extra".to_owned(), Some("1".to_owned())),
        ]
        .into();
        let headers = build_base_codex_headers(&m, Some(&options_headers), "acc", "token");
        // Option `None` deletes the model header (case-insensitively).
        assert!(!headers.contains_key("x-custom"));
        assert_eq!(headers.get("x-extra"), Some(&"1".to_owned()));
        // Auth/originator headers always win over model headers.
        assert_eq!(headers.get("originator"), Some(&"pi".to_owned()));
        assert_eq!(
            headers.get("authorization"),
            Some(&"Bearer token".to_owned())
        );
    }
}
