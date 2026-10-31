//! A script's result as the model reads it (port of
//! `packages/durable/src/codemode/output.ts` @ `4a080252f`).
//!
//! A "Script completed/failed" header with the wall time, the script's text
//! items (`==> text N/M <==` when there are several), its `console` lines in
//! one `<console_output>` block, the returned value, and on failure the
//! diagnostics. Text past the token budget is cut to its start and end, with
//! the complete text spilled to a file when the host has an execution
//! environment. Images are saved next to a path label when possible and always
//! returned as image blocks.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use base64::Engine as _;
use rpi_ai::types::{ImageContent, TextContent, ToolResultContent};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::chord::context::Context;
use crate::env::types::{ExecutionEnv, FileContent};

use super::{CodemodeOutcome, CodemodeTextItem};

/// Default token budget for script output.
pub const DEFAULT_MAX_OUTPUT_TOKENS: u64 = 10_000;
/// Characters per token when estimating.
const CHARS_PER_TOKEN: usize = 4;
/// Lines of the result besides the script's text: the header, markers, notes.
const LINE_MARGIN: usize = 100;

/// The tool's output limits, which the Harness enforces by cutting without a
/// spill file: results stay inside them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputLimits {
    /// Maximum retained bytes.
    pub max_bytes: u64,
    /// Maximum retained lines.
    pub max_lines: u64,
}

impl Default for OutputLimits {
    fn default() -> Self {
        Self {
            max_bytes: 4 * 1024 * 1024,
            max_lines: 100_000,
        }
    }
}

/// Status of one tool call a script made.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CodemodeCallStatus {
    /// The call is in flight.
    Running,
    /// The call returned.
    Ok,
    /// The call failed.
    Error,
    /// The call was still running when the script ended.
    Cancelled,
}

/// One tool call a script made, as the code mode call's details show it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodemodeNestedCall {
    /// The VM's number for the call.
    pub id: u64,
    /// Tool name, without the internal prefix.
    pub name: String,
    /// Compact JSON of the arguments, cut for display.
    pub args: String,
    /// Status.
    pub status: CodemodeCallStatus,
    /// Execution time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Error text, cut for display.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The code mode call's details, in its result.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodemodeToolDetails {
    /// Tool calls in call order.
    #[serde(default)]
    pub calls: Vec<CodemodeNestedCall>,
    /// Temp file with the complete text output, when the output was cut to its
    /// token budget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full_output_path: Option<String>,
    /// The script's structured value. The code mode tool declares no
    /// structured-output schema, so it belongs to the UI details channel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_output: Option<Value>,
}

/// Writes a file of the result under a fixed name; returns its path.
#[async_trait]
pub trait OutputFileWriter: Send + Sync {
    /// Write `data` as `name`; the returned path is shown to the model.
    async fn write(&self, name: &str, data: &[u8]) -> Result<String, String>;
}

/// Files under a temp directory of the call's execution environment.
pub struct EnvOutputFile {
    env: Arc<dyn ExecutionEnv>,
    context: Context,
    directory: Mutex<Option<String>>,
}

impl EnvOutputFile {
    /// A writer over `env`.
    pub fn new(env: Arc<dyn ExecutionEnv>, context: Context) -> Self {
        Self {
            env,
            context,
            directory: Mutex::new(None),
        }
    }

    async fn directory(&self) -> Result<String, String> {
        let existing = self
            .directory
            .lock()
            .map_err(|_| "output directory lock poisoned".to_string())?
            .clone();
        if let Some(directory) = existing {
            return Ok(directory);
        }
        let created = self
            .env
            .create_temp_dir(Some("pi-codemode-"), &self.context)
            .await
            .map_err(|error| error.message)?;
        if let Ok(mut directory) = self.directory.lock() {
            *directory = Some(created.clone());
        }
        Ok(created)
    }
}

