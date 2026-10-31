//! Read tool (port of `packages/durable/src/tools/read.ts` @ `4a080252f`).

use std::sync::Arc;

use async_trait::async_trait;
use rpi_ai::types::{TextContent, Tool, ToolResultContent};
use serde_json::{Value, json};

use crate::chord::context::Context;
use crate::chord::json::JsonObject;
use crate::env::decode::starts_with_bom;
use crate::env::types::{BinaryReader, FileInfo, ScanLinesOptions};
use crate::harness::output::character_end;
use crate::harness::types::{
    DiagnosticSeverity, ToolDiagnostic, ToolExecute, ToolExecuteError, ToolExecutionApi,
    ToolExecutionResult, ToolRegistration, ToolReplay,
};
use crate::tools::env::require_env;
use crate::tools::image::{ByteSource, detect_supported_image_mime_type_of};
use crate::tools::path_utils::resolve_read_tool_path;
use crate::tools::tool_error;
use crate::truncate::{
    DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, TruncatedBy, TruncationOptions, TruncationResult,
    format_size, truncate_head_of, utf8_byte_length,
};

const READ_CHUNK: u64 = 64 * 1024;
/// JavaScript's safe integer bound (`Number.isSafeInteger`).
const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

/// `Array.prototype.slice`'s conversion of an index: NaN is 0, other values
/// truncate toward zero.
fn slice_index(value: f64) -> f64 {
    if value.is_nan() { 0.0 } else { value.trunc() }
}

/// `Number.isSafeInteger`.
fn is_safe_integer(value: f64) -> bool {
    value.is_finite() && value.fract() == 0.0 && value.abs() <= MAX_SAFE_INTEGER
}

/// `Number.isInteger`.
fn is_integer(value: f64) -> bool {
    value.is_finite() && value.fract() == 0.0
}

/// JS template-literal rendering of a number.
fn format_number(value: f64) -> String {
    if value.is_nan() {
        "NaN".to_string()
    } else if value.is_infinite() {
        if value > 0.0 {
            "Infinity".to_string()
        } else {
            "-Infinity".to_string()
        }
    } else if value.fract() == 0.0 && value.abs() < 9.0e15 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

/// Details of a read (`ReadToolDetails`).
#[derive(Debug, Clone, PartialEq)]
pub struct ReadToolDetails {
    /// How the shown text was cut; the text itself is the result content.
    pub truncation: Value,
}

struct ReaderSource<'a> {
    reader: &'a dyn BinaryReader,
    context: &'a Context,
    size: u64,
}

#[async_trait]
impl ByteSource for ReaderSource<'_> {
    fn size(&self) -> u64 {
        self.size
    }

    async fn read(&self, offset: u64, length: u64) -> std::io::Result<Vec<u8>> {
        self.reader
            .read(offset, length, self.context)
            .await
            .map_err(|error| std::io::Error::other(error.to_string()))
    }
}

/// Options of [`create_read_tool_with_options`].
#[derive(Clone, Default)]
pub struct ReadToolOptions {
    /// Optional processor for BMP conversion, EXIF orientation and resizing.
    pub image_processor: Option<Arc<dyn crate::tools::image_processor::ImageProcessor>>,
}

struct ReadTool {
    options: ReadToolOptions,
}

#[async_trait]
impl ToolExecute for ReadTool {
    async fn execute(
        &self,
        args: JsonObject,
        api: &dyn ToolExecutionApi,
        context: &Context,
    ) -> Result<ToolExecutionResult, ToolExecuteError> {
        let path = args
            .get("path")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string();
        let offset = args.get("offset").and_then(Value::as_f64);
        let limit = args.get("limit").and_then(Value::as_f64);
        let env = require_env(api)?;
        let absolute = resolve_read_tool_path(env.as_ref(), &path, context).await?;
        let reader = env.open_binary_reader(&absolute, None, context).await?;
        let result = read_stable_text_with(
            reader.as_ref(),
            &path,
            offset,
            limit,
            context,
            &self.options,
            api,
        )
        .await;
        reader.close(context).await;
        result
    }
}

