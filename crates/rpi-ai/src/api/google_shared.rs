//! Port of `packages/ai/src/api/google-shared.ts` @ pi 0.82.1 (2efa728);
//! signed-empty-block retention, Gemini 3 tool call IDs, and the GenAI
//! request retry helper updated to 4181f66 (6138f5a07, cbaca6038, b9d360a2c).
//!
//! Shared utilities for the Google Generative AI (and later Vertex) adapters:
//! thinking-part detection, thought-signature retention/validation, message
//! conversion to Gemini `Content[]` (function-response merging, multimodal
//! function-response routing for Gemini 3+), tool conversion
//! (`parametersJsonSchema` vs legacy OpenAPI `parameters` with meta-key
//! stripping), strict tool sampling / function-calling-mode resolution,
//! finish-reason mapping, and the initial-request retry wrapper.
//!
//! Intentional differences (upstream deviations):
//! - Converted messages are plain [`serde_json::Value`] parts shaped like the
//!   SDK's `Content`/`Part` JSON (the SDK's `partToMldev` mapping passes every
//!   field pi uses through verbatim).
//! - `map_stop_reason` takes the wire string (the SDK's `FinishReason` enum is
//!   string-valued; the unknown-case `never` throw becomes an `Err`).

use serde_json::{json, Value};

use crate::api::constrained_sampling::resolve_json_schema_strict_sampling;
use crate::types::{
    AssistantContent, Context, InputModality, Message, Model, StopReason, StreamOptions, Tool,
    ToolResultContent, UserContent, UserContentBlock,
};
use crate::utils::provider_retry::{
    retry_provider_request, ProviderErrorInfo, ProviderRetryOptions, RetryError,
};
use crate::utils::sanitize_unicode::sanitize_surrogates;
use crate::utils::transform_messages::transform_messages;

/// `GoogleThinkingLevel` — thinking level for Gemini 3 models, mirroring
/// Google's `ThinkingLevel` enum values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoogleThinkingLevel {
    Unspecified,
    Minimal,
    Low,
    Medium,
    High,
}

impl GoogleThinkingLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unspecified => "THINKING_LEVEL_UNSPECIFIED",
            Self::Minimal => "MINIMAL",
            Self::Low => "LOW",
            Self::Medium => "MEDIUM",
            Self::High => "HIGH",
        }
    }
}

/// `isThinkingPart`: only `thought === true` marks thinking content.
///
/// Protocol note (Gemini / Vertex AI thought signatures): `thoughtSignature`
/// is an encrypted representation of the model's internal thought process for
/// context replay; it can appear on ANY part type and does NOT indicate
/// thinking content. See https://ai.google.dev/gemini-api/docs/thought-signatures
pub fn is_thinking_part(thought: Option<bool>) -> bool {
    thought == Some(true)
}

/// `retainThoughtSignature`: preserves the last non-empty signature within a
/// streamed block (some backends only send it on the first delta). Does NOT
/// merge or move signatures across distinct response parts.
pub fn retain_thought_signature(existing: Option<&str>, incoming: Option<&str>) -> Option<String> {
    if let Some(incoming) = incoming {
        if !incoming.is_empty() {
            return Some(incoming.to_owned());
        }
    }
    existing.map(str::to_owned)
}

/// Thought signatures must be base64 for Google APIs (TYPE_BYTES).
/// (`/^[A-Za-z0-9+/]+={0,2}$/` plus a length % 4 check.)
fn is_valid_thought_signature(signature: &str) -> bool {
    if signature.is_empty() || !signature.len().is_multiple_of(4) {
        return false;
    }
    let bytes = signature.as_bytes();
    let padding = bytes.iter().rev().take_while(|&&b| b == b'=').count();
    padding <= 2
        && bytes[..bytes.len() - padding]
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'+' || *b == b'/')
}

/// `resolveThoughtSignature`: only keep signatures from the same
/// provider/model and with valid base64.
fn resolve_thought_signature(
    is_same_provider_and_model: bool,
    signature: Option<&str>,
) -> Option<String> {
    match signature {
        Some(signature) if is_same_provider_and_model && is_valid_thought_signature(signature) => {
            Some(signature.to_owned())
        }
        _ => None,
    }
}

