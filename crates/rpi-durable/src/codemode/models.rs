//! `models.*` for code mode scripts (port of
//! `packages/durable/src/codemode/models.ts` @ `4a080252f`).
//!
//! The catalog functions are host globals; the paid classifier and image calls
//! are internal tools of the extension, hidden from scripts and turned from a
//! script's global call into a nested durable call by the host. rpi-durable
//! reuses [`rpi_ai::models::Models`] for both.
//!
//! Catalog entries are returned without their `headers` field: configured
//! headers can carry credentials.

use std::sync::Arc;

use async_trait::async_trait;
use rpi_ai::models::Models;
use rpi_ai::types::{
    AnyModel, ClassifierContext, ClassifierModel, ClassifierStopReason, ImageModel, ImagesContext,
    ImagesStopReason, ModelType, Tool, ToolResultContent, Usage,
};
use serde_json::{Value, json};
use tokio::sync::Semaphore;

use crate::chord::context::Context;
use crate::chord::json::JsonObject;
use crate::harness::types::{
    DiagnosticSeverity, ToolCaller, ToolDiagnostic, ToolExecute, ToolExecuteError,
    ToolExecutionApi, ToolExecutionResult, ToolRegistration, ToolReplay,
};

/// Reserved names of code mode's internal tools; they are never listed to
/// scripts or the model.
pub const CODEMODE_INTERNAL_PREFIX: &str = "pi.codemode.";
/// Internal tool behind `models.classify()`.
pub const CLASSIFY_TOOL: &str = "pi.codemode.models.classify";
/// Internal tool behind `models.generateImages()`.
pub const GENERATE_IMAGES_TOOL: &str = "pi.codemode.models.generateImages";

/// The global a script calls, by internal tool.
pub const MODEL_CALLS: [(&str, &str); 2] = [
    ("models.classify", CLASSIFY_TOOL),
    ("models.generateImages", GENERATE_IMAGES_TOOL),
];

/// The catalog functions, host globals of the `models` namespace.
pub const CATALOG_GLOBALS: [&str; 3] = [
    "models.getModelsOfType",
    "models.getAvailableOfType",
    "models.getModelOfType",
];

/// `models.classify()` and `models.generateImages()` calls one script may have
/// in flight; the rest wait.
pub const MAX_CONCURRENT_MODEL_CALLS: usize = 4;

/// What the model reads about `models.*`.
pub const MODELS_DESCRIPTION: &str = "`models` (all functions return promises):\n- `models.getModelsOfType(type, provider?)`, `models.getAvailableOfType(type, provider?)` (configured auth only), `models.getModelOfType(type, provider, id)`: the catalog; `type` is \"chat\", \"image\", or \"classifier\".\n- `models.classify(model, { state: { ... }, images?: [{ type: \"image\", data, mimeType }], questions: { <id>: { type: \"choice\", instructions, criteria: { <label>: <meaning> } } | { type: \"score\", instructions, criteria: [<lowest level>, ..., <highest level>] } | { type: \"bool\", instructions, criteria: { true: <meaning>, false: <meaning> } } } })` resolves to `{ answers: { <id>: ... }, stopReason, errorMessage?, usage? }`.\n- `models.generateImages(model, { input: [{ type: \"text\", text: <prompt> }, ...optional { type: \"image\", data, mimeType }] })` resolves to `{ output: [image blocks], stopReason, errorMessage?, usage? }`; show each image with `image(block)`.\n- At most 4 classify/generateImages calls run at once; more wait.";

/// The internal tool behind a global call.
pub fn model_tool_for(global: &str) -> Option<&'static str> {
    MODEL_CALLS
        .iter()
        .find_map(|(name, tool)| (*name == global).then_some(*tool))
}