/// Read the file, rereading once when a concurrent writer shrank or rewrote it
/// between the scan and the reads (upstream `createReadTool.execute`'s loop).
#[cfg(test)]
async fn read_stable_text(
    reader: &dyn BinaryReader,
    path: &str,
    offset: Option<f64>,
    limit: Option<f64>,
    context: &Context,
) -> Result<ToolExecutionResult, ToolExecuteError> {
    let model = ImageModel {
        vision: true,
        limits: crate::tools::image_processor::ImageLimits::default(),
    };
    read_stable_text_inner(reader, path, offset, limit, context, None, &model).await
}

/// Read with image processing and the conversation model's limits.
async fn read_stable_text_with(
    reader: &dyn BinaryReader,
    path: &str,
    offset: Option<f64>,
    limit: Option<f64>,
    context: &Context,
    options: &ReadToolOptions,
    api: &dyn ToolExecutionApi,
) -> Result<ToolExecutionResult, ToolExecuteError> {
    let model = image_model(api, context).await;
    read_stable_text_inner(
        reader,
        path,
        offset,
        limit,
        context,
        options.image_processor.as_ref(),
        &model,
    )
    .await
}

async fn read_stable_text_inner(
    reader: &dyn BinaryReader,
    path: &str,
    offset: Option<f64>,
    limit: Option<f64>,
    context: &Context,
    processor: Option<&Arc<dyn crate::tools::image_processor::ImageProcessor>>,
    model: &ImageModel,
) -> Result<ToolExecutionResult, ToolExecuteError> {
    let mut attempt = 0;
    loop {
        let before = reader.info(context).await?;
        let source = ReaderSource {
            reader,
            context,
            size: before.size,
        };
        // A concurrent writer can change the file between the scan and the
        // reads: appending (a growing log) leaves the scanned text as it was,
        // and an image must be whole, so a grown image is read again
        // (upstream `read.ts`).
        let (result, image) = match detect_supported_image_mime_type_of(&source)
            .await
            .map_err(|error| tool_error(error.to_string()))?
        {
            Some(mime_type) => (
                read_image(reader, &before, path, &mime_type, processor, model, context).await?,
                true,
            ),
            None => (
                read_text(reader, path, offset, limit, context).await?,
                false,
            ),
        };
        let after = reader.info(context).await?;
        let unchanged = after.size == before.size && after.mtime_ms == before.mtime_ms;
        if unchanged || (!image && after.size > before.size) {
            return Ok(result);
        }
        if attempt == 1 {
            return Err(tool_error(format!("{path} changed while it was read")));
        }
        attempt += 1;
    }
}

/// One image block, prepared by `processor` or, without one, as it is when its
/// format is inline and it fits the model's limits. What happened is an `info`
/// diagnostic, so programs get the bare image.
struct ImageModel {
    vision: bool,
    limits: crate::tools::image_processor::ImageLimits,
}

async fn image_model(api: &dyn ToolExecutionApi, context: &Context) -> ImageModel {
    let defaults = crate::tools::image_processor::ImageLimits::default();
    let agent = match api.agent(context).await {
        Ok(agent) => agent,
        Err(_) => {
            return ImageModel {
                vision: true,
                limits: defaults,
            };
        }
    };
    let Some(reference) = agent.model else {
        return ImageModel {
            vision: true,
            limits: defaults,
        };
    };
    let Some(model) = api
        .models()
        .get_model(&reference.provider, &reference.model_id)
    else {
        return ImageModel {
            vision: true,
            limits: defaults,
        };
    };
    let resize = model
        .input_limits
        .as_ref()
        .and_then(|limits| limits.images.as_ref())
        .and_then(|images| images.resize.as_ref());
    ImageModel {
        vision: model.input.contains(&rpi_ai::types::InputModality::Image),
        limits: crate::tools::image_processor::ImageLimits {
            max_width: resize
                .and_then(|resize| resize.max_width)
                .unwrap_or(defaults.max_width),
            max_height: resize
                .and_then(|resize| resize.max_height)
                .unwrap_or(defaults.max_height),
            max_bytes: resize
                .and_then(|resize| resize.max_bytes)
                .unwrap_or(defaults.max_bytes),
        },
    }
}