/// `requiresToolCallId`: models via Google APIs that require explicit tool
/// call IDs in function calls/responses. Extended to Gemini 3.x+ by
/// cbaca6038 (#7494): Gemini 3 echoes tool call IDs and rejects histories
/// that drop them.
pub fn requires_tool_call_id(model_id: &str) -> bool {
    let gemini_major_version = get_gemini_major_version(model_id);
    model_id.starts_with("claude-")
        || model_id.starts_with("gpt-oss-")
        || gemini_major_version.is_some_and(|major| major >= 3)
}

/// `getGeminiMajorVersion` (`/^gemini(?:-live)?-(\d+)/` on the lowercase id).
fn get_gemini_major_version(model_id: &str) -> Option<u32> {
    let id = model_id.to_lowercase();
    // Order matters: "gemini-live-" must be tried before "gemini-",
    // otherwise "gemini-live-3-001" is stripped to "live-3-001" and the
    // version parse fails (upstream regex `/^gemini(?:-live)?-(\d+)/`).
    let rest = id
        .strip_prefix("gemini-live-")
        .or_else(|| id.strip_prefix("gemini-"))?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse().ok()
}

/// `supportsMultimodalFunctionResponse`: Gemini 3+ (and any non-Gemini model)
/// accepts images nested inside `functionResponse.parts`.
fn supports_multimodal_function_response(model_id: &str) -> bool {
    match get_gemini_major_version(model_id) {
        Some(major) => major >= 3,
        None => true,
    }
}