/// `models.classify(model, context)` or `models.generateImages(model, context)`
/// arguments from the script's two-element argument list.
pub fn model_call_arguments(args: &Value) -> JsonObject {
    let list = args.as_array().cloned().unwrap_or_default();
    let model = list.first().cloned().unwrap_or(Value::Null);
    let context = list.get(1).cloned().unwrap_or(Value::Null);
    let mut object = JsonObject::new();
    object.insert("model".to_string(), model);
    object.insert("context".to_string(), context);
    object
}

/// A script value in an error message: `undefined`, `a string`, `an array`, or
/// its keys (`{ prompt }`).
fn describe_value(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(_) => "a boolean".to_string(),
        Value::Number(_) => "a number".to_string(),
        Value::String(_) => "a string".to_string(),
        Value::Array(items) if items.is_empty() => "an empty array".to_string(),
        Value::Array(_) => "an array".to_string(),
        Value::Object(object) if object.is_empty() => "{}".to_string(),
        Value::Object(object) => {
            let keys: Vec<&str> = object.keys().take(6).map(String::as_str).collect();
            let suffix = if object.len() > 6 { ", ..." } else { "" };
            format!("{{ {}{suffix} }}", keys.join(", "))
        }
    }
}

fn to_model_type(value: &Value) -> Result<ModelType, String> {
    match value.as_str() {
        Some("chat") => Ok(ModelType::Chat),
        Some("image") => Ok(ModelType::Image),
        Some("classifier") => Ok(ModelType::Classifier),
        _ => Err(format!(
            "Unknown model type {}. Use \"chat\", \"image\", or \"classifier\".",
            serde_json::to_string(value).unwrap_or_else(|_| "null".to_string())
        )),
    }
}

fn to_provider(value: &Value) -> Result<Option<&str>, String> {
    match value {
        Value::Null => Ok(None),
        Value::String(provider) => Ok(Some(provider)),
        _ => Err("provider must be a string".to_string()),
    }
}

/// Catalog entry for scripts. `headers` is dropped because configured headers
/// can carry credentials.
fn to_model_info(model: &AnyModel) -> Value {
    let mut value = serde_json::to_value(model).unwrap_or(Value::Null);
    if let Value::Object(object) = &mut value {
        object.remove("headers");
    }
    value
}

/// The catalog functions: what a script's call of `name` with `args` (the
/// argument list) resolves to.
pub async fn catalog_call(name: &str, args: &Value, models: &Models) -> Result<Value, String> {
    let list = args.as_array().cloned().unwrap_or_default();
    let get = |index: usize| list.get(index).cloned().unwrap_or(Value::Null);
    let model_type = to_model_type(&get(0))?;
    let provider_value = get(1);
    let provider = to_provider(&provider_value)?;
    match name {
        "models.getModelsOfType" => Ok(Value::Array(
            models
                .get_models_of_type(model_type, provider)
                .iter()
                .map(to_model_info)
                .collect(),
        )),
        "models.getAvailableOfType" => {
            let available = models
                .get_available_of_type(model_type, provider)
                .await
                .map_err(|error| error.message)?;
            Ok(Value::Array(available.iter().map(to_model_info).collect()))
        }
        "models.getModelOfType" => {
            let (Value::String(provider), Value::String(id)) = (get(1), get(2)) else {
                let shown = list
                    .iter()
                    .take(3)
                    .map(describe_value)
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(format!(
                    "models.getModelOfType(type, provider, id) expects three strings, got ({shown}). The provider and the id are separate arguments."
                ));
            };
            Ok(models
                .get_model_of_type(model_type, &provider, &id)
                .map(|model| to_model_info(&model))
                .unwrap_or(Value::Null))
        }
        other => Err(format!("Unknown model catalog global {other}")),
    }
}