fn parse_image_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G']) && bytes.len() >= 24 {
        return Some((
            u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]),
            u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]),
        ));
    }
    if bytes.starts_with(b"GIF") && bytes.len() >= 10 {
        return Some((
            u16::from_le_bytes([bytes[6], bytes[7]]) as u32,
            u16::from_le_bytes([bytes[8], bytes[9]]) as u32,
        ));
    }
    if bytes.starts_with(&[0xff, 0xd8]) {
        let mut index = 2;
        while index + 9 < bytes.len() {
            if bytes[index] != 0xff {
                index += 1;
                continue;
            }
            let marker = bytes[index + 1];
            let length = u16::from_be_bytes([bytes[index + 2], bytes[index + 3]]) as usize;
            if matches!(
                marker,
                0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf
            ) && index + 9 <= bytes.len()
            {
                return Some((
                    u16::from_be_bytes([bytes[index + 7], bytes[index + 8]]) as u32,
                    u16::from_be_bytes([bytes[index + 5], bytes[index + 6]]) as u32,
                ));
            }
            if length < 2 {
                break;
            }
            index += 2 + length;
        }
        return None;
    }
    if bytes.starts_with(b"RIFF") && bytes.len() >= 30 && &bytes[8..12] == b"WEBP" {
        match &bytes[12..16] {
            b"VP8X" => {
                return Some((
                    1 + u32::from_le_bytes([bytes[24], bytes[25], bytes[26], 0]),
                    1 + u32::from_le_bytes([bytes[27], bytes[28], bytes[29], 0]),
                ));
            }
            b"VP8 " => {
                return Some((
                    (u16::from_le_bytes([bytes[26], bytes[27]]) & 0x3fff) as u32,
                    (u16::from_le_bytes([bytes[28], bytes[29]]) & 0x3fff) as u32,
                ));
            }
            b"VP8L" => {
                let bits = u32::from_le_bytes([bytes[21], bytes[22], bytes[23], bytes[24]]);
                return Some(((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1));
            }
            _ => {}
        }
    }
    None
}

const INLINE_IMAGE_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];

fn image_error(message: String) -> ToolExecutionResult {
    ToolExecutionResult {
        output: Some(Vec::new()),
        is_error: Some(true),
        diagnostics: Some(vec![ToolDiagnostic {
            severity: DiagnosticSeverity::Error,
            code: Some("unsupported_image".to_string()),
            message,
        }]),
        ..Default::default()
    }
}

#[allow(clippy::too_many_arguments)]
async fn read_image(
    reader: &dyn BinaryReader,
    info: &FileInfo,
    path: &str,
    mime_type: &str,
    processor: Option<&Arc<dyn crate::tools::image_processor::ImageProcessor>>,
    model: &ImageModel,
    context: &Context,
) -> Result<ToolExecutionResult, ToolExecuteError> {
    let mut notes: Vec<String> = Vec::new();
    let image = if let Some(processor) = processor {
        let bytes = reader
            .read(0, info.size, context)
            .await
            .map_err(|error| tool_error(error.to_string()))?;
        let limits = model.limits;
        let mime = mime_type.to_string();
        let processor_owned = Arc::clone(processor);
        let prepared =
            tokio::task::spawn_blocking(move || processor_owned.prepare(&bytes, &mime, limits))
                .await
                .map_err(|error| tool_error(error.to_string()))?;
        let Some(prepared) = prepared else {
            return Ok(image_error(format!(
                "{path} is an image ({mime_type}) that cannot be prepared for the model"
            )));
        };
        if let Some(from) = &prepared.converted_from {
            notes.push(format!("Converted from {from} to {}.", prepared.mime_type));
        }
        if let Some((from_w, from_h, to_w, to_h)) = prepared.resized {
            let scale = (from_w as f64 / to_w as f64).max(0.01);
            notes.push(format!(
                "Resized from {from_w}x{from_h} to {to_w}x{to_h}. Multiply coordinates by {scale:.2} to map them to the original."
            ));
        }
        rpi_ai::types::ImageContent {
            data: prepared.data,
            mime_type: prepared.mime_type,
        }
    } else {
        if !INLINE_IMAGE_TYPES.contains(&mime_type) {
            return Ok(image_error(format!(
                "{path} is an image ({mime_type}) that needs converting, and no image processor is configured"
            )));
        }
        let base64_len = info.size.div_ceil(3) * 4;
        if base64_len > model.limits.max_bytes {
            return Ok(image_error(format!(
                "{path} is an image ({mime_type}) of {} bytes, too large to send, and no image processor is configured to shrink it",
                info.size
            )));
        }
        let bytes = reader
            .read(0, info.size, context)
            .await
            .map_err(|error| tool_error(error.to_string()))?;
        let Some((width, height)) = parse_image_dimensions(&bytes) else {
            return Ok(image_error(format!(
                "{path} is an image ({mime_type}) whose size cannot be read"
            )));
        };
        if width > model.limits.max_width || height > model.limits.max_height {
            return Ok(image_error(format!(
                "{path} is an image ({mime_type}) of {width}x{height}, larger than {}x{}, and no image processor is configured to shrink it",
                model.limits.max_width, model.limits.max_height
            )));
        }
        rpi_ai::types::ImageContent {
            data: base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &bytes),
            mime_type: mime_type.to_string(),
        }
    };
    if !model.vision {
        notes.push(
            "The current model does not support images; it sees a placeholder instead.".to_string(),
        );
    }
    let message = if notes.is_empty() {
        format!("Read image file [{}].", image.mime_type)
    } else {
        format!("Read image file [{}]. {}", image.mime_type, notes.join(" "))
    };
    Ok(ToolExecutionResult {
        output: Some(vec![ToolResultContent::Image(image)]),
        diagnostics: Some(vec![ToolDiagnostic {
            severity: DiagnosticSeverity::Info,
            code: Some("image".to_string()),
            message,
        }]),
        ..Default::default()
    })
}

