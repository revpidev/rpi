//! Port of `packages/coding-agent/src/core/tools/read.ts` @ pi 0.82.1 (2efa728).
//!
//! The read tool supports text files (with truncation) and images (with
//! auto-resize). TUI rendering methods (`renderCall`, `renderResult`,
//! compact-resource classification) are intentionally omitted — the Rust port
//! handles rendering in the TUI layer, not in the tool definition.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use rpi_agent::{AgentError, AgentTool, AgentToolResult, AgentToolUpdateCallback};
use rpi_ai::types::{ImageContent, ModelImageResizeOptions, TextContent, ToolResultContent};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::tools::ToolContext;
use crate::tools::image_process::{
    ProcessImageOptions, process_image, resolved_image_resize_options,
};
use crate::tools::mime::{IMAGE_TYPE_SNIFF_BYTES, detect_supported_image_mime_type};
use crate::tools::path_utils::resolve_read_path;
use crate::tools::truncate::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, format_size, truncate_head};

// ---------------------------------------------------------------------------
// ReadOperations (read.ts:43-50)
// ---------------------------------------------------------------------------

/// A byte stream opened for text selection. Regular files carry the size at
/// open; special files have no fixed size and are read to EOF.
pub struct ReadByteStream {
    pub reader: std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>>,
    pub initial_size: Option<u64>,
}

/// Pluggable operations for the read tool.
///
/// Override these to delegate file reading to remote systems (for example SSH).
#[async_trait]
pub trait ReadOperations: Send + Sync {
    /// Optional streaming reader. Older operations return `None` and retain
    /// the whole-file fallback.
    async fn create_read_stream(
        &self,
        _absolute_path: &Path,
    ) -> std::io::Result<Option<ReadByteStream>> {
        Ok(None)
    }

    /// Read file contents as a byte vector.
    async fn read_file(&self, absolute_path: &Path) -> std::io::Result<Vec<u8>>;

    /// Check if the file is readable (throw/return Err if not).
    async fn access(&self, absolute_path: &Path) -> std::io::Result<()>;

    /// Detect image MIME type from file magic bytes.
    /// Return `Ok(None)` for non-image files.
    async fn detect_image_mime_type(&self, absolute_path: &Path)
    -> std::io::Result<Option<String>>;
}

/// Default local-filesystem implementation of [`ReadOperations`] (read.ts:52-56).
///
/// Exposed so callers can wrap or delegate to the local behavior from custom
/// operations (extension/sandbox rerouting).
pub struct LocalReadOperations;

#[async_trait]
impl ReadOperations for LocalReadOperations {
    async fn create_read_stream(
        &self,
        absolute_path: &Path,
    ) -> std::io::Result<Option<ReadByteStream>> {
        let file = tokio::fs::File::open(absolute_path).await?;
        let metadata = file.metadata().await?;
        let initial_size = (metadata.is_file() && metadata.len() > 0).then_some(metadata.len());
        Ok(Some(ReadByteStream {
            reader: Box::pin(file),
            initial_size,
        }))
    }

    async fn read_file(&self, absolute_path: &Path) -> std::io::Result<Vec<u8>> {
        tokio::fs::read(absolute_path).await
    }

    async fn access(&self, absolute_path: &Path) -> std::io::Result<()> {
        // Equivalent to Node's fs.access(path, R_OK) — opens the file to verify
        // readability, then drops the handle.
        tokio::fs::File::open(absolute_path).await?;
        Ok(())
    }

    async fn detect_image_mime_type(
        &self,
        absolute_path: &Path,
    ) -> std::io::Result<Option<String>> {
        use tokio::io::AsyncReadExt;
        let mut file = tokio::fs::File::open(absolute_path).await?;
        let mut buffer = vec![0u8; IMAGE_TYPE_SNIFF_BYTES];
        let bytes_read = file.read(&mut buffer).await?;
        Ok(detect_supported_image_mime_type(&buffer[..bytes_read]).map(|s| s.to_string()))
    }
}

// ---------------------------------------------------------------------------
// ReadToolOptions (read.ts:58-63)
// ---------------------------------------------------------------------------

/// Options for creating a read tool instance.
pub struct ReadToolOptions {
    /// Whether to auto-resize images to the inline provider limits.
    /// Default: `true`.
    pub auto_resize_images: bool,
    /// Fallback resize profile when the execution context carries no live
    /// model (read.ts:60 @ f5c946480).
    pub resize_options: Option<ModelImageResizeOptions>,
    /// Custom operations for file reading. Default: local filesystem.
    pub operations: Option<Arc<dyn ReadOperations>>,
    /// Whether the current model supports image input.
    /// `None` = unknown (no non-vision note appended).
    /// `Some(false)` = append the non-vision omission note.
    /// `Some(true)` = model supports images (no note).
    pub model_supports_images: Option<bool>,
}

impl Default for ReadToolOptions {
    fn default() -> Self {
        Self {
            auto_resize_images: true,
            resize_options: None,
            operations: None,
            model_supports_images: None,
        }
    }
}

/// Create a read tool bound to the given context.
pub fn create_read_tool(ctx: &ToolContext, options: ReadToolOptions) -> Arc<dyn AgentTool> {
    let operations = options
        .operations
        .unwrap_or_else(|| Arc::new(LocalReadOperations));
    Arc::new(ReadTool {
        cwd: ctx.cwd.clone(),
        auto_resize_images: options.auto_resize_images,
        // `ctx?.model?.inputLimits?.images?.resize` (read.ts:112-117): the
        // live accessor follows permissionless model switches; the static
        // option is the upstream fallback for contexts without model
        // metadata.
        current_model: ctx.current_model.clone(),
        resize_options: options.resize_options,
        operations,
        model_supports_images: options.model_supports_images,
    })
}

// ---------------------------------------------------------------------------
// ReadTool
// ---------------------------------------------------------------------------

struct ReadTool {
    cwd: PathBuf,
    auto_resize_images: bool,
    /// Live session-model accessor (V16-04 FR-H); `None` in bare test
    /// contexts without a session.
    current_model: Option<Arc<dyn Fn() -> Option<rpi_ai::types::Model> + Send + Sync>>,
    /// Static fallback profile (read.ts:60).
    resize_options: Option<ModelImageResizeOptions>,
    operations: Arc<dyn ReadOperations>,
    model_supports_images: Option<bool>,
}

/// Non-vision model omission note (read.ts:87-92).
const NON_VISION_NOTE: &str =
    "[Current model does not support images. The image will be omitted from this request.]";

