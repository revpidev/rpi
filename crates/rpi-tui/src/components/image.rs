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

use std::cell::{Cell, RefCell};
use std::sync::{Arc, Mutex};

use crate::terminal_image::{
    ImageDimensions, ImageProtocol, ImageRenderOptions, allocate_image_id, capabilities_generation,
    get_capabilities, get_cell_dimensions, get_image_dimensions, get_png_dimensions,
    image_fallback, render_image,
};
use crate::tui::Component;

/// Theme callback (upstream `(str: string) => string`).
pub type ImageThemeFn = Box<dyn Fn(&str) -> String + Send + Sync>;

/// `ImageTranscoder` (image.ts:18 @ a276dabe5, #10292): converts base64
/// image data to base64 PNG data, or returns `None` if it cannot. Called
/// synchronously during rendering.
pub type ImageTranscoder = Arc<dyn Fn(&str, &str) -> Option<String> + Send + Sync>;

/// `imageTranscoder` (image.ts:20): the registered converter.
static IMAGE_TRANSCODER: Mutex<Option<ImageTranscoder>> = Mutex::new(None);

/// 256-bit content digest of the base64 source (V19-04 R1.4.7.1). Four
/// independent 64-bit lanes over one pass; the cache verifies length and a
/// short prefix/suffix on every hit, so a collision cannot serve the wrong
/// image. The digest is computed once in `Image::new`, never in the render
/// lock.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct ContentDigest([u64; 4]);

fn content_digest(data: &[u8]) -> ContentDigest {
    const P1: u64 = 0x9e37_79b9_7f4a_7c15;
    const P2: u64 = 0xc2b2_ae3d_27d4_eb4f;
    const P3: u64 = 0x1656_67b1_9e37_79f9;
    const P4: u64 = 0x27d4_eb2f_1656_67c5;
    let mut lanes = [
        0xcbf2_9ce4_8422_2325,
        0x9e37_79b9_7f4a_7c15,
        0xff51_afd7_ed55_8ccd,
        0xc4ce_b9fe_1a85_ec53,
    ];
    for (index, byte) in data.iter().enumerate() {
        let byte = u64::from(*byte);
        let index = index as u64;
        lanes[0] = (lanes[0] ^ byte.wrapping_add(index)).wrapping_mul(P1);
        lanes[1] = (lanes[1].wrapping_add(byte << 1))
            .wrapping_mul(P2)
            .rotate_left(17);
        lanes[2] = (lanes[2] ^ byte.wrapping_mul(P3)).wrapping_add(index);
        lanes[3] = (lanes[3].wrapping_add(byte << 2) ^ index).wrapping_mul(P4);
    }
    ContentDigest(lanes)
}

/// Digest key plus the short content checks that make a collision harmless.
struct PngKey {
    digest: ContentDigest,
    len: usize,
    prefix: String,
    suffix: String,
    mime_type: String,
}

impl PngKey {
    fn new(base64_data: &str, mime_type: &str) -> Self {
        const CHECK_CHARS: usize = 32;
        let prefix = base64_data
            .get(..CHECK_CHARS.min(base64_data.len()))
            .unwrap_or(base64_data)
            .to_string();
        let suffix = base64_data
            .get(base64_data.len().saturating_sub(CHECK_CHARS)..)
            .unwrap_or(base64_data)
            .to_string();
        Self {
            digest: content_digest(base64_data.as_bytes()),
            len: base64_data.len(),
            prefix,
            suffix,
            mime_type: mime_type.to_string(),
        }
    }

    fn matches(&self, entry: &PngCacheEntry) -> bool {
        entry.digest == self.digest
            && entry.len == self.len
            && entry.mime_type == self.mime_type
            && entry.prefix == self.prefix
            && entry.suffix == self.suffix
    }
}

/// `pngCache` (image.ts:22): backstop for callers that recreate `Image`
/// instances. V19-04 R1.4.7.2: the full base64 source is no longer a map and
/// order key; entries are keyed by digest with prefix/suffix verification and
/// bounded by both the 32-entry count and a total-byte budget (LRU).
struct PngCacheEntry {
    digest: ContentDigest,
    len: usize,
    prefix: String,
    suffix: String,
    mime_type: String,
    value: Option<Arc<str>>,
    bytes: usize,
}

impl PngCacheEntry {
    fn new(key: PngKey, value: Option<Arc<str>>) -> Self {
        let mut entry = Self {
            digest: key.digest,
            len: key.len,
            prefix: key.prefix,
            suffix: key.suffix,
            mime_type: key.mime_type,
            value,
            bytes: 0,
        };
        entry.bytes = entry.retained_bytes();
        entry
    }

