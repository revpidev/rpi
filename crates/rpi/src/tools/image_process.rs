//! Port of `packages/coding-agent/src/utils/image-process.ts` and
//! `packages/coding-agent/src/utils/image-resize-core.ts`
//! @ pi 0.82.1 (2efa728).
//!
//! Upstream uses Photon (Rust/WASM) for image decode/resize/encode and a
//! worker thread for non-blocking operation. This port replaces Photon with
//! the pure-Rust [`image`] crate (default-features=false, png/jpeg/gif/webp/bmp)
//! and runs synchronously — the image sizes involved are small enough that
//! blocking the executor briefly is acceptable.
//!
//! EXIF orientation is read via [`kamadak-exif`] (replaces upstream's manual
//! JPEG/WebP EXIF parser in `exif-orientation.ts`).

use base64::Engine;
use image::imageops::FilterType;
use image::DynamicImage;
use image::ImageEncoder;

// ---------------------------------------------------------------------------
// Constants (image-resize-core.ts:22-29)
// ---------------------------------------------------------------------------

/// Maximum width/height for inline images (image-resize-core.ts:25-26).
const MAX_WIDTH: u32 = 2000;
const MAX_HEIGHT: u32 = 2000;

/// 4.5 MB of base64 payload — headroom below Anthropic's 5 MB limit
/// (image-resize-core.ts:22).
const MAX_BYTES: usize = 4_718_592; // 4.5 * 1024 * 1024

/// Default JPEG quality (image-resize-core.ts:28).
const JPEG_QUALITY: u8 = 80;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Error from image processing. `message` is always one of the two upstream
/// error strings (image-process.ts:82, 91).
#[derive(Debug, Clone)]
pub struct ProcessImageError {
    pub message: String,
}

/// Successfully processed image ready for inline attachment.
///
/// **Intentional difference**: upstream `ProcessImageResult` has only
/// `{ data, mimeType, hints }`. Width/height are added for testability
/// and coordinate mapping.
#[derive(Debug, Clone)]
pub struct ProcessedImage {
    /// Base64-encoded image data.
    pub data: String,
    /// MIME type of the processed image (may differ from input after conversion).
    pub mime_type: String,
    /// Human-readable hints (conversion note, dimension note).
    pub hints: Vec<String>,
    /// Final displayed width (after resize, or original if unresized).
    pub width: u32,
    /// Final displayed height (after resize, or original if unresized).
    pub height: u32,
}

// ---------------------------------------------------------------------------
// Internal: NormalizedImage (image-process.ts:23-27)
// ---------------------------------------------------------------------------

struct NormalizedImage {
    bytes: Vec<u8>,
    mime_type: String,
    converted_from: Option<String>,
}

struct ResizedImage {
    data: String,
    mime_type: String,
    original_width: u32,
    original_height: u32,
    width: u32,
    height: u32,
    was_resized: bool,
}

// ---------------------------------------------------------------------------
// MIME normalisation (image-process.ts:29-47)
// ---------------------------------------------------------------------------

/// Extract the base MIME type (strip parameters, lowercase) (image-process.ts:29-31).
fn base_mime_type(mime_type: &str) -> String {
    mime_type
        .split(';')
        .next()
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| mime_type.to_lowercase())
}

