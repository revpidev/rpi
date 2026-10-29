//! Optional image processing for the read tool (port of
//! `packages/durable/src/tools/image-processor.ts` @ `4a080252f`).
//!
//! A processor orients by EXIF, converts BMP to PNG and shrinks an image to
//! the model's input limits. It runs off the async thread.

use base64::Engine;
use image::ImageReader;
use std::io::Cursor;

/// Input limits of one model's image input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageLimits {
    /// Maximum width in pixels.
    pub max_width: u32,
    /// Maximum height in pixels.
    pub max_height: u32,
    /// Maximum base64-encoded payload size.
    pub max_bytes: u64,
}

impl Default for ImageLimits {
    fn default() -> Self {
        Self {
            max_width: 2000,
            max_height: 2000,
            max_bytes: 4_500_000,
        }
    }
}

/// One prepared image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedImage {
    /// Base64 data.
    pub data: String,
    /// MIME type of the prepared data.
    pub mime_type: String,
    /// Format converted from, when conversion happened.
    pub converted_from: Option<String>,
    /// Original and resulting dimensions, when resized.
    pub resized: Option<(u32, u32, u32, u32)>,
}

/// Prepares image bytes for the model.
pub trait ImageProcessor: Send + Sync {
    /// Prepare `bytes` of `mime_type` within `limits`; `None` when the image
    /// cannot be decoded.
    fn prepare(&self, bytes: &[u8], mime_type: &str, limits: ImageLimits) -> Option<PreparedImage>;
}

/// The built-in processor: decode, apply EXIF orientation, shrink to the
/// limits and encode PNG.
#[derive(Debug, Clone, Copy, Default)]
pub struct BuiltinImageProcessor;

impl ImageProcessor for BuiltinImageProcessor {
    fn prepare(&self, bytes: &[u8], mime_type: &str, limits: ImageLimits) -> Option<PreparedImage> {
        let cursor = Cursor::new(bytes.to_vec());
        let reader = ImageReader::new(cursor).with_guessed_format().ok()?;
        let mut decoder = reader.into_decoder().ok()?;
        let orientation = image::ImageDecoder::orientation(&mut decoder).ok()?;
        let mut image = image::DynamicImage::from_decoder(decoder).ok()?;
        image.apply_orientation(orientation);
        let (original_width, original_height) = (image.width(), image.height());
        let (mut width, mut height) = (original_width, original_height);
        if width > limits.max_width || height > limits.max_height {
            let scale = (limits.max_width as f64 / width as f64)
                .min(limits.max_height as f64 / height as f64);
            width = ((width as f64 * scale).round() as u32).max(1);
            height = ((height as f64 * scale).round() as u32).max(1);
            image = image.resize_exact(width, height, image::imageops::FilterType::Lanczos3);
        }
        let resized = (width != original_width || height != original_height).then_some((
            original_width,
            original_height,
            width,
            height,
        ));
        let mut png = Vec::new();
        image
            .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
            .ok()?;
        if base64::engine::general_purpose::STANDARD.encode(&png).len() as u64 > limits.max_bytes {
            return None;
        }
        Some(PreparedImage {
            data: base64::engine::general_purpose::STANDARD.encode(&png),
            mime_type: "image/png".to_string(),
            converted_from: (mime_type != "image/png").then(|| mime_type.to_string()),
            resized,
        })
    }
}
