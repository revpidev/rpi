//! Shared System One classification transport (port of
//! `packages/ai/src/api/system-one-shared.ts` @ a13d35a74).
//!
//! TypeSafe's System One protocol is served by several providers
//! (typesafe / openrouter / vercel-ai-gateway / opencode) and by Cloudflare
//! Workers AI behind its REST envelope. This module owns the request shape,
//! retry/cancel semantics, `noul` bool mapping, and response parsing; each
//! transport supplies only url/payload/output differences.
//!
//! Never rejects: failures come back as a `ClassifierResult` with
//! `stopReason: "error"` (or `"aborted"`) and `errorMessage`.

use serde_json::{Map, Value, json};
use url::Url;

use crate::api::http_client::adapter_client_builder;
use crate::types::{
    ClassifierAnswer, ClassifierBoolAnswer, ClassifierChoiceAnswer, ClassifierContext,
    ClassifierModel, ClassifierOptions, ClassifierQuestion, ClassifierResult, ClassifierScoreAnswer,
    ClassifierStopReason, ProviderHeaders, ProviderResponse, Usage,
};
use crate::utils::cost::calculate_cost_for;
use crate::utils::custom_fetch::send_provider_request;
use crate::utils::error_body::{NormalizedProviderError, format_provider_error};
use crate::utils::headers::{headers_to_record, merge_headers, provider_headers_to_header_map};
use crate::utils::provider_retry::{
    ProviderErrorInfo, ProviderRetryOptions, RetryError, retry_provider_request,
};

/// TypeSafe System One request body without the transport-specific envelope.
#[derive(Debug, Clone)]
pub struct SystemOneWireRequest {
    pub state: Map<String, Value>,
    pub questions: Map<String, Value>,
}

/// Differences between services that serve System One models.
pub struct SystemOneTransport {
    /// Classifier API implemented by this transport.
    pub api: &'static str,
    /// Service name used in error messages.
    pub label: &'static str,
    /// Absolute request URL.
    pub url: fn(&ClassifierModel) -> Result<Url, String>,
    /// Wraps the System One request in the service's request envelope.
    pub payload: fn(&ClassifierModel, SystemOneWireRequest) -> Value,
    /// Extracts the System One output (`{ answers, usage }`) from the
    /// service's response envelope.
    pub output: fn(&Value) -> Result<Value, String>,
}

pub fn is_record(value: &Value) -> bool {
    value.is_object()
}

fn required_number(label: &str, value: &Value, field: &str) -> Result<f64, String> {
    value
        .as_f64()
        .filter(|number| number.is_finite())
        .ok_or_else(|| format!("{label} returned an invalid {field}"))
}

fn probabilities(
    label: &str,
    value: &Value,
    id: &str,
) -> Result<std::collections::BTreeMap<String, f64>, String> {
    let Some(object) = value.as_object() else {
        return Err(format!("{label} returned invalid probabilities for {id}"));
    };
    let mut result = std::collections::BTreeMap::new();
    for (key, probability) in object {
        result.insert(
            key.clone(),
            required_number(label, probability, &format!("probability for {id}.{key}"))?,
        );
    }
    Ok(result)
}

fn parse_answers(
    label: &str,
    value: &Value,
    context: &ClassifierContext,
) -> Result<std::collections::BTreeMap<String, ClassifierAnswer>, String> {
    let Some(object) = value.as_object() else {
        return Err(format!("{label} returned an unexpected response"));
    };
    let mut answers = std::collections::BTreeMap::new();
    for (id, question) in &context.questions {
        let Some(answer) = object.get(id) else {
            return Err(format!("{label} did not return an answer for {id}"));
        };
        match question {
            ClassifierQuestion::Choice(_) => {
                let answer_type = answer.get("type").and_then(Value::as_str);
                if answer_type != Some("choice") {
                    return Err(format!("{label} did not return a choice answer for {id}"));
                }
                let choice = answer
                    .get("choice")
                    .and_then(Value::as_str)
                    .ok_or_else(|| format!("{label} did not return a choice answer for {id}"))?;
                answers.insert(
                    id.clone(),
                    ClassifierAnswer::Choice(ClassifierChoiceAnswer {
                        choice: choice.to_owned(),
                        probabilities: probabilities(
                            label,
                            answer.get("probabilities").unwrap_or(&Value::Null),
                            id,
                        )?,
                        confidence: required_number(
                            label,
                            answer.get("confidence").unwrap_or(&Value::Null),
                            &format!("confidence for {id}"),
                        )?,
                    }),
                );
            }
            ClassifierQuestion::Score(_) => {
                if answer.get("type").and_then(Value::as_str) != Some("score") {
                    return Err(format!("{label} did not return a score answer for {id}"));
                }
                answers.insert(
                    id.clone(),
                    ClassifierAnswer::Score(ClassifierScoreAnswer {
                        score: required_number(
                            label,
                            answer.get("score").unwrap_or(&Value::Null),
                            &format!("score for {id}"),
                        )?,
                        confidence: required_number(
                            label,
                            answer.get("confidence").unwrap_or(&Value::Null),
                            &format!("confidence for {id}"),
                        )?,
                    }),
                );
            }
            ClassifierQuestion::Bool(_) => {
                // Wire-level `noul`.
                if answer.get("type").and_then(Value::as_str) != Some("noul") {
                    return Err(format!("{label} did not return a bool answer for {id}"));
                }
                answers.insert(
                    id.clone(),
                    ClassifierAnswer::Bool(ClassifierBoolAnswer {
                        probability: required_number(
                            label,
                            answer.get("noul").unwrap_or(&Value::Null),
                            &format!("probability for {id}"),
                        )?,
                    }),
                );
            }
        }
    }
    Ok(answers)
}

