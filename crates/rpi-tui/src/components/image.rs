//! Port of `packages/tui/src/components/image.ts` @ pi 0.82.1 (2efa728).
//!
//! Intentional differences:
//! - The theme callback is a `Box<dyn Fn(&str) -> String + Send + Sync>`
//!   field (upstream plain function).
//! - `ImageOptions.max_width_cells` / `max_height_cells` are `usize` instead
//!   of `number` (upstream fractional cell counts are floored by
//!   `calculateImageCellSize` anyway).
//! - The render cache and the kitty image id use interior mutability
//!   (`RefCell`) because `Component::render` takes `&self`.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use crate::terminal_image::{
    ImageDimensions, ImageProtocol, ImageRenderOptions, allocate_image_id, get_capabilities,
    get_cell_dimensions, get_image_dimensions, get_png_dimensions, image_fallback, render_image,
};
use crate::tui::Component;

/// Theme callback (upstream `(str: string) => string`).
pub type ImageThemeFn = Box<dyn Fn(&str) -> String + Send + Sync>;

/// `ImageTranscoder` (image.ts:26-29 @ a276dabe5, #10292): converts base64
/// image data to base64 PNG data, or returns `None` if it cannot. Called
/// synchronously during rendering.
pub type ImageTranscoder = Arc<dyn Fn(&str, &str) -> Option<String> + Send + Sync>;

/// `imageTranscoder` (image.ts:31): the registered converter.
static IMAGE_TRANSCODER: Mutex<Option<ImageTranscoder>> = Mutex::new(None);

/// `pngCache` (image.ts:33): backstop for callers that recreate `Image`
/// instances. Keyed by source data, least recently used first, capped at 32.
struct PngCache {
    map: HashMap<String, Option<String>>,
    order: VecDeque<String>,
}

static PNG_CACHE: std::sync::LazyLock<Mutex<PngCache>> = std::sync::LazyLock::new(|| {
    Mutex::new(PngCache {
        map: HashMap::new(),
        order: VecDeque::new(),
    })
});

/// Register the converter used for non-PNG images on Kitty-protocol
/// terminals, which only accept PNG. Without one, such images render as text
/// fallbacks (`setImageTranscoder`, image.ts:39-42 @ a276dabe5).
pub fn set_image_transcoder(transcoder: Option<ImageTranscoder>) {
    *IMAGE_TRANSCODER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = transcoder;
    let mut cache = PNG_CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    cache.map.clear();
    cache.order.clear();
}

/// `toPng` (image.ts:44-52): consult the LRU cache, then the transcoder.
fn to_png(base64_data: &str, mime_type: &str) -> Option<String> {
    let transcoder = IMAGE_TRANSCODER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()?;
    let mut cache = PNG_CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let png = match cache.map.get(base64_data) {
        Some(cached) => cached.clone(),
        None => {
            let png = transcoder(base64_data, mime_type);
            cache.map.insert(base64_data.to_owned(), png.clone());
            cache.order.push_back(base64_data.to_owned());
            while cache.order.len() > 32 {
                if let Some(oldest) = cache.order.pop_front() {
                    cache.map.remove(&oldest);
                }
            }
            png
        }
    };
    // Touch the entry so it survives the next eviction.
    if let Some(position) = cache.order.iter().position(|key| key == base64_data) {
        if let Some(key) = cache.order.remove(position) {
            cache.order.push_back(key);
        }
    }
    png
}

/// `ImageTheme` (image.ts:12-14).
pub struct ImageTheme {
    pub fallback_color: ImageThemeFn,
}

/// `ImageOptions` (image.ts:16-22).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ImageOptions {
    pub max_width_cells: Option<usize>,
    pub max_height_cells: Option<usize>,
    pub filename: Option<String>,
    /// Kitty image ID. If provided, reuses this ID (for animations/updates).
    pub image_id: Option<u32>,
}

/// Render cache entry (upstream `cachedLines` / `cachedWidth`, image.ts:32-33).
struct ImageCache {
    width: usize,
    lines: Vec<String>,
}

