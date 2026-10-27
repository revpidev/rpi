//! Shared System One classification transport (port of
//! `packages/ai/src/api/system-one-shared.ts` @ 4a080252f, 7f225f25d).
//!
//! TypeSafe's System One protocol is served by several providers and by
//! Cloudflare Workers AI behind its REST envelope. This module owns request
//! construction, image forms, retry/cancel semantics, `noul` bool mapping,
//! and response parsing; transports supply compat/url/payload/output details.

use serde_json::{Map, Value, json};
use url::Url;

use crate::api::classifier_shared::{
    parse_classifier_usage, post_classifier_request, required_number,
};
use crate::types::{
    ClassifierAnswer, ClassifierBoolAnswer, ClassifierChoiceAnswer, ClassifierContext,
    ClassifierModel, ClassifierOptions, ClassifierQuestion, ClassifierResult,
    ClassifierScoreAnswer, ClassifierStopReason, ImageContent, InputModality, ProviderHeaders,
    SystemOneImageInput,
};
use crate::utils::headers::merge_headers;

#[derive(Debug, Clone)]
pub struct SystemOneWireRequest {
    /// Context state, or image references followed by the state when images
    /// are present.
    pub state: Value,
    pub questions: Map<String, Value>,
    pub assets: Option<std::collections::BTreeMap<String, Value>>,
    pub images: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy)]
pub struct ResolvedSystemOneCompat {
    pub image_input: SystemOneImageInput,
    pub send_image_opt_in_header: bool,
}

/// Differences between services that serve System One models.
pub struct SystemOneTransport {
    pub api: &'static str,
    pub label: &'static str,
    pub detect_compat: fn(&ClassifierModel) -> ResolvedSystemOneCompat,
    pub url: fn(&ClassifierModel) -> Result<Url, String>,
    pub payload: fn(&ClassifierModel, SystemOneWireRequest) -> Value,
    pub output: fn(&Value) -> Result<Value, String>,
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

/// Maps public `bool` questions to TypeSafe's wire-level `noul` type.
fn wire_request(context: &ClassifierContext) -> SystemOneWireRequest {
    let state = Value::Object(context.state.clone());
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
    SystemOneWireRequest {
        state,
        questions,
        assets: None,
        images: None,
    }
}

const MAX_DATA_URL_IMAGES: usize = 4;

fn add_images(
    label: &str,
    model: &ClassifierModel,
    input: SystemOneImageInput,
    images: &[ImageContent],
    request: &mut SystemOneWireRequest,
) -> Result<(), String> {
    match input {
        SystemOneImageInput::Images => {
            if images.len() > MAX_DATA_URL_IMAGES {
                return Err(format!(
                    "{label} accepts at most {MAX_DATA_URL_IMAGES} images for {}, got {}",
                    model.id,
                    images.len()
                ));
            }
            let mut encoded = Vec::with_capacity(images.len());
            for image in images {
                if !matches!(
                    image.mime_type.as_str(),
                    "image/png" | "image/jpeg" | "image/webp"
                ) {
                    return Err(format!(
                        "{label} accepts PNG, JPEG, and WebP images for {}, got {}",
                        model.id, image.mime_type
                    ));
                }
                encoded.push(format!("data:{};base64,{}", image.mime_type, image.data));
            }
            request.images = Some(encoded);
        }
        SystemOneImageInput::StateImageUrls => {
            let mut parts: Vec<Value> = images
                .iter()
                .map(|image| {
                    json!({
                        "type": "image_url",
                        "image_url": {
                            "url": format!("data:{};base64,{}", image.mime_type, image.data)
                        }
                    })
                })
                .collect();
            parts.push(request.state.clone());
            request.state = Value::Array(parts);
        }
        SystemOneImageInput::Assets => {
            let mut assets = std::collections::BTreeMap::new();
            for (index, image) in images.iter().enumerate() {
                let format = match image.mime_type.as_str() {
                    "image/png" => "png",
                    "image/jpeg" => "jpeg",
                    other => {
                        return Err(format!(
                            "{label} accepts PNG and JPEG images for {}, got {other}",
                            model.id
                        ));
                    }
                };
                assets.insert(
                    format!("<image-{}>", index + 1),
                    json!({
                        "type": "image",
                        "format": format,
                        "data_base64": image.data,
                    }),
                );
            }
            let mut parts: Vec<Value> = assets
                .keys()
                .map(|key| Value::String(key.clone()))
                .collect();
            parts.push(request.state.clone());
            request.assets = Some(assets);
            request.state = Value::Array(parts);
        }
    }
    Ok(())
}

/// Runs one System One classification over the given transport. Never rejects.
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
    let mut request = wire_request(context);
    let mut request_options = options.cloned();
    if let Some(images) = context.images.as_ref().filter(|images| !images.is_empty()) {
        if !model.input.contains(&InputModality::Image) {
            return Err(format!(
                "{} does not support image input for {}",
                transport.label, model.id
            ));
        }
        let detected = (transport.detect_compat)(model);
        let image_input = model
            .compat
            .as_ref()
            .and_then(|compat| compat.image_input)
            .unwrap_or(detected.image_input);
        add_images(transport.label, model, image_input, images, &mut request)?;
        let send_opt_in = model
            .compat
            .as_ref()
            .and_then(|compat| compat.send_image_opt_in_header)
            .unwrap_or(detected.send_image_opt_in_header);
        if send_opt_in {
            let opt_in: ProviderHeaders = [(
                "TypeSafe-Danger-Zone-Images".to_owned(),
                Some("true".to_owned()),
            )]
            .into_iter()
            .collect();
            let mut options = request_options.unwrap_or_default();
            options.headers = merge_headers(Some(&opt_in), options.headers.as_ref());
            request_options = Some(options);
        }
    }
    let url = (transport.url)(model)?;
    let payload = (transport.payload)(model, request);
    let (body, _) = post_classifier_request(
        transport.label,
        url,
        model,
        payload,
        request_options.as_ref(),
        &[],
    )
    .await
    .map_err(|error| error.message)?;
    let result = (transport.output)(&body)?;
    // Set before parsing answers: a request with malformed answers was still billed.
    output.usage = parse_classifier_usage(result.get("usage"), model);
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
    object.insert("state".to_owned(), request.state);
    object.insert("questions".to_owned(), Value::Object(request.questions));
    if let Some(assets) = request.assets {
        object.insert(
            "assets".to_owned(),
            Value::Object(assets.into_iter().collect()),
        );
    }
    if let Some(images) = request.images {
        object.insert(
            "images".to_owned(),
            Value::Array(images.into_iter().map(Value::String).collect()),
        );
    }
    Value::Object(object)
}