fn resolve_classifier(model: &Value, models: &Models) -> Result<ClassifierModel, String> {
    let list_hint =
        "List the classifier models you can use with models.getAvailableOfType(\"classifier\").";
    let (Some(provider), Some(id)) = (
        model.get("provider").and_then(Value::as_str),
        model.get("id").and_then(Value::as_str),
    ) else {
        return Err(format!(
            "models.classify() expects a classifier model as its first argument, got {}. models.getModelOfType() returns undefined for an unknown provider or id. {list_hint}",
            describe_value(model)
        ));
    };
    match models.get_model_of_type(ModelType::Classifier, provider, id) {
        Some(AnyModel::Classifier(model)) => Ok(model),
        _ => Err(format!(
            "Unknown classifier model \"{provider}/{id}\". {list_hint}"
        )),
    }
}

fn resolve_image(model: &Value, models: &Models) -> Result<ImageModel, String> {
    let list_hint = "List the image models you can use with models.getAvailableOfType(\"image\").";
    let (Some(provider), Some(id)) = (
        model.get("provider").and_then(Value::as_str),
        model.get("id").and_then(Value::as_str),
    ) else {
        return Err(format!(
            "models.generateImages() expects an image model as its first argument, got {}. models.getModelOfType() returns undefined for an unknown provider or id. {list_hint}",
            describe_value(model)
        ));
    };
    match models.get_model_of_type(ModelType::Image, provider, id) {
        Some(AnyModel::Image(model)) => Ok(model),
        _ => Err(format!(
            "Unknown image model \"{provider}/{id}\". {list_hint}"
        )),
    }
}

fn text_result(text: String) -> Vec<ToolResultContent> {
    vec![ToolResultContent::Text(rpi_ai::types::TextContent {
        text,
        text_signature: None,
    })]
}

/// A paid call that cannot run: a structured error result, never a silent
/// success.
fn model_error(message: String) -> ToolExecutionResult {
    ToolExecutionResult {
        output: Some(text_result(message.clone())),
        structured_output: Some(json!({ "error": message })),
        is_error: Some(true),
        diagnostics: Some(vec![ToolDiagnostic {
            severity: DiagnosticSeverity::Error,
            message,
            code: Some("codemode_models".to_string()),
        }]),
        ..Default::default()
    }
}

/// The result of a model call as the tool returns it: the whole result for the
/// script, usage counted by the Harness.
fn model_result(
    label: &str,
    stop_reason: &str,
    error_message: Option<&str>,
    usage: Option<Usage>,
    value: Value,
) -> ToolExecutionResult {
    let ok = stop_reason == "stop";
    let text = if ok {
        format!("{label} done")
    } else {
        format!("{label} {stop_reason}: {}", error_message.unwrap_or(""))
    };
    ToolExecutionResult {
        output: Some(text_result(text)),
        structured_output: Some(value),
        is_error: (!ok).then_some(true),
        usage,
        ..Default::default()
    }
}

fn classifier_stop_reason(reason: ClassifierStopReason) -> &'static str {
    match reason {
        ClassifierStopReason::Stop => "stop",
        ClassifierStopReason::Error => "error",
        ClassifierStopReason::Aborted => "aborted",
    }
}

fn images_stop_reason(reason: ImagesStopReason) -> &'static str {
    match reason {
        ImagesStopReason::Stop => "stop",
        ImagesStopReason::Error => "error",
        ImagesStopReason::Aborted => "aborted",
    }
}

/// Shared concurrency gate for the paid calls of one script.
#[derive(Clone)]
struct ModelGate(Arc<Semaphore>);

impl ModelGate {
    fn new() -> Self {
        Self(Arc::new(Semaphore::new(MAX_CONCURRENT_MODEL_CALLS)))
    }
}

struct ClassifyTool {
    gate: ModelGate,
}