/// Image component (upstream `Image`, image.ts:24).
pub struct Image {
    base64_data: String,
    mime_type: String,
    dimensions: ImageDimensions,
    theme: ImageTheme,
    options: ImageOptions,
    image_id: RefCell<Option<u32>>,
    /// Converted PNG data for Kitty. Failures are not stored so a later
    /// transcoder can retry (`pngData`, image.ts:31).
    png_data: RefCell<Option<String>>,
    cache: RefCell<Option<ImageCache>>,
}

impl Image {
    pub fn new(
        base64_data: impl Into<String>,
        mime_type: impl Into<String>,
        theme: ImageTheme,
        options: Option<ImageOptions>,
        dimensions: Option<ImageDimensions>,
    ) -> Self {
        let options = options.unwrap_or_default();
        let base64_data = base64_data.into();
        let mime_type = mime_type.into();
        // `getImageDimensions(...) || { widthPx: 800, heightPx: 600 }`
        // (image.ts:46).
        let dimensions = dimensions
            .or_else(|| get_image_dimensions(&base64_data, &mime_type))
            .unwrap_or(ImageDimensions {
                width_px: 800.0,
                height_px: 600.0,
            });
        Self {
            base64_data,
            mime_type,
            dimensions,
            theme,
            image_id: RefCell::new(options.image_id),
            options,
            png_data: RefCell::new(None),
            cache: RefCell::new(None),
        }
    }

    /// Get the Kitty image ID used by this image (if any)
    /// (upstream `getImageId`, image.ts:50-53).
    pub fn get_image_id(&self) -> Option<u32> {
        *self.image_id.borrow()
    }
}

impl Component for Image {
    fn render(&self, width: usize) -> Vec<String> {
        if let Some(cache) = self.cache.borrow().as_ref()
            && cache.width == width
        {
            return cache.lines.clone();
        }

        // `Math.max(1, Math.min(width - 2, this.options.maxWidthCells ?? 60))`
        // (image.ts:65).
        let max_width = width
            .saturating_sub(2)
            .min(self.options.max_width_cells.unwrap_or(60))
            .max(1);
        let cell_dimensions = get_cell_dimensions();
        // `Math.max(1, Math.ceil((maxWidth * cellDimensions.widthPx) /
        // cellDimensions.heightPx))` (image.ts:67).
        let default_max_height = ((max_width as f64 * cell_dimensions.width_px)
            / cell_dimensions.height_px)
            .ceil()
            .max(1.0) as usize;
        let max_height = self.options.max_height_cells.unwrap_or(default_max_height);

        let caps = get_capabilities();
        // Kitty-protocol terminals only accept PNG: convert non-PNG images
        // through the registered transcoder and prefer the PNG's own
        // dimensions (conversion may apply EXIF rotation, image.ts:71-78 @
        // a276dabe5, #10292).
        let mut data = Some(self.base64_data.clone());
        let mut dimensions = self.dimensions;
        if caps.images == Some(ImageProtocol::Kitty) && self.mime_type != "image/png" {
            if self.png_data.borrow().is_none() {
                *self.png_data.borrow_mut() = to_png(&self.base64_data, &self.mime_type);
            }
            data = self.png_data.borrow().clone();
            if let Some(png) = &data
                && let Some(png_dimensions) = get_png_dimensions(png)
            {
                dimensions = png_dimensions;
            }
        }
        let lines: Vec<String> = if let (Some(protocol), Some(data)) = (caps.images, data) {
            // `caps.images === "kitty" && this.imageId === undefined` —
            // allocate an id for kitty (image.ts:74-76).
            if protocol == ImageProtocol::Kitty && self.image_id.borrow().is_none() {
                *self.image_id.borrow_mut() = Some(allocate_image_id());
            }
            let result = render_image(
                &data,
                dimensions,
                &ImageRenderOptions {
                    max_width_cells: Some(max_width as f64),
                    max_height_cells: Some(max_height as f64),
                    image_id: *self.image_id.borrow(),
                    move_cursor: Some(false),
                    ..Default::default()
                },
            );

            if let Some(result) = result {
                // Store the image ID for later cleanup (image.ts:85-88).
                if let Some(result_image_id) = result.image_id {
                    *self.image_id.borrow_mut() = Some(result_image_id);
                }

                if protocol == ImageProtocol::Kitty {
                    // For Kitty: C=1 prevents cursor movement. Return `rows`
                    // lines so the TUI accounts for image height (image.ts:
                    // 90-98).
                    let mut kitty_lines = vec![result.sequence];
                    for _ in 0..result.rows.saturating_sub(1) {
                        kitty_lines.push(String::new());
                    }
                    kitty_lines
                } else {
                    // iTerm2: first (rows-1) lines are empty and cleared
                    // before the image is drawn; the last line moves the
                    // cursor back up, draws the image, then back down so TUI
                    // cursor accounting stays inside the scroll area
                    // (image.ts:100-110).
                    let mut iterm2_lines: Vec<String> = Vec::new();
                    for _ in 0..result.rows.saturating_sub(1) {
                        iterm2_lines.push(String::new());
                    }
                    let row_offset = result.rows.saturating_sub(1);
                    let move_up = if row_offset > 0 {
                        format!("\x1b[{row_offset}A")
                    } else {
                        String::new()
                    };
                    iterm2_lines.push(move_up + &result.sequence);
                    iterm2_lines
                }
            } else {
                let fallback = image_fallback(
                    &self.mime_type,
                    Some(dimensions),
                    self.options.filename.as_deref(),
                );
                vec![(self.theme.fallback_color)(&fallback)]
            }
        } else {
            let fallback = image_fallback(
                &self.mime_type,
                Some(self.dimensions),
                self.options.filename.as_deref(),
            );
            vec![(self.theme.fallback_color)(&fallback)]
        };

        *self.cache.borrow_mut() = Some(ImageCache {
            width,
            lines: lines.clone(),
        });

        lines
    }