fn token_count(value: Option<&Value>) -> u64 {
    value
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite() && *value > 0.0)
        .map(|value| value as u64)
        .unwrap_or(0)
}

/// Usage from System One's `{ input_tokens, output_tokens }`, priced from the
/// model catalog like chat usage. A missing or malformed usage object leaves
/// the result without usage instead of failing it.
fn parse_usage(value: Option<&Value>, model: &ClassifierModel) -> Option<Usage> {
    let object = value?.as_object()?;
    if !object.contains_key("input_tokens") && !object.contains_key("output_tokens") {
        return None;
    }
    let input = token_count(object.get("input_tokens"));
    let output = token_count(object.get("output_tokens"));
    let mut usage = Usage {
        input,
        output,
        cache_read: 0,
        cache_write: 0,
        cache_write1h: None,
        reasoning: None,
        total_tokens: input + output,
        cost: Default::default(),
    };
    calculate_cost_for(&model.cost, &mut usage);
    Some(usage)
}

/// Maps public `bool` questions to TypeSafe's wire-level `noul` type.
fn wire_request(context: &ClassifierContext) -> SystemOneWireRequest {
    let state = context.state.clone();
    let questions = context
        .questions
        .iter()
        .map(|(id, question)| {
            let value = match question {
                ClassifierQuestion::Choice(question) => json!({
                    "type": "choice",
                    "instructions": question.instructions,
                    "criteria": question.criteria,
                }),
                ClassifierQuestion::Score(question) => json!({
                    "type": "score",
                    "instructions": question.instructions,
                    "criteria": question.criteria,
                }),
                ClassifierQuestion::Bool(question) => json!({
                    "type": "noul",
                    "instructions": question.instructions,
                    "criteria": { "true": question.criteria.yes, "false": question.criteria.no },
                }),
            };
            (id.clone(), value)
        })
        .collect();
    SystemOneWireRequest { state, questions }
}

fn request_headers(
    model: &ClassifierModel,
    api_key: &str,
    options_headers: Option<&ProviderHeaders>,
) -> Result<reqwest::header::HeaderMap, String> {
    let model_headers: Option<ProviderHeaders> = model.headers.as_ref().map(|headers| {
        headers
            .iter()
            .map(|(key, value)| (key.clone(), Some(value.clone())))
            .collect()
    });
    let merged = merge_headers(model_headers.as_ref(), options_headers);
    let mut map = reqwest::header::HeaderMap::new();
    map.insert(
        reqwest::header::AUTHORIZATION,
        reqwest::header::HeaderValue::from_str(&format!("Bearer {api_key}"))
            .map_err(|error| format!("Invalid api key header: {error}"))?,
    );
    map.insert(
        reqwest::header::CONTENT_TYPE,
        reqwest::header::HeaderValue::from_static("application/json"),
    );
    let empty = ProviderHeaders::new();
    let request_headers = provider_headers_to_header_map(merged.as_ref().unwrap_or(&empty))?;
    for (name, value) in &request_headers {
        map.insert(name, value.clone());
    }
    Ok(map)
}

/// Runs one System One classification over the given transport
/// (`classifySystemOne`). Never rejects.
pub async fn classify_system_one(
    transport: &SystemOneTransport,
    model: &ClassifierModel,
    context: &ClassifierContext,
    options: Option<&ClassifierOptions>,
) -> ClassifierResult {
    let mut output = ClassifierResult {
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        answers: std::collections::BTreeMap::new(),
        usage: None,
        stop_reason: ClassifierStopReason::Stop,
        error_message: None,
        timestamp: now_millis(),
    };
    match classify_inner(transport, model, context, options, &mut output).await {
        Ok(()) => output,
        Err(message) => {
            output.stop_reason = if options
                .and_then(|options| options.signal.as_ref())
                .is_some_and(|signal| signal.is_cancelled())
            {
                ClassifierStopReason::Aborted
            } else {
                ClassifierStopReason::Error
            };
            output.error_message = Some(message);
            output
        }
    }
}