#[async_trait]
impl ToolExecute for ClassifyTool {
    async fn execute(
        &self,
        args: JsonObject,
        api: &dyn ToolExecutionApi,
        _context: &Context,
    ) -> Result<ToolExecutionResult, ToolExecuteError> {
        let _permit = self
            .gate
            .0
            .acquire()
            .await
            .map_err(|_| -> ToolExecuteError { Box::new(ModelGateClosed) })?;
        let models: Models = api.models();
        let model_value = args.get("model").cloned().unwrap_or(Value::Null);
        let model = match resolve_classifier(&model_value, &models) {
            Ok(model) => model,
            Err(message) => return Ok(model_error(message)),
        };
        let context_value = args.get("context").cloned().unwrap_or(Value::Null);
        let classifier_context: ClassifierContext = match serde_json::from_value(context_value) {
            Ok(context) => context,
            Err(error) => {
                return Ok(model_error(format!(
                    "models.classify() context is invalid: {error}. Expected context: {{ state: {{ ... }}, images?: [...], questions: {{ ... }} }}."
                )));
            }
        };
        let result = models.classify(&model, &classifier_context, None).await;
        let value = serde_json::to_value(&result).unwrap_or(Value::Null);
        let stop = classifier_stop_reason(result.stop_reason);
        Ok(model_result(
            &format!("classify {}/{}", model.provider, model.id),
            stop,
            result.error_message.as_deref(),
            result.usage,
            value,
        ))
    }
}

struct GenerateImagesTool {
    gate: ModelGate,
}

#[async_trait]
impl ToolExecute for GenerateImagesTool {
    async fn execute(
        &self,
        args: JsonObject,
        api: &dyn ToolExecutionApi,
        _context: &Context,
    ) -> Result<ToolExecutionResult, ToolExecuteError> {
        let _permit = self
            .gate
            .0
            .acquire()
            .await
            .map_err(|_| -> ToolExecuteError { Box::new(ModelGateClosed) })?;
        let models: Models = api.models();
        let model_value = args.get("model").cloned().unwrap_or(Value::Null);
        let model = match resolve_image(&model_value, &models) {
            Ok(model) => model,
            Err(message) => return Ok(model_error(message)),
        };
        let context_value = args.get("context").cloned().unwrap_or(Value::Null);
        let images_context: ImagesContext = match serde_json::from_value(context_value) {
            Ok(context) => context,
            Err(error) => {
                return Ok(model_error(format!(
                    "models.generateImages() context is invalid: {error}. Expected context: {{ input: [ {{ type: \"text\", text: <prompt> }} ] }}."
                )));
            }
        };
        let result = models.generate_images(&model, &images_context, None).await;
        let value = serde_json::to_value(&result).unwrap_or(Value::Null);
        let stop = images_stop_reason(result.stop_reason);
        Ok(model_result(
            &format!("generateImages {}/{}", model.provider, model.id),
            stop,
            result.error_message.as_deref(),
            result.usage,
            value,
        ))
    }
}

#[derive(Debug, thiserror::Error)]
#[error("the model-call gate is closed")]
struct ModelGateClosed;

fn model_parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "model": {},
            "context": {}
        },
        "required": ["model", "context"]
    })
}

fn tool(name: &str, description: &str, execute: Arc<dyn ToolExecute>) -> ToolRegistration {
    ToolRegistration {
        tool: Tool {
            name: name.to_string(),
            description: description.to_string(),
            parameters: model_parameters(),
            constrained_sampling: None,
        },
        replay: ToolReplay::Unsafe,
        execution_mode: None,
        prepare_arguments: None,
        output_limits: None,
        structured_output_schema: Some(json!({})),
        callers: Some(vec![ToolCaller::Tools]),
        deferred: false,
        execute,
    }
}

/// The internal tools behind `models.classify()` and `models.generateImages()`.
pub fn create_model_tools() -> Vec<ToolRegistration> {
    let gate = ModelGate::new();
    vec![
        tool(
            CLASSIFY_TOOL,
            "models.classify() of code mode scripts.",
            Arc::new(ClassifyTool { gate: gate.clone() }),
        ),
        tool(
            GENERATE_IMAGES_TOOL,
            "models.generateImages() of code mode scripts.",
            Arc::new(GenerateImagesTool { gate }),
        ),
    ]
}