    fn retained_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.prefix.len()
            + self.suffix.len()
            + self.mime_type.len()
            + self.value.as_ref().map_or(0, |value| value.len())
    }
}

struct PngCache {
    /// Least recently used first.
    entries: Vec<PngCacheEntry>,
    total_bytes: usize,
}

const PNG_CACHE_MAX_ENTRIES: usize = 32;
const PNG_CACHE_MAX_BYTES: usize = 8 * 1024 * 1024;

static PNG_CACHE: std::sync::LazyLock<Mutex<PngCache>> = std::sync::LazyLock::new(|| {
    Mutex::new(PngCache {
        entries: Vec::new(),
        total_bytes: 0,
    })
});

/// Register the converter used for non-PNG images on Kitty-protocol
/// terminals, which only accept PNG. Without one, such images render as text
/// fallbacks (`setImageTranscoder`, image.ts:28-31 @ a276dabe5).
pub fn set_image_transcoder(transcoder: Option<ImageTranscoder>) {
    *IMAGE_TRANSCODER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = transcoder;
    let mut cache = PNG_CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    cache.entries.clear();
    cache.total_bytes = 0;
}

impl PngCache {
    /// Look up a verified entry and mark it most recently used.
    fn get(&mut self, key: &PngKey) -> Option<Option<Arc<str>>> {
        let position = self.entries.iter().position(|entry| key.matches(entry))?;
        let entry = self.entries.remove(position);
        let value = entry.value.clone();
        self.entries.push(entry);
        Some(value)
    }

    /// Insert an entry and evict least recently used entries until both the
    /// count and the total-byte budget hold.
    fn insert(&mut self, key: PngKey, value: Option<Arc<str>>) {
        let entry = PngCacheEntry::new(key, value);
        // A single entry larger than the whole budget cannot satisfy it;
        // leave it uncached (the caller keeps its own `png_data`).
        if entry.bytes > PNG_CACHE_MAX_BYTES {
            return;
        }
        self.total_bytes = self.total_bytes.saturating_add(entry.bytes);
        self.entries.push(entry);
        while self.entries.len() > PNG_CACHE_MAX_ENTRIES
            || (self.total_bytes > PNG_CACHE_MAX_BYTES && self.entries.len() > 1)
        {
            let evicted = self.entries.remove(0);
            self.total_bytes = self.total_bytes.saturating_sub(evicted.bytes);
        }
    }
}