/// The read result for the opened file. It equals decoding the whole file,
/// splitting it on `\n`, and bounding the selected lines with
/// [`truncate_head_of`], while reading only one scan's worth of the file plus
/// the head.
async fn read_text(
    reader: &dyn BinaryReader,
    path: &str,
    offset: Option<f64>,
    limit: Option<f64>,
    context: &Context,
) -> Result<ToolExecutionResult, ToolExecuteError> {
    let start_line = match offset {
        Some(value) if value != 0.0 && !value.is_nan() => (value - 1.0).max(0.0),
        _ => 0.0,
    };
    let start_line_display = start_line + 1.0;
    // Lines are selected like `allLines.slice(startLine, endLine)`, which
    // truncates fractional indices.
    let slice_start = slice_index(start_line);
    // One pass finds the line count and the selection; a selection past the
    // last line ends with it, as `slice` does, and an empty one (a zero or
    // negative limit) is scanned as one line and then ignored.
    let scan_start = if is_safe_integer(slice_start) {
        slice_start as u64
    } else {
        0
    };
    let requested_end =
        limit.map(|limit| (scan_start as f64 + 1.0).max(slice_index(start_line + limit)));
    let scan_end = requested_end
        .filter(|value| is_safe_integer(*value))
        .map(|value| value as u64);
    let mut scan = reader
        .scan_lines(
            ScanLinesOptions {
                start_line: scan_start,
                end_line: scan_end,
            },
            context,
        )
        .await?;
    let total_file_lines = scan.newlines + 1;
    if start_line >= total_file_lines as f64 {
        return Err(tool_error(format!(
            "Offset {} is beyond end of file ({} lines total)",
            format_number(offset.unwrap_or(0.0)),
            total_file_lines
        )));
    }

    let mut user_limited_lines: Option<f64> = None;
    let mut selected_line_count = total_file_lines as f64 - slice_start;
    if let Some(limit) = limit {
        let end_line = (start_line + limit).min(total_file_lines as f64);
        user_limited_lines = Some(end_line - start_line);
        // `slice` counts a negative end from the end of the lines, which only
        // the line count tells; scan again for it.
        let relative_end = slice_index(end_line);
        let slice_end = if relative_end < 0.0 {
            (total_file_lines as f64 + relative_end).max(0.0)
        } else {
            relative_end
        };
        selected_line_count = (slice_end - slice_start).max(0.0);
        if selected_line_count > 0.0 && relative_end < 0.0 {
            scan = reader
                .scan_lines(
                    ScanLinesOptions {
                        start_line: scan_start,
                        end_line: Some(slice_end as u64),
                    },
                    context,
                )
                .await?;
        }
    }
    let empty = selected_line_count == 0.0;
    // Counted like `truncateHead`: a trailing newline adds no line, and empty
    // text has none.
    let ends_with_newline =
        !empty && scan.last_line_start == scan.end && scan.last_line_start > scan.start;
    let totals_lines = if empty || scan.selected_bytes == 0 {
        0
    } else {
        (selected_line_count - if ends_with_newline { 1.0 } else { 0.0 }) as u64
    };
    let totals_bytes = if empty { 0 } else { scan.selected_bytes };
    let first_bytes = reader.read(0, 3, context).await?;
    let head = if empty {
        String::new()
    } else {
        read_head(
            reader,
            scan.start,
            scan.end,
            starts_with_bom(&first_bytes),
            context,
        )
        .await?
    };

    let truncation = truncate_head_of(
        &head,
        totals_lines,
        totals_bytes,
        TruncationOptions::default(),
    );
    let mut diagnostics: Vec<ToolDiagnostic> = Vec::new();
    let mut output_text = truncation.content.clone();
    let mut details: Option<Value> = None;
    if truncation.first_line_exceeds_limit {
        // Show the start of the line, cut at the byte limit on a character
        // boundary. Like `allLines[startLine]`, a fractional start line names
        // no line.
        let integral = is_integer(start_line);
        let line_text = if integral {
            head.split('\n').next().unwrap_or("").to_string()
        } else {
            String::new()
        };
        let line_bytes = line_text.into_bytes();
        let line_size = if integral { scan.first_line_bytes } else { 0 };
        let end = character_end(&line_bytes, DEFAULT_MAX_BYTES as usize) as u64;
        output_text = String::from_utf8_lossy(&line_bytes[..end as usize]).into_owned();
        diagnostics.push(ToolDiagnostic {
            severity: DiagnosticSeverity::Warn,
            code: Some("truncated".to_string()),
            message: format!(
                "Line {} is {}, exceeds the {} limit; showing its first {}. Use bash: sed -n '{}p' {} | tail -c +{}",
                format_number(start_line_display),
                format_size(line_size),
                format_size(DEFAULT_MAX_BYTES),
                format_size(end),
                format_number(start_line_display),
                path,
                end + 1
            ),
        });
        details = Some(truncation_json(&truncation, Some(end), Some(1)));
    } else if truncation.truncated {
        let end_line_display = start_line_display + truncation.output_lines as f64 - 1.0;
        let next_offset = end_line_display + 1.0;
        let limit_text = if truncation.truncated_by == Some(TruncatedBy::Lines) {
            String::new()
        } else {
            format!(" ({} limit)", format_size(DEFAULT_MAX_BYTES))
        };
        diagnostics.push(ToolDiagnostic {
            severity: DiagnosticSeverity::Info,
            code: Some("truncated".to_string()),
            message: format!(
                "Showing lines {}-{} of {}{}. Use offset={} to continue.",
                format_number(start_line_display),
                format_number(end_line_display),
                total_file_lines,
                limit_text,
                format_number(next_offset)
            ),
        });
        details = Some(truncation_json(&truncation, None, None));
    } else if let Some(user_limited_lines) = user_limited_lines
        && start_line + user_limited_lines < total_file_lines as f64
    {
        let remaining = total_file_lines as f64 - (start_line + user_limited_lines);
        let next_offset = start_line + user_limited_lines + 1.0;
        diagnostics.push(ToolDiagnostic {
            severity: DiagnosticSeverity::Info,
            code: None,
            message: format!(
                "{} more lines in file. Use offset={} to continue.",
                format_number(remaining),
                format_number(next_offset)
            ),
        });
    }

    Ok(ToolExecutionResult {
        output: Some(if output_text.is_empty() {
            Vec::new()
        } else {
            vec![ToolResultContent::Text(TextContent {
                text: output_text,
                text_signature: None,
            })]
        }),
        details,
        diagnostics: Some(diagnostics),
        ..Default::default()
    })
}