/// Result for programmatic callers such as codemode scripts: the text for
/// text files, and an image block for images that codemode's `image()`
/// accepts (`readOutputSchema`, read.ts:33-36). `note` is the text that goes
/// with the image, such as resize hints.
fn read_output_schema() -> &'static Value {
    static SCHEMA: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
    SCHEMA.get_or_init(|| {
        json!({
            "anyOf": [
                { "type": "string" },
                {
                    "type": "object",
                    "required": ["type", "data", "mimeType", "note"],
                    "properties": {
                        "type": { "type": "string", "const": "image" },
                        "data": { "type": "string" },
                        "mimeType": { "type": "string" },
                        "note": { "type": "string" }
                    }
                }
            ]
        })
    })
}

/// The image block and its note, or the text for text files and images that
/// could not be processed (`toReadOutput`, read.ts:73-77).
fn to_read_output(content: &[ToolResultContent]) -> Value {
    let text = content
        .iter()
        .find_map(|block| match block {
            ToolResultContent::Text(text) => Some(text.text.clone()),
            ToolResultContent::Image(_) => None,
        })
        .unwrap_or_default();
    let image = content.iter().find_map(|block| match block {
        ToolResultContent::Image(image) => Some(image),
        ToolResultContent::Text(_) => None,
    });
    match image {
        Some(image) => json!({
            "type": "image",
            "data": image.data,
            "mimeType": image.mime_type,
            "note": text,
        }),
        None => Value::String(text),
    }
}

/// Tool description with constants expanded (read.ts:212).
const DESCRIPTION: &str = "Read the contents of a file. Supports text files and \
images (jpg, png, gif, webp, bmp). Images are sent as attachments. For text files, \
output is truncated to 2000 lines or 50KB (whichever is hit first). Use offset/limit \
for large files. When you need the full file, continue with offset until complete.";

