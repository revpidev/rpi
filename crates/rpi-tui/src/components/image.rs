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

use crate::terminal_image::{
    allocate_image_id, get_capabilities, get_cell_dimensions, get_image_dimensions, image_fallback,
    render_image, ImageDimensions, ImageProtocol, ImageRenderOptions,
};
use crate::tui::Component;

/// Theme callback (upstream `(str: string) => string`).
pub type ImageThemeFn = Box<dyn Fn(&str) -> String + Send + Sync>;

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
        if let Some(cache) = self.cache.borrow().as_ref() {
            if cache.width == width {
                return cache.lines.clone();
            }
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
        let lines: Vec<String> = if let Some(protocol) = caps.images {
            // `caps.images === "kitty" && this.imageId === undefined` —
            // allocate an id for kitty (image.ts:74-76).
            if protocol == ImageProtocol::Kitty && self.image_id.borrow().is_none() {
                *self.image_id.borrow_mut() = Some(allocate_image_id());
            }
            let result = render_image(
                &self.base64_data,
                self.dimensions,
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
                    Some(self.dimensions),
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
    use crate::terminal_image::{reset_capabilities_cache, set_capabilities, TerminalCapabilities};

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
}
