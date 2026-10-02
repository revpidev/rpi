//! Runs one codemode script (port of
//! `packages/coding-agent/src/extensions/codemode/execute.ts` @ a13d35a74).
//! Split from `tool.rs` so the sandbox runtime only loads when a script runs.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use futures::future::BoxFuture;
use rpi_agent::types::AgentToolResult;
use rpi_ai::types::{AssistantImages, ClassifierResult, ToolResultContent, Usage};
use rpi_codemode::{
    CodemodeErrorKind, CodemodeExecuteOptions, CodemodeOutputItem, CodemodeResult, CodemodeSandbox,
    CodemodeSandboxOptions, CodemodeTimeout, CodemodeTool, CodemodeToolContext,
    parse_codemode_source, render_tool_sample, to_codemode_identifier,
};
use rpi_ext_host::api::{ExecuteToolOptions, ExtensionApi, ExtensionContext};
use rpi_ext_host::types::{SessionEntryInfo, ToolExecuteRequest};
use serde_json::{Value, json};

use crate::extensions::codemode::tool::{
    CODEMODE_STORE_ENTRY_TYPE, CodemodeMode, ToolInfo, callable_tools, to_codemode_declaration,
};
use crate::extensions::tool_search::{
    Bm25Ranker, DEFAULT_TOOL_SEARCH_LIMIT, create_tool_search_document,
};

const ARGS_PREVIEW_CHARS: usize = 200;
const ERROR_PREVIEW_CHARS: usize = 500;
/// `models.*` calls one script may have in flight (execute.ts:42).
const MAX_CONCURRENT_MODEL_CALLS: usize = 4;
/// Heap limit for the QuickJS VM (execute.ts:48).
const CODEMODE_MEMORY_LIMIT_BYTES: usize = 256 * 1024 * 1024;
const DEFAULT_MAX_OUTPUT_TOKENS: usize = 10_000;
const CHARS_PER_TOKEN: usize = 4;
const MODEL_TYPES: [&str; 3] = ["chat", "image", "classifier"];

/// Options captured by the tool definition; `Clone` so the loadout hook and
/// the execute closure each own a copy.
#[derive(Clone)]
pub struct CodemodeToolOptions {
    pub api: ExtensionApi,
    pub model_runtime: Arc<crate::core::model_runtime::ModelRuntime>,
    pub models: bool,
    pub get_mode: Arc<dyn Fn() -> CodemodeMode + Send + Sync>,
    pub get_inline_budget: Arc<dyn Fn() -> Option<u64> + Send + Sync>,
}

/// `CodemodeNestedCall` (tool.ts:105-117): the renderer-facing call row.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NestedCall {
    /// Tool call id of the nested call, `<codemode call id>/<n>`.
    pub id: String,
    pub name: String,
    /// Compact JSON of the arguments, truncated for display.
    pub args: String,
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Cost in USD of a `models.*` call that reported usage.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<f64>,
}

fn truncate_text(text: &str, max_chars: usize) -> String {
    if text.chars().count() > max_chars {
        let head: String = text.chars().take(max_chars.saturating_sub(3)).collect();
        format!("{head}...")
    } else {
        text.to_owned()
    }
}

fn preview_args(args: &Value) -> String {
    truncate_text(&args.to_string(), ARGS_PREVIEW_CHARS)
}

fn text_of(result: &AgentToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|block| match block {
            ToolResultContent::Text(text) => Some(text.text.clone()),
            ToolResultContent::Image(_) => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn model_type_of(name: &str) -> rpi_ai::types::ModelType {
    match name {
        "image" => rpi_ai::types::ModelType::Image,
        "classifier" => rpi_ai::types::ModelType::Classifier,
        _ => rpi_ai::types::ModelType::Chat,
    }
}

fn to_model_type(value: &Value) -> Result<&'static str, String> {
    if let Some(text) = value.as_str()
        && MODEL_TYPES.contains(&text)
    {
        return Ok(match text {
            "chat" => "chat",
            "image" => "image",
            _ => "classifier",
        });
    }
    Err(format!(
        "Unknown model type {}. Use \"chat\", \"image\", or \"classifier\".",
        value
    ))
}

fn to_provider(value: Option<&Value>) -> Result<Option<String>, String> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(provider)) => Ok(Some(provider.clone())),
        Some(_) => Err("provider must be a string".to_owned()),
    }
}

/// Catalog entry for scripts; `headers` is dropped because models.json
/// headers can carry credentials (execute.ts:62-66).
fn to_model_info(model: &rpi_ai::types::AnyModel) -> Value {
    let mut info = serde_json::to_value(model).unwrap_or(Value::Null);
    if let Some(object) = info.as_object_mut() {
        object.remove("headers");
    }
    info
}

/// `an image`, `a classifier`.
fn with_article(word: &str) -> String {
    let article = if word.starts_with(['a', 'e', 'i', 'o', 'u']) {
        "an"
    } else {
        "a"
    };
    format!("{article} {word}")
}

