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
            max_bytes: 4_718_592,
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
    /// cannot be decoded or made to fit.
    fn prepare(&self, bytes: &[u8], mime_type: &str, limits: ImageLimits) -> Option<PreparedImage>;
}

const INLINE_IMAGE_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];
const JPEG_QUALITIES: [u8; 4] = [80, 70, 55, 40];

fn base64_length(byte_length: usize) -> u64 {
    (byte_length as u64).div_ceil(3) * 4
}

fn encode_within(
    image: &image::DynamicImage,
    max_bytes: u64,
    jpeg_first: bool,
) -> Option<(Vec<u8>, &'static str)> {
    let mut candidates = Vec::with_capacity(JPEG_QUALITIES.len() + 1);
    if jpeg_first {
        candidates.extend(JPEG_QUALITIES.map(|quality| ("image/jpeg", Some(quality))));
        candidates.push(("image/png", None));
    } else {
        candidates.push(("image/png", None));
        candidates.extend(JPEG_QUALITIES.map(|quality| ("image/jpeg", Some(quality))));
    }
    for (mime_type, quality) in candidates {
        let mut encoded = Vec::new();
        if let Some(quality) = quality {
            image
                .write_with_encoder(image::codecs::jpeg::JpegEncoder::new_with_quality(
                    &mut encoded,
                    quality,
                ))
                .ok()?;
        } else {
            image
                .write_to(&mut Cursor::new(&mut encoded), image::ImageFormat::Png)
                .ok()?;
        }
        if base64_length(encoded.len()) <= max_bytes {
            return Some((encoded, mime_type));
        }
    }
    None
}

/// The built-in processor: decode, apply EXIF orientation, pass through images
/// that already fit, or re-encode and shrink until the model limits are met.
#[derive(Debug, Clone, Copy, Default)]
pub struct BuiltinImageProcessor;