async fn classify_inner(
    transport: &SystemOneTransport,
    model: &ClassifierModel,
    context: &ClassifierContext,
    options: Option<&ClassifierOptions>,
    output: &mut ClassifierResult,
) -> Result<(), String> {
    if model.api.as_str() != transport.api {
        return Err(format!("Unsupported classifier API: {}", model.api));
    }
    let Some(api_key) = options.and_then(|options| options.api_key.clone()) else {
        return Err(format!("No API key for provider: {}", model.provider));
    };
    let url = (transport.url)(model)?;
    let mut payload = (transport.payload)(model, wire_request(context));
    if let Some(on_payload) = options.and_then(|options| options.on_payload.as_ref())
        && let Some(next) = on_payload(payload.clone(), model).await
    {
        payload = next;
    }
    let headers = request_headers(model, &api_key, options.and_then(|o| o.headers.as_ref()))?;
    let timeout_ms = options.and_then(|options| options.timeout_ms);
    let mut client_builder =
        adapter_client_builder(options.and_then(|o| o.env.as_ref()), url.as_str())?;
    if let Some(timeout_ms) = timeout_ms {
        client_builder = client_builder.timeout(std::time::Duration::from_millis(timeout_ms));
    }
    let client = client_builder.build().map_err(|error| error.to_string())?;
    let error_context = format!("{} error", transport.label);

    let (body, response_meta) = retry_provider_request(
        || {
            let request = client
                .post(url.clone())
                .headers(headers.clone())
                .json(&payload);
            let signal = options.and_then(|options| options.signal.clone());
            let fetch = options.and_then(|options| options.fetch.clone());
            let label = transport.label;
            let error_context = error_context.clone();
            async move {
                let result =
                    send_provider_request(request, fetch.as_ref(), signal.as_ref(), None).await;
                match result {
                    Ok(response) => {
                        let status = response.status();
                        let response_meta = ProviderResponse {
                            status: status.as_u16(),
                            headers: headers_to_record(response.headers()),
                        };
                        if status.is_success() {
                            let body = response.text().await.map_err(|error| {
                                ProviderErrorInfo {
                                    status: None,
                                    headers: None,
                                    message: error.to_string(),
                                }
                            })?;
                            let parsed = serde_json::from_str::<Value>(&body).map_err(|error| {
                                ProviderErrorInfo {
                                    status: None,
                                    headers: None,
                                    message: format!(
                                        "{label} returned an unexpected response: {error}"
                                    ),
                                }
                            })?;
                            Ok((parsed, response_meta))
                        } else {
                            let status = status.as_u16();
                            let response_headers = headers_to_record(response.headers());
                            let body = response.text().await.unwrap_or_default();
                            let normalized = NormalizedProviderError::new(
                                Some(status),
                                Some(body),
                                format!("{label} returned {status}"),
                            );
                            Err(ProviderErrorInfo {
                                status: Some(status),
                                headers: Some(response_headers),
                                message: format_provider_error(
                                    &normalized,
                                    Some(&error_context),
                                ),
                            })
                        }
                    }
                    Err(error) => Err(error.into_provider_error_info()),
                }
            }
        },
        ProviderRetryOptions {
            // `maxRetries ?? 2` (system-one-shared.ts:179).
            max_retries: options.and_then(|options| options.max_retries).or(Some(2)),
            max_retry_delay_ms: options.and_then(|options| options.max_retry_delay_ms),
        },
        options.and_then(|options| options.signal.as_ref()),
    )
    .await
    .map_err(|error| match error {
        RetryError::Aborted => "Request aborted".to_owned(),
        RetryError::Provider(info) => info.message,
        RetryError::Message(message) => message,
    })?;

    if let Some(on_response) = options.and_then(|options| options.on_response.as_ref()) {
        on_response(response_meta, model).await;
    }
    let result = (transport.output)(&body)?;
    // Set before parsing answers: a request with malformed answers was still
    // billed.
    output.usage = parse_usage(result.get("usage"), model);
    output.answers = parse_answers(
        transport.label,
        result.get("answers").unwrap_or(&Value::Null),
        context,
    )?;
    Ok(())
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

/// Helper for transports that expose the standard `{ model, ...request }`
/// envelope (TypeSafe's native protocol).
pub fn typesafe_payload(model: &ClassifierModel, request: SystemOneWireRequest) -> Value {
    let mut object = Map::new();
    object.insert("model".to_owned(), Value::String(model.id.clone()));
    object.insert("state".to_owned(), Value::Object(request.state));
    object.insert("questions".to_owned(), Value::Object(request.questions));
    Value::Object(object)
}