/// `convertMessages`: internal messages to Gemini `Content[]` format.
pub fn convert_messages(model: &Model, context: &Context) -> Vec<Value> {
    let mut contents: Vec<Value> = Vec::new();
    let requires_id = requires_tool_call_id(&model.id);
    let mut normalize_tool_call_id =
        move |id: &str, _model: &Model, _msg: &crate::types::AssistantMessage| {
            if !requires_id {
                return id.to_owned();
            }
            // The allowed set is ASCII, so `chars().take(64)` matches JS
            // `slice(0, 64)`.
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
        };
    let transformed_messages =
        transform_messages(&context.messages, model, Some(&mut normalize_tool_call_id));

    for msg in &transformed_messages {
        match msg {
            Message::User(user) => match &user.content {
                UserContent::Text(text) => {
                    contents.push(json!({
                        "role": "user",
                        "parts": [{"text": sanitize_surrogates(text)}],
                    }));
                }
                UserContent::Blocks(blocks) => {
                    let parts: Vec<Value> = blocks
                        .iter()
                        .map(|item| match item {
                            UserContentBlock::Text(text) => {
                                json!({"text": sanitize_surrogates(&text.text)})
                            }
                            UserContentBlock::Image(image) => json!({
                                "inlineData": {
                                    "mimeType": image.mime_type,
                                    "data": image.data,
                                },
                            }),
                        })
                        .collect();
                    if parts.is_empty() {
                        continue;
                    }
                    contents.push(json!({"role": "user", "parts": parts}));
                }
            },
            Message::Assistant(assistant) => {
                let mut parts: Vec<Value> = Vec::new();
                // Only keep thinking blocks / signatures when the message is
                // from the same provider and model.
                let is_same_provider_and_model =
                    assistant.provider == model.provider && assistant.model == model.id;

                for block in &assistant.content {
                    match block {
                        AssistantContent::Text(text) => {
                            // 6138f5a07 (#7362): skip empty text blocks —
                            // unless they carry a thought signature. Gemini can
                            // attach the signature to a part whose visible text
                            // is empty and requires it echoed back; dropping it
                            // breaks the reasoning chain and the model
                            // intermittently ends mid-task turns with a
                            // thought-only STOP (empty completion, no tool
                            // call).
                            let thought_signature = resolve_thought_signature(
                                is_same_provider_and_model,
                                text.text_signature.as_deref(),
                            );
                            if text.text.trim().is_empty() && thought_signature.is_none() {
                                continue;
                            }
                            let mut part = json!({"text": sanitize_surrogates(&text.text)});
                            if let Some(signature) = thought_signature {
                                part["thoughtSignature"] = json!(signature);
                            }
                            parts.push(part);
                        }
                        AssistantContent::Thinking(thinking) => {
                            // Same provider AND same model: keep as a thought
                            // part; otherwise convert to plain text (no tags to
                            // avoid the model mimicking them).
                            if is_same_provider_and_model {
                                let thought_signature = resolve_thought_signature(
                                    is_same_provider_and_model,
                                    thinking.thinking_signature.as_deref(),
                                );
                                // Same rule as text blocks (6138f5a07): an
                                // empty thinking block is dropped only when it
                                // carries no signature (mirrors the anthropic
                                // converter's handling).
                                if thinking.thinking.trim().is_empty()
                                    && thought_signature.is_none()
                                {
                                    continue;
                                }
                                let mut part = json!({
                                    "thought": true,
                                    "text": sanitize_surrogates(&thinking.thinking),
                                });
                                if let Some(signature) = thought_signature {
                                    part["thoughtSignature"] = json!(signature);
                                }
                                parts.push(part);
                            } else {
                                // Cross-provider/model: the signature is
                                // unusable, empty blocks stay dropped.
                                if thinking.thinking.trim().is_empty() {
                                    continue;
                                }
                                parts
                                    .push(json!({"text": sanitize_surrogates(&thinking.thinking)}));
                            }
                        }
                        AssistantContent::ToolCall(call) => {
                            let mut function_call = json!({
                                "name": call.name,
                                "args": call.arguments,
                            });
                            if requires_id {
                                function_call["id"] = json!(call.id);
                            }
                            let mut part = json!({"functionCall": function_call});
                            if let Some(signature) = resolve_thought_signature(
                                is_same_provider_and_model,
                                call.thought_signature.as_deref(),
                            ) {
                                part["thoughtSignature"] = json!(signature);
                            }
                            parts.push(part);
                        }
                    }
                }

                if parts.is_empty() {
                    continue;
                }
                contents.push(json!({"role": "model", "parts": parts}));
            }
            Message::ToolResult(result) => {
                // Extract text and image content.
                let text_result = result
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ToolResultContent::Text(text) => Some(text.text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let image_content: Vec<&crate::types::ImageContent> =
                    if model.input.contains(&InputModality::Image) {
                        result
                            .content
                            .iter()
                            .filter_map(|block| match block {
                                ToolResultContent::Image(image) => Some(image),
                                _ => None,
                            })
                            .collect()
                    } else {
                        Vec::new()
                    };

                let has_text = !text_result.is_empty();
                let has_images = !image_content.is_empty();

                // Gemini 3+ models support multimodal function responses with
                // images nested inside functionResponse.parts. Claude and other
                // non-Gemini models behind Cloud Code Assist / Gemini < 3 still
                // need a separate user image turn.
                let model_supports_multimodal = supports_multimodal_function_response(&model.id);

                // "output" key for success, "error" key for errors (SDK docs).
                let response_value = if has_text {
                    sanitize_surrogates(&text_result).to_owned()
                } else if has_images {
                    "(see attached image)".to_owned()
                } else {
                    String::new()
                };

                let image_parts: Vec<Value> = image_content
                    .iter()
                    .map(|image| {
                        json!({
                            "inlineData": {
                                "mimeType": image.mime_type,
                                "data": image.data,
                            },
                        })
                    })
                    .collect();

                let mut function_response = json!({
                    "name": result.tool_name,
                    "response": if result.is_error {
                        json!({"error": response_value})
                    } else {
                        json!({"output": response_value})
                    },
                });
                if has_images && model_supports_multimodal {
                    function_response["parts"] = Value::Array(image_parts.clone());
                }
                if requires_id {
                    function_response["id"] = json!(result.tool_call_id);
                }
                let function_response_part = json!({"functionResponse": function_response});

                // Cloud Code Assist API requires all function responses in a
                // single user turn; merge into a trailing user turn that
                // already carries function responses.
                let merge_into_last = contents.last().is_some_and(|last| {
                    last.get("role").and_then(Value::as_str) == Some("user")
                        && last
                            .get("parts")
                            .and_then(Value::as_array)
                            .is_some_and(|parts| {
                                parts
                                    .iter()
                                    .any(|part| part.get("functionResponse").is_some())
                            })
                });
                if merge_into_last {
                    // invariant: merge_into_last implies last has a parts array
                    if let Some(parts) = contents
                        .last_mut()
                        .and_then(|last| last.get_mut("parts"))
                        .and_then(Value::as_array_mut)
                    {
                        parts.push(function_response_part);
                    }
                } else {
                    contents.push(json!({
                        "role": "user",
                        "parts": [function_response_part],
                    }));
                }

                // For Gemini < 3, add images in a separate user message.
                if has_images && !model_supports_multimodal {
                    let mut parts = vec![json!({"text": "Tool result image:"})];
                    parts.extend(image_parts);
                    contents.push(json!({"role": "user", "parts": parts}));
                }
            }
        }
    }

    contents
}

/// JSON Schema meta-declarations stripped by `sanitizeForOpenApi`.
const JSON_SCHEMA_META_DECLARATIONS: [&str; 8] = [
    "$schema",
    "$id",
    "$anchor",
    "$dynamicAnchor",
    "$vocabulary",
    "$comment",
    "$defs",
    // pre-draft-2019-09 equivalent of $defs
    "definitions",
];

/// `sanitizeForOpenApi`: strips meta-declarations from a schema object.
/// Arrays pass through untouched (upstream does not recurse into them).
fn sanitize_for_open_api(schema: &Value) -> Value {
    let Value::Object(object) = schema else {
        return schema.clone();
    };
    object
        .iter()
        .filter(|(key, _)| !JSON_SCHEMA_META_DECLARATIONS.contains(&key.as_str()))
        .map(|(key, value)| (key.clone(), sanitize_for_open_api(value)))
        .collect::<serde_json::Map<String, Value>>()
        .into()
}

/// `convertTools`: tools to Gemini function-declaration format. By default
/// uses `parametersJsonSchema` (full JSON Schema); `use_parameters` selects
/// the legacy OpenAPI 3.03 `parameters` field with meta keys stripped (Cloud
/// Code Assist with Claude models). `None` for an empty tool list.
pub fn convert_tools(tools: &[Tool], use_parameters: bool) -> Option<Value> {
    if tools.is_empty() {
        return None;
    }
    let declarations: Vec<Value> = tools
        .iter()
        .map(|tool| {
            if use_parameters {
                json!({
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": sanitize_for_open_api(&tool.parameters),
                })
            } else {
                json!({
                    "name": tool.name,
                    "description": tool.description,
                    "parametersJsonSchema": tool.parameters,
                })
            }
        })
        .collect();
    Some(json!([{ "functionDeclarations": declarations }]))
}

/// `supportsGoogleStrictToolSampling`: Gemini 3+ enforces required function
/// parameters in validated tool-calling modes.
pub fn supports_google_strict_tool_sampling(model_id: &str) -> bool {
    get_gemini_major_version(model_id).is_some_and(|major| major >= 3)
}

/// `mapToolChoice`: tool choice string to Gemini `FunctionCallingConfigMode`.
/// Unknown values fall back to AUTO (upstream `default` arm).
pub fn map_tool_choice(choice: &str) -> &'static str {
    match choice {
        "none" => "NONE",
        "any" => "ANY",
        _ => "AUTO",
    }
}