/// `toPng` (image.ts:33-41): consult the LRU cache, then the transcoder.
fn to_png(key: &PngKey, base64_data: &str, mime_type: &str) -> Option<Arc<str>> {
    let transcoder = IMAGE_TRANSCODER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()?;

    {
        let mut cache = PNG_CACHE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(value) = cache.get(key) {
            return value;
        }
    }

    let value: Option<Arc<str>> = transcoder(base64_data, mime_type).map(Arc::from);

    let mut cache = PNG_CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    cache.insert(
        PngKey {
            digest: key.digest,
            len: key.len,
            prefix: key.prefix.clone(),
            suffix: key.suffix.clone(),
            mime_type: key.mime_type.clone(),
        },
        value.clone(),
    );
    value
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
    base64_data: Arc<str>,
    mime_type: String,
    /// Content digest + collision checks, computed once in `new`
    /// (V19-04 R1.4.7.1) so the render path never hashes the source.
    png_key: PngKey,
    dimensions: ImageDimensions,
    theme: ImageTheme,
    options: ImageOptions,
    image_id: RefCell<Option<u32>>,
    /// Converted PNG data for Kitty. Failures are not stored so a later
    /// transcoder can retry (`pngData`, image.ts:63). Shared with the cache.
    png_data: RefCell<Option<Arc<str>>>,
    cache: RefCell<Option<ImageCache>>,
    /// Monotonic revision for [`Component::cache_revision`] (V19-04 FR-E).
    revision: Cell<u64>,
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
        let base64_data: Arc<str> = Arc::from(base64_data.into().as_str());
        let mime_type = mime_type.into();
        let png_key = PngKey::new(&base64_data, &mime_type);
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
            png_key,
            dimensions,
            theme,
            image_id: RefCell::new(options.image_id),
            options,
            png_data: RefCell::new(None),
            cache: RefCell::new(None),
            revision: Cell::new(0),
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
        // dimensions (conversion may apply EXIF rotation, image.ts:105-110 @
        // a276dabe5, #10292).
        let mut data = Some(Arc::clone(&self.base64_data));
        let mut dimensions = self.dimensions;
        if caps.images == Some(ImageProtocol::Kitty) && self.mime_type != "image/png" {
            if self.png_data.borrow().is_none() {
                *self.png_data.borrow_mut() =
                    to_png(&self.png_key, &self.base64_data, &self.mime_type);
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
        self.revision.set(self.revision.get().wrapping_add(1));
    }

    fn cache_revision(&self) -> Option<u64> {
        // The rendered image depends on the terminal capabilities (protocol)
        // as well as the component's own state.
        Some(self.revision.get().wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ capabilities_generation())
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

    /// Serializes tests that mutate the global capabilities cache. Shared
    /// with the markdown/alt-screen/main-screen suites.
    use crate::terminal_image::TEST_CAPS_LOCK;

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

    // ── V19-04 FR-F: digest identity and byte-budget LRU ──────────────────

    #[test]
    fn content_digest_is_stable_and_content_sensitive() {
        let first = content_digest(b"same bytes");
        assert_eq!(first, content_digest(b"same bytes"));
        assert_ne!(first, content_digest(b"same byteS"));
        assert_ne!(content_digest(b"a"), content_digest(b"b"));
        // 256-bit output: all four lanes participate.
        assert_ne!(
            content_digest(b"0123456789abcdef"),
            content_digest(b"fedcba9876543210")
        );
    }

    #[test]
    fn image_computes_the_digest_at_construction() {
        let image = Image::new(
            JPEG,
            "image/jpeg",
            ImageTheme {
                fallback_color: Box::new(|value: &str| value.to_string()),
            },
            None,
            None,
        );
        assert_eq!(image.png_key.digest, content_digest(JPEG.as_bytes()));
        assert_eq!(image.png_key.len, JPEG.len());
        assert_eq!(image.png_key.mime_type, "image/jpeg");
    }

    #[test]
    fn png_key_collision_check_rejects_different_content() {
        let mut entry = PngCacheEntry::new(PngKey::new("AAAA", "image/jpeg"), None);
        // Same digest/len but a different prefix/suffix must not match.
        entry.prefix = "BBBB".to_string();
        let key = PngKey {
            digest: entry.digest,
            len: entry.len,
            prefix: "AAAA".to_string(),
            suffix: entry.suffix.clone(),
            mime_type: entry.mime_type.clone(),
        };
        assert!(!key.matches(&entry));
        entry.prefix = "AAAA".to_string();
        assert!(key.matches(&entry));
        // A different mime type is also a different conversion input.
        let other_mime = PngKey {
            digest: entry.digest,
            len: entry.len,
            prefix: entry.prefix.clone(),
            suffix: entry.suffix.clone(),
            mime_type: "image/png".to_string(),
        };
        assert!(!other_mime.matches(&entry));
    }

    #[test]
    fn png_cache_enforces_count_and_byte_budget() {
        let mut cache = PngCache {
            entries: Vec::new(),
            total_bytes: 0,
        };
        for index in 0..(PNG_CACHE_MAX_ENTRIES * 2) {
            let source = format!("image-{index}");
            cache.insert(PngKey::new(&source, "image/jpeg"), None);
        }
        assert_eq!(cache.entries.len(), PNG_CACHE_MAX_ENTRIES);

        // A single value larger than the whole budget is not cached, so the
        // total never exceeds the limit.
        let large: Arc<str> = Arc::from("x".repeat(PNG_CACHE_MAX_BYTES + 1024).as_str());
        cache.insert(PngKey::new("large", "image/jpeg"), Some(Arc::clone(&large)));
        assert_eq!(cache.entries.len(), PNG_CACHE_MAX_ENTRIES);
        assert!(cache.total_bytes <= PNG_CACHE_MAX_BYTES);

        // An entry that fits is admitted and evicts older entries until both
        // limits hold.
        let fits: Arc<str> = Arc::from("y".repeat(PNG_CACHE_MAX_BYTES / 2).as_str());
        cache.insert(PngKey::new("fits", "image/jpeg"), Some(Arc::clone(&fits)));
        assert!(!cache.entries.is_empty());
        assert!(cache.total_bytes <= PNG_CACHE_MAX_BYTES);
    }

    #[test]
    fn png_cache_hit_moves_the_entry_to_the_back() {
        let mut cache = PngCache {
            entries: Vec::new(),
            total_bytes: 0,
        };
        for index in 0..PNG_CACHE_MAX_ENTRIES {
            let source = format!("image-{index}");
            cache.insert(PngKey::new(&source, "image/jpeg"), None);
        }
        let first_key = PngKey::new("image-0", "image/jpeg");
        assert!(cache.get(&first_key).is_some());
        // Fill one more: the oldest untouched entry (image-1) is evicted,
        // not the refreshed image-0.
        cache.insert(PngKey::new("new", "image/jpeg"), None);
        assert!(cache.get(&first_key).is_some());
        assert!(cache.get(&PngKey::new("image-1", "image/jpeg")).is_none());
    }
}
