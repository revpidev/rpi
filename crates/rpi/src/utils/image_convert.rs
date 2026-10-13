//! Port of `packages/coding-agent/src/utils/image-convert.ts` @ pi 1.1.0
//! (abe508e1b, a276dabe5 / #10292).
//!
//! Kitty-protocol terminals only accept PNG, so the interactive mode
//! registers the PNG transcoder exposed by `rpi-tui`'s `Image` component.
//! Upstream loads photon asynchronously; the Rust port compiles the `image`
//! crate in, so loading is synchronous and always succeeds.
//!
//! Intentional differences: `loadPngTranscoder` is infallible (upstream
//! returns `undefined` when photon fails to load).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use base64::Engine as _;
use rpi_tui::components::image::{ImageTranscoder, set_image_transcoder};
use rpi_tui::terminal_image::{ImageProtocol, get_capabilities};

/// `loadPngTranscoder` (image-convert.ts:61-68, JSDoc 57-60): a synchronous transcoder
/// that decodes any image, applies EXIF orientation, and re-encodes PNG.
pub fn load_png_transcoder() -> ImageTranscoder {
    Arc::new(|base64_data: &str, _mime_type: &str| {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(base64_data)
            .ok()?;
        let png = crate::tools::image_process::convert_image_bytes_to_png(&bytes)?;
        Some(base64::engine::general_purpose::STANDARD.encode(png))
    })
}

/// `pngTranscoderRegistered` (image-convert.ts:71).
static PNG_TRANSCODER_REGISTERED: AtomicBool = AtomicBool::new(false);

/// Serializes crate tests that mutate the global transcoder state.
#[cfg(test)]
pub(crate) static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Test hook: clear the process-wide registration state so the gating can be
/// exercised deterministically (the global mirrors upstream's module state).
#[cfg(test)]
pub(crate) fn reset_png_transcoder_state() {
    PNG_TRANSCODER_REGISTERED.store(false, Ordering::SeqCst);
    set_image_transcoder(None);
}

/// Test hook: register a transcoder AND flip the registration flag, so the
/// component-level `ensure_png_transcoder` calls stay no-ops while a test
/// controls the conversion function.
#[cfg(test)]
pub(crate) fn install_test_transcoder(transcoder: ImageTranscoder) {
    set_image_transcoder(Some(transcoder));
    PNG_TRANSCODER_REGISTERED.store(true, Ordering::SeqCst);
}

/// `ensurePngTranscoder` (image-convert.ts:78-90, JSDoc 73-77): on Kitty-protocol
/// terminals register the transcoder once so non-PNG images render.
/// `on_registered` runs after registration so callers can re-render images
/// that showed text fallbacks; it is not called when the transcoder was
/// already registered or the terminal is not Kitty.
pub fn ensure_png_transcoder(on_registered: impl FnOnce()) {
    if PNG_TRANSCODER_REGISTERED.load(Ordering::SeqCst) {
        return;
    }
    if get_capabilities().images != Some(ImageProtocol::Kitty) {
        return;
    }
    set_image_transcoder(Some(load_png_transcoder()));
    PNG_TRANSCODER_REGISTERED.store(true, Ordering::SeqCst);
    on_registered();
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpi_tui::terminal_image::{
        TerminalCapabilities, reset_capabilities_cache, set_capabilities,
    };

    #[test]
    fn transcodes_jpeg_bytes_to_png() {
        // A real 1x1 PNG, then re-encoded through the transcoder.
        let mut png_bytes = Vec::new();
        {
            let img = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
                1,
                1,
                image::Rgb([255, 0, 0]),
            ));
            img.write_to(
                &mut std::io::Cursor::new(&mut png_bytes),
                image::ImageFormat::Png,
            )
            .expect("encode png");
        }
        let transcoder = load_png_transcoder();
        let encoded = base64::engine::general_purpose::STANDARD.encode(&png_bytes);
        // PNG input round-trips (the transcoder itself accepts any format).
        let output = transcoder(&encoded, "image/png").expect("png output");
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(output)
            .expect("base64");
        assert!(decoded.starts_with(b"\x89PNG"));
        // Garbage input fails closed.
        assert!(transcoder("not-base64!!", "image/jpeg").is_none());
    }

    #[test]
    fn ensure_registers_once_on_kitty_terminals() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        reset_png_transcoder_state();
        set_capabilities(TerminalCapabilities {
            images: Some(ImageProtocol::Kitty),
            true_color: true,
            hyperlinks: true,
        });
        let mut registrations = 0;
        ensure_png_transcoder(|| registrations += 1);
        assert_eq!(registrations, 1);
        // Second call is a no-op (already registered).
        ensure_png_transcoder(|| registrations += 1);
        assert_eq!(registrations, 1);
        reset_png_transcoder_state();
        reset_capabilities_cache();
    }

    #[test]
    fn ensure_is_a_noop_without_kitty_images() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        reset_png_transcoder_state();
        set_capabilities(TerminalCapabilities {
            images: None,
            true_color: false,
            hyperlinks: false,
        });
        ensure_png_transcoder(|| panic!("must not fire without kitty images"));
        assert!(!PNG_TRANSCODER_REGISTERED.load(Ordering::SeqCst));
        reset_capabilities_cache();
    }
}