/// `resolveGoogleFunctionCallingMode`. The strict-mode probe runs first and
/// can fail (strict `require` tool on an unsupported model), matching
/// upstream's throw-before-toolChoice order.
pub fn resolve_google_function_calling_mode(
    tools: &[Tool],
    tool_choice: Option<&str>,
    supports_strict_mode: bool,
) -> Result<Option<&'static str>, String> {
    let mut use_strict_mode = false;
    for tool in tools {
        if resolve_json_schema_strict_sampling(tool, supports_strict_mode)? == Some(true) {
            use_strict_mode = true;
            break;
        }
    }
    if tool_choice == Some("none") || tool_choice == Some("any") {
        // invariant: tool_choice is one of these two values here
        return Ok(Some(map_tool_choice(tool_choice.unwrap_or("auto"))));
    }
    if use_strict_mode {
        return Ok(Some("VALIDATED"));
    }
    Ok(tool_choice.map(map_tool_choice))
}

/// `mapStopReason`: Gemini `FinishReason` (wire string) to our `StopReason`;
/// unknown reasons are an error (the API may add new values — upstream's
/// `never` check throws).
pub fn map_stop_reason(reason: &str) -> Result<StopReason, String> {
    match reason {
        "STOP" => Ok(StopReason::Stop),
        "MAX_TOKENS" => Ok(StopReason::Length),
        "BLOCKLIST"
        | "PROHIBITED_CONTENT"
        | "SPII"
        | "SAFETY"
        | "IMAGE_SAFETY"
        | "IMAGE_PROHIBITED_CONTENT"
        | "IMAGE_RECITATION"
        | "IMAGE_OTHER"
        | "RECITATION"
        | "FINISH_REASON_UNSPECIFIED"
        | "OTHER"
        | "LANGUAGE"
        | "MALFORMED_FUNCTION_CALL"
        | "UNEXPECTED_TOOL_CALL"
        | "NO_IMAGE" => Ok(StopReason::Error),
        other => Err(format!("Unhandled stop reason: {other}")),
    }
}

