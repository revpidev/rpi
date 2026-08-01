//! Port of `packages/coding-agent/src/core/tools/read.ts` @ pi 0.82.1 (2efa728).
//!
//! The read tool supports text files (with truncation) and images (with
//! auto-resize). TUI rendering methods (`renderCall`, `renderResult`,
//! compact-resource classification) are intentionally omitted — the Rust port
//! handles rendering in the TUI layer, not in the tool definition.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use pir_agent::{AgentError, AgentTool, AgentToolResult, AgentToolUpdateCallback};
use pir_ai::types::{ImageContent, TextContent, ToolResultContent};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::tools::image_process::process_image;
use crate::tools::mime::{detect_supported_image_mime_type, IMAGE_TYPE_SNIFF_BYTES};
use crate::tools::path_utils::resolve_read_path;
use crate::tools::truncate::{format_size, truncate_head, DEFAULT_MAX_BYTES};
use crate::tools::ToolContext;

// ---------------------------------------------------------------------------
// ReadOperations (read.ts:43-50)
// ---------------------------------------------------------------------------

/// Pluggable operations for the read tool.
///
/// Override these to delegate file reading to remote systems (for example SSH).
#[async_trait]
pub trait ReadOperations: Send + Sync {
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
    /// Whether to auto-resize images to 2000×2000 max. Default: `true`.
    pub auto_resize_images: bool,
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
            operations: None,
            model_supports_images: None,
        }
    }
}

// ---------------------------------------------------------------------------
// createReadTool (read.ts:203-215, 349-351)
// ---------------------------------------------------------------------------

/// Create a read tool bound to the given context.
pub fn create_read_tool(ctx: &ToolContext, options: ReadToolOptions) -> Arc<dyn AgentTool> {
    let operations = options
        .operations
        .unwrap_or_else(|| Arc::new(LocalReadOperations));
    Arc::new(ReadTool {
        cwd: ctx.cwd.clone(),
        auto_resize_images: options.auto_resize_images,
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
    operations: Arc<dyn ReadOperations>,
    model_supports_images: Option<bool>,
}

/// Non-vision model omission note (read.ts:87-92).
const NON_VISION_NOTE: &str =
    "[Current model does not support images. The image will be omitted from this request.]";

/// Tool description with constants expanded (read.ts:212).
const DESCRIPTION: &str = "Read the contents of a file. Supports text files and \
images (jpg, png, gif, webp, bmp). Images are sent as attachments. For text files, \
output is truncated to 2000 lines or 50KB (whichever is hit first). Use offset/limit \
for large files. When you need the full file, continue with offset until complete.";

/// Format a number for display in error messages (JS `${offset}` semantics).
fn format_number(v: f64) -> String {
    if v.fract() == 0.0 && v.is_finite() {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
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
            let result = process_image(&buffer, &mime, self.auto_resize_images);

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
                content,
                details: Value::Null,
                usage: None,
                added_tool_names: None,
                terminate: None,
            });
        }

        // ===================== Text branch (read.ts:264-316) =====================
        let buffer = self.operations.read_file(&absolute_path).await?;
        // buffer.toString("utf-8") — invalid sequences become U+FFFD.
        let text_content = String::from_utf8_lossy(&buffer);
        let all_lines: Vec<&str> = text_content.split('\n').collect();
        let total_file_lines = all_lines.len();

        // Offset handling (1-indexed → 0-indexed) (read.ts:271).
        // JS: offset ? Math.max(0, offset - 1) : 0   (0 is falsy).
        let start_line_raw = match offset {
            Some(o) if o != 0.0 => (o - 1.0).max(0.0),
            _ => 0.0,
        };
        let start_line_display = start_line_raw as usize + 1;

        // Offset out-of-bounds check (read.ts:274-276).
        if start_line_raw >= total_file_lines as f64 {
            return Err(AgentError::Message(format!(
                "Offset {} is beyond end of file ({} lines total)",
                format_number(offset.unwrap_or(0.0)),
                total_file_lines
            )));
        }

        let start_line = start_line_raw as usize;

        // Apply limit if specified (read.ts:280-286).
        let user_limited_lines: Option<usize>;
        let selected_content: String;

        if let Some(lim) = limit {
            let end_line_raw = (start_line_raw + lim).min(total_file_lines as f64);
            let end_line = end_line_raw as usize;
            selected_content = all_lines[start_line..end_line].join("\n");
            user_limited_lines = Some(end_line - start_line);
        } else {
            selected_content = all_lines[start_line..].join("\n");
            user_limited_lines = None;
        }

        // Apply truncation (read.ts:288-314).
        let truncation = truncate_head(&selected_content, None);
        let output_text: String;
        let details: Value;

        if truncation.first_line_exceeds_limit {
            // First line alone exceeds byte limit — sed fallback (read.ts:290-293).
            let first_line_size = format_size(all_lines[start_line].len());
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
            content: vec![ToolResultContent::Text(TextContent {
                text: output_text,
                text_signature: None,
            })],
            details,
            usage: None,
            added_tool_names: None,
            terminate: None,
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
    use std::path::PathBuf;

    /// Minimal temp dir for test file creation.
    struct TestDir(PathBuf);
    impl TestDir {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("pir-read-test-{:x}", rand_seed()));
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
                assert!(t
                    .text
                    .contains("[Image converted from image/bmp to image/png.]"));
            }
            _ => panic!("expected text block"),
        }
        match &result.content[1] {
            ToolResultContent::Image(img) => assert_eq!(img.mime_type, "image/png"),
            _ => panic!("expected image block"),
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
