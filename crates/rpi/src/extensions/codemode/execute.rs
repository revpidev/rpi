//! Runs one codemode script (port of
//! `packages/coding-agent/src/extensions/codemode/execute.ts` @ 4a080252f).
//! Split from `tool.rs` so the sandbox runtime only loads when a script runs.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use base64::Engine as _;
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
use crate::extensions::codemode::workers::{Admission, WORKER_WAIT_MS, WorkerPool, pool};
use crate::extensions::tool_search::{
    Bm25Ranker, DEFAULT_TOOL_SEARCH_LIMIT, create_tool_search_document,
};

const ARGS_PREVIEW_CHARS: usize = 200;
const ERROR_PREVIEW_CHARS: usize = 500;
/// `models.*` calls one script may have in flight (execute.ts:50-51).
const MAX_CONCURRENT_MODEL_CALLS: usize = 4;
/// Heap limit for the QuickJS VM (execute.ts:52-57).
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

/// `CodemodeNestedCall` (tool.ts:88-101): the renderer-facing call row.
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
/// headers can carry credentials (execute.ts:146-152).
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

/// A script value in an error message: `null`, `a string`, `an array`, or
/// its keys (`{ prompt }`) (`describeValue`, execute.ts:107-116 @ 4a080252f).
fn describe_value(value: &Value) -> String {
    match value {
        Value::Null => "null".to_owned(),
        Value::Bool(_) => "a boolean".to_owned(),
        Value::Number(_) => "a number".to_owned(),
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

/// [`describe_value`] for a possibly missing field: JSON has no `undefined`,
/// so the option carries the distinction (`undefined` vs `null`).
fn describe_option(value: Option<&Value>) -> String {
    match value {
        Some(value) => describe_value(value),
        None => "undefined".to_owned(),
    }
}

const CLASSIFIER_CONTEXT_SHAPE: &str = "{ state: { ... }, images?: [{ type: \"image\", data: <base64>, mimeType }], questions: { <id>: { type: \"choice\", instructions, criteria: { <label>: <meaning> } } | { type: \"score\", instructions, criteria: [<lowest level>, ..., <highest level>] } | { type: \"bool\", instructions, criteria: { true: <meaning>, false: <meaning> } } } }";

/// Check a script's classifier context (execute.ts:176-237).
fn check_classifier_context(
    value: Option<&Value>,
) -> Result<rpi_ai::types::ClassifierContext, String> {
    let fail = |problem: &str| {
        format!(
            "models.classify() {problem}. Expected context: {CLASSIFIER_CONTEXT_SHAPE}. See \"Classify\" in {}.",
            crate::extensions::codemode::description::codemode_docs_path()
        )
    };
    let Some(context) = value.and_then(Value::as_object) else {
        return Err(fail(&format!(
            "expects a context object as its second argument, got {}",
            describe_option(value)
        )));
    };
    let Some(state) = context.get("state").and_then(Value::as_object) else {
        return Err(fail(&format!(
            "context.state must be an object, got {}",
            describe_option(context.get("state"))
        )));
    };
    // Optional images, judged together with the state (ce8972a0e). The
    // transport (or the model's catalog entry) rejects them when unsupported.
    let images = match context.get("images") {
        None => None,
        Some(value) => {
            let Some(items) = value.as_array() else {
                return Err(fail(&format!(
                    "context.images must be an array, got {}",
                    describe_value(value)
                )));
            };
            let mut images = Vec::with_capacity(items.len());
            for (index, item) in items.iter().enumerate() {
                let valid = item.as_object().is_some_and(|object| {
                    object.get("type").and_then(Value::as_str) == Some("image")
                        && object.get("data").and_then(Value::as_str).is_some()
                        && object.get("mimeType").and_then(Value::as_str).is_some()
                });
                if !valid {
                    return Err(fail(&format!(
                        "context.images[{index}] must be an image block, got {}",
                        describe_value(item)
                    )));
                }
                images.push(rpi_ai::types::ImageContent {
                    data: item["data"].as_str().unwrap_or_default().to_owned(),
                    mime_type: item["mimeType"].as_str().unwrap_or_default().to_owned(),
                });
            }
            Some(images)
        }
    };
    let Some(questions) = context.get("questions").and_then(Value::as_object) else {
        return Err(fail(&format!(
            "context.questions must map question IDs to questions, got {}",
            describe_option(context.get("questions"))
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
                // `JSON.stringify(question.type)`: missing renders as
                // `undefined`, a non-string JSON value as its compact form.
                let rendered = match question.get("type") {
                    None => "undefined".to_owned(),
                    Some(value) => value.to_string(),
                };
                return Err(fail(&format!(
                    "{at}.type must be \"choice\", \"score\", or \"bool\", got {rendered}"
                )));
            }
        };
        parsed.insert(id.clone(), parsed_question);
    }
    Ok(rpi_ai::types::ClassifierContext {
        state: state.clone(),
        images,
        questions: parsed,
    })
}

/// Check a script's image context (execute.ts:238-258).
fn check_images_context(value: Option<&Value>) -> Result<rpi_ai::types::ImagesContext, String> {
    let docs = crate::extensions::codemode::description::codemode_docs_path();
    let fail = |problem: &str| {
        format!(
            "models.generateImages() {problem}. Expected context: {{ input: [{{ type: \"text\", text: <prompt> }}, ...optional {{ type: \"image\", data: <base64>, mimeType }} references] }}. See \"Generate images\" in {docs}."
        )
    };
    let Some(context) = value.and_then(Value::as_object) else {
        return Err(fail(&format!(
            "expects a context object as its second argument, got {}",
            describe_option(value)
        )));
    };
    let Some(input) = context.get("input").and_then(Value::as_array) else {
        return Err(fail(&format!(
            "context.input must be a non-empty array of blocks, got {}",
            describe_option(context.get("input"))
        )));
    };
    if input.is_empty() {
        return Err(fail(&format!(
            "context.input must be a non-empty array of blocks, got {}",
            describe_option(context.get("input"))
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
/// output. Every output file goes through the shared helper, which creates
/// the file exclusively and mode 0600 (`writeOutputFile`, output-files.ts:23-27).
fn spill_output(text: &str) -> Result<std::path::PathBuf, String> {
    crate::utils::output_files::write_output_file("rpi-codemode", ".txt", text.as_bytes())
        .map_err(|error| error.to_string())
}

/// `truncateOutput` (execute.ts:449-479).
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

/// File extensions of the image types the sandbox's `image()` accepts. Must
/// list every type `IMAGE_SIGNATURES` in the prelude detects
/// (`IMAGE_EXTENSIONS`, execute.ts:326-331).
fn image_extension(mime_type: &str) -> Option<&'static str> {
    match mime_type {
        "image/png" => Some(".png"),
        "image/jpeg" => Some(".jpg"),
        "image/gif" => Some(".gif"),
        "image/webp" => Some(".webp"),
        _ => None,
    }
}

/// The `[Image saved to …]` label for one image (`label`, execute.ts:340-353).
fn image_label(image: &rpi_ai::types::ImageContent) -> String {
    // The prelude validates the base64 and the signature, so this only
    // guards a hand-built host result.
    let Some(bytes) = base64::engine::general_purpose::STANDARD
        .decode(&image.data)
        .ok()
    else {
        return format!(
            "[Image ({}) could not be saved: invalid base64 image data]",
            image.mime_type
        );
    };
    let kind = format!(
        "{}, {}",
        image.mime_type,
        crate::tools::truncate::format_size(bytes.len())
    );
    let Some(extension) = image_extension(&image.mime_type) else {
        return format!(
            "[Image ({kind}) could not be saved: no file extension for image type {}]",
            image.mime_type
        );
    };
    // A failed write (disk full, unwritable temp dir) must not discard the
    // result of a script whose tool calls already ran, so it becomes part of
    // the label.
    match crate::utils::output_files::write_output_file("rpi-codemode", extension, &bytes) {
        Ok(path) => format!("[Image saved to {} ({kind})]", path.display()),
        Err(error) => format!("[Image ({kind}) could not be saved: {error}]"),
    }
}

/// Lay out the script's output so the model can tell items apart: providers
/// join adjacent text blocks with a newline or with nothing. With more than
/// one text item (`text()` or the returned value), each starts with a
/// `==> text N/M <==` line; `console.*` lines follow all other output in one
/// `<console_output>` block (`formatOutput`, execute.ts:262-281).
fn format_output(output: Vec<CodemodeOutputItem>) -> Vec<ToolResultContent> {
    let total = output
        .iter()
        .filter(|item| matches!(item, CodemodeOutputItem::Text { console: false, .. }))
        .count();
    let mut items: Vec<ToolResultContent> = Vec::with_capacity(output.len());
    let mut console_lines: Vec<String> = Vec::new();
    let mut index = 0usize;
    for item in output {
        match item {
            CodemodeOutputItem::Image { data, mime_type } => {
                items.push(ToolResultContent::Image(rpi_ai::types::ImageContent {
                    data,
                    mime_type,
                }));
            }
            CodemodeOutputItem::Text {
                text,
                console: true,
            } => console_lines.push(text),
            CodemodeOutputItem::Text {
                text,
                console: false,
            } => {
                index += 1;
                let text = if total > 1 {
                    format!("==> text {index}/{total} <==\n{text}")
                } else {
                    text
                };
                items.push(ToolResultContent::Text(rpi_ai::types::TextContent {
                    text,
                    text_signature: None,
                }));
            }
        }
    }
    if !console_lines.is_empty() {
        items.push(ToolResultContent::Text(rpi_ai::types::TextContent {
            text: format!(
                "<console_output>\n{}\n</console_output>",
                console_lines.join("\n")
            ),
            text_signature: None,
        }));
    }
    items
}

/// Join adjacent text items into one, each part starting on its own line
/// (`joinAdjacentText`, execute.ts:341-374). The separator check looks at the
/// last non-empty part, not the joined text: reading the end of a growing
/// concatenation copies it on every item, which is quadratic (#10678).
fn join_adjacent_text(items: Vec<ToolResultContent>) -> Vec<ToolResultContent> {
    fn flush(joined: &mut Vec<ToolResultContent>, parts: &mut Vec<String>, last_part: &mut String) {
        if !parts.is_empty() {
            joined.push(ToolResultContent::Text(rpi_ai::types::TextContent {
                text: parts.concat(),
                text_signature: None,
            }));
        }
        parts.clear();
        last_part.clear();
    }
    let mut joined: Vec<ToolResultContent> = Vec::with_capacity(items.len());
    let mut parts: Vec<String> = Vec::new();
    let mut last_part = String::new();
    for item in items {
        let ToolResultContent::Text(text) = item else {
            flush(&mut joined, &mut parts, &mut last_part);
            joined.push(item);
            continue;
        };
        if !last_part.is_empty() && !last_part.ends_with('\n') {
            parts.push("\n".to_owned());
            last_part = "\n".to_owned();
        }
        parts.push(text.text.clone());
        if !text.text.is_empty() {
            last_part = text.text;
        }
    }
    flush(&mut joined, &mut parts, &mut last_part);
    joined
}

/// Save each image to a temp file and put a text item with its path before
/// it (`saveImages`, execute.ts:338-366). The model sees the image but has no
/// other way to reach its bytes: scripts cannot write files, and `write`
/// only takes text. Images shown more than once are saved once.
fn save_images(items: Vec<ToolResultContent>) -> Vec<ToolResultContent> {
    let mut labels: HashMap<String, String> = HashMap::new();
    let mut output: Vec<ToolResultContent> = Vec::with_capacity(items.len());
    for item in items {
        match item {
            ToolResultContent::Image(image) => {
                let label = labels
                    .entry(image.data.clone())
                    .or_insert_with(|| image_label(&image))
                    .clone();
                output.push(ToolResultContent::Text(rpi_ai::types::TextContent {
                    text: label,
                    text_signature: None,
                }));
                output.push(ToolResultContent::Image(image));
            }
            other => output.push(other),
        }
    }
    output
}

/// Values of `load()`: the `codemode-store` entries on the branch, applied
/// from the root (execute.ts:293-305).
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

/// `isNamespaceName` (execute.ts:658-668).
fn namespace_suffix(name: &str) -> Option<&str> {
    name.rfind("__")
        .map(|index| &name[index + 2..])
        .filter(|value| !value.is_empty())
}

fn is_namespace_name(namespace: &rpi_ext_host::types::ToolNamespace, query: &str) -> bool {
    let query_id = to_codemode_identifier(query);
    let mut names: Vec<&str> = vec![namespace.name.as_str()];
    names.extend(namespace.aliases.iter().flatten().map(String::as_str));
    names.into_iter().any(|name| {
        let id = to_codemode_identifier(name);
        name == query
            || id == query_id
            || namespace_suffix(name) == Some(query)
            || namespace_suffix(&id) == Some(query_id.as_str())
    })
}

fn sample_map(tools: &[ToolInfo]) -> HashMap<String, String> {
    tools
        .iter()
        .map(|tool| {
            (
                tool.name.clone(),
                render_tool_sample(&to_codemode_declaration(tool, &tool.prompt_guidelines)),
            )
        })
        .collect()
}

/// `searchTools()`, `describeTool()`, and `describeNamespace()`
/// (execute.ts:669-741).
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
                                .is_some_and(|namespace| is_namespace_name(namespace, query)),
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
                        if !is_namespace_name(tool_namespace, name) {
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

type CheckFn = Arc<dyn Fn(Option<&Value>) -> Result<CheckedContext, String> + Send + Sync>;
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

/// `models.*` for scripts (execute.ts:627-733).
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
                let model = args.first();
                let fail_first = |detail: &str| {
                    // The hint only rides the missing/null model error
                    // (`model === undefined || model === null`, execute.ts:652-655).
                    let undefined_hint = if model.is_none_or(Value::is_null) {
                        " models.getModelOfType() returns undefined for an unknown provider or id."
                    } else {
                        ""
                    };
                    format!(
                        "{name}() expects {} model as its first argument, got {detail}.{undefined_hint} {list_hint}",
                        with_article(model_type)
                    )
                };
                let (Some(provider), Some(id)) = model
                    .and_then(Value::as_object)
                    .map(|object| {
                        (
                            object.get("provider").and_then(Value::as_str),
                            object.get("id").and_then(Value::as_str),
                        )
                    })
                    .unwrap_or((None, None))
                else {
                    return Err(fail_first(&describe_option(model)));
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
                let checked = check(args.get(1))?;

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

/// Releases an admission whose worker never started. The sandbox's
/// `acquire_worker` disarms it when it takes the slot over (execute.ts:559-607).
struct ReservationGuard {
    pool: Arc<WorkerPool>,
    reserved: Arc<AtomicBool>,
}

impl Drop for ReservationGuard {
    fn drop(&mut self) {
        if self.reserved.swap(false, Ordering::SeqCst) {
            self.pool.release();
        }
    }
}

/// Execute one codemode script (execute.ts:416-537).
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

    // Show the script as running before its first nested call, so the row's
    // timer starts (execute.ts:515-517).
    publish();

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

    // The wait for a worker counts toward the script's deadline
    // (execute.ts:555-575).
    let deadline = parsed.options.timeout_ms;
    let wait_timeout =
        Duration::from_millis(deadline.unwrap_or(WORKER_WAIT_MS).min(WORKER_WAIT_MS));
    let wait_started = Instant::now();
    let pool = pool();
    let admission = pool.reserve(Some(&signal), wait_timeout).await;
    let waited_ms = wait_started.elapsed().as_secs_f64() * 1000.0;
    if admission != Admission::Reserved {
        let waited = started.elapsed().as_secs_f64();
        let error = match admission {
            Admission::Aborted => "Script aborted: Execution aborted".to_owned(),
            Admission::Timeout if deadline.is_some_and(|ms| ms <= WORKER_WAIT_MS) => format!(
                "Script timed out: Execution timed out after {} ms while waiting for a free codemode worker",
                deadline.unwrap_or(WORKER_WAIT_MS)
            ),
            Admission::Timeout => format!(
                "No codemode worker became free within {} seconds. All {} workers, shared by every session in this process, are held by other scripts. Retry after they finish.",
                WORKER_WAIT_MS / 1000,
                crate::extensions::codemode::workers::max_workers()
            ),
            Admission::Reserved => unreachable!("handled above"),
        };
        return Ok(AgentToolResult {
            content: vec![
                ToolResultContent::Text(rpi_ai::types::TextContent {
                    text: format!("Script failed\nWall time {waited:.1} seconds\nOutput:\n"),
                    text_signature: None,
                }),
                ToolResultContent::Text(rpi_ai::types::TextContent {
                    text: format!("Script error:\n{error}\n\nThe script did not start."),
                    text_signature: None,
                }),
            ],
            details: json!({ "calls": [] }),
            is_error: Some(true),
            ..Default::default()
        });
    }
    let reserved = Arc::new(AtomicBool::new(true));
    let _reservation = ReservationGuard {
        pool: pool.clone(),
        reserved: reserved.clone(),
    };

    let store = {
        let entries = ctx
            .session_entries(Some(CODEMODE_STORE_ENTRY_TYPE), None)
            .unwrap_or_default();
        read_codemode_store(&entries)
    };
    let acquire_worker = {
        let pool = pool.clone();
        let reserved = reserved.clone();
        Arc::new(move || {
            if !reserved.swap(false, Ordering::SeqCst) {
                return Err("The script already settled".to_owned());
            }
            let pool = pool.clone();
            Ok(Box::new(move || pool.release()) as rpi_codemode::WorkerRelease)
        })
    };
    let sandbox = CodemodeSandbox::new(CodemodeSandboxOptions {
        tools: sandbox_tools,
        globals,
        timeout_ms: match deadline {
            Some(ms) => {
                CodemodeTimeout::Milliseconds((ms as f64 - waited_ms).round().max(1.0) as u64)
            }
            None => CodemodeTimeout::Infinite,
        },
        memory_limit_bytes: Some(CODEMODE_MEMORY_LIMIT_BYTES as u64),
        acquire_worker: Some(acquire_worker),
        ..Default::default()
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
    // The worker started (or will never start); the guard keeps the slot only
    // for a sandbox that failed to construct.
    drop(_reservation);
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

    let mut script_output: Vec<CodemodeOutputItem> = match &result {
        CodemodeResult::Ok { output, .. } | CodemodeResult::Err { output, .. } => output.clone(),
    };
    match &result {
        CodemodeResult::Ok {
            value,
            store_writes,
            ..
        } => {
            // Store writes are persisted for every successful script — a
            // script that reports success without returning a value (a bare
            // `exit()` or a body with no `return`, both `value: None`) must
            // still commit them (v0.1.6 review P1-3; upstream execute.ts:504-506
            // persists them for `result.ok` regardless of `result.value`).
            if !store_writes.set.is_empty() || !store_writes.delete.is_empty() {
                let _ = options.api.append_entry(
                    CODEMODE_STORE_ENTRY_TYPE,
                    Some(json!({ "set": store_writes.set, "delete": store_writes.delete })),
                );
            }
            // A returned value is appended like text(); `None` adds nothing.
            if let Some(value) = value {
                script_output.push(CodemodeOutputItem::Text {
                    text: value_text(value),
                    console: false,
                });
            }
        }
        CodemodeResult::Err { .. } => {}
    }
    let mut items = format_output(script_output);
    if matches!(result, CodemodeResult::Err { .. }) {
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
        join_adjacent_text(items),
        parsed
            .options
            .max_output_tokens
            .map(|tokens| tokens as usize)
            .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS),
    );
    // After truncation, which joins the text items and moves images after
    // them, so each path stays next to its image and is never cut.
    let items = join_adjacent_text(save_images(items));
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

/// The value a script receives for a nested call (execute.ts:404-410).
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

#[cfg(test)]
mod tests {
    use super::*;

    fn text_item(text: &str) -> ToolResultContent {
        ToolResultContent::Text(rpi_ai::types::TextContent {
            text: text.to_owned(),
            text_signature: None,
        })
    }

    fn joined(items: Vec<ToolResultContent>) -> Vec<String> {
        join_adjacent_text(items)
            .into_iter()
            .map(|item| match item {
                ToolResultContent::Text(text) => format!("text:{:?}", text.text),
                ToolResultContent::Image(image) => format!("image:{}", image.mime_type),
            })
            .collect()
    }

    #[test]
    fn joins_adjacent_text_linearly_with_the_last_non_empty_part_separator() {
        assert_eq!(
            joined(vec![text_item("a"), text_item("b"), text_item("c")]),
            vec!["text:\"a\\nb\\nc\"".to_owned()]
        );
        // An empty part neither ends the run nor starts a new line.
        assert_eq!(
            joined(vec![text_item("a"), text_item(""), text_item("b")]),
            vec!["text:\"a\\nb\"".to_owned()]
        );
        // A trailing newline suppresses the separator for the next part.
        assert_eq!(
            joined(vec![text_item("a\n"), text_item("b")]),
            vec!["text:\"a\\nb\"".to_owned()]
        );
        // Images flush the text run.
        let image = ToolResultContent::Image(rpi_ai::types::ImageContent {
            data: "aGVsbG8=".to_owned(),
            mime_type: "image/png".to_owned(),
        });
        assert_eq!(
            joined(vec![text_item("a"), image, text_item("b")]),
            vec![
                "text:\"a\"".to_owned(),
                "image:image/png".to_owned(),
                "text:\"b\"".to_owned()
            ]
        );
    }

    fn questions() -> Value {
        json!({
            "q1": {
                "type": "choice",
                "instructions": "Pick",
                "criteria": { "a": "first", "b": "second" }
            }
        })
    }

    #[test]
    fn classifier_context_accepts_optional_images() {
        let context = json!({
            "state": { "files": 2 },
            "images": [
                { "type": "image", "data": "aGVsbG8=", "mimeType": "image/png" }
            ],
            "questions": questions(),
        });
        let parsed = check_classifier_context(Some(&context)).expect("parses");
        let images = parsed.images.expect("images");
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].data, "aGVsbG8=");
        assert_eq!(images[0].mime_type, "image/png");

        // Without images the field stays absent.
        let without = json!({ "state": { "files": 2 }, "questions": questions() });
        assert!(
            check_classifier_context(Some(&without))
                .expect("parses")
                .images
                .is_none()
        );

        // An empty array is allowed and stays empty.
        let empty = json!({ "state": {}, "images": [], "questions": questions() });
        assert_eq!(
            check_classifier_context(Some(&empty))
                .expect("parses")
                .images,
            Some(Vec::new())
        );
    }

    #[test]
    fn classifier_context_rejects_malformed_images() {
        let not_an_array = json!({
            "state": {},
            "images": { "type": "image" },
            "questions": questions(),
        });
        let error = check_classifier_context(Some(&not_an_array)).expect_err("rejects");
        assert!(
            error.contains("context.images must be an array, got"),
            "{error}"
        );

        let not_an_image = json!({
            "state": {},
            "images": [{ "type": "text", "data": "x", "mimeType": "text/plain" }],
            "questions": questions(),
        });
        let error = check_classifier_context(Some(&not_an_image)).expect_err("rejects");
        assert!(
            error.contains("context.images[0] must be an image block, got"),
            "{error}"
        );

        let missing_data = json!({
            "state": {},
            "images": [{ "type": "image", "mimeType": "image/png" }],
            "questions": questions(),
        });
        let error = check_classifier_context(Some(&missing_data)).expect_err("rejects");
        assert!(
            error.contains("context.images[0] must be an image block, got"),
            "{error}"
        );
    }

    /// Missing fields render as `undefined` like upstream
    /// (`agent-session-codemode.test.ts:952/958`); explicit `null` stays
    /// `null`, and other values use the `describeValue` wording.
    #[test]
    fn context_errors_match_upstream_wording() {
        let no_state = json!({ "questions": questions() });
        let error = check_classifier_context(Some(&no_state)).expect_err("rejects");
        assert!(
            error.contains("context.state must be an object, got undefined."),
            "{error}"
        );

        let null_state = json!({ "state": null, "questions": questions() });
        let error = check_classifier_context(Some(&null_state)).expect_err("rejects");
        assert!(
            error.contains("context.state must be an object, got null."),
            "{error}"
        );

        let no_context = check_classifier_context(None).expect_err("rejects");
        assert!(
            no_context.contains("expects a context object as its second argument, got undefined."),
            "{no_context}"
        );

        let no_input = json!({ "prompt": "a fox" });
        let error = check_images_context(Some(&no_input)).expect_err("rejects");
        assert!(
            error.contains("context.input must be a non-empty array of blocks, got undefined."),
            "{error}"
        );

        let number_state = json!({ "state": 5, "questions": questions() });
        let error = check_classifier_context(Some(&number_state)).expect_err("rejects");
        assert!(
            error.contains("context.state must be an object, got a number."),
            "{error}"
        );

        let missing_type = json!({
            "state": {},
            "questions": { "q1": { "instructions": "Pick", "criteria": {} } }
        });
        let error = check_classifier_context(Some(&missing_type)).expect_err("rejects");
        assert!(
            error.contains(
                "context.questions.q1.type must be \"choice\", \"score\", or \"bool\", got undefined"
            ),
            "{error}"
        );
    }
    #[test]
    fn namespace_names_accept_aliases_and_identifiers() {
        let namespace = rpi_ext_host::types::ToolNamespace {
            name: "mcp__docs-server".to_owned(),
            aliases: Some(vec!["docs".to_owned(), "documentation".to_owned()]),
            description: None,
            instructions: None,
        };
        assert!(is_namespace_name(&namespace, "docs"));
        assert!(is_namespace_name(&namespace, "documentation"));
        assert!(is_namespace_name(&namespace, "mcp__docs-server"));
        assert!(is_namespace_name(&namespace, "docs_server"));
        assert!(!is_namespace_name(&namespace, "other"));
    }
}