/// `mapStopReasonString`: string finish reason to our `StopReason` (for raw
/// API responses).
pub fn map_stop_reason_string(reason: &str) -> StopReason {
    match reason {
        "STOP" => StopReason::Stop,
        "MAX_TOKENS" => StopReason::Length,
        _ => StopReason::Error,
    }
}

/// `retryGoogleRequest` (b9d360a2c, #7471): run the initial Google GenAI
/// request under the shared provider retry policy (408/409/429/5xx with
/// backoff, honoring retry-after), mirroring how the Anthropic and OpenAI
/// adapters wrap their initial request in `retryProviderRequest`.
///
/// Upstream additionally normalizes the `@google/genai` SDK's `ApiError`
/// (which has `status` but no `headers` property) so `retryProviderRequest`'s
/// provider-error guard recognizes it; rpi adapters construct
/// [`ProviderErrorInfo`] from the raw reqwest response, which always carries
/// both fields, so no normalization is needed here. Opt-in via
/// `StreamOptions::max_retries`; the default (unset) performs no retries.
pub async fn retry_google_request<T, F, Fut>(
    request: F,
    options: &StreamOptions,
) -> Result<T, RetryError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, ProviderErrorInfo>>,
{
    retry_provider_request(
        request,
        ProviderRetryOptions {
            max_retries: options.max_retries,
            max_retry_delay_ms: options.max_retry_delay_ms,
        },
        options.signal.as_ref(),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `gemini-live-*` must parse as Gemini versions: the `gemini-live-`
    /// prefix is tried before `gemini-` (upstream
    /// `/^gemini(?:-live)?-(\d+)/` on google-shared.ts:81).
    #[test]
    fn get_gemini_major_version_live_prefix() {
        assert_eq!(get_gemini_major_version("gemini-live-3-001"), Some(3));
        assert_eq!(get_gemini_major_version("gemini-live-2-001"), Some(2));
    }

    /// Fractional versions parse as their leading major digits
    /// (`gemini-2.5-pro` → 2), anchoring the existing behavior.
    #[test]
    fn get_gemini_major_version_plain_prefix() {
        assert_eq!(get_gemini_major_version("gemini-2.5-pro"), Some(2));
        assert_eq!(get_gemini_major_version("gemini-3-pro"), Some(3));
    }

    #[test]
    fn get_gemini_major_version_non_gemini() {
        assert_eq!(get_gemini_major_version("claude-sonnet"), None);
        assert_eq!(get_gemini_major_version("gemini-"), None);
        assert_eq!(get_gemini_major_version("gemini-live-"), None);
    }

    /// `requires_tool_call_id`: gemini-live-3+ requires tool call IDs
    /// (bug: wrong prefix order made this return false).
    #[test]
    fn requires_tool_call_id_live_gemini3() {
        assert!(requires_tool_call_id("gemini-live-3-001"));
        assert!(!requires_tool_call_id("gemini-live-2-001"));
        assert!(!requires_tool_call_id("gemini-2.5-pro"));
    }
}