    fn invalidate(&mut self) {
        *self.cache.borrow_mut() = None;
    }
}

#[cfg(test)]
mod tests {
    //! `image-test.ts` is a manual screenshot harness upstream, not a test
    //! suite; these cover the component contract (kitty sequence lines,
    //! iTerm2 layout, fallback text, caching).

    use super::*;
    use crate::terminal_image::{
        CellDimensions, TerminalCapabilities, reset_capabilities_cache, set_capabilities,
        set_cell_dimensions,
    };

    /// Serializes tests that mutate the global capabilities cache.
    static TEST_CAPS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn kitty_caps() -> TerminalCapabilities {
        TerminalCapabilities {
            images: Some(ImageProtocol::Kitty),
            true_color: true,
            hyperlinks: true,
        }
    }

    fn iterm2_caps() -> TerminalCapabilities {
        TerminalCapabilities {
            images: Some(ImageProtocol::ITerm2),
            true_color: true,
            hyperlinks: true,
        }
    }

    fn no_image_caps() -> TerminalCapabilities {
        TerminalCapabilities {
            images: None,
            true_color: false,
            hyperlinks: false,
        }
    }

    fn image() -> Image {
        Image::new(
            "iVBORw0KGgo=",
            "image/png",
            ImageTheme {
                fallback_color: Box::new(|text: &str| format!("\x1b[33m{text}\x1b[0m")),
            },
            None,
            Some(ImageDimensions {
                width_px: 800.0,
                height_px: 600.0,
            }),
        )
    }

    #[test]
    fn renders_kitty_sequence_with_height_rows() {
        let _guard = TEST_CAPS_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        set_capabilities(kitty_caps());
        let image = image();
        let lines = image.render(80);
        reset_capabilities_cache();

        // Default cell 9x18, image 800x600, maxWidth=60: the aspect ratio
        // dominates (ceil(600 * (60*9/800) / 18) = 23 rows).
        assert!(lines[0].starts_with("\x1b_G"), "kitty sequence expected");
        assert!(lines[0].contains("c=60"));
        assert!(lines[0].contains("r=23"));
        assert_eq!(lines.len(), 23);
        assert!(lines[1..].iter().all(|line| line.is_empty()));

        // A kitty image id was allocated and exposed.
        assert!(image.get_image_id().is_some());
    }

    #[test]
    fn reuses_provided_kitty_image_id() {
        let _guard = TEST_CAPS_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        set_capabilities(kitty_caps());
        let image = Image::new(
            "iVBORw0KGgo=",
            "image/png",
            ImageTheme {
                fallback_color: Box::new(|text: &str| text.to_string()),
            },
            Some(ImageOptions {
                image_id: Some(42),
                ..Default::default()
            }),
            Some(ImageDimensions {
                width_px: 800.0,
                height_px: 600.0,
            }),
        );
        let lines = image.render(80);
        reset_capabilities_cache();
        assert!(lines[0].contains("i=42"), "must reuse the provided id");
        assert_eq!(image.get_image_id(), Some(42));
    }

