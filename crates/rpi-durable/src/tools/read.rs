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

struct ReadTool;

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
        let result = read_stable_text(reader.as_ref(), &path, offset, limit, context).await;
        reader.close(context).await;
        result
    }
}

/// Read the file, rereading once when a concurrent writer shrank or rewrote it
/// between the scan and the reads (upstream `createReadTool.execute`'s loop).
async fn read_stable_text(
    reader: &dyn BinaryReader,
    path: &str,
    offset: Option<f64>,
    limit: Option<f64>,
    context: &Context,
) -> Result<ToolExecutionResult, ToolExecuteError> {
    let mut attempt = 0;
    loop {
        let before = reader.info(context).await?;
        let result = read_text(reader, &before, path, offset, limit, context).await?;
        let after = reader.info(context).await?;
        if after.size > before.size
            || (after.size == before.size && after.mtime_ms == before.mtime_ms)
        {
            return Ok(result);
        }
        if attempt == 1 {
            return Err(tool_error(format!("{path} changed while it was read")));
        }
        attempt += 1;
    }
}

/// Read a supported image as one image block. Without an image processor,
/// PNG, JPEG, GIF and WebP pass through undecoded within the default limits
/// (2000x2000 pixels and about 3.4 MB raw); larger images are error results.
async fn read_image(
    reader: &dyn BinaryReader,
    info: &FileInfo,
    path: &str,
    mime_type: &str,
    context: &Context,
) -> Result<ToolExecutionResult, ToolExecuteError> {
    const MAX_PIXELS: u32 = 2000;
    const MAX_RAW_BYTES: u64 = 3_375_000;
    let (width, height) = image_dimensions(reader, context).await?;
    if width > MAX_PIXELS || height > MAX_PIXELS {
        return Ok(ToolExecutionResult {
            output: Some(Vec::new()),
            is_error: Some(true),
            diagnostics: Some(vec![ToolDiagnostic {
                severity: DiagnosticSeverity::Error,
                code: Some("image_too_large".to_string()),
                message: format!(
                    "{path} is {width}x{height}; the limit is {MAX_PIXELS}x{MAX_PIXELS}"
                ),
            }]),
            ..Default::default()
        });
    }
    if info.size > MAX_RAW_BYTES {
        return Ok(ToolExecutionResult {
            output: Some(Vec::new()),
            is_error: Some(true),
            diagnostics: Some(vec![ToolDiagnostic {
                severity: DiagnosticSeverity::Error,
                code: Some("image_too_large".to_string()),
                message: format!(
                    "{path} is {} bytes; the image limit is {MAX_RAW_BYTES} bytes",
                    info.size
                ),
            }]),
            ..Default::default()
        });
    }
    let bytes = reader
        .read(0, info.size, context)
        .await
        .map_err(|error| tool_error(error.to_string()))?;
    let data = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &bytes);
    Ok(ToolExecutionResult {
        output: Some(vec![ToolResultContent::Image(
            rpi_ai::types::ImageContent {
                data,
                mime_type: mime_type.to_string(),
            },
        )]),
        diagnostics: Some(vec![ToolDiagnostic {
            severity: DiagnosticSeverity::Info,
            code: Some("image".to_string()),
            message: format!("Read image {path} ({width}x{height}, {mime_type})"),
        }]),
        ..Default::default()
    })
}

/// Pixel dimensions from the image header, `(0, 0)` when the format's header
/// does not carry them in a shape this reader understands.
async fn image_dimensions(
    reader: &dyn BinaryReader,
    context: &Context,
) -> Result<(u32, u32), ToolExecuteError> {
    let header = reader
        .read(0, 64, context)
        .await
        .map_err(|error| tool_error(error.to_string()))?;
    Ok(parse_image_dimensions(&header))
}

fn parse_image_dimensions(bytes: &[u8]) -> (u32, u32) {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G']) && bytes.len() >= 24 {
        let width = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
        let height = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
        return (width, height);
    }
    if bytes.starts_with(b"GIF") && bytes.len() >= 10 {
        let width = u16::from_le_bytes([bytes[6], bytes[7]]) as u32;
        let height = u16::from_le_bytes([bytes[8], bytes[9]]) as u32;
        return (width, height);
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
                let height = u16::from_be_bytes([bytes[index + 5], bytes[index + 6]]) as u32;
                let width = u16::from_be_bytes([bytes[index + 7], bytes[index + 8]]) as u32;
                return (width, height);
            }
            if length < 2 {
                break;
            }
            index += 2 + length;
        }
        return (0, 0);
    }
    if bytes.starts_with(b"RIFF") && bytes.len() >= 30 && &bytes[8..12] == b"WEBP" {
        match &bytes[12..16] {
            b"VP8X" => {
                let width = 1 + u32::from_le_bytes([bytes[24], bytes[25], bytes[26], 0]);
                let height = 1 + u32::from_le_bytes([bytes[27], bytes[28], bytes[29], 0]);
                return (width, height);
            }
            b"VP8 " => {
                let width = (u16::from_le_bytes([bytes[26], bytes[27]]) & 0x3fff) as u32;
                let height = (u16::from_le_bytes([bytes[28], bytes[29]]) & 0x3fff) as u32;
                return (width, height);
            }
            b"VP8L" => {
                let bits = u32::from_le_bytes([bytes[21], bytes[22], bytes[23], bytes[24]]);
                let width = (bits & 0x3fff) + 1;
                let height = ((bits >> 14) & 0x3fff) + 1;
                return (width, height);
            }
            _ => {}
        }
    }
    (0, 0)
}

/// The read result for the opened file. It equals decoding the whole file,
/// splitting it on `\n`, and bounding the selected lines with
/// [`truncate_head_of`], while reading only one scan's worth of the file plus
/// the head.
async fn read_text(
    reader: &dyn BinaryReader,
    info: &FileInfo,
    path: &str,
    offset: Option<f64>,
    limit: Option<f64>,
    context: &Context,
) -> Result<ToolExecutionResult, ToolExecuteError> {
    let source = ReaderSource {
        reader,
        context,
        size: info.size,
    };
    let mime_type = detect_supported_image_mime_type_of(&source)
        .await
        .map_err(|error| tool_error(error.to_string()))?;
    if let Some(mime_type) = mime_type {
        return read_image(reader, info, path, &mime_type, context).await;
    }

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
        execute: Arc::new(ReadTool),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::line_scan::LineScanner;
    use crate::env::types::{FileKind, LineScan, ScanLinesOptions};
    use crate::errors::FileErrorCode;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A reader whose `info()` reports the scheduled sizes/mtimes and whose
    /// content is whatever the test set.
    struct ScheduledReader {
        content: Mutex<Vec<u8>>,
        infos: Vec<(u64, f64)>,
        call: AtomicUsize,
        path: String,
    }

    impl ScheduledReader {
        fn new(content: &str, infos: Vec<(u64, f64)>) -> Self {
            Self {
                content: Mutex::new(content.as_bytes().to_vec()),
                infos,
                call: AtomicUsize::new(0),
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

    #[tokio::test]
    async fn accepts_a_log_that_grew_while_it_was_read() {
        let reader = ScheduledReader::new("one\ntwo\n", vec![(8, 1.0), (12, 1.0)]);
        let result = read_stable_text(&reader, "f.txt", None, Some(2.0), &context())
            .await
            .expect("read");
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