#[async_trait]
impl OutputFileWriter for EnvOutputFile {
    async fn write(&self, name: &str, data: &[u8]) -> Result<String, String> {
        let directory = self.directory().await?;
        let path = self
            .env
            .join_path(&[&directory, name], &self.context)
            .await
            .map_err(|error| error.message)?;
        self.env
            .write_file(&path, FileContent::Bytes(data.to_vec()), &self.context)
            .await
            .map_err(|error| error.message)?;
        Ok(path)
    }
}

/// The tool result's content for a script's result, plus details.
#[derive(Debug, Clone, PartialEq)]
pub struct FormattedOutput {
    /// Blocks the model receives.
    pub content: Vec<ToolResultContent>,
    /// Path of the complete text when it was cut.
    pub full_output_path: Option<String>,
    /// Whether the text was cut.
    pub truncated: bool,
}

/// File extensions of the image types `image()` accepts.
fn image_extension(mime_type: &str) -> &'static str {
    match mime_type {
        "image/png" => ".png",
        "image/jpeg" => ".jpg",
        "image/gif" => ".gif",
        "image/webp" => ".webp",
        _ => ".bin",
    }
}

fn text(text: impl Into<String>) -> ToolResultContent {
    ToolResultContent::Text(TextContent {
        text: text.into(),
        text_signature: None,
    })
}

fn display_items(outcome: &CodemodeOutcome) -> Vec<CodemodeTextItem> {
    if outcome.text_items.is_empty() {
        if outcome.output.is_empty() {
            Vec::new()
        } else {
            vec![CodemodeTextItem {
                text: outcome.output.clone(),
                console: false,
            }]
        }
    } else {
        outcome.text_items.clone()
    }
}

fn count_chars(text: &str) -> usize {
    text.chars().count()
}