    #[test]
    fn renders_iterm2_layout_with_cursor_move_up() {
        let _guard = TEST_CAPS_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        set_capabilities(iterm2_caps());
        let image = image();
        let lines = image.render(80);
        reset_capabilities_cache();

        // 23 rows: 22 empty lines, then move-up + sequence.
        assert_eq!(lines.len(), 23);
        assert!(lines[..22].iter().all(|line| line.is_empty()));
        assert!(
            lines[22].starts_with("\x1b[22A\x1b]1337;File="),
            "got: {:?}",
            lines[22]
        );
    }

    #[test]
    fn renders_fallback_text_without_image_support() {
        let _guard = TEST_CAPS_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        set_capabilities(no_image_caps());
        let image = Image::new(
            "iVBORw0KGgo=",
            "image/png",
            ImageTheme {
                fallback_color: Box::new(|text: &str| format!("\x1b[33m{text}\x1b[0m")),
            },
            Some(ImageOptions {
                filename: Some("photo.png".to_string()),
                ..Default::default()
            }),
            Some(ImageDimensions {
                width_px: 800.0,
                height_px: 600.0,
            }),
        );
        let lines = image.render(80);
        reset_capabilities_cache();
        assert_eq!(lines.len(), 1);
        assert_eq!(
            lines[0],
            "\x1b[33m[Image: photo.png [image/png] 800x600]\x1b[0m"
        );
    }

    #[test]
    fn falls_back_to_default_dimensions_when_unknown() {
        // Invalid base64 → getImageDimensions returns None → 800x600.
        let _guard = TEST_CAPS_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        set_capabilities(no_image_caps());
        let image = Image::new(
            "not-base64!!",
            "image/png",
            ImageTheme {
                fallback_color: Box::new(|text: &str| text.to_string()),
            },
            None,
            None,
        );
        let lines = image.render(80);
        reset_capabilities_cache();
        assert_eq!(lines[0], "[Image: [image/png] 800x600]");
    }

    #[test]
    fn caches_render_per_width() {
        let _guard = TEST_CAPS_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        set_capabilities(kitty_caps());
        let mut image = image();
        let at_80 = image.render(80);
        assert_eq!(image.render(80), at_80, "same width must be cached");
        let at_40 = image.render(40);
        assert_ne!(at_40, at_80, "different width must re-render");
        image.invalidate();
        assert_eq!(image.render(80), at_80);
        reset_capabilities_cache();
    }

    #[test]
    fn respects_max_width_and_height_options() {
        let _guard = TEST_CAPS_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        set_capabilities(kitty_caps());
        let image = Image::new(
            "iVBORw0KGgo=",
            "image/png",
            ImageTheme {
                fallback_color: Box::new(|text: &str| text.to_string()),
            },
            Some(ImageOptions {
                max_width_cells: Some(10),
                max_height_cells: Some(4),
                ..Default::default()
            }),
            Some(ImageDimensions {
                width_px: 800.0,
                height_px: 600.0,
            }),
        );
        let lines = image.render(80);
        reset_capabilities_cache();
        assert!(lines[0].contains("c=10"));
        assert!(lines[0].contains("r=4"));
        assert_eq!(lines.len(), 4);
    }

    // terminal-image.test.ts `Image transcoding` (#10292, a276dabe5):
    // Kitty only accepts PNG (f=100); non-PNG images are transcoded.
    const JPEG: &str = "anBlZw==";
    /// Minimal PNG header (signature + IHDR) for a 40x10 image.
    const PNG_40X10: &str = "iVBORw0KGgoAAAANSUhEUgAAACgAAAAK";

    fn counting_transcoder(calls: &Arc<std::sync::Mutex<Vec<String>>>) -> super::ImageTranscoder {
        let calls = Arc::clone(calls);
        Arc::new(move |data: &str, _mime_type: &str| {
            calls
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .push(data.to_string());
            if data == JPEG {
                Some(PNG_40X10.to_string())
            } else {
                None
            }
        })
    }