impl ImageProcessor for BuiltinImageProcessor {
    fn prepare(&self, bytes: &[u8], mime_type: &str, limits: ImageLimits) -> Option<PreparedImage> {
        let reader = ImageReader::new(Cursor::new(bytes))
            .with_guessed_format()
            .ok()?;
        let mut decoder = reader.into_decoder().ok()?;
        let orientation = image::ImageDecoder::orientation(&mut decoder).ok()?;
        let mut image = image::DynamicImage::from_decoder(decoder).ok()?;
        if INLINE_IMAGE_TYPES.contains(&mime_type)
            && orientation == image::metadata::Orientation::NoTransforms
            && image.width() <= limits.max_width
            && image.height() <= limits.max_height
            && base64_length(bytes.len()) <= limits.max_bytes
        {
            return Some(PreparedImage {
                data: base64::engine::general_purpose::STANDARD.encode(bytes),
                mime_type: mime_type.to_string(),
                converted_from: None,
                resized: None,
            });
        }

        image.apply_orientation(orientation);
        let (original_width, original_height) = (image.width(), image.height());
        let scale = (limits.max_width as f64 / original_width as f64)
            .min(limits.max_height as f64 / original_height as f64)
            .min(1.0);
        let mut target = (
            ((original_width as f64 * scale).round() as u32).max(1),
            ((original_height as f64 * scale).round() as u32).max(1),
        );
        loop {
            let scaled = (target.0 != original_width || target.1 != original_height).then(|| {
                image.resize_exact(target.0, target.1, image::imageops::FilterType::Lanczos3)
            });
            let candidate = scaled.as_ref().unwrap_or(&image);
            if let Some((encoded, prepared_mime)) =
                encode_within(candidate, limits.max_bytes, mime_type == "image/jpeg")
            {
                return Some(PreparedImage {
                    data: base64::engine::general_purpose::STANDARD.encode(encoded),
                    mime_type: prepared_mime.to_string(),
                    converted_from: (prepared_mime != mime_type).then(|| mime_type.to_string()),
                    resized: (target.0 != original_width || target.1 != original_height)
                        .then_some((original_width, original_height, target.0, target.1)),
                });
            }
            if target == (1, 1) {
                return None;
            }
            target = (
                ((target.0 as f64 * 0.75).floor() as u32).max(1),
                ((target.1 as f64 * 0.75).floor() as u32).max(1),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode(image: &image::DynamicImage, format: image::ImageFormat) -> Vec<u8> {
        let mut bytes = Vec::new();
        image
            .write_to(&mut Cursor::new(&mut bytes), format)
            .expect("encode");
        bytes
    }

    fn noisy(width: u32, height: u32) -> image::DynamicImage {
        let mut image = image::RgbaImage::new(width, height);
        let mut seed = 1u32;
        for pixel in image.pixels_mut() {
            for channel in &mut pixel.0[..3] {
                seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                *channel = (seed >> 24) as u8;
            }
            pixel.0[3] = 255;
        }
        image::DynamicImage::ImageRgba8(image)
    }

    #[test]
    fn a_small_jpeg_is_returned_unchanged() {
        let jpeg = encode(&noisy(8, 6), image::ImageFormat::Jpeg);
        let prepared = BuiltinImageProcessor
            .prepare(&jpeg, "image/jpeg", ImageLimits::default())
            .expect("prepared");
        assert_eq!(prepared.mime_type, "image/jpeg");
        assert_eq!(
            prepared.data,
            base64::engine::general_purpose::STANDARD.encode(jpeg)
        );
        assert_eq!(prepared.converted_from, None);
        assert_eq!(prepared.resized, None);
    }

    #[test]
    fn a_jpeg_within_the_inline_limits_does_not_depend_on_its_png_size() {
        let image = noisy(1200, 1200);
        let jpeg = encode(&image, image::ImageFormat::Jpeg);
        let limits = ImageLimits::default();
        assert!((jpeg.len() as u64).div_ceil(3) * 4 <= limits.max_bytes);
        let png = encode(&image, image::ImageFormat::Png);
        assert!((png.len() as u64).div_ceil(3) * 4 > limits.max_bytes);

        let prepared = BuiltinImageProcessor
            .prepare(&jpeg, "image/jpeg", limits)
            .expect("the original JPEG already fits");
        assert_eq!(prepared.mime_type, "image/jpeg");
        assert_eq!(
            prepared.data,
            base64::engine::general_purpose::STANDARD.encode(jpeg)
        );
        assert_eq!(prepared.converted_from, None);
    }

    #[test]
    fn an_oversized_png_becomes_jpeg_and_shrinks_until_it_fits() {
        let png = encode(&noisy(1100, 1100), image::ImageFormat::Png);
        let prepared = BuiltinImageProcessor
            .prepare(&png, "image/png", ImageLimits::default())
            .expect("JPEG fallback");
        assert_eq!(prepared.mime_type, "image/jpeg");
        assert_eq!(prepared.converted_from.as_deref(), Some("image/png"));
        assert!(prepared.data.len() as u64 <= ImageLimits::default().max_bytes);
        assert_eq!(prepared.resized, None);

        let png = encode(&noisy(2000, 2000), image::ImageFormat::Png);
        let limits = ImageLimits {
            max_bytes: 200_000,
            ..ImageLimits::default()
        };
        let shrunk = BuiltinImageProcessor
            .prepare(&png, "image/png", limits)
            .expect("shrunk");
        assert!(shrunk.data.len() as u64 <= limits.max_bytes);
        let (_, _, width, height) = shrunk.resized.expect("resized");
        assert!(width < 2000 && height < 2000);
    }

    fn with_orientation(jpeg: &[u8], orientation: u8) -> Vec<u8> {
        let tiff = [
            0x4d,
            0x4d,
            0,
            42,
            0,
            0,
            0,
            8,
            0,
            1,
            0x01,
            0x12,
            0,
            3,
            0,
            0,
            0,
            1,
            0,
            orientation,
            0,
            0,
            0,
            0,
            0,
            0,
        ];
        let mut exif = b"Exif\0\0".to_vec();
        exif.extend_from_slice(&tiff);
        let length = exif.len() + 2;
        let mut app1 = vec![0xff, 0xe1, (length >> 8) as u8, length as u8];
        app1.extend_from_slice(&exif);
        let mut bytes = jpeg[..2].to_vec();
        bytes.extend_from_slice(&app1);
        bytes.extend_from_slice(&jpeg[2..]);
        bytes
    }

    #[test]
    fn exif_orientation_is_applied_before_encoding() {
        const COLORS: [[u8; 3]; 6] = [
            [255, 0, 0],
            [0, 255, 0],
            [0, 0, 255],
            [255, 255, 255],
            [0, 0, 0],
            [255, 255, 0],
        ];
        let mut stored = image::RgbaImage::new(48, 32);
        for (x, y, pixel) in stored.enumerate_pixels_mut() {
            let index = (y / 16 * 3 + x / 16) as usize;
            *pixel = image::Rgba([COLORS[index][0], COLORS[index][1], COLORS[index][2], 255]);
        }
        let jpeg = encode(
            &image::DynamicImage::ImageRgba8(stored),
            image::ImageFormat::Jpeg,
        );
        let prepared = BuiltinImageProcessor
            .prepare(
                &with_orientation(&jpeg, 6),
                "image/jpeg",
                ImageLimits::default(),
            )
            .expect("oriented");
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(prepared.data)
            .expect("base64");
        let image = image::load_from_memory(&decoded).expect("decode").to_rgb8();
        assert_eq!((image.width(), image.height()), (32, 48));
        for (index, (x, y)) in [(8, 8), (24, 8), (8, 24), (24, 24), (8, 40), (24, 40)]
            .into_iter()
            .enumerate()
        {
            let expected = [3, 0, 4, 1, 5, 2][index];
            let actual = image.get_pixel(x, y).0;
            for channel in 0..3 {
                assert_eq!(
                    actual[channel] > 127,
                    COLORS[expected][channel] > 127,
                    "pixel {index}, channel {channel}"
                );
            }
        }
    }

    #[test]
    fn the_default_byte_limit_is_four_and_a_half_mib() {
        assert_eq!(ImageLimits::default().max_bytes, 4_718_592);
    }
}