/// Map a MIME type to a normalised supported type, or `None` if unsupported
/// (image-process.ts:33-47).
fn normalize_supported_image_mime_type(mime_type: &str) -> Option<String> {
    match base_mime_type(mime_type).as_str() {
        "image/png" => Some("image/png".to_string()),
        "image/jpeg" | "image/jpg" => Some("image/jpeg".to_string()),
        "image/gif" => Some("image/gif".to_string()),
        "image/webp" => Some("image/webp".to_string()),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Hint helpers (image-process.ts:67-70, image-resize.ts:116-122)
// ---------------------------------------------------------------------------

/// Conversion hint (image-process.ts:67-70).
fn conversion_hint(from: &str, to: &str) -> Option<String> {
    if from == to {
        return None;
    }
    Some(format!("[Image converted from {from} to {to}.]"))
}

/// Dimension note for resized images (image-resize.ts:116-122).
fn format_dimension_note(result: &ResizedImage) -> Option<String> {
    if !result.was_resized {
        return None;
    }
    let scale = result.original_width as f64 / result.width as f64;
    Some(format!(
        "[Image: original {}x{}, displayed at {}x{}. \
         Multiply coordinates by {:.2} to map to original image.]",
        result.original_width, result.original_height, result.width, result.height, scale
    ))
}

// ---------------------------------------------------------------------------
// EXIF orientation (exif-orientation.ts)
// ---------------------------------------------------------------------------

/// Read EXIF orientation value (1-8) from image bytes.
///
/// Uses [`kamadak-exif`] which supports JPEG, TIFF, HEIF, and WebP containers.
/// Returns 1 (normal) if no EXIF data is found or the container is unsupported
/// (e.g. PNG, GIF, BMP).
fn read_exif_orientation(bytes: &[u8]) -> u32 {
    let mut cursor = std::io::Cursor::new(bytes);
    let reader = exif::Reader::new();
    match reader.read_from_container(&mut cursor) {
        Ok(exif_data) => {
            if let Some(field) = exif_data.get_field(exif::Tag::Orientation, exif::In::PRIMARY) {
                if let Some(v) = field.value.get_uint(0) {
                    if (1..=8).contains(&v) {
                        return v;
                    }
                }
            }
            1
        }
        Err(_) => 1,
    }
}

/// Apply EXIF orientation to a [`DynamicImage`] (exif-orientation.ts:147-183).
///
/// Orientation mapping (EXIF spec):
/// - 1: normal · 2: flip-H · 3: rotate-180 · 4: flip-V
/// - 5: transpose (rotate-90-CW + flip-H)
/// - 6: rotate-90-CW
/// - 7: transverse (rotate-90-CCW + flip-H)
/// - 8: rotate-90-CCW (= rotate-270-CW)
fn apply_exif_orientation(img: DynamicImage, bytes: &[u8]) -> DynamicImage {
    let orientation = read_exif_orientation(bytes);
    match orientation {
        1 => img,
        2 => img.fliph(),
        3 => img.rotate180(),
        4 => img.flipv(),
        5 => img.rotate90().fliph(),
        6 => img.rotate90(),
        7 => img.rotate270().fliph(),
        8 => img.rotate270(),
        _ => img,
    }
}

// ---------------------------------------------------------------------------
// Encoding helpers
// ---------------------------------------------------------------------------

/// Encode a [`DynamicImage`] as RGBA PNG bytes.
fn encode_png(img: &DynamicImage) -> Result<Vec<u8>, image::ImageError> {
    let mut buf = Vec::new();
    let rgba = img.to_rgba8();
    image::codecs::png::PngEncoder::new(&mut buf).write_image(
        rgba.as_raw(),
        rgba.width(),
        rgba.height(),
        image::ExtendedColorType::Rgba8,
    )?;
    Ok(buf)
}

/// Encode a [`DynamicImage`] as JPEG bytes with the given quality (1-100).
fn encode_jpeg(img: &DynamicImage, quality: u8) -> Result<Vec<u8>, image::ImageError> {
    let mut buf = Vec::new();
    let rgb = img.to_rgb8();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, quality).write_image(
        rgb.as_raw(),
        rgb.width(),
        rgb.height(),
        image::ExtendedColorType::Rgb8,
    )?;
    Ok(buf)
}

// ---------------------------------------------------------------------------
// convertImageBytesToPng (image-convert.ts:4-24)
// ---------------------------------------------------------------------------

/// Decode arbitrary image bytes, apply EXIF orientation, and re-encode as PNG.
///
/// Returns `None` if the image cannot be decoded or encoded (upstream returns
/// `null` from Photon failure).
fn convert_image_bytes_to_png(bytes: &[u8]) -> Option<Vec<u8>> {
    let raw_img = image::load_from_memory(bytes).ok()?;
    let img = apply_exif_orientation(raw_img, bytes);
    encode_png(&img).ok()
}

// ---------------------------------------------------------------------------
// normalizeImage (image-process.ts:49-65)
// ---------------------------------------------------------------------------

/// Normalise image bytes to a supported inline format.
///
/// Supported types (png/jpeg/gif/webp) are passed through unchanged.
/// Unsupported types are converted to PNG. Returns `None` if conversion fails.
fn normalize_image(bytes: &[u8], mime_type: &str) -> Option<NormalizedImage> {
    if let Some(normalized_mime) = normalize_supported_image_mime_type(mime_type) {
        return Some(NormalizedImage {
            bytes: bytes.to_vec(),
            mime_type: normalized_mime,
            converted_from: None,
        });
    }

    // Unsupported type: try to convert to PNG.
    let png_bytes = convert_image_bytes_to_png(bytes)?;
    Some(NormalizedImage {
        bytes: png_bytes,
        mime_type: "image/png".to_string(),
        converted_from: Some(base_mime_type(mime_type)),
    })
}

// ---------------------------------------------------------------------------
// resizeImageInProcess (image-resize-core.ts:59-163)
// ---------------------------------------------------------------------------

/// Resize an image to fit within max dimensions and encoded file size.
///
/// Returns `None` if the image cannot be resized below `MAX_BYTES`.
///
/// Strategy (image-resize-core.ts:46-57):
/// 1. Resize to maxWidth/maxHeight maintaining aspect ratio.
/// 2. Try PNG and JPEG (multiple qualities), pick the first that fits.
/// 3. If still too large, progressively reduce dimensions (×0.75) until 1×1.
///
/// **Intentional difference**: the upstream `image` crate decodes GIF to the
/// first frame, matching Photon's behaviour (single-frame processing).
fn resize_image(input_bytes: &[u8], mime_type: &str) -> Option<ResizedImage> {
    let input_base64_size = input_bytes.len().div_ceil(3) * 4;

    // Decode image. Any decode error → None (upstream try-catch → null).
    let raw_img = image::load_from_memory(input_bytes).ok()?;
    // Apply EXIF orientation from the original bytes.
    let img = apply_exif_orientation(raw_img, input_bytes);

    let original_width = img.width();
    let original_height = img.height();
    let format = mime_type.split('/').nth(1).unwrap_or("png");

    // Check if already within all limits (dimensions AND encoded size)
    // (image-resize-core.ts:82-93).
    if original_width <= MAX_WIDTH && original_height <= MAX_HEIGHT && input_base64_size < MAX_BYTES
    {
        let data = base64::engine::general_purpose::STANDARD.encode(input_bytes);
        return Some(ResizedImage {
            data,
            mime_type: if mime_type.is_empty() {
                format!("image/{format}")
            } else {
                mime_type.to_string()
            },
            original_width,
            original_height,
            width: original_width,
            height: original_height,
            was_resized: false,
        });
    }

    // Calculate initial dimensions respecting max limits (image-resize-core.ts:95-106).
    let mut target_width = original_width;
    let mut target_height = original_height;

    if target_width > MAX_WIDTH {
        target_height =
            ((target_height as f64 * MAX_WIDTH as f64) / target_width as f64).round() as u32;
        target_width = MAX_WIDTH;
    }
    if target_height > MAX_HEIGHT {
        target_width =
            ((target_width as f64 * MAX_HEIGHT as f64) / target_height as f64).round() as u32;
        target_height = MAX_HEIGHT;
    }

    // Quality gradient with deduplication (image-resize-core.ts:122).
    let quality_steps: Vec<u8> = {
        let raw = [JPEG_QUALITY, 85, 70, 55, 40];
        let mut seen = std::collections::HashSet::new();
        raw.iter().filter(|q| seen.insert(**q)).copied().collect()
    };

    let mut current_width = target_width;
    let mut current_height = target_height;

    loop {
        let resized = img.resize_exact(current_width, current_height, FilterType::Lanczos3);

        // PNG candidate (image-resize-core.ts:112).
        if let Ok(png_data) = encode_png(&resized) {
            let png_b64 = base64::engine::general_purpose::STANDARD.encode(&png_data);
            if png_b64.len() < MAX_BYTES {
                return Some(ResizedImage {
                    data: png_b64,
                    mime_type: "image/png".to_string(),
                    original_width,
                    original_height,
                    width: current_width,
                    height: current_height,
                    was_resized: true,
                });
            }
        }

        // JPEG candidates at each quality level (image-resize-core.ts:113-115).
        for &quality in &quality_steps {
            if let Ok(jpeg_data) = encode_jpeg(&resized, quality) {
                let jpeg_b64 = base64::engine::general_purpose::STANDARD.encode(&jpeg_data);
                if jpeg_b64.len() < MAX_BYTES {
                    return Some(ResizedImage {
                        data: jpeg_b64,
                        mime_type: "image/jpeg".to_string(),
                        original_width,
                        original_height,
                        width: current_width,
                        height: current_height,
                        was_resized: true,
                    });
                }
            }
        }

        // Check if we've reached 1×1 (image-resize-core.ts:142-144).
        if current_width == 1 && current_height == 1 {
            break;
        }

        // Reduce dimensions by 0.75 (image-resize-core.ts:146-154).
        let next_width = if current_width == 1 {
            1
        } else {
            ((current_width as f64) * 0.75).floor().max(1.0) as u32
        };
        let next_height = if current_height == 1 {
            1
        } else {
            ((current_height as f64) * 0.75).floor().max(1.0) as u32
        };

        if next_width == current_width && next_height == current_height {
            break;
        }

        current_width = next_width;
        current_height = next_height;
    }

    None
}

// ---------------------------------------------------------------------------
// processImage (image-process.ts:72-118)
// ---------------------------------------------------------------------------

/// Process an image for inline display.
///
/// * `bytes` — raw image bytes.
/// * `mime_type` — detected MIME type (e.g. `"image/png"`, `"image/bmp"`).
/// * `auto_resize` — whether to resize to 2000×2000 / 4.5 MB limits.
///
/// On error, `message` is one of:
/// - `"[Image omitted: could not be converted to a supported inline image format.]"`
/// - `"[Image omitted: could not be resized below the inline image size limit.]"`
pub fn process_image(
    bytes: &[u8],
    mime_type: &str,
    auto_resize: bool,
) -> Result<ProcessedImage, ProcessImageError> {
    let normalized = normalize_image(bytes, mime_type).ok_or_else(|| ProcessImageError {
        message: "[Image omitted: could not be converted to a supported inline image format.]"
            .to_string(),
    })?;

    if auto_resize {
        let resized = resize_image(&normalized.bytes, &normalized.mime_type).ok_or_else(|| {
            ProcessImageError {
                message: "[Image omitted: could not be resized below the inline image size limit.]"
                    .to_string(),
            }
        })?;

        let mut hints = Vec::new();
        if let Some(from) = &normalized.converted_from {
            if let Some(h) = conversion_hint(from, &resized.mime_type) {
                hints.push(h);
            }
        }
        if let Some(note) = format_dimension_note(&resized) {
            hints.push(note);
        }

        Ok(ProcessedImage {
            data: resized.data,
            mime_type: resized.mime_type,
            hints,
            width: resized.width,
            height: resized.height,
        })
    } else {
        // No auto-resize: base64 the normalised bytes (image-process.ts:109-118).
        let mut hints = Vec::new();
        if let Some(from) = &normalized.converted_from {
            if let Some(h) = conversion_hint(from, &normalized.mime_type) {
                hints.push(h);
            }
        }

        // Decode for dimensions; default to (0, 0) if undecodable.
        let (width, height) = image::load_from_memory(&normalized.bytes)
            .map(|img| (img.width(), img.height()))
            .unwrap_or((0, 0));

        let data = base64::engine::general_purpose::STANDARD.encode(&normalized.bytes);
        Ok(ProcessedImage {
            data,
            mime_type: normalized.mime_type.clone(),
            hints,
            width,
            height,
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageBuffer, Rgb, Rgba};

    /// Generate a small solid-colour PNG image (10×10).
    fn make_small_png() -> Vec<u8> {
        let img: ImageBuffer<Rgba<u8>, Vec<u8>> =
            ImageBuffer::from_pixel(10, 10, Rgba([255, 0, 0, 255]));
        let mut buf = Vec::new();
        let mut cursor = std::io::Cursor::new(&mut buf);
        img.write_to(&mut cursor, image::ImageFormat::Png).unwrap();
        buf
    }

    /// Generate a large solid-colour PNG image (3000×3000) that triggers resize.
    fn make_large_png() -> Vec<u8> {
        let img: ImageBuffer<Rgba<u8>, Vec<u8>> =
            ImageBuffer::from_pixel(3000, 3000, Rgba([0, 128, 255, 255]));
        let mut buf = Vec::new();
        let mut cursor = std::io::Cursor::new(&mut buf);
        img.write_to(&mut cursor, image::ImageFormat::Png).unwrap();
        buf
    }

    /// Generate a small BMP image (20×20).
    fn make_small_bmp() -> Vec<u8> {
        let img: ImageBuffer<Rgb<u8>, Vec<u8>> =
            ImageBuffer::from_pixel(20, 20, Rgb([100, 150, 200]));
        let mut buf = Vec::new();
        let mut cursor = std::io::Cursor::new(&mut buf);
        img.write_to(&mut cursor, image::ImageFormat::Bmp).unwrap();
        buf
    }

    #[test]
    fn test_small_png_no_resize() {
        let png = make_small_png();
        let result = process_image(&png, "image/png", true).unwrap();
        assert_eq!(result.mime_type, "image/png");
        assert!(result.hints.is_empty());
        assert_eq!(result.width, 10);
        assert_eq!(result.height, 10);
        // Data should be valid base64 of the original bytes.
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&result.data)
            .unwrap();
        assert_eq!(decoded, png);
    }

    #[test]
    fn test_large_png_triggers_resize_hint() {
        let png = make_large_png();
        let result = process_image(&png, "image/png", true).unwrap();
        // Was resized: dimensions should be <= 2000.
        assert!(result.width <= MAX_WIDTH);
        assert!(result.height <= MAX_HEIGHT);
        // Should have a dimension hint.
        assert!(result
            .hints
            .iter()
            .any(|h| h.starts_with("[Image: original")));
        // Hint should mention original 3000x3000.
        let dim_hint = result
            .hints
            .iter()
            .find(|h| h.starts_with("[Image: original"))
            .unwrap();
        assert!(dim_hint.contains("3000x3000"), "hint: {dim_hint}");
        // And the displayed dimensions.
        assert!(
            dim_hint.contains(&format!("displayed at {}x{}", result.width, result.height)),
            "hint: {dim_hint}"
        );
    }

    #[test]
    fn test_bmp_converted_to_png() {
        let bmp = make_small_bmp();
        let result = process_image(&bmp, "image/bmp", true).unwrap();
        assert_eq!(result.mime_type, "image/png");
        // Should have conversion hint.
        assert!(result
            .hints
            .iter()
            .any(|h| h == "[Image converted from image/bmp to image/png.]"));
        // Dimensions preserved (small image, no resize needed).
        assert_eq!(result.width, 20);
        assert_eq!(result.height, 20);
    }

    #[test]
    fn test_corrupt_png_resize_error() {
        // Valid PNG magic but corrupt data → decode fails → resize error.
        let corrupt = [
            0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, // PNG signature
            0x00, 0x00, 0x00, 0x0d, // IHDR length
            b'I', b'H', b'D', b'R', // IHDR
            0xFF, 0xFF, 0xFF, 0xFF, // garbage width/height
            0xFF, 0xFF, 0xFF, 0xFF, 0xFF, // garbage
        ];
        let result = process_image(&corrupt, "image/png", true);
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().message,
            "[Image omitted: could not be resized below the inline image size limit.]"
        );
    }

    #[test]
    fn test_auto_resize_false_passes_through() {
        let png = make_small_png();
        let result = process_image(&png, "image/png", false).unwrap();
        assert_eq!(result.mime_type, "image/png");
        assert!(result.hints.is_empty());
        // Data should be base64 of original.
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&result.data)
            .unwrap();
        assert_eq!(decoded, png);
    }

    #[test]
    fn test_auto_resize_false_bmp_conversion() {
        let bmp = make_small_bmp();
        let result = process_image(&bmp, "image/bmp", false).unwrap();
        assert_eq!(result.mime_type, "image/png");
        assert!(result
            .hints
            .iter()
            .any(|h| h == "[Image converted from image/bmp to image/png.]"));
    }

    #[test]
    fn test_unsupported_garbage_normalize_error() {
        let garbage = b"not an image at all".to_vec();
        let result = process_image(&garbage, "application/octet-stream", true);
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().message,
            "[Image omitted: could not be converted to a supported inline image format.]"
        );
    }

    #[test]
    fn test_jpeg_mime_normalization() {
        let png = make_small_png();
        // "image/jpg" should be normalised to "image/jpeg".
        let result = process_image(&png, "image/jpg", true).unwrap();
        assert_eq!(result.mime_type, "image/jpeg");
    }

    #[test]
    fn test_mime_with_parameters() {
        let png = make_small_png();
        // MIME with charset parameter should still be recognised.
        let result = process_image(&png, "image/png; charset=utf-8", true).unwrap();
        assert_eq!(result.mime_type, "image/png");
    }
}