    fn render_with(
        data: &str,
        mime_type: &str,
        dimensions: Option<ImageDimensions>,
    ) -> Vec<String> {
        Image::new(
            data,
            mime_type,
            ImageTheme {
                fallback_color: Box::new(|text: &str| text.to_string()),
            },
            None,
            dimensions,
        )
        .render(20)
    }

    #[test]
    fn transcodes_non_png_kitty_images_and_sizes_from_the_png() {
        let _guard = TEST_CAPS_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        set_capabilities(kitty_caps());
        set_cell_dimensions(CellDimensions {
            width_px: 10.0,
            height_px: 10.0,
        });
        set_image_transcoder(Some(counting_transcoder(&Arc::new(std::sync::Mutex::new(
            Vec::new(),
        )))));
        let lines = render_with(
            JPEG,
            "image/jpeg",
            Some(ImageDimensions {
                width_px: 20.0,
                height_px: 20.0,
            }),
        );
        set_image_transcoder(None);
        set_cell_dimensions(CellDimensions::default());
        reset_capabilities_cache();

        assert!(
            lines[0].contains("f=100") && lines[0].contains(&format!(";{PNG_40X10}\x1b\\")),
            "got: {:?}",
            lines[0]
        );
        // 40x10 PNG at 18 columns: 5 rows, not the 18 rows of the 20x20
        // source dimensions.
        assert_eq!(lines.len(), 5);
    }

    #[test]
    fn renders_a_text_fallback_until_a_working_transcoder_is_registered() {
        let _guard = TEST_CAPS_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        set_capabilities(kitty_caps());
        let mut image = Image::new(
            JPEG,
            "image/jpeg",
            ImageTheme {
                fallback_color: Box::new(|value: &str| value.to_string()),
            },
            None,
            None,
        );
        assert!(image.render(80)[0].starts_with("[Image: [image/jpeg]"));
        set_image_transcoder(Some(Arc::new(|_data: &str, _mime_type: &str| None)));
        image.invalidate();
        assert!(image.render(80)[0].starts_with("[Image: [image/jpeg]"));
        set_image_transcoder(Some(Arc::new(|_data: &str, _mime_type: &str| {
            Some(PNG_40X10.to_string())
        })));
        image.invalidate();
        let lines = image.render(80);
        set_image_transcoder(None);
        reset_capabilities_cache();
        assert!(lines[0].contains("\x1b_G"), "got: {:?}", lines[0]);
    }

    #[test]
    fn converts_each_image_once() {
        let _guard = TEST_CAPS_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        set_capabilities(kitty_caps());
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        set_image_transcoder(Some(counting_transcoder(&calls)));
        let mut image = Image::new(
            JPEG,
            "image/jpeg",
            ImageTheme {
                fallback_color: Box::new(|value: &str| value.to_string()),
            },
            None,
            None,
        );
        image.render(80);
        // New instance hits the shared cache.
        render_with(JPEG, "image/jpeg", None);
        for index in 0..40 {
            render_with(&format!("other-{index}"), "image/jpeg", None);
        }
        image.invalidate();
        image.render(40);
        set_image_transcoder(None);
        reset_capabilities_cache();
        let jpeg_calls = calls
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .filter(|data| data.as_str() == JPEG)
            .count();
        assert_eq!(jpeg_calls, 1, "the instance and shared cache convert once");
    }

    #[test]
    fn does_not_convert_png_data_or_iterm2_output() {
        let _guard = TEST_CAPS_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        set_image_transcoder(Some(counting_transcoder(&calls)));
        set_capabilities(kitty_caps());
        let png_lines = render_with(PNG_40X10, "image/png", None);
        assert!(
            png_lines[0].contains(&format!(";{PNG_40X10}\x1b\\")),
            "PNG data passes through: {:?}",
            png_lines[0]
        );
        set_capabilities(iterm2_caps());
        let iterm2_lines = render_with(JPEG, "image/jpeg", None);
        set_image_transcoder(None);
        reset_capabilities_cache();
        assert!(
            iterm2_lines
                .last()
                .is_some_and(|line| line.ends_with(&format!(":{JPEG}\x07"))),
            "iTerm2 keeps the source data: {:?}",
            iterm2_lines.last()
        );
        assert!(
            calls.lock().unwrap_or_else(|p| p.into_inner()).is_empty(),
            "no conversion for PNG data or iTerm2 output"
        );
    }
}