/// The decoded start of bytes `[start, end)` of the file, decoded as part of
/// the whole file.
async fn read_head(
    reader: &dyn BinaryReader,
    start: u64,
    end: u64,
    skip_bom: bool,
    context: &Context,
) -> Result<String, ToolExecuteError> {
    let mut decoder = crate::env::decode::range_decoder();
    let mut text = String::new();
    let mut newlines: u64 = 0;
    let mut position = if skip_bom && start == 0 { 3 } else { start };
    while position < end {
        let bytes = reader
            .read(position, READ_CHUNK.min(end - position), context)
            .await?;
        if bytes.is_empty() {
            break;
        }
        position += bytes.len() as u64;
        let decoded = decoder.decode(&bytes);
        text.push_str(&decoded);
        newlines += decoded.matches('\n').count() as u64;
        if newlines >= DEFAULT_MAX_LINES || utf8_byte_length(&text) > DEFAULT_MAX_BYTES + 1 {
            return Ok(text);
        }
    }
    text.push_str(&decoder.finish());
    Ok(text)
}

fn truncation_json(
    truncation: &TruncationResult,
    output_bytes: Option<u64>,
    output_lines: Option<u64>,
) -> Value {
    json!({
        "truncation": {
            "truncated": truncation.truncated,
            "truncatedBy": match truncation.truncated_by {
                Some(TruncatedBy::Lines) => Value::String("lines".to_string()),
                Some(TruncatedBy::Bytes) => Value::String("bytes".to_string()),
                None => Value::Null,
            },
            "totalLines": truncation.total_lines,
            "totalBytes": truncation.total_bytes,
            "outputLines": output_lines.unwrap_or(truncation.output_lines),
            "outputBytes": output_bytes.unwrap_or(truncation.output_bytes),
            "lastLinePartial": truncation.last_line_partial,
            "firstLineExceedsLimit": truncation.first_line_exceeds_limit,
            "maxLines": truncation.max_lines,
            "maxBytes": truncation.max_bytes,
        }
    })
}