/// Format a number for display in error messages (JS `${offset}` semantics —
/// including `Infinity` for non-finite values, rc.12 review nit).
fn format_number(v: f64) -> String {
    if v.is_infinite() {
        if v > 0.0 {
            "Infinity".to_owned()
        } else {
            "-Infinity".to_owned()
        }
    } else if v.fract() == 0.0 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

struct StreamSelection {
    content: String,
    total_file_lines: usize,
    selected_total_lines: usize,
    selected_total_bytes: usize,
    first_selected_line_len: usize,
    #[cfg(test)]
    stored_lines: usize,
    #[cfg(test)]
    stored_bytes: usize,
}

/// Streaming UTF-8 decoder with the same lossy, BOM-preserving behavior as
/// Node's `TextDecoder("utf-8", { ignoreBOM: true })` (read.ts:158): a BOM is
/// kept, invalid sequences become U+FFFD, and a multi-byte character split
/// across chunks is decoded once its remaining bytes arrive.
struct Utf8StreamDecoder {
    pending: Vec<u8>,
}

impl Utf8StreamDecoder {
    fn new() -> Self {
        Self {
            pending: Vec::with_capacity(4),
        }
    }

    fn feed(&mut self, bytes: &[u8], selector: &mut LineSelector) {
        if self.pending.is_empty() {
            self.feed_slice(bytes, selector);
        } else {
            self.pending.extend_from_slice(bytes);
            let data = std::mem::take(&mut self.pending);
            self.feed_slice(&data, selector);
        }
    }

    fn feed_slice(&mut self, buf: &[u8], selector: &mut LineSelector) {
        let mut offset = 0;
        loop {
            match std::str::from_utf8(&buf[offset..]) {
                Ok(valid) => {
                    if !valid.is_empty() {
                        selector.select(valid);
                    }
                    break;
                }
                Err(error) => {
                    let valid_up_to = offset + error.valid_up_to();
                    if valid_up_to > offset {
                        let valid = std::str::from_utf8(&buf[offset..valid_up_to])
                            .expect("valid_up_to marks a valid prefix");
                        selector.select(valid);
                    }
                    match error.error_len() {
                        Some(len) => {
                            selector.select("\u{FFFD}");
                            offset = valid_up_to + len;
                            if offset >= buf.len() {
                                break;
                            }
                        }
                        None => {
                            // An incomplete trailing sequence: keep it for the
                            // next chunk (at most three bytes).
                            self.pending.extend_from_slice(&buf[valid_up_to..]);
                            break;
                        }
                    }
                }
            }
        }
    }

    fn finish(&mut self, selector: &mut LineSelector) {
        if self.pending.is_empty() {
            return;
        }
        let buf = std::mem::take(&mut self.pending);
        selector.select(&String::from_utf8_lossy(&buf));
    }
}

/// Byte-bounded line selector (read.ts:150-235 `selectLinesFromStream`).
///
/// Only the lines `truncate_head` can show are stored: a line over the byte
/// cap is measured but not retained, and once the stored line/byte caps are
/// exceeded no further line is stored. Counters still advance over the whole
/// stream, so `total_file_lines`/`selected_total_bytes` describe the file.
struct LineSelector {
    start_line: usize,
    end_line: f64,
    line: usize,
    selected_lines: usize,
    selected_bytes: usize,
    first_line_bytes: usize,
    last_line_empty: bool,
    stored_lines: Vec<String>,
    stored_bytes: usize,
    stored_past_caps: bool,
    pieces: Option<Vec<String>>,
    line_bytes: usize,
    line_has_text: bool,
}

impl LineSelector {
    fn new(start_line: usize, limit: Option<f64>) -> Self {
        let end_line = limit.map_or(f64::INFINITY, |limit| start_line as f64 + limit);
        Self {
            start_line,
            end_line,
            line: 0,
            selected_lines: 0,
            selected_bytes: 0,
            first_line_bytes: 0,
            last_line_empty: true,
            stored_lines: Vec::new(),
            stored_bytes: 0,
            stored_past_caps: false,
            pieces: Some(Vec::new()),
            line_bytes: 0,
            line_has_text: false,
        }
    }

    fn append_to_line(&mut self, text: &str, start: usize, end: usize) {
        self.line_has_text = true;
        let Some(pieces) = self.pieces.as_mut() else {
            return;
        };
        let piece = &text[start..end];
        // A line over the byte cap is never shown, and a first line over it is
        // only measured: never store all of a 1 GB line.
        if self.line_bytes <= DEFAULT_MAX_BYTES {
            pieces.push(piece.to_owned());
        }
        self.line_bytes += piece.len();
    }

    fn finish_line(&mut self) {
        if let Some(pieces) = self.pieces.take() {
            if self.selected_lines == 0 {
                self.first_line_bytes = self.line_bytes;
            }
            // Storing continues one line past the caps: truncate_head ignores
            // a trailing empty line, so the line after the cap must be there
            // for it to decide truncation as for the full text.
            if self.stored_lines.len() > DEFAULT_MAX_LINES || self.stored_bytes > DEFAULT_MAX_BYTES
            {
                self.stored_past_caps = true;
            }
            let text = pieces.join("");
            self.stored_bytes += text.len() + usize::from(!self.stored_lines.is_empty());
            self.stored_lines.push(text);
        }
        self.selected_lines += 1;
        self.last_line_empty = !self.line_has_text;
        self.pieces = if self.stored_past_caps {
            None
        } else {
            Some(Vec::new())
        };
        self.line_bytes = 0;
        self.line_has_text = false;
    }

    fn select(&mut self, text: &str) {
        let mut position = 0usize;
        // Start of the selected text in this chunk, measured as a whole: the
        // joined lines' size.
        let mut selection_start: Option<usize> = None;
        loop {
            let selected = self.line >= self.start_line && (self.line as f64) < self.end_line;
            if selected && selection_start.is_none() {
                selection_start = Some(position);
            }
            let newline = text[position..].find('\n').map(|index| position + index);
            let end = newline.unwrap_or(text.len());
            if selected && end > position {
                self.append_to_line(text, position, end);
            }
            let Some(newline) = newline else {
                break;
            };
            if selected {
                self.finish_line();
                if (self.line + 1) as f64 == self.end_line {
                    if let Some(start) = selection_start {
                        self.selected_bytes += text[start..newline].len();
                    }
                    selection_start = None;
                }
            }
            self.line += 1;
            position = newline + 1;
        }
        if let Some(start) = selection_start {
            self.selected_bytes += text[start..].len();
        }
    }

    fn finish(&mut self) {
        if self.line >= self.start_line && (self.line as f64) < self.end_line {
            self.finish_line();
        }
    }
}

fn valid_stream_range(offset: Option<f64>, limit: Option<f64>) -> bool {
    fn valid(value: Option<f64>) -> bool {
        value.is_none_or(|value| value.is_finite() && value >= 0.0 && value.fract() == 0.0)
    }
    valid(offset) && valid(limit)
}

async fn read_selected_stream(
    mut stream: ReadByteStream,
    start_line_raw: f64,
    limit: Option<f64>,
    signal: &CancellationToken,
) -> Result<StreamSelection, AgentError> {
    use tokio::io::AsyncReadExt;

    let mut selector = LineSelector::new(start_line_raw as usize, limit);
    let mut decoder = Utf8StreamDecoder::new();
    let mut buffer = [0u8; 64 * 1024];

    loop {
        if signal.is_cancelled() {
            return Err(AgentError::Message("Operation aborted".to_string()));
        }
        let allowed = stream.initial_size.map_or(buffer.len(), |remaining| {
            remaining.min(buffer.len() as u64) as usize
        });
        if allowed == 0 {
            break;
        }
        let read = stream.reader.read(&mut buffer[..allowed]).await?;
        if read == 0 {
            break;
        }
        if let Some(remaining) = &mut stream.initial_size {
            *remaining = remaining.saturating_sub(read as u64);
        }
        decoder.feed(&buffer[..read], &mut selector);
    }
    decoder.finish(&mut selector);
    selector.finish();

    let content = selector.stored_lines.join("\n");
    let selected_total_lines = if selector.selected_bytes == 0 {
        0
    } else {
        selector.selected_lines - usize::from(selector.last_line_empty)
    };
    Ok(StreamSelection {
        content,
        total_file_lines: selector.line + 1,
        selected_total_lines,
        selected_total_bytes: selector.selected_bytes,
        first_selected_line_len: selector.first_line_bytes,
        #[cfg(test)]
        stored_lines: selector.stored_lines.len(),
        #[cfg(test)]
        stored_bytes: selector.stored_bytes,
    })
}

#[async_trait]
impl AgentTool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }

    fn label(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn parameters(&self) -> &Value {
        // TypeBox Type.Object with additionalProperties: false.
        static PARAMETERS: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
        PARAMETERS.get_or_init(|| {
            json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file to read (relative or absolute)"
                    },
                    "offset": {
                        "type": "number",
                        "description": "Line number to start reading from (1-indexed)"
                    },
                    "limit": {
                        "type": "number",
                        "description": "Maximum number of lines to read"
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            })
        })
    }

    fn constrained_sampling(&self) -> Option<rpi_ai::types::ConstrainedSampling> {
        // read.ts:77 (`fcff255b0`): strict-prefer by default, no gate.
        crate::tools::builtin_strict_prefer_sampling()
    }

    fn output_schema(&self) -> Option<&Value> {
        // read.ts:100: programmatic callers (codemode) get the same content
        // as a string or an image block with its note (#10251).
        Some(read_output_schema())
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Value,
        signal: CancellationToken,
        _on_update: Option<AgentToolUpdateCallback>,
    ) -> Result<AgentToolResult, AgentError> {
        // --- Extract parameters (read.ts:218) ---
        let path = params["path"]
            .as_str()
            .ok_or_else(|| AgentError::Message("Missing required parameter: path".to_string()))?;
        let offset = params.get("offset").and_then(|v| v.as_f64());
        let limit = params.get("limit").and_then(|v| v.as_f64());

        // --- Abort check at entry (read.ts:225-228) ---
        if signal.is_cancelled() {
            return Err(AgentError::Message("Operation aborted".to_string()));
        }

        // --- Resolve path (read.ts:238) ---
        let absolute_path = resolve_read_path(path, &self.cwd).await;

        if signal.is_cancelled() {
            return Err(AgentError::Message("Operation aborted".to_string()));
        }

        // --- Access check (read.ts:241) ---
        self.operations.access(&absolute_path).await?;

        if signal.is_cancelled() {
            return Err(AgentError::Message("Operation aborted".to_string()));
        }

        // --- Detect image MIME type (read.ts:243) ---
        let mime_type = self
            .operations
            .detect_image_mime_type(&absolute_path)
            .await?;

        let non_vision_note = match self.model_supports_images {
            Some(false) => Some(NON_VISION_NOTE),
            _ => None,
        };

        if let Some(mime) = mime_type {
            // ===================== Image branch (read.ts:247-263) =====================
            let buffer = self.operations.read_file(&absolute_path).await?;
            let resize_options = self
                .current_model
                .as_ref()
                .and_then(|current| current())
                .as_ref()
                .and_then(resolved_image_resize_options)
                .or(self.resize_options);
            let result = process_image(
                &buffer,
                &mime,
                ProcessImageOptions {
                    auto_resize_images: Some(self.auto_resize_images),
                    resize_options,
                },
            );

            let content = match result {
                Err(err) => {
                    // processImage failure (read.ts:251-254).
                    let mut text_note = format!("Read image file [{mime}]\n{}", err.message);
                    if let Some(note) = non_vision_note {
                        text_note.push('\n');
                        text_note.push_str(note);
                    }
                    vec![ToolResultContent::Text(TextContent {
                        text: text_note,
                        text_signature: None,
                    })]
                }
                Ok(processed) => {
                    // processImage success (read.ts:255-263).
                    let mut text_note = format!("Read image file [{}]", processed.mime_type);
                    if !processed.hints.is_empty() {
                        text_note.push('\n');
                        text_note.push_str(&processed.hints.join("\n"));
                    }
                    if let Some(note) = non_vision_note {
                        text_note.push('\n');
                        text_note.push_str(note);
                    }
                    vec![
                        ToolResultContent::Text(TextContent {
                            text: text_note,
                            text_signature: None,
                        }),
                        ToolResultContent::Image(ImageContent {
                            data: processed.data,
                            mime_type: processed.mime_type,
                        }),
                    ]
                }
            };

            if signal.is_cancelled() {
                return Err(AgentError::Message("Operation aborted".to_string()));
            }

            return Ok(AgentToolResult {
                structured_content: Some(to_read_output(&content)),
                content,
                details: Value::Null,
                usage: None,
                terminate: None,

                is_error: None,
            });
        }

        // ===================== Text branch (read.ts:264-316) =====================
        let start_line_raw = match offset {
            Some(o) if o != 0.0 => (o - 1.0).max(0.0),
            _ => 0.0,
        };
        let start_line_display = start_line_raw as usize + 1;
        let start_line = start_line_raw as usize;
        let (
            selected_content,
            total_file_lines,
            selected_total_lines,
            selected_total_bytes,
            first_selected_line_len,
            user_limited_lines,
        ) = if valid_stream_range(offset, limit)
            && let Some(stream) = self.operations.create_read_stream(&absolute_path).await?
        {
            let selection = read_selected_stream(stream, start_line_raw, limit, &signal).await?;
            if start_line_raw >= selection.total_file_lines as f64 {
                return Err(AgentError::Message(format!(
                    "Offset {} is beyond end of file ({} lines total)",
                    format_number(offset.unwrap_or(0.0)),
                    selection.total_file_lines
                )));
            }
            let limited = limit.map(|lim| {
                let end = ((start_line_raw + lim).min(selection.total_file_lines as f64) as usize)
                    .max(start_line);
                end.saturating_sub(start_line)
            });
            (
                selection.content,
                selection.total_file_lines,
                selection.selected_total_lines,
                selection.selected_total_bytes,
                selection.first_selected_line_len,
                limited,
            )
        } else {
            let buffer = self.operations.read_file(&absolute_path).await?;
            let text_content = String::from_utf8_lossy(&buffer);
            let all_lines: Vec<&str> = text_content.split('\n').collect();
            let total_file_lines = all_lines.len();
            if start_line_raw >= total_file_lines as f64 {
                return Err(AgentError::Message(format!(
                    "Offset {} is beyond end of file ({} lines total)",
                    format_number(offset.unwrap_or(0.0)),
                    total_file_lines
                )));
            }
            let (selected_content, user_limited_lines) = if let Some(lim) = limit {
                let end_line_raw = (start_line_raw + lim).min(total_file_lines as f64);
                let end_line = end_line_raw.max(start_line_raw) as usize;
                let content = if end_line > start_line {
                    all_lines[start_line..end_line].join("\n")
                } else {
                    String::new()
                };
                (content, Some(end_line.saturating_sub(start_line)))
            } else {
                (all_lines[start_line..].join("\n"), None)
            };
            let selected_total_lines = if selected_content.is_empty() {
                0
            } else {
                selected_content.split('\n').count() - usize::from(selected_content.ends_with('\n'))
            };
            (
                selected_content.clone(),
                total_file_lines,
                selected_total_lines,
                selected_content.len(),
                all_lines.get(start_line).map_or(0, |line| line.len()),
                user_limited_lines,
            )
        };

        // Apply truncation (read.ts:288-314).
        let mut truncation = truncate_head(&selected_content, None);
        truncation.total_lines = selected_total_lines;
        truncation.total_bytes = selected_total_bytes;
        let output_text: String;
        let details: Value;

        if truncation.first_line_exceeds_limit {
            // First line alone exceeds byte limit — sed fallback (read.ts:290-293).
            let first_line_size = format_size(first_selected_line_len);
            output_text = format!(
                "[Line {start_line_display} is {first_line_size}, exceeds {} limit. \
                 Use bash: sed -n '{start_line_display}p' {path} | head -c {DEFAULT_MAX_BYTES}]",
                format_size(DEFAULT_MAX_BYTES)
            );
            details =
                serde_json::to_value(json!({ "truncation": &truncation })).unwrap_or(Value::Null);
        } else if truncation.truncated {
            // Truncation occurred — continuation notice (read.ts:295-305).
            let end_line_display = start_line_display + truncation.output_lines - 1;
            let next_offset = end_line_display + 1;
            output_text = match truncation.truncated_by {
                Some(crate::tools::truncate::TruncatedBy::Lines) => {
                    format!(
                        "{content}\n\n[Showing lines {start_line_display}-{end_line_display} \
                         of {total_file_lines}. Use offset={next_offset} to continue.]",
                        content = truncation.content
                    )
                }
                _ => {
                    format!(
                        "{content}\n\n[Showing lines {start_line_display}-{end_line_display} \
                         of {total_file_lines} ({size} limit). Use offset={next_offset} to continue.]",
                        content = truncation.content,
                        size = format_size(DEFAULT_MAX_BYTES)
                    )
                }
            };
            details =
                serde_json::to_value(json!({ "truncation": &truncation })).unwrap_or(Value::Null);
        } else if let Some(limited) = user_limited_lines {
            if start_line + limited < total_file_lines {
                // User-specified limit stopped early (read.ts:306-310).
                let remaining = total_file_lines - (start_line + limited);
                let next_offset = start_line + limited + 1;
                output_text = format!(
                    "{content}\n\n[{remaining} more lines in file. Use offset={next_offset} to continue.]",
                    content = truncation.content
                );
            } else {
                // No truncation, limit covers rest of file.
                output_text = truncation.content;
            }
            details = Value::Null;
        } else {
            // No truncation and no user limit (read.ts:312-313).
            output_text = truncation.content;
            details = Value::Null;
        }

        if signal.is_cancelled() {
            return Err(AgentError::Message("Operation aborted".to_string()));
        }

        Ok(AgentToolResult {
            structured_content: Some(Value::String(output_text.clone())),
            content: vec![ToolResultContent::Text(TextContent {
                text: output_text,
                text_signature: None,
            })],
            details,
            usage: None,
            terminate: None,

            is_error: None,
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolContext;
    use base64::Engine;
    use std::path::PathBuf;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context as TaskContext, Poll};
    use tokio::io::{AsyncRead, ReadBuf};

    /// Minimal temp dir for test file creation.
    struct TestDir(PathBuf);
    impl TestDir {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("rpi-read-test-{:x}", rand_seed()));
            std::fs::create_dir_all(&dir).unwrap();
            TestDir(dir)
        }
        fn path(&self) -> &Path {
            &self.0
        }
        fn write(&self, name: &str, content: &str) -> PathBuf {
            let p = self.0.join(name);
            std::fs::write(&p, content).unwrap();
            p
        }
        fn write_bytes(&self, name: &str, content: &[u8]) -> PathBuf {
            let p = self.0.join(name);
            std::fs::write(&p, content).unwrap();
            p
        }
    }
    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn rand_seed() -> u64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0)
    }

    fn ctx(cwd: &Path) -> ToolContext {
        ToolContext {
            cwd: cwd.to_path_buf(),
            session_env: None,
            current_model: None,
        }
    }

    fn no_signal() -> CancellationToken {
        CancellationToken::new()
    }

    async fn read_file(tool: &dyn AgentTool, path: &str) -> AgentToolResult {
        tool.execute("test", json!({ "path": path }), no_signal(), None)
            .await
            .unwrap()
    }

    async fn read_file_params(tool: &dyn AgentTool, params: Value) -> AgentToolResult {
        tool.execute("test", params, no_signal(), None)
            .await
            .unwrap()
    }

    fn text_of(result: &AgentToolResult) -> &str {
        match &result.content[0] {
            ToolResultContent::Text(t) => &t.text,
            _ => panic!("expected text content"),
        }
    }

    // ---- should read file contents that fit within limits ----
    #[tokio::test]
    async fn test_read_small_file() {
        let dir = TestDir::new();
        dir.write("small.txt", "line1\nline2\nline3");
        let tool = create_read_tool(&ctx(dir.path()), ReadToolOptions::default());
        let result = read_file(&*tool, "small.txt").await;
        assert_eq!(text_of(&result), "line1\nline2\nline3");
        assert_eq!(result.details, Value::Null);
        // Programmatic callers (codemode) get the text as a string
        // (read.ts:216 `structuredContent: toReadOutput(result.content)`).
        assert_eq!(
            result.structured_content,
            Some(Value::String("line1\nline2\nline3".to_owned()))
        );
    }

    // ---- should handle non-existent files ----
    #[tokio::test]
    async fn test_read_nonexistent_file() {
        let dir = TestDir::new();
        let tool = create_read_tool(&ctx(dir.path()), ReadToolOptions::default());
        let result = tool
            .execute(
                "test",
                json!({ "path": "nonexistent.txt" }),
                no_signal(),
                None,
            )
            .await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        // Should mention the missing file (OS-dependent wording).
        assert!(
            msg.contains("No such file") || msg.contains("ENOENT") || msg.contains("not found"),
            "error: {msg}"
        );
    }

    // ---- should truncate files exceeding line limit ----
    #[tokio::test]
    async fn test_truncate_by_lines() {
        let dir = TestDir::new();
        let content: String = (1..=2500)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        dir.write("big.txt", &content);
        let tool = create_read_tool(&ctx(dir.path()), ReadToolOptions::default());
        let result = read_file(&*tool, "big.txt").await;
        let text = text_of(&result);
        assert!(text.contains("\n\n[Showing lines 1-2000 of 2500. Use offset=2001 to continue.]"));
        // Details should include truncation.
        assert!(result.details.get("truncation").is_some());
    }

    // ---- should truncate when byte limit exceeded ----
    #[tokio::test]
    async fn test_truncate_by_bytes() {
        let dir = TestDir::new();
        // 500 lines × ~200 bytes each = ~100KB, exceeds 50KB byte limit.
        let content: String = (0..500)
            .map(|_| "x".repeat(200))
            .collect::<Vec<_>>()
            .join("\n");
        dir.write("wide.txt", &content);
        let tool = create_read_tool(&ctx(dir.path()), ReadToolOptions::default());
        let result = read_file(&*tool, "wide.txt").await;
        let text = text_of(&result);
        // Byte-truncation message includes the size limit.
        assert!(text.contains("50.0KB limit"));
        assert!(text.contains("Use offset="));
        assert!(result.details.get("truncation").is_some());
    }

    // ---- should handle offset parameter ----
    #[tokio::test]
    async fn test_offset() {
        let dir = TestDir::new();
        let content: String = (1..=100)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        dir.write("offset.txt", &content);
        let tool = create_read_tool(&ctx(dir.path()), ReadToolOptions::default());
        let result = read_file_params(&*tool, json!({ "path": "offset.txt", "offset": 51 })).await;
        let text = text_of(&result);
        assert!(text.starts_with("line 51"));
        assert!(text.ends_with("line 100"));
    }

    // ---- should handle limit parameter ----
    #[tokio::test]
    async fn test_limit() {
        let dir = TestDir::new();
        let content: String = (1..=100)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        dir.write("limit.txt", &content);
        let tool = create_read_tool(&ctx(dir.path()), ReadToolOptions::default());
        let result = read_file_params(&*tool, json!({ "path": "limit.txt", "limit": 10 })).await;
        let text = text_of(&result);
        assert!(text.starts_with("line 1"));
        assert!(text.contains("line 10"));
        assert!(!text.contains("line 11"));
        assert!(text.contains("[90 more lines in file. Use offset=11 to continue.]"));
    }

    // ---- P1-1 regression: model-controlled out-of-range limit must not
    // panic. JS `slice(start, end)` with end < start yields `[]`; the port
    // must clamp the same way (negative / NaN / ±inf / i64 extremes).
    #[tokio::test]
    async fn test_negative_limit_returns_empty_selection() {
        let dir = TestDir::new();
        dir.write("neg.txt", "line 1\nline 2\nline 3");
        let tool = create_read_tool(&ctx(dir.path()), ReadToolOptions::default());
        let result = read_file_params(&*tool, json!({ "path": "neg.txt", "limit": -5 })).await;
        let text = text_of(&result);
        assert!(
            !text.contains("line 1") && !text.contains("line 2"),
            "negative limit must select no file content: {text:?}"
        );
    }

    #[tokio::test]
    async fn test_extreme_limits_do_not_panic() {
        let dir = TestDir::new();
        dir.write("extreme.txt", "line 1\nline 2\nline 3");
        let tool = create_read_tool(&ctx(dir.path()), ReadToolOptions::default());
        // Finite negatives clamp to an empty selection (JS slice semantics);
        // non-finite values cannot be represented in JSON and arrive as
        // `null` → treated as "no limit" (whole file) — both must return a
        // result, never panic.
        for limit in [
            -0.0_f64,
            -1.0_f64,
            -9_007_199_254_740_993.0_f64, // beyond i64 range
            9_007_199_254_740_993.0_f64,
            1.0e308_f64,
        ] {
            let result =
                read_file_params(&*tool, json!({ "path": "extreme.txt", "limit": limit })).await;
            let text = text_of(&result);
            if limit.is_sign_negative() {
                assert!(
                    !text.contains("line 1"),
                    "limit {limit} selects nothing: {text:?}"
                );
            } else {
                assert!(text.contains("line 1"), "limit {limit} still reads");
            }
        }
        for limit in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            // json! serializes non-finite f64 as null → no limit → whole file.
            let result =
                read_file_params(&*tool, json!({ "path": "extreme.txt", "limit": limit })).await;
            assert!(text_of(&result).contains("line 1"), "limit {limit} = null");
        }
    }

    // ---- should handle offset + limit together ----
    #[tokio::test]
    async fn test_offset_and_limit() {
        let dir = TestDir::new();
        let content: String = (1..=100)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        dir.write("offlim.txt", &content);
        let tool = create_read_tool(&ctx(dir.path()), ReadToolOptions::default());
        let result = read_file_params(
            &*tool,
            json!({ "path": "offlim.txt", "offset": 41, "limit": 20 }),
        )
        .await;
        let text = text_of(&result);
        assert!(text.starts_with("line 41"));
        assert!(text.contains("line 60"));
        assert!(!text.contains("line 61"));
        assert!(text.contains("[40 more lines in file. Use offset=61 to continue.]"));
    }

    // ---- should show error when offset is beyond file length ----
    #[tokio::test]
    async fn test_offset_beyond_file() {
        let dir = TestDir::new();
        dir.write("tiny.txt", "a\nb\nc");
        let tool = create_read_tool(&ctx(dir.path()), ReadToolOptions::default());
        let result = tool
            .execute(
                "test",
                json!({ "path": "tiny.txt", "offset": 100 }),
                no_signal(),
                None,
            )
            .await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("Offset 100 is beyond end of file (3 lines total)"));
    }

    // ---- should include truncation details when truncated ----
    #[tokio::test]
    async fn test_details_truncation() {
        let dir = TestDir::new();
        let content: String = (1..=2500)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        dir.write("big.txt", &content);
        let tool = create_read_tool(&ctx(dir.path()), ReadToolOptions::default());
        let result = read_file(&*tool, "big.txt").await;
        let truncation = result
            .details
            .get("truncation")
            .expect("truncation in details");
        assert_eq!(truncation["truncated"], json!(true));
        assert_eq!(truncation["totalLines"], json!(2500));
        assert_eq!(truncation["outputLines"], json!(2000));
    }

    // ---- should detect image MIME type from file magic (not extension) ----
    #[tokio::test]
    async fn test_detect_image_by_magic() {
        let dir = TestDir::new();
        // Create a small PNG and name it .txt.
        let img: image::ImageBuffer<image::Rgba<u8>, Vec<u8>> =
            image::ImageBuffer::from_pixel(10, 10, image::Rgba([255, 0, 0, 255]));
        let mut buf = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
            .unwrap();
        dir.write_bytes("image.txt", &buf);
        let tool = create_read_tool(&ctx(dir.path()), ReadToolOptions::default());
        let result = read_file(&*tool, "image.txt").await;
        // Should be treated as an image, not text.
        assert_eq!(result.content.len(), 2); // text + image blocks
        match &result.content[0] {
            ToolResultContent::Text(t) => assert!(t.text.starts_with("Read image file [")),
            _ => panic!("expected text block"),
        }
        assert!(matches!(result.content[1], ToolResultContent::Image(_)));
        // Programmatic callers get the same image as a block with its note.
        let structured = result
            .structured_content
            .clone()
            .expect("structured content");
        assert_eq!(structured["type"], json!("image"));
        assert_eq!(structured["mimeType"], json!("image/png"));
        assert!(
            structured["data"]
                .as_str()
                .is_some_and(|data| !data.is_empty())
        );
        assert_eq!(structured["note"].as_str(), Some(text_of(&result)));
    }

    // ---- should read BMP files as PNG image attachments ----
    #[tokio::test]
    async fn test_bmp_converted_to_png() {
        let dir = TestDir::new();
        let img: image::ImageBuffer<image::Rgb<u8>, Vec<u8>> =
            image::ImageBuffer::from_pixel(20, 20, image::Rgb([100, 150, 200]));
        let mut buf = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Bmp)
            .unwrap();
        dir.write_bytes("test.bmp", &buf);
        let tool = create_read_tool(&ctx(dir.path()), ReadToolOptions::default());
        let result = read_file(&*tool, "test.bmp").await;
        assert_eq!(result.content.len(), 2);
        match &result.content[0] {
            ToolResultContent::Text(t) => {
                assert!(
                    t.text
                        .contains("[Image converted from image/bmp to image/png.]")
                );
            }
            _ => panic!("expected text block"),
        }
        match &result.content[1] {
            ToolResultContent::Image(img) => assert_eq!(img.mime_type, "image/png"),
            _ => panic!("expected image block"),
        }
    }

    // ---- V16-04 FR-H (#9631): the live model resize profile applies ----
    #[tokio::test]
    async fn test_read_image_uses_live_model_resize_profile() {
        let dir = TestDir::new();
        let img: image::ImageBuffer<image::Rgba<u8>, Vec<u8>> =
            image::ImageBuffer::from_pixel(300, 200, image::Rgba([1, 2, 3, 255]));
        let mut buf = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
            .unwrap();
        dir.write_bytes("wide.png", &buf);
        let model: rpi_ai::types::Model = serde_json::from_value(serde_json::json!({
            "id": "m", "name": "m", "api": "openai-completions", "provider": "p",
            "baseUrl": "https://example.com", "reasoning": false,
            "input": ["text", "image"],
            "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
            "contextWindow": 1000, "maxTokens": 100,
            "inputLimits": {"images": {"resize": {"maxWidth": 40, "maxHeight": 30}}}
        }))
        .expect("model");
        let mut tool_ctx = ctx(dir.path());
        tool_ctx.current_model = Some(Arc::new(move || Some(model.clone())));
        let tool = create_read_tool(&tool_ctx, ReadToolOptions::default());

        let result = read_file(&*tool, "wide.png").await;
        match &result.content[1] {
            ToolResultContent::Image(img) => {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(&img.data)
                    .expect("base64");
                let decoded = image::load_from_memory(&bytes).expect("decoded image");
                assert!(
                    decoded.width() <= 40 && decoded.height() <= 30,
                    "profile capped to {}x{}",
                    decoded.width(),
                    decoded.height()
                );
            }
            other => panic!("expected image block, got {other:?}"),
        }
    }

    // ---- should treat files with image extension but non-image content as text ----
    #[tokio::test]
    async fn test_image_extension_text_content() {
        let dir = TestDir::new();
        dir.write("fake.png", "this is actually text content");
        let tool = create_read_tool(&ctx(dir.path()), ReadToolOptions::default());
        let result = read_file(&*tool, "fake.png").await;
        // Single text block, not image.
        assert_eq!(result.content.len(), 1);
        assert_eq!(text_of(&result), "this is actually text content");
    }

    // ---- first line exceeds byte limit — sed fallback ----
    #[tokio::test]
    async fn test_first_line_exceeds_limit() {
        let dir = TestDir::new();
        // One line of 60000 bytes — exceeds 50KB limit.
        let content = "x".repeat(60000);
        dir.write("huge_line.txt", &content);
        let tool = create_read_tool(&ctx(dir.path()), ReadToolOptions::default());
        let result = read_file(&*tool, "huge_line.txt").await;
        let text = text_of(&result);
        assert!(text.contains("[Line 1 is"));
        assert!(text.contains("exceeds 50.0KB limit"));
        assert!(text.contains("Use bash: sed -n '1p'"));
        assert!(text.contains(&format!("head -c {DEFAULT_MAX_BYTES}")));
    }

    // ---- non-vision model note ----
    #[tokio::test]
    async fn test_non_vision_note() {
        let dir = TestDir::new();
        let img: image::ImageBuffer<image::Rgba<u8>, Vec<u8>> =
            image::ImageBuffer::from_pixel(10, 10, image::Rgba([255, 0, 0, 255]));
        let mut buf = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
            .unwrap();
        dir.write_bytes("img.png", &buf);
        let tool = create_read_tool(
            &ctx(dir.path()),
            ReadToolOptions {
                model_supports_images: Some(false),
                ..Default::default()
            },
        );
        let result = read_file(&*tool, "img.png").await;
        match &result.content[0] {
            ToolResultContent::Text(t) => {
                assert!(t.text.contains(NON_VISION_NOTE));
            }
            _ => panic!("expected text block"),
        }
    }

    // ---- B07: streaming read is byte-bounded and keeps whole-file semantics ----

    struct VirtualOps {
        data: Vec<u8>,
        initial_size: Option<u64>,
        chunk: usize,
        stream: bool,
        cancel_after_first_read: Option<CancellationToken>,
        reads: Arc<AtomicUsize>,
    }

    impl VirtualOps {
        fn streaming(data: Vec<u8>, chunk: usize) -> Self {
            Self {
                initial_size: None,
                data,
                chunk,
                stream: true,
                cancel_after_first_read: None,
                reads: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn whole_file(data: Vec<u8>) -> Self {
            Self {
                initial_size: None,
                data,
                chunk: 0,
                stream: false,
                cancel_after_first_read: None,
                reads: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    struct ChunkedReader {
        data: Vec<u8>,
        offset: usize,
        chunk: usize,
        cancel_after_first_read: Option<CancellationToken>,
        reads: Arc<AtomicUsize>,
    }

    impl AsyncRead for ChunkedReader {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut TaskContext<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            let this = self.as_mut().get_mut();
            if let Some(token) = this.cancel_after_first_read.take() {
                token.cancel();
            }
            let remaining = this.data.len().saturating_sub(this.offset);
            let len = remaining.min(this.chunk).min(buf.remaining());
            if len == 0 {
                return Poll::Ready(Ok(()));
            }
            buf.put_slice(&this.data[this.offset..this.offset + len]);
            this.offset += len;
            this.reads.fetch_add(1, Ordering::SeqCst);
            Poll::Ready(Ok(()))
        }
    }

    #[async_trait]
    impl ReadOperations for VirtualOps {
        async fn create_read_stream(
            &self,
            _absolute_path: &Path,
        ) -> std::io::Result<Option<ReadByteStream>> {
            if !self.stream {
                return Ok(None);
            }
            Ok(Some(ReadByteStream {
                reader: Box::pin(ChunkedReader {
                    data: self.data.clone(),
                    offset: 0,
                    chunk: self.chunk.max(1),
                    cancel_after_first_read: self.cancel_after_first_read.clone(),
                    reads: Arc::clone(&self.reads),
                }),
                initial_size: self.initial_size,
            }))
        }

        async fn read_file(&self, _absolute_path: &Path) -> std::io::Result<Vec<u8>> {
            Ok(self.data.clone())
        }

        async fn access(&self, _absolute_path: &Path) -> std::io::Result<()> {
            Ok(())
        }

        async fn detect_image_mime_type(
            &self,
            _absolute_path: &Path,
        ) -> std::io::Result<Option<String>> {
            Ok(None)
        }
    }

    /// Older operations only implement the required methods; the default
    /// `create_read_stream` returns `None` and the whole-file path must remain.
    struct NoStreamOps {
        data: Vec<u8>,
    }

    #[async_trait]
    impl ReadOperations for NoStreamOps {
        async fn read_file(&self, _absolute_path: &Path) -> std::io::Result<Vec<u8>> {
            Ok(self.data.clone())
        }

        async fn access(&self, _absolute_path: &Path) -> std::io::Result<()> {
            Ok(())
        }

        async fn detect_image_mime_type(
            &self,
            _absolute_path: &Path,
        ) -> std::io::Result<Option<String>> {
            Ok(None)
        }
    }

    fn virtual_tool(ops: Arc<dyn ReadOperations>) -> Arc<dyn AgentTool> {
        create_read_tool(
            &ctx(Path::new("/virtual")),
            ReadToolOptions {
                operations: Some(ops),
                ..Default::default()
            },
        )
    }

    async fn run_virtual(
        ops: Arc<dyn ReadOperations>,
        params: Value,
    ) -> Result<AgentToolResult, String> {
        let tool = virtual_tool(ops);
        tool.execute("test", params, no_signal(), None)
            .await
            .map_err(|error| error.to_string())
    }

    async fn selection_for(
        data: Vec<u8>,
        chunk: usize,
        start_line: f64,
        limit: Option<f64>,
    ) -> StreamSelection {
        let ops = VirtualOps::streaming(data, chunk);
        let stream = ops
            .create_read_stream(Path::new("/virtual/file.txt"))
            .await
            .unwrap()
            .expect("stream");
        read_selected_stream(stream, start_line, limit, &no_signal())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn test_stream_storage_is_bounded_for_a_huge_single_line() {
        let size = 8 * 1024 * 1024;
        let selection = selection_for(vec![b'a'; size], 64 * 1024, 0.0, None).await;
        assert_eq!(selection.first_selected_line_len, size);
        assert_eq!(selection.total_file_lines, 1);
        assert!(
            selection.stored_bytes <= DEFAULT_MAX_BYTES + 64 * 1024,
            "stored {} bytes for an {size}-byte line",
            selection.stored_bytes
        );
        assert!(selection.stored_lines <= 2);
        let mut truncation = truncate_head(&selection.content, None);
        truncation.total_lines = selection.selected_total_lines;
        truncation.total_bytes = selection.selected_total_bytes;
        assert!(
            truncation.first_line_exceeds_limit,
            "the first line still exceeds the byte limit"
        );
    }

    #[tokio::test]
    async fn test_stream_storage_stops_after_the_line_cap() {
        let data = (0..3000)
            .map(|index| format!("line {index}\n"))
            .collect::<String>();
        let selection = selection_for(data.into_bytes(), 3, 0.0, None).await;
        assert_eq!(selection.total_file_lines, 3001);
        assert_eq!(selection.selected_total_lines, 3000);
        assert!(
            selection.stored_lines <= DEFAULT_MAX_LINES + 2,
            "stored {} lines",
            selection.stored_lines
        );
        assert!(selection.stored_bytes <= DEFAULT_MAX_BYTES + 64);
    }

    #[tokio::test]
    async fn test_stream_storage_stops_after_the_byte_cap() {
        let data = (0..500)
            .map(|_| "x".repeat(200))
            .collect::<Vec<_>>()
            .join("\n");
        let selection = selection_for(data.into_bytes(), 7, 0.0, None).await;
        assert_eq!(selection.total_file_lines, 500);
        assert!(
            selection.stored_lines <= 300,
            "stored {} lines past the byte cap",
            selection.stored_lines
        );
        assert!(
            selection.stored_bytes <= DEFAULT_MAX_BYTES + 512,
            "stored {} bytes past the byte cap",
            selection.stored_bytes
        );
    }

    #[tokio::test]
    async fn test_stream_stops_at_the_size_seen_at_open() {
        let ops = Arc::new(VirtualOps {
            initial_size: Some(11),
            ..VirtualOps::streaming(b"line1\nline2\nline3".to_vec(), 4)
        });
        let result = run_virtual(ops, json!({ "path": "/virtual/file.txt" }))
            .await
            .unwrap();
        // Bytes appended after the size was captured are not counted.
        assert_eq!(text_of(&result), "line1\nline2");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn test_read_reads_a_zero_size_special_file_to_eof() {
        if !Path::new("/proc/self/status").exists() {
            eprintln!("skipping: /proc/self/status is unavailable");
            return;
        }
        let stream = LocalReadOperations
            .create_read_stream(Path::new("/proc/self/status"))
            .await
            .unwrap()
            .expect("proc file stream");
        assert!(
            stream.initial_size.is_none(),
            "a size-0 file must be read to EOF"
        );
        let tool = create_read_tool(&ctx(Path::new("/")), ReadToolOptions::default());
        let result = read_file(&*tool, "/proc/self/status").await;
        assert!(
            text_of(&result).contains("Name:"),
            "proc status was not read: {:?}",
            text_of(&result)
        );
    }

    #[tokio::test]
    async fn test_stream_abort_stops_before_the_next_chunk() {
        let token = CancellationToken::new();
        let reads = Arc::new(AtomicUsize::new(0));
        let ops = Arc::new(VirtualOps {
            cancel_after_first_read: Some(token.clone()),
            reads: Arc::clone(&reads),
            ..VirtualOps::streaming(vec![b'a'; 4096], 64)
        });
        let tool = virtual_tool(ops);
        let error = tool
            .execute("test", json!({ "path": "/virtual/file.txt" }), token, None)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("Operation aborted"));
        assert_eq!(reads.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_old_operations_without_a_stream_keep_the_whole_file_path() {
        let tool = virtual_tool(Arc::new(NoStreamOps {
            data: b"old\npath\n".to_vec(),
        }));
        let result = read_file(&*tool, "/virtual/file.txt").await;
        assert_eq!(text_of(&result), "old\npath\n");
    }

    /// The streaming path must produce the same text, details and structured
    /// output as the whole-file path for the same bytes and range.
    #[tokio::test]
    async fn test_stream_matches_the_whole_file_oracle() {
        let mut cases: Vec<(&str, Vec<u8>, Value)> = vec![
            ("plain", b"a\nb\nc".to_vec(), json!({})),
            ("trailing-newline", b"a\nb\nc\n".to_vec(), json!({})),
            ("empty", Vec::new(), json!({})),
            ("blank-lines", b"\n\na\n\n".to_vec(), json!({})),
            ("crlf", b"a\r\nb\r\n".to_vec(), json!({})),
            ("bom", "\u{feff}a\nb".as_bytes().to_vec(), json!({})),
            ("invalid-utf8", vec![0xff, 0x0a, b'a', 0x0a], json!({})),
            (
                "split-utf8",
                "\u{e9}\u{65e5}\u{672c}\nok".as_bytes().to_vec(),
                json!({}),
            ),
            ("offset", b"a\nb\nc\nd".to_vec(), json!({ "offset": 2 })),
            (
                "offset-limit",
                b"a\nb\nc\nd".to_vec(),
                json!({ "offset": 2, "limit": 2 }),
            ),
            ("limit-zero", b"a\nb".to_vec(), json!({ "limit": 0 })),
            ("beyond", b"a\nb".to_vec(), json!({ "offset": 5 })),
            (
                "first-line-over",
                format!("{}\nshort", "x".repeat(60_000)).into_bytes(),
                json!({}),
            ),
            (
                "byte-cap",
                (0..500)
                    .map(|_| "x".repeat(200))
                    .collect::<Vec<_>>()
                    .join("\n")
                    .into_bytes(),
                json!({}),
            ),
        ];
        cases.push((
            "line-cap",
            (0..2500)
                .map(|index| format!("line {index}"))
                .collect::<Vec<_>>()
                .join("\n")
                .into_bytes(),
            json!({}),
        ));

        for (name, data, params) in cases {
            for chunk in [1usize, 5, 4096] {
                let mut stream_params = params.clone();
                stream_params["path"] = json!("/virtual/file.txt");
                let streamed = run_virtual(
                    Arc::new(VirtualOps::streaming(data.clone(), chunk)),
                    stream_params,
                )
                .await;
                let mut whole_params = params.clone();
                whole_params["path"] = json!("/virtual/file.txt");
                let whole =
                    run_virtual(Arc::new(VirtualOps::whole_file(data.clone())), whole_params).await;
                match (streamed, whole) {
                    (Ok(streamed), Ok(whole)) => {
                        assert_eq!(text_of(&streamed), text_of(&whole), "{name} chunk={chunk}");
                        assert_eq!(streamed.details, whole.details, "{name} chunk={chunk}");
                        assert_eq!(
                            streamed.structured_content, whole.structured_content,
                            "{name} chunk={chunk}"
                        );
                    }
                    (Err(streamed), Err(whole)) => {
                        assert_eq!(streamed, whole, "{name} chunk={chunk}");
                    }
                    (streamed, whole) => {
                        panic!("{name} chunk={chunk}: stream={streamed:?} whole={whole:?}")
                    }
                }
            }
        }
    }

    // ---- tool metadata ----
    #[test]
    fn test_tool_metadata() {
        let dir = TestDir::new();
        let tool = create_read_tool(&ctx(dir.path()), ReadToolOptions::default());
        assert_eq!(tool.name(), "read");
        assert_eq!(tool.label(), "read");
        assert!(tool.description().contains("jpg, png, gif, webp, bmp"));
        let params = tool.parameters();
        assert_eq!(params["type"], "object");
        assert_eq!(params["required"][0], "path");
        assert_eq!(params["additionalProperties"], false);
    }
}