fn head_chars(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

fn tail_chars(text: &str, max: usize) -> String {
    let count = text.chars().count();
    text.chars().skip(count.saturating_sub(max)).collect()
}

/// The first `max_lines` lines of the first `max_chars` characters, as
/// upstream `combined.slice(0, headChars).split("\n").slice(0, lineBudget)`.
fn head_lines(text: &str, max_chars: usize, max_lines: usize) -> String {
    head_chars(text, max_chars)
        .split('\n')
        .take(max_lines)
        .collect::<Vec<_>>()
        .join("\n")
}

/// The last `max_lines` lines of the last `max_chars` characters, as upstream
/// `combined.slice(-tailChars).split("\n").slice(-lineBudget)`.
fn tail_lines(text: &str, max_chars: usize, max_lines: usize) -> String {
    if max_lines == 0 {
        return String::new();
    }
    let sliced = tail_chars(text, max_chars);
    let lines: Vec<&str> = sliced.split('\n').collect();
    let start = lines.len().saturating_sub(max_lines);
    lines[start..].join("\n")
}

/// Lay out the script's text items so the model can tell them apart: number
/// them first (upstream `formatItems`), then join adjacent text of one stream
/// into one block (`joinAdjacentText`).
fn text_blocks(outcome: &CodemodeOutcome) -> Vec<String> {
    let mut items = display_items(outcome);
    if !outcome.is_error
        && let Some(value) = &outcome.structured_output
    {
        // Upstream `formatResult` appends the returned value before
        // `formatItems`, so it is one of the numbered text items.
        items.push(CodemodeTextItem {
            text: structured_text(value),
            console: false,
        });
    }
    let total = items.iter().filter(|item| !item.console).count();
    let mut numbered: Vec<CodemodeTextItem> = Vec::new();
    let mut index = 0;
    for item in items {
        if item.console {
            numbered.push(item);
            continue;
        }
        index += 1;
        numbered.push(CodemodeTextItem {
            text: if total > 1 {
                format!("==> text {index}/{total} <==\n{}", item.text)
            } else {
                item.text
            },
            console: false,
        });
    }
    let mut blocks: Vec<String> = Vec::new();
    let mut console_lines: Vec<String> = Vec::new();
    for item in join_adjacent_text(&numbered) {
        if item.console {
            console_lines.push(item.text);
            continue;
        }
        blocks.push(item.text);
    }
    if !console_lines.is_empty() {
        blocks.push(format!(
            "<console_output>\n{}\n</console_output>",
            console_lines.join("\n")
        ));
    }
    blocks
}

fn structured_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

fn error_text(outcome: &CodemodeOutcome, calls: &[CodemodeNestedCall]) -> Option<String> {
    if !outcome.is_error {
        return None;
    }
    let diagnostics: Vec<&str> = outcome
        .diagnostics
        .iter()
        .filter(|diagnostic| {
            matches!(
                diagnostic.severity,
                crate::harness::types::DiagnosticSeverity::Error
            )
        })
        .map(|diagnostic| diagnostic.message.as_str())
        .collect();
    let head = if diagnostics.is_empty() {
        "Script error".to_string()
    } else {
        format!("Script error:\n{}", diagnostics.join("\n"))
    };
    // Upstream `formatError`: a failed script tells the model which side effects
    // already happened, so they are not repeated blindly.
    let summary = if calls.is_empty() {
        "No tool calls were made.".to_string()
    } else {
        format!(
            "Tool calls made before the failure (they are not undone): {}",
            calls
                .iter()
                .map(|call| format!(
                    "{} ({})",
                    call.name,
                    match call.status {
                        CodemodeCallStatus::Running => "running",
                        CodemodeCallStatus::Ok => "ok",
                        CodemodeCallStatus::Error => "error",
                        CodemodeCallStatus::Cancelled => "cancelled",
                    }
                ))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    Some(format!("{head}\n\n{summary}"))
}

/// Join adjacent text items of the same stream, each part starting on its own
/// line (upstream `joinAdjacentText`).
fn join_adjacent_text(items: &[CodemodeTextItem]) -> Vec<CodemodeTextItem> {
    let mut joined: Vec<CodemodeTextItem> = Vec::new();
    for item in items {
        if let Some(last) = joined.last_mut()
            && last.console == item.console
        {
            if !last.text.is_empty() && !last.text.ends_with('\n') {
                last.text.push('\n');
            }
            last.text.push_str(&item.text);
            continue;
        }
        joined.push(item.clone());
    }
    joined
}

/// Format a script result for the model.
pub async fn format_outcome(
    outcome: &CodemodeOutcome,
    wall_ms: f64,
    max_output_tokens: u64,
    writer: Option<&dyn OutputFileWriter>,
    calls: &[CodemodeNestedCall],
    generated_images: usize,
) -> FormattedOutput {
    format_with_limits(
        outcome,
        wall_ms,
        max_output_tokens,
        OutputLimits::default(),
        writer,
        calls,
        generated_images,
    )
    .await
}

/// Format with explicit output limits.
pub async fn format_with_limits(
    outcome: &CodemodeOutcome,
    wall_ms: f64,
    max_output_tokens: u64,
    limits: OutputLimits,
    writer: Option<&dyn OutputFileWriter>,
    calls: &[CodemodeNestedCall],
    generated_images: usize,
) -> FormattedOutput {
    let header = format!(
        "{}\nWall time {:.1} seconds\nOutput:\n",
        if outcome.is_error {
            "Script failed"
        } else {
            "Script completed"
        },
        wall_ms / 1000.0
    );
    let mut blocks: Vec<String> = text_blocks(outcome);
    if let Some(error) = error_text(outcome, calls) {
        blocks.push(error);
    }
    if generated_images > 0 && outcome.images.is_empty() {
        blocks.push(format!(
            "Note: models.generateImages() returned {generated_images} image{} that the script did not show. Show each image block of result.output with image(block).",
            if generated_images == 1 { "" } else { "s" }
        ));
    }

    let combined = blocks.join("\n");
    let budget = (max_output_tokens as usize)
        .saturating_mul(CHARS_PER_TOKEN)
        .min((limits.max_bytes / 4) as usize);
    let line_budget = (limits.max_lines as usize).saturating_sub(LINE_MARGIN) / 2;
    let line_count = combined.split('\n').count();
    let truncated = count_chars(&combined) > budget || line_count > 2 * line_budget;
    let mut full_output_path = None;
    let blocks: Vec<String> = if truncated {
        let head_chars_budget = budget / 2;
        let tail_chars_budget = budget.saturating_sub(head_chars_budget);
        let head = head_lines(&combined, head_chars_budget, line_budget);
        let tail = tail_lines(&combined, tail_chars_budget, line_budget);
        let removed = count_chars(&combined)
            .saturating_sub(count_chars(&head))
            .saturating_sub(count_chars(&tail));
        let mut block = format!(
            "Warning: truncated output (original token count: {})\nTotal output lines: {line_count}\n\n{head}…{} tokens truncated…{tail}",
            count_chars(&combined).div_ceil(CHARS_PER_TOKEN),
            removed.div_ceil(CHARS_PER_TOKEN),
        );
        if let Some(writer) = writer {
            match writer.write("output.txt", combined.as_bytes()).await {
                Ok(path) => {
                    block.push_str(&format!(
                        "\n\n[Full output: {path} (read with offset/limit)]"
                    ));
                    full_output_path = Some(path);
                }
                Err(error) => {
                    block.push_str(&format!("\n\n[Could not save the full output: {error}]"))
                }
            }
        }
        vec![block]
    } else {
        blocks
    };

    let mut content: Vec<ToolResultContent> = vec![text(header)];
    for block in blocks {
        content.push(text(block));
    }

    // Upstream `saveImages`: one file and one label per distinct image data,
    // reused before every occurrence of that image.
    let mut labels: HashMap<String, String> = HashMap::new();
    let mut saved = 0;
    for image in &outcome.images {
        let label = if let Some(label) = labels.get(&image.data) {
            label.clone()
        } else {
            saved += 1;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(image.data.as_bytes())
                .unwrap_or_default();
            let kind = format!("{}, {} bytes", image.mime_type, bytes.len());
            let label = if let Some(writer) = writer {
                let name = format!("image-{}{}", saved, image_extension(&image.mime_type));
                match writer.write(&name, &bytes).await {
                    Ok(path) => format!("[Image saved to {path} ({kind})]"),
                    Err(error) => format!("[Image ({kind}) could not be saved: {error}]"),
                }
            } else {
                format!("[Image ({kind}) not saved: no execution environment]")
            };
            labels.insert(image.data.clone(), label.clone());
            label
        };
        content.push(text(label));
        content.push(ToolResultContent::Image(ImageContent {
            data: image.data.clone(),
            mime_type: image.mime_type.clone(),
        }));
    }

    FormattedOutput {
        content,
        full_output_path,
        truncated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codemode::CodemodeImage;

    #[tokio::test]
    async fn formats_text_items_console_and_images() {
        let outcome = CodemodeOutcome {
            text_items: vec![
                CodemodeTextItem {
                    text: "first".to_string(),
                    console: false,
                },
                CodemodeTextItem {
                    text: "log line".to_string(),
                    console: true,
                },
                CodemodeTextItem {
                    text: "second".to_string(),
                    console: false,
                },
            ],
            images: vec![CodemodeImage {
                data: base64::engine::general_purpose::STANDARD.encode([1_u8, 2, 3]),
                mime_type: "image/png".to_string(),
            }],
            ..Default::default()
        };
        let formatted = format_outcome(&outcome, 1500.0, 10_000, None, &[], 0).await;
        let rendered = formatted
            .content
            .iter()
            .filter_map(|content| match content {
                ToolResultContent::Text(text) => Some(text.text.clone()),
                ToolResultContent::Image(_) => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("==> text 1/2 <==\nfirst"));
        assert!(rendered.contains("==> text 2/2 <==\nsecond"));
        assert!(rendered.contains("<console_output>\nlog line\n</console_output>"));
        assert!(
            formatted
                .content
                .iter()
                .any(|content| matches!(content, ToolResultContent::Image(_)))
        );
    }

    /// Upstream `saveImages`: an image shown twice is saved and labeled once.
    #[tokio::test]
    async fn repeated_images_share_one_file_and_label() {
        struct Writer(Mutex<Vec<String>>);
        #[async_trait]
        impl OutputFileWriter for Writer {
            async fn write(&self, name: &str, _data: &[u8]) -> Result<String, String> {
                self.0.lock().expect("names").push(name.to_string());
                Ok(format!("/tmp/{name}"))
            }
        }
        let writer = Writer(Mutex::new(Vec::new()));
        let image = CodemodeImage {
            data: base64::engine::general_purpose::STANDARD.encode([1_u8, 2, 3]),
            mime_type: "image/png".to_string(),
        };
        let outcome = CodemodeOutcome::output("x").with_images(vec![image.clone(), image]);
        let formatted = format_outcome(&outcome, 0.0, 10_000, Some(&writer), &[], 0).await;
        assert_eq!(writer.0.lock().expect("names").len(), 1, "one file");
        let labels: Vec<String> = formatted
            .content
            .iter()
            .filter_map(|content| match content {
                ToolResultContent::Text(text) if text.text.starts_with("[Image saved") => {
                    Some(text.text.clone())
                }
                _ => None,
            })
            .collect();
        assert_eq!(labels.len(), 2, "a label before each occurrence");
        assert_eq!(labels[0], labels[1]);
        assert!(labels[0].contains("/tmp/image-1.png"), "{}", labels[0]);
    }

    #[tokio::test]
    async fn truncates_and_spills_the_complete_text() {
        struct Writer(Mutex<Vec<u8>>);
        #[async_trait]
        impl OutputFileWriter for Writer {
            async fn write(&self, _name: &str, data: &[u8]) -> Result<String, String> {
                *self.0.lock().map_err(|_| "lock".to_string())? = data.to_vec();
                Ok("/tmp/output.txt".to_string())
            }
        }
        let writer = Writer(Mutex::new(Vec::new()));
        let outcome = CodemodeOutcome::output("x".repeat(500));
        let formatted = format_outcome(&outcome, 0.0, 10, Some(&writer), &[], 0).await;
        assert!(formatted.truncated);
        assert_eq!(
            formatted.full_output_path.as_deref(),
            Some("/tmp/output.txt")
        );
        assert!(
            writer
                .0
                .lock()
                .map(|value| String::from_utf8_lossy(&value).contains(&"x".repeat(500)))
                .unwrap_or(false)
        );
    }

    fn rendered(formatted: &FormattedOutput) -> String {
        formatted
            .content
            .iter()
            .filter_map(|content| match content {
                ToolResultContent::Text(text) => Some(text.text.clone()),
                ToolResultContent::Image(_) => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Upstream `codemode.test.ts` "keeps a result inside the tool's output
    /// limits": 200,000 short lines are cut by the line limit on both sides,
    /// not only by the character budget.
    #[tokio::test]
    async fn truncates_by_lines_and_keeps_both_ends() {
        let text = (0..200_000)
            .map(|index| match index {
                0 => "BEGIN".to_string(),
                199_999 => "END".to_string(),
                _ => "a".to_string(),
            })
            .collect::<Vec<_>>()
            .join("\n");
        let formatted =
            format_outcome(&CodemodeOutcome::output(text), 0.0, 2_000_000, None, &[], 0).await;
        let text = rendered(&formatted);
        assert!(formatted.truncated);
        assert!(text.contains("Warning: truncated output"), "{text}");
        assert!(text.contains("BEGIN"), "the head is kept");
        assert!(text.contains("END"), "the tail is kept");
        assert!(
            text.split('\n').count() < 100_000,
            "the line limit applies to head and tail"
        );
        assert!(!text.contains("0 tokens truncated"), "{text}");
    }

    /// Upstream `formatResult`: generated images the script did not show are
    /// named, so the model knows they exist; shown images are not noted.
    #[tokio::test]
    async fn notes_generated_images_the_script_did_not_show() {
        let hidden = CodemodeOutcome::output("done");
        let formatted = format_outcome(&hidden, 0.0, 10_000, None, &[], 1).await;
        let text = rendered(&formatted);
        assert!(
            text.contains(
                "Note: models.generateImages() returned 1 image that the script did not show"
            ),
            "{text}"
        );

        let shown = hidden.with_images(vec![CodemodeImage {
            data: base64::engine::general_purpose::STANDARD.encode([1_u8, 2, 3]),
            mime_type: "image/png".to_string(),
        }]);
        let formatted = format_outcome(&shown, 0.0, 10_000, None, &[], 1).await;
        assert!(!rendered(&formatted).contains("Note:"));
    }

    /// Upstream `formatResult` then `formatItems`: a returned value is one of
    /// the numbered text items, not an unnumbered block after them.
    #[tokio::test]
    async fn a_returned_value_is_a_numbered_text_item() {
        let outcome = CodemodeOutcome {
            output: "hello".to_string(),
            structured_output: Some(serde_json::json!({ "answer": 42 })),
            ..Default::default()
        };
        let formatted = format_outcome(&outcome, 0.0, 10_000, None, &[], 0).await;
        let text = rendered(&formatted);
        assert!(text.contains("==> text 1/2 <==\nhello"), "{text}");
        assert!(text.contains("==> text 2/2 <==\n{\"answer\":42}"), "{text}");
    }

    /// Upstream `formatError`: a failed script names the calls it already made so
    /// the model does not repeat their side effects.
    #[tokio::test]
    async fn a_failed_script_lists_the_calls_it_made() {
        let calls = vec![
            CodemodeNestedCall {
                id: 1,
                name: "write".to_string(),
                args: "{}".to_string(),
                status: CodemodeCallStatus::Ok,
                duration_ms: Some(3),
                error: None,
            },
            CodemodeNestedCall {
                id: 2,
                name: "bash".to_string(),
                args: "{}".to_string(),
                status: CodemodeCallStatus::Error,
                duration_ms: Some(4),
                error: Some("exit 1".to_string()),
            },
        ];
        let mut outcome = CodemodeOutcome::error("boom");
        outcome.diagnostics = vec![crate::harness::types::ToolDiagnostic {
            severity: crate::harness::types::DiagnosticSeverity::Error,
            message: "boom".to_string(),
            code: None,
        }];
        let formatted = format_outcome(&outcome, 10.0, 10_000, None, &calls, 0).await;
        let text = rendered(&formatted);
        assert!(
            text.contains(
                "Tool calls made before the failure (they are not undone): write (ok), bash (error)"
            ),
            "{text}"
        );
        let empty = format_outcome(&outcome, 10.0, 10_000, None, &[], 0).await;
        assert!(rendered(&empty).contains("No tool calls were made."));
    }

    /// Upstream `formatItems` then `joinAdjacentText`: each text item keeps its
    /// `==> text N/M <==` marker, and adjacent ones become one block.
    #[tokio::test]
    async fn adjacent_text_items_keep_their_markers_and_join() {
        let outcome = CodemodeOutcome {
            text_items: vec![
                CodemodeTextItem {
                    text: "first".to_string(),
                    console: false,
                },
                CodemodeTextItem {
                    text: "second".to_string(),
                    console: false,
                },
                CodemodeTextItem {
                    text: "third".to_string(),
                    console: false,
                },
            ],
            ..Default::default()
        };
        let formatted = format_outcome(&outcome, 0.0, 10_000, None, &[], 0).await;
        let text = rendered(&formatted);
        assert!(
            text.contains(
                "==> text 1/3 <==\nfirst\n==> text 2/3 <==\nsecond\n==> text 3/3 <==\nthird"
            ),
            "{text}"
        );
        let blocks: Vec<&ToolResultContent> = formatted
            .content
            .iter()
            .filter(|content| matches!(content, ToolResultContent::Text(_)))
            .collect();
        assert_eq!(blocks.len(), 2, "the header and one joined block: {text}");
    }

    /// A script that returns `null` shows `null` (upstream `result.value !== undefined`).
    #[tokio::test]
    async fn a_returned_null_is_shown() {
        let outcome = CodemodeOutcome {
            structured_output: Some(Value::Null),
            ..Default::default()
        };
        let formatted = format_outcome(&outcome, 0.0, 10_000, None, &[], 0).await;
        assert!(rendered(&formatted).contains("null"));
    }
}