fn describe_value(value: &Value) -> String {
    match value {
        Value::Null => "null".to_owned(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::String(_) => "a string".to_owned(),
        Value::Array(values) => {
            if values.is_empty() {
                "an empty array".to_owned()
            } else {
                "an array".to_owned()
            }
        }
        Value::Object(object) => {
            if object.is_empty() {
                "{}".to_owned()
            } else {
                let keys: Vec<&str> = object.keys().take(6).map(String::as_str).collect();
                let suffix = if object.len() > 6 { ", ..." } else { "" };
                format!("{{ {}{} }}", keys.join(", "), suffix)
            }
        }
    }
}

const CLASSIFIER_CONTEXT_SHAPE: &str = "{ state: { ... }, questions: { <id>: { type: \"choice\", instructions, criteria: { <label>: <meaning> } } | { type: \"score\", instructions, criteria: [<lowest level>, ..., <highest level>] } | { type: \"bool\", instructions, criteria: { true: <meaning>, false: <meaning> } } } }";

/// Check a script's classifier context (execute.ts:86-126).
fn check_classifier_context(value: &Value) -> Result<rpi_ai::types::ClassifierContext, String> {
    let fail = |problem: &str| {
        format!(
            "models.classify() {problem}. Expected context: {CLASSIFIER_CONTEXT_SHAPE}. See \"Classify\" in {}.",
            crate::extensions::codemode::description::codemode_docs_path()
        )
    };
    let Some(context) = value.as_object() else {
        return Err(fail(&format!(
            "expects a context object as its second argument, got {}",
            describe_value(value)
        )));
    };
    let Some(state) = context.get("state").and_then(Value::as_object) else {
        return Err(fail(&format!(
            "context.state must be an object, got {}",
            describe_value(context.get("state").unwrap_or(&Value::Null))
        )));
    };
    let Some(questions) = context.get("questions").and_then(Value::as_object) else {
        return Err(fail(&format!(
            "context.questions must map question IDs to questions, got {}",
            describe_value(context.get("questions").unwrap_or(&Value::Null))
        )));
    };
    if questions.is_empty() {
        return Err(fail(
            "context.questions must map question IDs to questions, got {}",
        ));
    }
    let is_strings =
        |values: &[&Value]| !values.is_empty() && values.iter().all(|value| value.is_string());
    let mut parsed = std::collections::BTreeMap::new();
    for (id, question) in questions {
        let at = format!("context.questions.{id}");
        let Some(question) = question.as_object() else {
            return Err(fail(&format!(
                "{at} must be a question object, got {}",
                describe_value(question)
            )));
        };
        let instructions = question
            .get("instructions")
            .and_then(Value::as_str)
            .ok_or_else(|| fail(&format!("{at}.instructions must be a string")))?;
        let criteria = question.get("criteria").unwrap_or(&Value::Null);
        let parsed_question = match question.get("type").and_then(Value::as_str) {
            Some("choice") => {
                let Some(criteria) = criteria.as_object() else {
                    return Err(fail(&format!(
                        "{at} is a \"choice\" question, so criteria must map each label to its meaning"
                    )));
                };
                if !is_strings(&criteria.values().collect::<Vec<_>>()) {
                    return Err(fail(&format!(
                        "{at} is a \"choice\" question, so criteria must map each label to its meaning"
                    )));
                }
                rpi_ai::types::ClassifierQuestion::Choice(rpi_ai::types::ClassifierChoiceQuestion {
                    instructions: instructions.to_owned(),
                    criteria: criteria
                        .iter()
                        .map(|(key, value)| {
                            (key.clone(), value.as_str().unwrap_or_default().to_owned())
                        })
                        .collect(),
                })
            }
            Some("score") => {
                let Some(criteria) = criteria.as_array() else {
                    return Err(fail(&format!(
                        "{at} is a \"score\" question, so criteria must list the levels as strings, lowest first"
                    )));
                };
                if !is_strings(&criteria.iter().collect::<Vec<_>>()) {
                    return Err(fail(&format!(
                        "{at} is a \"score\" question, so criteria must list the levels as strings, lowest first"
                    )));
                }
                rpi_ai::types::ClassifierQuestion::Score(rpi_ai::types::ClassifierScoreQuestion {
                    instructions: instructions.to_owned(),
                    criteria: criteria
                        .iter()
                        .map(|value| value.as_str().unwrap_or_default().to_owned())
                        .collect(),
                })
            }
            Some("bool") => {
                let Some(criteria) = criteria.as_object() else {
                    return Err(fail(&format!(
                        "{at} is a \"bool\" question, so criteria must be {{ true: string, false: string }}"
                    )));
                };
                let (Some(yes), Some(no)) = (
                    criteria.get("true").and_then(Value::as_str),
                    criteria.get("false").and_then(Value::as_str),
                ) else {
                    return Err(fail(&format!(
                        "{at} is a \"bool\" question, so criteria must be {{ true: string, false: string }}"
                    )));
                };
                rpi_ai::types::ClassifierQuestion::Bool(rpi_ai::types::ClassifierBoolQuestion {
                    instructions: instructions.to_owned(),
                    criteria: rpi_ai::types::ClassifierBoolCriteria {
                        yes: yes.to_owned(),
                        no: no.to_owned(),
                    },
                })
            }
            Some(other) => {
                return Err(fail(&format!(
                    "{at}.type must be \"choice\", \"score\", or \"bool\", got {other:?}"
                )));
            }
            None => {
                return Err(fail(&format!(
                    "{at}.type must be \"choice\", \"score\", or \"bool\", got null"
                )));
            }
        };
        parsed.insert(id.clone(), parsed_question);
    }
    Ok(rpi_ai::types::ClassifierContext {
        state: state.clone(),
        questions: parsed,
    })
}

/// Check a script's image context (execute.ts:129-151).
fn check_images_context(value: &Value) -> Result<rpi_ai::types::ImagesContext, String> {
    let docs = crate::extensions::codemode::description::codemode_docs_path();
    let fail = |problem: &str| {
        format!(
            "models.generateImages() {problem}. Expected context: {{ input: [{{ type: \"text\", text: <prompt> }}, ...optional {{ type: \"image\", data: <base64>, mimeType }} references] }}. See \"Generate images\" in {docs}."
        )
    };
    let Some(context) = value.as_object() else {
        return Err(fail(&format!(
            "expects a context object as its second argument, got {}",
            describe_value(value)
        )));
    };
    let Some(input) = context.get("input").and_then(Value::as_array) else {
        return Err(fail(&format!(
            "context.input must be a non-empty array of blocks, got {}",
            describe_value(context.get("input").unwrap_or(&Value::Null))
        )));
    };
    if input.is_empty() {
        return Err(fail(&format!(
            "context.input must be a non-empty array of blocks, got {}",
            describe_value(context.get("input").unwrap_or(&Value::Null))
        )));
    }
    let mut blocks = Vec::new();
    for (index, block) in input.iter().enumerate() {
        let Some(object) = block.as_object() else {
            return Err(fail(&format!(
                "context.input[{index}] must be a text or image block, got {}",
                describe_value(block)
            )));
        };
        match object.get("type").and_then(Value::as_str) {
            Some("text") if object.get("text").and_then(Value::as_str).is_some() => {
                blocks.push(rpi_ai::types::ImagesInputContent::Text(
                    rpi_ai::types::TextContent {
                        text: object
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                        text_signature: None,
                    },
                ));
            }
            Some("image")
                if object.get("data").and_then(Value::as_str).is_some()
                    && object.get("mimeType").and_then(Value::as_str).is_some() =>
            {
                blocks.push(rpi_ai::types::ImagesInputContent::Image(
                    rpi_ai::types::ImageContent {
                        data: object
                            .get("data")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                        mime_type: object
                            .get("mimeType")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                    },
                ));
            }
            _ => {
                return Err(fail(&format!(
                    "context.input[{index}] must be a text or image block, got {}",
                    describe_value(block)
                )));
            }
        }
    }
    Ok(rpi_ai::types::ImagesContext { input: blocks })
}

fn format_call_summary(calls: &[NestedCall]) -> String {
    if calls.is_empty() {
        return "No tool calls were made.".to_owned();
    }
    format!(
        "Tool calls made before the failure (they are not undone): {}",
        calls
            .iter()
            .map(|call| format!("{} ({})", call.name, call.status))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn format_error(result: &CodemodeResult, calls: &[NestedCall]) -> String {
    let CodemodeResult::Err { error, .. } = result else {
        return String::new();
    };
    let head = match error.kind {
        CodemodeErrorKind::Script => error.stack.clone().unwrap_or_else(|| {
            format!(
                "{}: {}",
                error.name.as_deref().unwrap_or("Error"),
                error.message
            )
        }),
        CodemodeErrorKind::Timeout => format!("Script timed out: {}", error.message),
        CodemodeErrorKind::Aborted => format!("Script aborted: {}", error.message),
        CodemodeErrorKind::Sandbox => format!("Script sandbox failed: {}", error.message),
    };
    format!("{head}\n\n{}", format_call_summary(calls))
}

/// Like the script's `text()`: strings as is, other values as compact JSON.
fn value_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// Write the full text output to a temp file, like bash does for truncated
/// output.
fn spill_output(text: &str) -> Result<std::path::PathBuf, String> {
    let unique = format!(
        "rpi-codemode-{:016x}.txt",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
            ^ (std::process::id() as u128)
    );
    let path = std::env::temp_dir().join(unique);
    std::fs::write(&path, text).map_err(|error| error.to_string())?;
    Ok(path)
}

/// `truncateOutput` (execute.ts:284-320).
fn truncate_output(
    items: Vec<ToolResultContent>,
    max_tokens: usize,
) -> (Vec<ToolResultContent>, Option<String>) {
    let texts: Vec<String> = items
        .iter()
        .filter_map(|item| match item {
            ToolResultContent::Text(text) => Some(text.text.clone()),
            ToolResultContent::Image(_) => None,
        })
        .collect();
    let combined = texts.join("\n");
    let budget = max_tokens * CHARS_PER_TOKEN;
    if texts.is_empty() || combined.chars().count() <= budget {
        return (items, None);
    }
    let head_chars = budget / 2;
    let tail_chars = budget - head_chars;
    let total_chars = combined.chars().count();
    let removed = total_chars.saturating_sub(head_chars + tail_chars);
    let head: String = combined.chars().take(head_chars).collect();
    let tail: String = combined
        .chars()
        .rev()
        .take(tail_chars)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let mut text = format!(
        "Warning: truncated output (original token count: {})\nTotal output lines: {}\n\n{head}…{} tokens truncated…{tail}",
        total_chars.div_ceil(CHARS_PER_TOKEN),
        combined.split('\n').count(),
        removed.div_ceil(CHARS_PER_TOKEN)
    );
    let mut full_output_path = None;
    match spill_output(&combined) {
        Ok(path) => {
            text.push_str(&format!(
                "\n\n[Full output: {} (read with offset/limit)]",
                path.display()
            ));
            full_output_path = Some(path.to_string_lossy().into_owned());
        }
        Err(error) => {
            text.push_str(&format!("\n\n[Could not save the full output: {error}]"));
        }
    }
    let mut output: Vec<ToolResultContent> =
        vec![ToolResultContent::Text(rpi_ai::types::TextContent {
            text,
            text_signature: None,
        })];
    output.extend(
        items
            .into_iter()
            .filter(|item| matches!(item, ToolResultContent::Image(_))),
    );
    (output, full_output_path)
}

/// Values of `load()`: the `codemode-store` entries on the branch, applied
/// from the root (execute.ts:225-236).
fn read_codemode_store(entries: &[SessionEntryInfo]) -> Value {
    let mut store: serde_json::Map<String, Value> = serde_json::Map::new();
    for entry in entries {
        if entry.custom_type != CODEMODE_STORE_ENTRY_TYPE {
            continue;
        }
        let Some(data) = entry.data.as_object() else {
            continue;
        };
        if let Some(deleted) = data.get("delete").and_then(Value::as_array) {
            for key in deleted.iter().filter_map(Value::as_str) {
                store.remove(key);
            }
        }
        if let Some(set) = data.get("set").and_then(Value::as_object) {
            for (key, value) in set {
                store.insert(key.clone(), value.clone());
            }
        }
    }
    Value::Object(store)
}

/// `isNamespaceName` (execute.ts:435-441).
fn namespace_suffix(name: &str) -> Option<&str> {
    name.rfind("__")
        .map(|index| &name[index + 2..])
        .filter(|value| !value.is_empty())
}

fn is_namespace_name(namespace: &str, query: &str) -> bool {
    let id = to_codemode_identifier(namespace);
    let query_id = to_codemode_identifier(query);
    namespace == query
        || id == query_id
        || namespace_suffix(namespace) == Some(query)
        || namespace_suffix(&id) == Some(query_id.as_str())
}

fn sample_map(tools: &[ToolInfo]) -> HashMap<String, String> {
    tools
        .iter()
        .map(|tool| {
            (
                tool.name.clone(),
                render_tool_sample(&to_codemode_declaration(tool)),
            )
        })
        .collect()
}

/// `searchTools()`, `describeTool()`, and `describeNamespace()`
/// (execute.ts:443-517).
fn create_discovery_globals(
    tools: &[ToolInfo],
    samples: &HashMap<String, String>,
) -> Vec<CodemodeTool> {
    let tools_for_search = tools.to_vec();
    let samples_for_search = samples.clone();
    let tools_for_describe = tools.to_vec();
    let samples_for_describe = samples.clone();
    let tools_for_namespace = tools.to_vec();
    let entry = |name: &str, samples: &HashMap<String, String>| {
        (
            to_codemode_identifier(name),
            samples.get(name).cloned().unwrap_or_default(),
        )
    };
    vec![
        CodemodeTool {
            name: "searchTools".to_owned(),
            description: None,
            input_schema: None,
            output_schema: None,
            spread: true,
            signature: None,
            execute: Arc::new(move |args: Value, _ctx: CodemodeToolContext| {
                let tools = tools_for_search.clone();
                let samples = samples_for_search.clone();
                Box::pin(async move {
                    let array = args.as_array().cloned().unwrap_or_default();
                    let query = array
                        .first()
                        .and_then(Value::as_str)
                        .ok_or_else(|| "searchTools() expects a query string".to_owned())?;
                    let options = array.get(1).and_then(Value::as_object);
                    let limit = match options.and_then(|options| options.get("limit")) {
                        None | Some(Value::Null) => DEFAULT_TOOL_SEARCH_LIMIT,
                        Some(value) => value
                            .as_f64()
                            .filter(|number| number.fract() == 0.0 && *number > 0.0)
                            .map(|number| number as usize)
                            .ok_or_else(|| {
                                "searchTools() limit must be a positive integer".to_owned()
                            })?,
                    };
                    let namespace = match options.and_then(|options| options.get("namespace")) {
                        None | Some(Value::Null) => None,
                        Some(Value::String(namespace)) => Some(namespace.clone()),
                        Some(_) => {
                            return Err("searchTools() namespace must be a string".to_owned());
                        }
                    };
                    let documents: Vec<_> = tools
                        .iter()
                        .filter(|tool| match &namespace {
                            None => true,
                            Some(query) => tool
                                .namespace
                                .as_ref()
                                .is_some_and(|namespace| is_namespace_name(&namespace.name, query)),
                        })
                        .map(|tool| {
                            create_tool_search_document(
                                &tool.name,
                                &tool.description,
                                &tool.parameters,
                                tool.namespace.as_ref(),
                            )
                        })
                        .collect();
                    Ok(Value::Array(
                        Bm25Ranker::new()
                            .rank(query, &documents, limit)
                            .into_iter()
                            .map(|matched| {
                                let (name, description) = entry(&matched.name, &samples);
                                json!({ "name": name, "description": description })
                            })
                            .collect(),
                    ))
                })
            }),
        },
        CodemodeTool {
            name: "describeTool".to_owned(),
            description: None,
            input_schema: None,
            output_schema: None,
            spread: true,
            signature: None,
            execute: Arc::new(move |args: Value, _ctx: CodemodeToolContext| {
                let tools = tools_for_describe.clone();
                let samples = samples_for_describe.clone();
                Box::pin(async move {
                    let name = args
                        .as_array()
                        .and_then(|array| array.first())
                        .and_then(Value::as_str)
                        .ok_or_else(|| "describeTool() expects a tool name".to_owned())?;
                    let tool = tools.iter().find(|tool| {
                        tool.name == name || to_codemode_identifier(&tool.name) == name
                    });
                    Ok(tool
                        .and_then(|tool| samples.get(&tool.name).cloned())
                        .map(Value::String)
                        .unwrap_or(Value::Null))
                })
            }),
        },
        CodemodeTool {
            name: "describeNamespace".to_owned(),
            description: None,
            input_schema: None,
            output_schema: None,
            spread: true,
            signature: None,
            execute: Arc::new(move |args: Value, _ctx: CodemodeToolContext| {
                let tools = tools_for_namespace.clone();
                Box::pin(async move {
                    let name = args
                        .as_array()
                        .and_then(|array| array.first())
                        .and_then(Value::as_str)
                        .ok_or_else(|| "describeNamespace() expects a namespace name".to_owned())?;
                    let mut namespace: Option<rpi_ext_host::types::ToolNamespace> = None;
                    let mut names: Vec<String> = Vec::new();
                    for tool in &tools {
                        let Some(tool_namespace) = tool.namespace.as_ref() else {
                            continue;
                        };
                        if !is_namespace_name(&tool_namespace.name, name) {
                            continue;
                        }
                        if namespace.is_none() {
                            namespace = Some(tool_namespace.clone());
                        }
                        names.push(to_codemode_identifier(&tool.name));
                    }
                    let Some(namespace) = namespace else {
                        return Ok(Value::Null);
                    };
                    let mut object = serde_json::Map::new();
                    object.insert("name".to_owned(), Value::String(namespace.name.clone()));
                    if let Some(description) = namespace.description.clone() {
                        object.insert("description".to_owned(), Value::String(description));
                    }
                    if let Some(instructions) = namespace.instructions.clone() {
                        object.insert("instructions".to_owned(), Value::String(instructions));
                    }
                    object.insert(
                        "tools".to_owned(),
                        Value::Array(names.into_iter().map(Value::String).collect()),
                    );
                    Ok(Value::Object(object))
                })
            }),
        },
    ]
}

/// Checked `models.*` context.
enum CheckedContext {
    Classifier(rpi_ai::types::ClassifierContext),
    Images(rpi_ai::types::ImagesContext),
}

type CheckFn = Arc<dyn Fn(&Value) -> Result<CheckedContext, String> + Send + Sync>;
type RunFn =
    Arc<dyn Fn(rpi_ai::types::AnyModel, CheckedContext) -> BoxFuture<'static, Value> + Send + Sync>;
type RunModelCallFn = Arc<
    dyn Fn(
            &'static str,
            &'static str,
            Vec<Value>,
            CheckFn,
            RunFn,
        ) -> BoxFuture<'static, Result<Value, String>>
        + Send
        + Sync,
>;

/// `models.*` for scripts (execute.ts:519-630).
fn create_model_globals(
    models: Arc<crate::core::model_runtime::ModelRuntime>,
    tool_call_id: String,
    calls: Arc<std::sync::Mutex<Vec<NestedCall>>>,
    publish: Arc<dyn Fn() + Send + Sync>,
    add_usage: Arc<dyn Fn(Usage) + Send + Sync>,
    add_generated_images: Arc<dyn Fn(usize) + Send + Sync>,
) -> Vec<CodemodeTool> {
    let limit = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_MODEL_CALLS));
    let call_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let run_model_call: RunModelCallFn = {
        let models = models.clone();
        let calls = calls.clone();
        let publish = publish.clone();
        let add_usage = add_usage.clone();
        let limit = limit.clone();
        let call_count = call_count.clone();
        let tool_call_id = tool_call_id.clone();
        Arc::new(move |name, model_type, args, check, run| {
            let models = models.clone();
            let calls = calls.clone();
            let publish = publish.clone();
            let add_usage = add_usage.clone();
            let limit = limit.clone();
            let count = call_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            let record_id = format!("{tool_call_id}/{name}/{count}");
            Box::pin(async move {
                let list_hint = format!(
                    "List the {model_type} models you can use with models.getAvailableOfType(\"{model_type}\")."
                );
                let model = args.first().cloned().unwrap_or(Value::Null);
                let fail_first = |detail: &str| {
                    let undefined_hint =
                        " models.getModelOfType() returns undefined for an unknown provider or id.";
                    format!(
                        "{name}() expects {} model as its first argument, got {detail}.{undefined_hint} {list_hint}",
                        with_article(model_type)
                    )
                };
                let (Some(provider), Some(id)) = model
                    .as_object()
                    .map(|object| {
                        (
                            object.get("provider").and_then(Value::as_str),
                            object.get("id").and_then(Value::as_str),
                        )
                    })
                    .unwrap_or((None, None))
                else {
                    return Err(fail_first(&describe_value(&model)));
                };
                let ref_name = format!("{provider}/{id}");
                let Some(resolved) =
                    models.get_model_of_type(model_type_of(model_type), provider, id)
                else {
                    let actual_type = MODEL_TYPES
                        .iter()
                        .filter(|other| **other != model_type)
                        .find(|other| {
                            models
                                .get_model_of_type(model_type_of(other), provider, id)
                                .is_some()
                        });
                    return Err(match actual_type {
                        Some(actual) => format!(
                            "\"{ref_name}\" is {} model, not {} model. {list_hint}",
                            with_article(actual),
                            with_article(model_type)
                        ),
                        None => {
                            format!("Unknown {model_type} model \"{ref_name}\". {list_hint}")
                        }
                    });
                };
                let checked = check(args.get(1).unwrap_or(&Value::Null))?;

                let index = {
                    let mut calls = calls.lock().unwrap_or_else(|error| error.into_inner());
                    calls.push(NestedCall {
                        id: record_id.clone(),
                        name: name.to_owned(),
                        args: ref_name.clone(),
                        status: "running",
                        duration_ms: None,
                        error: None,
                        cost: None,
                    });
                    calls.len() - 1
                };
                publish();
                let started = Instant::now();
                let permit = limit
                    .acquire_owned()
                    .await
                    .map_err(|_| "model call limiter closed".to_owned())?;
                let result = run(resolved, checked).await;
                drop(permit);
                let duration = started.elapsed().as_secs_f64() * 1000.0;
                let status = result
                    .get("stopReason")
                    .and_then(Value::as_str)
                    .unwrap_or("error")
                    .to_owned();
                let error_message = result
                    .get("errorMessage")
                    .and_then(Value::as_str)
                    .map(|error| truncate_text(error, ERROR_PREVIEW_CHARS));
                let usage: Option<Usage> = result
                    .get("usage")
                    .and_then(|usage| serde_json::from_value(usage.clone()).ok());
                {
                    let mut calls = calls.lock().unwrap_or_else(|error| error.into_inner());
                    let record = &mut calls[index];
                    record.duration_ms = Some(duration);
                    record.status = match status.as_str() {
                        "stop" => "ok",
                        "aborted" => "cancelled",
                        _ => "error",
                    };
                    record.error = error_message;
                    if let Some(usage) = &usage {
                        record.cost = Some(usage.cost.total);
                        add_usage(usage.clone());
                    }
                }
                publish();
                Ok(result)
            })
        })
    };

    let mut globals = Vec::new();

    let models_for_get = models.clone();
    globals.push(CodemodeTool {
        name: "models.getModelsOfType".to_owned(),
        description: None,
        input_schema: None,
        output_schema: None,
        spread: true,
        signature: None,
        execute: Arc::new(move |args: Value, _ctx| {
            let models = models_for_get.clone();
            Box::pin(async move {
                let array = args.as_array().cloned().unwrap_or_default();
                let model_type = to_model_type(array.first().unwrap_or(&Value::Null))?;
                let provider = to_provider(array.get(1))?;
                Ok(Value::Array(
                    models
                        .get_models_of_type(model_type_of(model_type), provider.as_deref())
                        .iter()
                        .map(to_model_info)
                        .collect(),
                ))
            })
        }),
    });

    let models_for_available = models.clone();
    globals.push(CodemodeTool {
        name: "models.getAvailableOfType".to_owned(),
        description: None,
        input_schema: None,
        output_schema: None,
        spread: true,
        signature: None,
        execute: Arc::new(move |args: Value, _ctx| {
            let models = models_for_available.clone();
            Box::pin(async move {
                let array = args.as_array().cloned().unwrap_or_default();
                let model_type = to_model_type(array.first().unwrap_or(&Value::Null))?;
                let provider = to_provider(array.get(1))?;
                let available = models
                    .get_available_of_type(model_type_of(model_type), provider.as_deref())
                    .await
                    .map_err(|error| error.message.clone())?;
                Ok(Value::Array(available.iter().map(to_model_info).collect()))
            })
        }),
    });

    let models_for_one = models.clone();
    globals.push(CodemodeTool {
        name: "models.getModelOfType".to_owned(),
        description: None,
        input_schema: None,
        output_schema: None,
        spread: true,
        signature: None,
        execute: Arc::new(move |args: Value, _ctx| {
            let models = models_for_one.clone();
            Box::pin(async move {
                let array = args.as_array().cloned().unwrap_or_default();
                let model_type = to_model_type(array.first().unwrap_or(&Value::Null))?;
                let (Some(provider), Some(id)) = (
                    array.get(1).and_then(Value::as_str),
                    array.get(2).and_then(Value::as_str),
                ) else {
                    return Err(format!(
                        "models.getModelOfType(type, provider, id) expects three strings, got ({}). The provider and the id are separate arguments, for example models.getModelOfType(\"classifier\", \"typesafe\", \"jev-latest\").",
                        array.iter().map(describe_value).collect::<Vec<_>>().join(", ")
                    ));
                };
                Ok(models
                    .get_model_of_type(model_type_of(model_type), provider, id)
                    .map(|model| to_model_info(&model))
                    .unwrap_or(Value::Null))
            })
        }),
    });

    let classify_models = models.clone();
    let classify_run_model_call = run_model_call.clone();
    globals.push(CodemodeTool {
        name: "models.classify".to_owned(),
        description: None,
        input_schema: None,
        output_schema: None,
        spread: true,
        signature: None,
        execute: Arc::new(move |args: Value, _ctx| {
            let run_model_call = classify_run_model_call.clone();
            let models = classify_models.clone();
            let array = args.as_array().cloned().unwrap_or_default();
            Box::pin(async move {
                run_model_call(
                    "models.classify",
                    "classifier",
                    array,
                    Arc::new(|context| {
                        check_classifier_context(context).map(CheckedContext::Classifier)
                    }),
                    Arc::new(move |model, checked| {
                        let models = models.clone();
                        let CheckedContext::Classifier(context) = checked else {
                            return Box::pin(async { Value::Null });
                        };
                        Box::pin(async move {
                            let rpi_ai::types::AnyModel::Classifier(classifier) = model else {
                                return Value::Null;
                            };
                            let result: ClassifierResult =
                                models.classify(&classifier, &context, None).await;
                            serde_json::to_value(result).unwrap_or(Value::Null)
                        })
                    }),
                )
                .await
            })
        }),
    });

    let image_models = models.clone();
    let image_run_model_call = run_model_call.clone();
    let add_generated_images_for_globals = add_generated_images.clone();
    globals.push(CodemodeTool {
        name: "models.generateImages".to_owned(),
        description: None,
        input_schema: None,
        output_schema: None,
        spread: true,
        signature: None,
        execute: Arc::new(move |args: Value, _ctx| {
            let run_model_call = image_run_model_call.clone();
            let models = image_models.clone();
            let add_generated_images = add_generated_images_for_globals.clone();
            let array = args.as_array().cloned().unwrap_or_default();
            Box::pin(async move {
                run_model_call(
                    "models.generateImages",
                    "image",
                    array,
                    Arc::new(|context| check_images_context(context).map(CheckedContext::Images)),
                    Arc::new(move |model, checked| {
                        let models = models.clone();
                        let add_generated_images = add_generated_images.clone();
                        let CheckedContext::Images(context) = checked else {
                            return Box::pin(async { Value::Null });
                        };
                        Box::pin(async move {
                            let rpi_ai::types::AnyModel::Image(image) = model else {
                                return Value::Null;
                            };
                            let result: AssistantImages =
                                models.generate_images(&image, &context, None).await;
                            let images = result
                                .output
                                .iter()
                                .filter(|block| {
                                    matches!(block, rpi_ai::types::ImagesOutputContent::Image(_))
                                })
                                .count();
                            add_generated_images(images);
                            serde_json::to_value(result).unwrap_or(Value::Null)
                        })
                    }),
                )
                .await
            })
        }),
    });

    globals
}

/// Execute one codemode script (execute.ts:353-433).
pub async fn execute_codemode(
    request: ToolExecuteRequest,
    ctx: ExtensionContext,
    options: CodemodeToolOptions,
) -> Result<AgentToolResult, String> {
    let started = Instant::now();
    let ToolExecuteRequest {
        tool_call_id,
        params,
        signal,
        on_update,
    } = request;
    let code = params
        .get("code")
        .and_then(Value::as_str)
        .ok_or_else(|| "codemode expects a `code` string".to_owned())?
        .to_owned();
    let parsed = match parse_codemode_source(&code) {
        Ok(parsed) => parsed,
        Err(error) => {
            // Source validation is model-visible text: return it as an error
            // result with no host prefix (upstream throws the same message).
            return Ok(AgentToolResult {
                content: vec![ToolResultContent::Text(rpi_ai::types::TextContent {
                    text: error.message,
                    text_signature: None,
                })],
                is_error: Some(true),
                ..Default::default()
            });
        }
    };
    let calls: Arc<std::sync::Mutex<Vec<NestedCall>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let model_usage: Arc<std::sync::Mutex<Option<Usage>>> = Arc::new(std::sync::Mutex::new(None));
    let generated_images = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let on_update: Option<Arc<dyn Fn(AgentToolResult) + Send + Sync>> = on_update.map(Arc::from);
    let publish: Arc<dyn Fn() + Send + Sync> = {
        let calls = calls.clone();
        let on_update = on_update.clone();
        Arc::new(move || {
            if let Some(on_update) = &on_update {
                let snapshot = calls
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .clone();
                on_update(AgentToolResult {
                    content: Vec::new(),
                    details: json!({ "calls": snapshot }),
                    ..Default::default()
                });
            }
        })
    };
    let add_usage: Arc<dyn Fn(Usage) + Send + Sync> = {
        let model_usage = model_usage.clone();
        Arc::new(move |usage: Usage| {
            let mut current = model_usage
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            *current = Some(match current.take() {
                Some(existing) => existing.combined(&usage),
                None => usage,
            });
        })
    };
    let add_generated_images: Arc<dyn Fn(usize) + Send + Sync> = {
        let generated_images = generated_images.clone();
        Arc::new(move |count: usize| {
            generated_images.fetch_add(count, std::sync::atomic::Ordering::SeqCst);
        })
    };

    let callable = callable_tools(&options.api);
    let samples = sample_map(&callable);
    let sandbox_tools: Vec<CodemodeTool> = callable
        .iter()
        .map(|tool| {
            let ctx = ctx.clone();
            let calls = calls.clone();
            let publish = publish.clone();
            let tool_call_id = tool_call_id.clone();
            let output_schema = tool.output_schema.clone();
            let name = tool.name.clone();
            CodemodeTool {
                name: tool.name.clone(),
                description: samples.get(&tool.name).cloned(),
                input_schema: Some(tool.parameters.clone()),
                output_schema: tool.output_schema.clone(),
                spread: false,
                signature: None,
                execute: Arc::new(move |args: Value, call_ctx: CodemodeToolContext| {
                    let ctx = ctx.clone();
                    let calls = calls.clone();
                    let publish = publish.clone();
                    let tool_call_id = tool_call_id.clone();
                    let output_schema = output_schema.clone();
                    let name = name.clone();
                    Box::pin(async move {
                        let index = {
                            let mut calls = calls.lock().unwrap_or_else(|error| error.into_inner());
                            calls.push(NestedCall {
                                id: format!("{tool_call_id}/?"),
                                name: name.clone(),
                                args: preview_args(&args),
                                status: "running",
                                duration_ms: None,
                                error: None,
                                cost: None,
                            });
                            calls.len() - 1
                        };
                        publish();
                        let call_started = Instant::now();
                        let outcome = ctx
                            .execute_tool(
                                &name,
                                args,
                                Some(ExecuteToolOptions {
                                    signal: Some(call_ctx.signal.clone()),
                                    on_update: None,
                                }),
                            )
                            .await
                            .map_err(|error| error.to_string())?;
                        let duration = call_started.elapsed().as_secs_f64() * 1000.0;
                        let call_id = outcome
                            .tool_call
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned();
                        let error_text = if outcome.is_error {
                            let text = text_of(&outcome.result);
                            let default = format!("Tool \"{name}\" failed");
                            Some(truncate_text(
                                if text.is_empty() { &default } else { &text },
                                ERROR_PREVIEW_CHARS,
                            ))
                        } else {
                            None
                        };
                        {
                            let mut calls = calls.lock().unwrap_or_else(|error| error.into_inner());
                            let record = &mut calls[index];
                            record.id = call_id;
                            record.duration_ms = Some(duration);
                            record.status = if outcome.is_error {
                                if call_ctx.signal.is_cancelled() {
                                    "cancelled"
                                } else {
                                    "error"
                                }
                            } else {
                                "ok"
                            };
                            record.error = error_text;
                        }
                        publish();
                        to_script_value(output_schema.as_ref(), &name, &outcome)
                    })
                }),
            }
        })
        .collect();

    let mut globals = create_discovery_globals(&callable, &samples);
    if options.models {
        globals.extend(create_model_globals(
            options.model_runtime.clone(),
            tool_call_id.clone(),
            calls.clone(),
            publish.clone(),
            add_usage.clone(),
            add_generated_images.clone(),
        ));
    }

    let store = {
        let entries = ctx
            .session_entries(Some(CODEMODE_STORE_ENTRY_TYPE), None)
            .unwrap_or_default();
        read_codemode_store(&entries)
    };
    let sandbox = CodemodeSandbox::new(CodemodeSandboxOptions {
        tools: sandbox_tools,
        globals,
        timeout_ms: match parsed.options.timeout_ms {
            Some(ms) => CodemodeTimeout::Milliseconds(ms),
            None => CodemodeTimeout::Infinite,
        },
        memory_limit_bytes: Some(CODEMODE_MEMORY_LIMIT_BYTES as u64),
    })?;
    let result = sandbox
        .execute(
            &parsed.code,
            CodemodeExecuteOptions {
                signal: Some(signal.clone()),
                timeout_ms: None,
                store: Some(store),
            },
        )
        .await;
    sandbox.close().await;
    let result = result?;

    let mut nested = calls
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone();
    for call in nested.iter_mut() {
        if call.status == "running" {
            call.status = "cancelled";
        }
    }

    let output_items: &[CodemodeOutputItem] = match &result {
        CodemodeResult::Ok { output, .. } | CodemodeResult::Err { output, .. } => output,
    };
    let mut items: Vec<ToolResultContent> = output_items
        .iter()
        .map(|item| match item {
            CodemodeOutputItem::Text { text } => {
                ToolResultContent::Text(rpi_ai::types::TextContent {
                    text: text.clone(),
                    text_signature: None,
                })
            }
            CodemodeOutputItem::Image { data, mime_type } => {
                ToolResultContent::Image(rpi_ai::types::ImageContent {
                    data: data.clone(),
                    mime_type: mime_type.clone(),
                })
            }
        })
        .collect();
    if let CodemodeResult::Ok {
        value: Some(value),
        store_writes,
        ..
    } = &result
    {
        if !store_writes.set.is_empty() || !store_writes.delete.is_empty() {
            let _ = options.api.append_entry(
                CODEMODE_STORE_ENTRY_TYPE,
                Some(json!({ "set": store_writes.set, "delete": store_writes.delete })),
            );
        }
        items.push(ToolResultContent::Text(rpi_ai::types::TextContent {
            text: value_text(value),
            text_signature: None,
        }));
    } else {
        items.push(ToolResultContent::Text(rpi_ai::types::TextContent {
            text: format!("Script error:\n{}", format_error(&result, &nested)),
            text_signature: None,
        }));
    }
    let generated = generated_images.load(std::sync::atomic::Ordering::SeqCst);
    if generated > 0
        && !items
            .iter()
            .any(|item| matches!(item, ToolResultContent::Image(_)))
    {
        items.push(ToolResultContent::Text(rpi_ai::types::TextContent {
            text: format!(
                "Note: models.generateImages() returned {generated} image{} that the script did not show. Show each image block of result.output with image(block).",
                if generated == 1 { "" } else { "s" }
            ),
            text_signature: None,
        }));
    }

    let (items, full_output_path) = truncate_output(
        items,
        parsed
            .options
            .max_output_tokens
            .map(|tokens| tokens as usize)
            .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS),
    );
    let ok = matches!(result, CodemodeResult::Ok { .. });
    let wall_time = started.elapsed().as_secs_f64();
    let header = format!(
        "{}\nWall time {wall_time:.1} seconds\nOutput:\n",
        if ok {
            "Script completed"
        } else {
            "Script failed"
        }
    );
    let mut content: Vec<ToolResultContent> =
        vec![ToolResultContent::Text(rpi_ai::types::TextContent {
            text: header,
            text_signature: None,
        })];
    content.extend(items);
    let usage = model_usage
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone();
    let mut details = json!({ "calls": nested });
    if let Some(path) = &full_output_path {
        details["fullOutputPath"] = Value::String(path.clone());
    }
    Ok(AgentToolResult {
        content,
        details,
        structured_content: None,
        usage,
        is_error: if ok { None } else { Some(true) },
        terminate: None,
    })
}

/// The value a script receives for a nested call (execute.ts:322-332).
fn to_script_value(
    output_schema: Option<&Value>,
    name: &str,
    outcome: &rpi_ext_host::api::ExecuteToolOutcome,
) -> Result<Value, String> {
    if output_schema.is_some()
        && let Some(structured) = outcome.result.structured_content.clone()
    {
        return Ok(structured);
    }
    let text = text_of(&outcome.result);
    if outcome.is_error {
        return Err(if text.is_empty() {
            format!("Tool \"{name}\" failed")
        } else {
            text
        });
    }
    Ok(Value::String(text))
}
