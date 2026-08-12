//! Port of `packages/coding-agent/src/utils/tool-result-images.ts`
//! @ pi 0.84.1+ (4181f66, introduced in b0e05b442).
//!
//! Normalizes image blocks in tool results so images produced by tools
//! (extensions, MCP bridges, screenshot tools) are resized/converted to
//! inline provider limits before entering session history. The `read` tool
//! and `@file` CLI attachments already run through [`process_image`]; this
//! module covers the remaining tool-result paths.
//!
//! Oversized images make the provider reject the whole conversation, not
//! just the offending turn, so normalize them once as they enter history
//! (upstream commit b0e05b442).

use base64::Engine;
use rpi_ai::types::{ImageContent, TextContent, ToolResultContent};

use crate::tools::image_process::process_image;

/// Result of normalizing tool-result content.
#[derive(Debug)]
pub struct NormalizedToolResult {
    /// The (possibly rewritten) content array. When [`changed`] is `false`
    /// this is functionally identical to the input.
    ///
    /// [`changed`]: NormalizedToolResult::changed
    pub content: Vec<ToolResultContent>,
    /// Whether any image block was modified or had hint text appended.
    pub changed: bool,
}

/// Normalize image blocks returned by tool results.
///
/// Port of `normalizeToolResultImages` (tool-result-images.ts:22-61,
/// introduced in b0e05b442). Runs in `afterToolCall` **after** the extension
/// `tool_result` hook so images injected or replaced by extensions are
/// normalized too (agent-session.ts:517-520).
///
/// Each image block is run through [`process_image`] with the caller-supplied
/// `auto_resize_images` flag (bound to `settings.images.autoResize`, default
/// true). On processing failure the original block is kept — unlike the
/// `read` tool, tools already produced this image and the failure may just
/// be an unavailable image backend, so passing it through preserves current
/// behavior instead of silently deleting tool output
/// (tool-result-images.ts:41-47).
///
/// When nothing changed, `changed` is `false`, letting the caller skip
/// rewriting the result (tool-result-images.ts:61).
pub fn normalize_tool_result_images(
    content: Vec<ToolResultContent>,
    auto_resize_images: bool,
) -> NormalizedToolResult {
    // Fast path: no image blocks (tool-result-images.ts:26-28).
    if !content
        .iter()
        .any(|b| matches!(b, ToolResultContent::Image(_)))
    {
        return NormalizedToolResult {
            content,
            changed: false,
        };
    }

    let mut normalized: Vec<ToolResultContent> = Vec::with_capacity(content.len());
    let mut changed = false;

    for block in content {
        let (data, mime_type) = match &block {
            ToolResultContent::Image(img) => (&img.data, &img.mime_type),
            _ => {
                normalized.push(block);
                continue;
            }
        };

        // Decode base64 → raw bytes for process_image. If decoding fails,
        // keep the original block (treated as a processing failure).
        let bytes = match base64::engine::general_purpose::STANDARD.decode(data) {
            Ok(bytes) => bytes,
            Err(_) => {
                normalized.push(block);
                continue;
            }
        };

        let processed = match process_image(&bytes, mime_type, auto_resize_images) {
            Ok(p) => p,
            Err(_) => {
                // Keep original on processing failure
                // (tool-result-images.ts:41-47).
                normalized.push(block);
                continue;
            }
        };

        // Check if anything actually changed (tool-result-images.ts:49-52).
        if processed.data == *data
            && processed.mime_type == *mime_type
            && processed.hints.is_empty()
        {
            normalized.push(block);
            continue;
        }

        // Replace with processed image (tool-result-images.ts:54-57).
        normalized.push(ToolResultContent::Image(ImageContent {
            data: processed.data,
            mime_type: processed.mime_type,
        }));
        if !processed.hints.is_empty() {
            normalized.push(ToolResultContent::Text(TextContent {
                text: processed.hints.join("\n"),
                text_signature: None,
            }));
        }
        changed = true;
    }

    NormalizedToolResult {
        content: normalized,
        changed,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageBuffer, Rgba};

    /// Generate a 1×1 PNG (well within limits).
    fn make_tiny_png() -> Vec<u8> {
        let img: ImageBuffer<Rgba<u8>, Vec<u8>> =
            ImageBuffer::from_pixel(1, 1, Rgba([255, 0, 0, 255]));
        let mut buf = Vec::new();
        let mut cursor = std::io::Cursor::new(&mut buf);
        img.write_to(&mut cursor, image::ImageFormat::Png).unwrap();
        buf
    }

    /// Generate an oversized PNG (2400×4800) that triggers resize.
    fn make_oversized_png() -> Vec<u8> {
        let img: ImageBuffer<Rgba<u8>, Vec<u8>> =
            ImageBuffer::from_pixel(2400, 4800, Rgba([0, 128, 255, 255]));
        let mut buf = Vec::new();
        let mut cursor = std::io::Cursor::new(&mut buf);
        img.write_to(&mut cursor, image::ImageFormat::Png).unwrap();
        buf
    }

    /// Generate a small BMP (1×1, 24-bpp) — unsupported inline format.
    fn make_tiny_bmp() -> Vec<u8> {
        let img: image::ImageBuffer<image::Rgb<u8>, Vec<u8>> =
            image::ImageBuffer::from_pixel(1, 1, image::Rgb([255, 0, 0]));
        let mut buf = Vec::new();
        let mut cursor = std::io::Cursor::new(&mut buf);
        img.write_to(&mut cursor, image::ImageFormat::Bmp).unwrap();
        buf
    }

    fn b64(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    fn image_block(bytes: &[u8], mime_type: &str) -> ToolResultContent {
        ToolResultContent::Image(ImageContent {
            data: b64(bytes),
            mime_type: mime_type.to_string(),
        })
    }

    fn text_block(text: &str) -> ToolResultContent {
        ToolResultContent::Text(TextContent {
            text: text.to_string(),
            text_signature: None,
        })
    }

    fn read_png_dimensions(base64_data: &str) -> (u32, u32) {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(base64_data)
            .unwrap();
        // PNG IHDR starts at byte 16; width at 16-19, height at 20-23.
        let width = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
        let height = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
        (width, height)
    }

    // --- No image blocks / already within limits ---

    #[test]
    fn test_no_image_blocks_returns_unchanged() {
        let content = vec![text_block("no images here")];
        let result = normalize_tool_result_images(content.clone(), true);
        assert!(!result.changed);
        assert_eq!(result.content, content);
    }

    #[test]
    fn test_small_image_within_limits_returns_unchanged() {
        let png = make_tiny_png();
        let content = vec![text_block("screenshot"), image_block(&png, "image/png")];
        let result = normalize_tool_result_images(content.clone(), true);
        assert!(!result.changed);
        assert_eq!(result.content, content);
    }

    // --- Path 1: built-in tool produces oversized image ---

    #[test]
    fn test_built_in_tool_oversized_image_is_resized() {
        let png = make_oversized_png();
        let content = vec![text_block("captured"), image_block(&png, "image/png")];
        let result = normalize_tool_result_images(content, true);
        assert!(result.changed);

        // Should be: text + resized-image + dimension-hint
        assert_eq!(result.content.len(), 3);
        assert!(matches!(&result.content[0], ToolResultContent::Text(t) if t.text == "captured"));

        if let ToolResultContent::Image(img) = &result.content[1] {
            let (w, h) = read_png_dimensions(&img.data);
            assert!(w <= 2000, "width {w} exceeds 2000");
            assert!(h <= 2000, "height {h} exceeds 2000");
        } else {
            panic!("expected Image block at index 1");
        }

        if let ToolResultContent::Text(hint) = &result.content[2] {
            assert!(
                hint.text.contains("original 2400x4800"),
                "hint: {}",
                hint.text
            );
        } else {
            panic!("expected dimension Text hint at index 2");
        }
    }

    // --- Path 2: extension injects an oversized image alongside text ---
    // (Extension hooks can add image blocks to existing content.)

    #[test]
    fn test_extension_injected_oversized_image_is_resized() {
        // Simulates extension injecting an image into a text-only tool result.
        let png = make_oversized_png();
        let content = vec![
            text_block("tool output"),
            image_block(&png, "image/png"),
            text_block("injected by extension"),
        ];
        let result = normalize_tool_result_images(content, true);
        assert!(result.changed);

        // Image hint is inserted immediately after the image block
        // (tool-result-images.ts:54-57), so the order is:
        // [text "tool output", image resized, text "dimension hint", text "injected"]
        assert!(
            matches!(&result.content[0], ToolResultContent::Text(t) if t.text == "tool output")
        );
        assert!(matches!(&result.content[1], ToolResultContent::Image(_)));
        assert!(
            matches!(&result.content[2], ToolResultContent::Text(t) if t.text.contains("original 2400x4800"))
        );
        assert!(
            matches!(&result.content[3], ToolResultContent::Text(t) if t.text == "injected by extension")
        );
    }

    // --- Path 3: extension replaces the entire tool_result with an image ---
    // (Extension hooks can fully replace content.)

    #[test]
    fn test_extension_replaced_result_image_is_resized() {
        // Simulates extension completely replacing tool result content.
        let png = make_oversized_png();
        let content = vec![image_block(&png, "image/png")];
        let result = normalize_tool_result_images(content, true);
        assert!(result.changed);
        // Image + dimension hint.
        assert_eq!(result.content.len(), 2);
        assert!(matches!(&result.content[0], ToolResultContent::Image(_)));
        assert!(
            matches!(&result.content[1], ToolResultContent::Text(t) if t.text.contains("original 2400x4800"))
        );
    }

    // --- autoResize disabled ---

    #[test]
    fn test_auto_resize_disabled_oversized_image_unchanged() {
        let png = make_oversized_png();
        let content = vec![image_block(&png, "image/png")];
        let original_data = if let ToolResultContent::Image(img) = &content[0] {
            img.data.clone()
        } else {
            unreachable!()
        };

        let result = normalize_tool_result_images(content, false);
        // Oversized image with auto-resize off: no change.
        assert!(!result.changed);
        if let ToolResultContent::Image(img) = &result.content[0] {
            assert_eq!(img.data, original_data, "image data must be unchanged");
        } else {
            panic!("expected Image block");
        }
    }

    #[test]
    fn test_auto_resize_disabled_unsupported_format_still_converted() {
        // BMP is not an inline-supported format; even with auto-resize off,
        // process_image converts it to PNG (image-process.ts:105-118).
        let bmp = make_tiny_bmp();
        let content = vec![image_block(&bmp, "image/bmp")];
        let result = normalize_tool_result_images(content, false);
        assert!(result.changed);
        // Converted image + conversion hint.
        assert_eq!(result.content.len(), 2);
        if let ToolResultContent::Image(img) = &result.content[0] {
            assert_eq!(img.mime_type, "image/png");
        } else {
            panic!("expected Image block");
        }
        assert!(
            matches!(&result.content[1], ToolResultContent::Text(t) if t.text == "[Image converted from image/bmp to image/png.]")
        );
    }

    // --- Failure preserves original ---

    #[test]
    fn test_undecodable_image_keeps_original() {
        // Valid base64 but not a real image — process_image will fail.
        let content = vec![ToolResultContent::Image(ImageContent {
            data: b64(b"not-an-image"),
            mime_type: "image/png".to_string(),
        })];
        let result = normalize_tool_result_images(content.clone(), true);
        // Failure: original preserved, no change reported.
        assert!(!result.changed);
        assert_eq!(result.content, content);
    }

    #[test]
    fn test_invalid_base64_keeps_original() {
        let content = vec![ToolResultContent::Image(ImageContent {
            data: "!!!not-base64!!!".to_string(),
            mime_type: "image/png".to_string(),
        })];
        let result = normalize_tool_result_images(content.clone(), true);
        assert!(!result.changed);
        assert_eq!(result.content, content);
    }

    // --- Surrounding text preservation ---

    #[test]
    fn test_preserves_surrounding_text_order() {
        let png = make_oversized_png();
        let content = vec![
            text_block("before"),
            image_block(&png, "image/png"),
            text_block("after"),
        ];
        let result = normalize_tool_result_images(content, true);
        assert!(result.changed);

        let types: Vec<&str> = result
            .content
            .iter()
            .map(|b| match b {
                ToolResultContent::Text(_) => "text",
                ToolResultContent::Image(_) => "image",
            })
            .collect();
        assert_eq!(types, vec!["text", "image", "text", "text"]);

        assert!(matches!(&result.content[0], ToolResultContent::Text(t) if t.text == "before"));
        // Index 2 is the dimension hint (inserted right after the resized image);
        // index 3 is the original "after" text.
        assert!(
            matches!(&result.content[2], ToolResultContent::Text(t) if t.text.contains("original 2400x4800"))
        );
        assert!(matches!(&result.content[3], ToolResultContent::Text(t) if t.text == "after"));
    }
}