fn parameters() -> Value {
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
        "required": ["path"]
    })
}

/// `createReadTool` (read.ts:73). Remarks about truncation and continuation are
/// diagnostics; the content is only file text.
pub fn create_read_tool() -> ToolRegistration {
    create_read_tool_with_options(ReadToolOptions::default())
}

/// `createReadTool` with image processing.
pub fn create_read_tool_with_options(options: ReadToolOptions) -> ToolRegistration {
    ToolRegistration {
        tool: Tool {
            name: "read".to_string(),
            description: format!(
                "Read the contents of a text file. Output is truncated to {DEFAULT_MAX_LINES} lines or {}KB (whichever is hit first). Use offset/limit for large files. When you need the full file, continue with offset until complete.",
                DEFAULT_MAX_BYTES / 1024
            ),
            parameters: parameters(),
            constrained_sampling: None,
        },
        replay: ToolReplay::Unsafe,
        execution_mode: None,
        prepare_arguments: None,
        output_limits: None,
        structured_output_schema: None,
        callers: None,
        deferred: false,
        execute: Arc::new(ReadTool { options }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::line_scan::LineScanner;
    use crate::env::types::{FileKind, LineScan, ScanLinesOptions};
    use crate::errors::FileErrorCode;
    use base64::Engine as _;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A reader whose `info()` reports the scheduled sizes/mtimes and whose
    /// content is whatever the test set.
    struct ScheduledReader {
        content: Mutex<Vec<u8>>,
        infos: Vec<(u64, f64)>,
        call: AtomicUsize,
        reads: AtomicUsize,
        path: String,
    }

    impl ScheduledReader {
        fn new(content: &str, infos: Vec<(u64, f64)>) -> Self {
            Self::bytes(content.as_bytes().to_vec(), infos)
        }

        fn bytes(content: Vec<u8>, infos: Vec<(u64, f64)>) -> Self {
            Self {
                content: Mutex::new(content),
                infos,
                call: AtomicUsize::new(0),
                reads: AtomicUsize::new(0),
                path: "f.txt".to_string(),
            }
        }
    }

    #[async_trait]
    impl BinaryReader for ScheduledReader {
        async fn info(&self, _context: &Context) -> Result<FileInfo, crate::errors::FileError> {
            let index = self.call.fetch_add(1, Ordering::SeqCst);
            let (size, mtime_ms) = self
                .infos
                .get(index)
                .copied()
                .unwrap_or_else(|| *self.infos.last().expect("infos"));
            Ok(FileInfo {
                name: "f.txt".to_string(),
                path: self.path.clone(),
                kind: FileKind::File,
                size,
                mtime_ms,
            })
        }

        async fn read(
            &self,
            offset: u64,
            length: u64,
            _context: &Context,
        ) -> Result<Vec<u8>, crate::errors::FileError> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            let content = self.content.lock().expect("content");
            let from = (offset as usize).min(content.len());
            let to = ((offset + length) as usize).min(content.len());
            Ok(content[from..to].to_vec())
        }

        async fn scan_lines(
            &self,
            options: ScanLinesOptions,
            _context: &Context,
        ) -> Result<LineScan, crate::errors::FileError> {
            let content = self.content.lock().expect("content");
            let mut scanner =
                LineScanner::new(options.start_line, options.end_line).map_err(|_| {
                    crate::errors::FileError::with_path(
                        FileErrorCode::Invalid,
                        "Invalid line range",
                        &self.path,
                    )
                })?;
            scanner.push(&content);
            Ok(scanner.finish())
        }

        async fn close(&self, _context: &Context) {}
    }

    fn context() -> Context {
        crate::chord::context::BACKGROUND_CONTEXT.clone()
    }

    /// 1x1 PNG.
    const PNG_1X1: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

    /// Upstream: "An image must be read whole: unlike text, one that grows while
    /// it is read is read again." (spec §"read"); a text log that grows keeps
    /// the scanned text (see the test below).
    #[tokio::test]
    async fn rereads_an_image_that_grew_while_it_was_read() {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(PNG_1X1)
            .expect("png");
        let size = bytes.len() as u64;
        let reader = ScheduledReader::bytes(
            bytes,
            vec![
                (size, 1.0),
                (size + 10, 1.0),
                (size + 10, 1.0),
                (size + 10, 1.0),
            ],
        );
        let result = read_stable_text(&reader, "f.png", None, None, &context())
            .await
            .expect("read");
        assert_eq!(
            reader.call.load(Ordering::SeqCst),
            4,
            "one before/after pair per attempt: the image is read again"
        );
        assert!(
            result
                .output
                .as_ref()
                .map(|content| content
                    .iter()
                    .any(|part| matches!(part, ToolResultContent::Image(_))))
                .unwrap_or(false),
            "a whole image comes back"
        );
    }

    #[tokio::test]
    async fn an_image_that_keeps_growing_fails_the_read() {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(PNG_1X1)
            .expect("png");
        let size = bytes.len() as u64;
        let reader = ScheduledReader::bytes(
            bytes,
            vec![
                (size, 1.0),
                (size + 10, 1.0),
                (size + 10, 1.0),
                (size + 20, 1.0),
            ],
        );
        let error = read_stable_text(&reader, "f.png", None, None, &context())
            .await
            .expect_err("a growing image fails");
        assert!(
            error.to_string().contains("changed while it was read"),
            "got {error}"
        );
        assert_eq!(reader.call.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn accepts_a_log_that_grew_while_it_was_read() {
        let reader = ScheduledReader::new("one\ntwo\n", vec![(8, 1.0), (12, 1.0)]);
        let result = read_stable_text(&reader, "f.txt", None, Some(2.0), &context())
            .await
            .expect("read");
        // One before/after pair: text that grew is accepted as it was scanned
        // (an image would be read again).
        assert_eq!(reader.call.load(Ordering::SeqCst), 2);
        assert_eq!(
            result
                .output
                .as_ref()
                .and_then(|content| content.first())
                .map(|part| match part {
                    ToolResultContent::Text(text) => text.text.clone(),
                    _ => String::new(),
                }),
            Some("one\ntwo".to_string())
        );
    }

    #[tokio::test]
    async fn accepts_an_unchanged_file() {
        let reader = ScheduledReader::new("one\ntwo\n", vec![(8, 1.0), (8, 1.0)]);
        let result = read_stable_text(&reader, "f.txt", None, Some(2.0), &context())
            .await
            .expect("read");
        assert!(result.is_error.is_none());
    }

    #[tokio::test]
    async fn rereads_once_then_reports_a_file_changing_twice() {
        let reader =
            ScheduledReader::new("one\ntwo\n", vec![(8, 2.0), (8, 1.0), (8, 1.0), (8, 0.5)]);
        let error = read_stable_text(&reader, "f.txt", None, None, &context())
            .await
            .expect_err("must fail");
        assert_eq!(error.to_string(), "f.txt changed while it was read");
    }
}
