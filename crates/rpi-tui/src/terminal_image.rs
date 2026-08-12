//! Terminal image support: Kitty/iTerm2 graphics sequences, terminal
//! capability detection, cell/image sizing (terminal-image.ts).
//!
//! Port of `packages/tui/src/terminal-image.ts` @ pi 0.82.1 (2efa728), with
//! `detect_capabilities_with` tracking the 4181f66 revision (fa07e7bd9:
//! Windows consoles fall back to truecolor when no terminal is positively
//! identified).
//!
//! Intentional differences:
//! - `probeTmuxHyperlinks` runs `tmux display-message` through `try_wait`
//!   polling with the same 250 ms budget as upstream's `execSync` timeout and
//!   kills the child when it is exceeded; a stuck tmux falls back to `false`
//!   exactly like upstream.
//! - `allocateImageId` mixes `RandomState` (seeded from OS entropy) with a
//!   counter instead of `Math.random()`; same range and distribution, no RNG
//!   dependency.
//! - Base64 decoding in `get*Dimensions` uses the strict `base64` crate
//!   engine; Node's `Buffer.from(_, "base64")` silently ignores non-alphabet
//!   characters. Clean base64 input behaves identically; invalid input yields
//!   `None` instead of a best-effort parse.
//! - `encodeITerm2` models upstream's `number | string` `width`/`height`
//!   parameters as `Option<String>`; callers format numbers themselves.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use base64::Engine;

/// `ImageProtocol` (terminal-image.ts:3): `"kitty" | "iterm2" | null`; the
/// `null` case is represented by `TerminalCapabilities::images` being `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageProtocol {
    Kitty,
    ITerm2,
}

/// `TerminalCapabilities` (terminal-image.ts:5-9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalCapabilities {
    pub images: Option<ImageProtocol>,
    pub true_color: bool,
    pub hyperlinks: bool,
}

/// `CellDimensions` (terminal-image.ts:11-14).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CellDimensions {
    pub width_px: f64,
    pub height_px: f64,
}

impl CellDimensions {
    /// Upstream default `{ widthPx: 9, heightPx: 18 }` (terminal-image.ts:33-34).
    pub const DEFAULT: CellDimensions = CellDimensions {
        width_px: 9.0,
        height_px: 18.0,
    };
}

impl Default for CellDimensions {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// `ImageDimensions` (terminal-image.ts:16-19).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ImageDimensions {
    pub width_px: f64,
    pub height_px: f64,
}

/// `ImageRenderOptions` (terminal-image.ts:21-29).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ImageRenderOptions {
    pub max_width_cells: Option<f64>,
    pub max_height_cells: Option<f64>,
    pub preserve_aspect_ratio: Option<bool>,
    /// Kitty image ID. If provided, reuses/replaces existing image with this ID.
    pub image_id: Option<u32>,
    /// Whether Kitty should apply its default cursor movement after placement.
    pub move_cursor: Option<bool>,
}

// Default cell dimensions — updated by TUI when terminal responds to query
// (terminal-image.ts:33-34).
static CELL_DIMENSIONS: Mutex<CellDimensions> = Mutex::new(CellDimensions::DEFAULT);

static CACHED_CAPABILITIES: Mutex<Option<TerminalCapabilities>> = Mutex::new(None);

fn lock_cell_dimensions() -> std::sync::MutexGuard<'static, CellDimensions> {
    CELL_DIMENSIONS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn lock_cached_capabilities() -> std::sync::MutexGuard<'static, Option<TerminalCapabilities>> {
    CACHED_CAPABILITIES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// `getCellDimensions` (terminal-image.ts:36-38).
pub fn get_cell_dimensions() -> CellDimensions {
    *lock_cell_dimensions()
}

/// `setCellDimensions` (terminal-image.ts:40-42).
pub fn set_cell_dimensions(dims: CellDimensions) {
    *lock_cell_dimensions() = dims;
}

fn env_var(key: &str) -> String {
    std::env::var(key).unwrap_or_default()
}

/// JS truthiness of `process.env[key]`: set to a non-empty string.
fn env_flag(key: &str) -> bool {
    std::env::var(key).is_ok_and(|v| !v.is_empty())
}

/// `probeTmuxHyperlinks` (terminal-image.ts:44-63): checks whether the attached
/// tmux client forwards OSC 8 hyperlinks to the outer terminal. tmux only
/// re-emits them when its `client_termfeatures` lists `hyperlinks`, and strips
/// them otherwise. On any error falls back to `false`.
fn probe_tmux_hyperlinks() -> bool {
    let Ok(mut child) = std::process::Command::new("tmux")
        .args(["display-message", "-p", "#{client_termfeatures}"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };

    // Upstream `execSync(..., { timeout: 250 })` (terminal-image.ts:51-55);
    // poll `try_wait` so a stuck tmux is killed after the same budget.
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(250);
    let output = loop {
        match child.try_wait() {
            Ok(Some(_)) => break child.wait_with_output(),
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return false;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(_) => {
                // Reap the child before returning, like the deadline branch.
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    };
    let Ok(output) = output else {
        return false;
    };
    if !output.status.success() {
        return false;
    }

    // `termfeatures.split(",").map(trim).includes("hyperlinks")`
    // (terminal-image.ts:56-59).
    String::from_utf8_lossy(&output.stdout)
        .split(',')
        .map(str::trim)
        .any(|feature| feature == "hyperlinks")
}

/// `detectCapabilities()` (terminal-image.ts:65) with the default
/// `probeTmuxHyperlinks` probe.
pub fn detect_capabilities() -> TerminalCapabilities {
    detect_capabilities_with(probe_tmux_hyperlinks)
}

/// `detectCapabilities(tmuxForwardsHyperlink)` (terminal-image.ts:65-133
/// @ 4181f66).
pub fn detect_capabilities_with(
    tmux_forwards_hyperlink: impl Fn() -> bool,
) -> TerminalCapabilities {
    // `isWindowsConsole = process.platform === "win32"` (terminal-image.ts:74).
    detect_capabilities_inner(cfg!(windows), tmux_forwards_hyperlink)
}

/// Body of `detectCapabilities` with the platform check explicit so tests can
/// exercise the Windows branch (upstream mocks `process.platform`).
fn detect_capabilities_inner(
    is_windows_console: bool,
    tmux_forwards_hyperlink: impl Fn() -> bool,
) -> TerminalCapabilities {
    let term_program = env_var("TERM_PROGRAM").to_lowercase();
    let terminal_emulator = env_var("TERMINAL_EMULATOR").to_lowercase();
    let term = env_var("TERM").to_lowercase();
    let color_term = env_var("COLORTERM").to_lowercase();
    let has_true_color_hint = color_term == "truecolor" || color_term == "24bit";

    // Emit OSC 8 hyperlinks only when tmux confirms it forwards.
    // Image protocols are unreliable under tmux, so leave `images: null`.
    if env_flag("TMUX") || term.starts_with("tmux") {
        return TerminalCapabilities {
            images: None,
            true_color: has_true_color_hint,
            hyperlinks: tmux_forwards_hyperlink(),
        };
    }

    // screen does not forward OSC 8 hyperlinks, so keep them off there.
    if term.starts_with("screen") {
        return TerminalCapabilities {
            images: None,
            true_color: has_true_color_hint,
            hyperlinks: false,
        };
    }

    if env_flag("KITTY_WINDOW_ID") || term_program == "kitty" {
        return TerminalCapabilities {
            images: Some(ImageProtocol::Kitty),
            true_color: true,
            hyperlinks: true,
        };
    }

    if term_program == "ghostty" || term.contains("ghostty") || env_flag("GHOSTTY_RESOURCES_DIR") {
        return TerminalCapabilities {
            images: Some(ImageProtocol::Kitty),
            true_color: true,
            hyperlinks: true,
        };
    }

    if env_flag("WEZTERM_PANE") || term_program == "wezterm" {
        return TerminalCapabilities {
            images: Some(ImageProtocol::Kitty),
            true_color: true,
            hyperlinks: true,
        };
    }

    // Warp supports the Kitty graphics protocol and OSC 8 hyperlinks.
    if term_program == "warpterminal"
        || env_flag("WARP_SESSION_ID")
        || env_flag("WARP_TERMINAL_SESSION_UUID")
    {
        return TerminalCapabilities {
            images: Some(ImageProtocol::Kitty),
            true_color: true,
            hyperlinks: true,
        };
    }

    if env_flag("ITERM_SESSION_ID") || term_program == "iterm.app" {
        return TerminalCapabilities {
            images: Some(ImageProtocol::ITerm2),
            true_color: true,
            hyperlinks: true,
        };
    }

    if env_flag("WT_SESSION") {
        return TerminalCapabilities {
            images: None,
            true_color: true,
            hyperlinks: true,
        };
    }

    if term_program == "vscode" {
        return TerminalCapabilities {
            images: None,
            true_color: true,
            hyperlinks: true,
        };
    }

    if term_program == "alacritty" {
        return TerminalCapabilities {
            images: None,
            true_color: true,
            hyperlinks: true,
        };
    }

    if terminal_emulator == "jetbrains-jediterm" {
        return TerminalCapabilities {
            images: None,
            true_color: true,
            hyperlinks: false,
        };
    }

    // Windows Terminal does not always set WT_SESSION, for example when it
    // hosts a cmd.exe launched directly from Win+R. Modern Windows consoles
    // support truecolor; keep hyperlinks off unless we positively detected
    // support above (terminal-image.ts:124-130 @ 4181f66, fa07e7bd9).
    if is_windows_console {
        return TerminalCapabilities {
            images: None,
            true_color: true,
            hyperlinks: false,
        };
    }

    // Unknown terminal: be conservative. OSC 8 is rendered invisibly as "just
    // text" on terminals that swallow it, which means the URL disappears from
    // the rendered output. Default to the legacy `text (url)` behavior unless we
    // have positively identified a hyperlink-capable terminal above.
    TerminalCapabilities {
        images: None,
        true_color: has_true_color_hint,
        hyperlinks: false,
    }
}

/// `getCapabilities` (terminal-image.ts:127-132).
pub fn get_capabilities() -> TerminalCapabilities {
    let mut cache = lock_cached_capabilities();
    *cache.get_or_insert_with(detect_capabilities)
}

/// `resetCapabilitiesCache` (terminal-image.ts:134-136).
pub fn reset_capabilities_cache() {
    *lock_cached_capabilities() = None;
}

/// `setCapabilities` (terminal-image.ts:138-141): override the cached
/// capabilities. Useful in tests to exercise both code paths.
pub fn set_capabilities(caps: TerminalCapabilities) {
    *lock_cached_capabilities() = Some(caps);
}

const KITTY_PREFIX: &str = "\x1b_G";
const ITERM2_PREFIX: &str = "\x1b]1337;File=";

/// `isImageLine` (terminal-image.ts:146-153).
pub fn is_image_line(line: &str) -> bool {
    // Fast path: sequence at line start (single-row images)
    if line.starts_with(KITTY_PREFIX) || line.starts_with(ITERM2_PREFIX) {
        return true;
    }
    // Slow path: sequence elsewhere (multi-row images have cursor-up prefix)
    line.contains(KITTY_PREFIX) || line.contains(ITERM2_PREFIX)
}

/// `allocateImageId` (terminal-image.ts:155-163): random ID in range
/// [1, 0xfffffffe] to avoid collisions between different module instances
/// (e.g., main app vs extensions).
pub fn allocate_image_id() -> u32 {
    let counter = IMAGE_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos() as u64);
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u64(counter);
    hasher.write_u64(nanos);
    // `Math.floor(Math.random() * 0xfffffffe) + 1` (terminal-image.ts:162).
    hasher.finish() as u32 % 0xffff_fffe + 1
}

static IMAGE_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Options for `encodeKitty` (terminal-image.ts:165-174).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct KittyEncodeOptions {
    pub columns: Option<u32>,
    pub rows: Option<u32>,
    /// Kitty image ID. If provided, reuses/replaces existing image with this ID.
    pub image_id: Option<u32>,
    /// Whether Kitty should apply its default cursor movement after placement. Default: true.
    pub move_cursor: Option<bool>,
}

const KITTY_CHUNK_SIZE: usize = 4096;

/// `encodeKitty` (terminal-image.ts:165-209).
pub fn encode_kitty(base64_data: &str, options: &KittyEncodeOptions) -> String {
    let mut params: Vec<String> = vec!["a=T".into(), "f=100".into(), "q=2".into()];

    if options.move_cursor == Some(false) {
        params.push("C=1".into());
    }
    // Upstream uses JS truthiness (`if (options.columns)`, ...), so zero is
    // omitted (terminal-image.ts:180-182).
    if let Some(columns) = options.columns {
        if columns != 0 {
            params.push(format!("c={columns}"));
        }
    }
    if let Some(rows) = options.rows {
        if rows != 0 {
            params.push(format!("r={rows}"));
        }
    }
    if let Some(image_id) = options.image_id {
        if image_id != 0 {
            params.push(format!("i={image_id}"));
        }
    }
    let params = params.join(",");

    if base64_data.len() <= KITTY_CHUNK_SIZE {
        return format!("\x1b_G{params};{base64_data}\x1b\\");
    }

    let mut chunks = String::new();
    let mut offset = 0;
    let mut is_first = true;
    while offset < base64_data.len() {
        let end = (offset + KITTY_CHUNK_SIZE).min(base64_data.len());
        // Base64 data is ASCII, so chunk boundaries are always char-aligned;
        // the `unwrap_or_default` keeps the function total for invalid input.
        let chunk = base64_data.get(offset..end).unwrap_or_default();
        let is_last = end >= base64_data.len();

        if is_first {
            chunks.push_str(&format!("\x1b_G{params},m=1;{chunk}\x1b\\"));
            is_first = false;
        } else if is_last {
            chunks.push_str(&format!("\x1b_Gm=0;{chunk}\x1b\\"));
        } else {
            chunks.push_str(&format!("\x1b_Gm=1;{chunk}\x1b\\"));
        }

        offset = end;
    }

    chunks
}

/// `deleteKittyImage` (terminal-image.ts:211-217). Uses uppercase 'I' to also
/// free the image data.
pub fn delete_kitty_image(image_id: u32) -> String {
    format!("\x1b_Ga=d,d=I,i={image_id},q=2\x1b\\")
}

/// `deleteAllKittyImages` (terminal-image.ts:219-225). Uses uppercase 'A' to
/// also free the image data.
pub fn delete_all_kitty_images() -> String {
    "\x1b_Ga=d,d=A,q=2\x1b\\".to_string()
}

/// Options for `encodeITerm2` (terminal-image.ts:227-236). Upstream's
/// `width`/`height` are `number | string`; numbers are formatted by the caller.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ITerm2EncodeOptions {
    pub width: Option<String>,
    pub height: Option<String>,
    pub name: Option<String>,
    pub preserve_aspect_ratio: Option<bool>,
    pub inline: Option<bool>,
}

/// `encodeITerm2` (terminal-image.ts:227-250).
pub fn encode_iterm2(base64_data: &str, options: &ITerm2EncodeOptions) -> String {
    let inline = if options.inline != Some(false) {
        "inline=1"
    } else {
        "inline=0"
    };
    let mut params: Vec<String> = vec![inline.to_string()];

    if let Some(width) = &options.width {
        params.push(format!("width={width}"));
    }
    if let Some(height) = &options.height {
        params.push(format!("height={height}"));
    }
    // `if (options.name)` — empty names are omitted (terminal-image.ts:241-244).
    if let Some(name) = options.name.as_deref().filter(|n| !n.is_empty()) {
        params.push(format!("name={}", base64_encode(name)));
    }
    if options.preserve_aspect_ratio == Some(false) {
        params.push("preserveAspectRatio=0".into());
    }

    format!("\x1b]1337;File={}:{base64_data}\x07", params.join(";"))
}

/// `Buffer.from(name, "utf8").toString("base64")` (terminal-image.ts:242).
fn base64_encode(name: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(name)
}

/// `ImageCellSize` (terminal-image.ts:252-255).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageCellSize {
    pub columns: u32,
    pub rows: u32,
}

/// `calculateImageCellSize` (terminal-image.ts:257-281). Mirror upstream's
/// f64 arithmetic exactly so results match `Math.floor`/`Math.ceil` behavior.
pub fn calculate_image_cell_size(
    image_dimensions: ImageDimensions,
    max_width_cells: f64,
    max_height_cells: Option<f64>,
    cell_dimensions: CellDimensions,
) -> ImageCellSize {
    let max_width = max_width_cells.floor().max(1.0);
    let max_height = max_height_cells.map(|h| h.floor().max(1.0));
    let image_width = image_dimensions.width_px.max(1.0);
    let image_height = image_dimensions.height_px.max(1.0);

    let width_scale = (max_width * cell_dimensions.width_px) / image_width;
    let height_scale = match max_height {
        Some(h) => (h * cell_dimensions.height_px) / image_height,
        None => width_scale,
    };
    let scale = width_scale.min(height_scale);

    let scaled_width_px = image_width * scale;
    let scaled_height_px = image_height * scale;
    let columns = (scaled_width_px / cell_dimensions.width_px).ceil();
    let rows = (scaled_height_px / cell_dimensions.height_px).ceil();

    ImageCellSize {
        columns: columns.clamp(1.0, max_width) as u32,
        rows: match max_height {
            Some(h) => rows.clamp(1.0, h) as u32,
            None => rows.max(1.0) as u32,
        },
    }
}

/// `calculateImageRows` (terminal-image.ts:283-289).
pub fn calculate_image_rows(
    image_dimensions: ImageDimensions,
    target_width_cells: f64,
    cell_dimensions: CellDimensions,
) -> u32 {
    calculate_image_cell_size(image_dimensions, target_width_cells, None, cell_dimensions).rows
}

/// `Buffer.from(base64Data, "base64")` (terminal-image.ts:293). Strict engine
/// vs. Node's lenient decoder — see module-level Intentional differences.
fn decode_base64(base64_data: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::STANDARD
        .decode(base64_data)
        .ok()
}

/// `getPngDimensions` (terminal-image.ts:291-310).
pub fn get_png_dimensions(base64_data: &str) -> Option<ImageDimensions> {
    let buffer = decode_base64(base64_data)?;

    if buffer.len() < 24 {
        return None;
    }

    if buffer[0] != 0x89 || buffer[1] != 0x50 || buffer[2] != 0x4e || buffer[3] != 0x47 {
        return None;
    }

    let width = u32::from_be_bytes([buffer[16], buffer[17], buffer[18], buffer[19]]);
    let height = u32::from_be_bytes([buffer[20], buffer[21], buffer[22], buffer[23]]);

    Some(ImageDimensions {
        width_px: width as f64,
        height_px: height as f64,
    })
}

/// `getJpegDimensions` (terminal-image.ts:312-353).
pub fn get_jpeg_dimensions(base64_data: &str) -> Option<ImageDimensions> {
    let buffer = decode_base64(base64_data)?;

    if buffer.len() < 2 {
        return None;
    }

    if buffer[0] != 0xff || buffer[1] != 0xd8 {
        return None;
    }

    let mut offset = 2usize;
    while offset + 9 < buffer.len() {
        if buffer[offset] != 0xff {
            offset += 1;
            continue;
        }

        let marker = buffer[offset + 1];

        if (0xc0..=0xc2).contains(&marker) {
            let height = u16::from_be_bytes([buffer[offset + 5], buffer[offset + 6]]);
            let width = u16::from_be_bytes([buffer[offset + 7], buffer[offset + 8]]);
            return Some(ImageDimensions {
                width_px: width as f64,
                height_px: height as f64,
            });
        }

        if offset + 3 >= buffer.len() {
            return None;
        }
        let length = u16::from_be_bytes([buffer[offset + 2], buffer[offset + 3]]);
        if length < 2 {
            return None;
        }
        offset += 2 + length as usize;
    }

    None
}

/// `getGifDimensions` (terminal-image.ts:355-375).
pub fn get_gif_dimensions(base64_data: &str) -> Option<ImageDimensions> {
    let buffer = decode_base64(base64_data)?;

    if buffer.len() < 10 {
        return None;
    }

    if &buffer[..6] != b"GIF87a" && &buffer[..6] != b"GIF89a" {
        return None;
    }

    let width = u16::from_le_bytes([buffer[6], buffer[7]]);
    let height = u16::from_le_bytes([buffer[8], buffer[9]]);

    Some(ImageDimensions {
        width_px: width as f64,
        height_px: height as f64,
    })
}

/// `getWebpDimensions` (terminal-image.ts:377-414).
pub fn get_webp_dimensions(base64_data: &str) -> Option<ImageDimensions> {
    let buffer = decode_base64(base64_data)?;

    if buffer.len() < 30 {
        return None;
    }

    if &buffer[..4] != b"RIFF" || &buffer[8..12] != b"WEBP" {
        return None;
    }

    let chunk = &buffer[12..16];
    if chunk == b"VP8 " {
        let width = u16::from_le_bytes([buffer[26], buffer[27]]) & 0x3fff;
        let height = u16::from_le_bytes([buffer[28], buffer[29]]) & 0x3fff;
        return Some(ImageDimensions {
            width_px: width as f64,
            height_px: height as f64,
        });
    } else if chunk == b"VP8L" {
        let bits = u32::from_le_bytes([buffer[21], buffer[22], buffer[23], buffer[24]]);
        let width = (bits & 0x3fff) + 1;
        let height = ((bits >> 14) & 0x3fff) + 1;
        return Some(ImageDimensions {
            width_px: width as f64,
            height_px: height as f64,
        });
    } else if chunk == b"VP8X" {
        let width =
            (u32::from(buffer[24]) | u32::from(buffer[25]) << 8 | u32::from(buffer[26]) << 16) + 1;
        let height =
            (u32::from(buffer[27]) | u32::from(buffer[28]) << 8 | u32::from(buffer[29]) << 16) + 1;
        return Some(ImageDimensions {
            width_px: width as f64,
            height_px: height as f64,
        });
    }

    None
}

/// `getImageDimensions` (terminal-image.ts:416-430).
pub fn get_image_dimensions(base64_data: &str, mime_type: &str) -> Option<ImageDimensions> {
    match mime_type {
        "image/png" => get_png_dimensions(base64_data),
        "image/jpeg" => get_jpeg_dimensions(base64_data),
        "image/gif" => get_gif_dimensions(base64_data),
        "image/webp" => get_webp_dimensions(base64_data),
        _ => None,
    }
}

/// Result of `renderImage` (terminal-image.ts:432-434).
#[derive(Debug, Clone, PartialEq)]
pub struct RenderImageResult {
    pub sequence: String,
    pub rows: u32,
    pub image_id: Option<u32>,
}

/// `renderImage` (terminal-image.ts:432-466).
pub fn render_image(
    base64_data: &str,
    image_dimensions: ImageDimensions,
    options: &ImageRenderOptions,
) -> Option<RenderImageResult> {
    let caps = get_capabilities();

    // `if (!caps.images) return null` (terminal-image.ts:439-441).
    caps.images?;

    // `options.maxWidthCells ?? 80` (terminal-image.ts:443).
    let max_width = options.max_width_cells.unwrap_or(80.0);
    let size = calculate_image_cell_size(
        image_dimensions,
        max_width,
        options.max_height_cells,
        get_cell_dimensions(),
    );

    if caps.images == Some(ImageProtocol::Kitty) {
        let sequence = encode_kitty(
            base64_data,
            &KittyEncodeOptions {
                columns: Some(size.columns),
                rows: Some(size.rows),
                image_id: options.image_id,
                move_cursor: options.move_cursor,
            },
        );
        return Some(RenderImageResult {
            sequence,
            rows: size.rows,
            image_id: options.image_id,
        });
    }

    // iterm2
    let sequence = encode_iterm2(
        base64_data,
        &ITerm2EncodeOptions {
            width: Some(size.columns.to_string()),
            height: Some("auto".to_string()),
            preserve_aspect_ratio: Some(options.preserve_aspect_ratio.unwrap_or(true)),
            ..Default::default()
        },
    );
    Some(RenderImageResult {
        sequence,
        rows: size.rows,
        image_id: None,
    })
}

/// `hyperlink` (terminal-image.ts:468-480).
pub fn hyperlink(text: &str, url: &str) -> String {
    format!("\x1b]8;;{url}\x1b\\{text}\x1b]8;;\x1b\\")
}

/// `imageFallback` (terminal-image.ts:482-487).
pub fn image_fallback(
    mime_type: &str,
    dimensions: Option<ImageDimensions>,
    filename: Option<&str>,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    // Upstream `if (filename)` treats "" as falsy and skips it; an empty
    // name must not leave a dangling space in the placeholder.
    if let Some(filename) = filename.filter(|name| !name.is_empty()) {
        parts.push(filename.to_string());
    }
    parts.push(format!("[{mime_type}]"));
    if let Some(dimensions) = dimensions {
        parts.push(format!("{}x{}", dimensions.width_px, dimensions.height_px));
    }
    format!("[Image: {}]", parts.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes tests that mutate process env or module globals; cargo runs
    /// tests in one binary with parallel threads.
    static TEST_STATE_LOCK: Mutex<()> = Mutex::new(());

    /// `withEnv` (terminal-image.test.ts:37-55): clears every capability env
    /// var, applies `overrides` (None = unset), runs `f`, then restores.
    fn with_env(overrides: &[(&str, Option<&str>)], f: impl FnOnce()) {
        const ENV_KEYS: [&str; 13] = [
            "TERM",
            "TERM_PROGRAM",
            "TERMINAL_EMULATOR",
            "COLORTERM",
            "TMUX",
            "KITTY_WINDOW_ID",
            "GHOSTTY_RESOURCES_DIR",
            "WEZTERM_PANE",
            "ITERM_SESSION_ID",
            "WT_SESSION",
            "CMUX_WORKSPACE_ID",
            "WARP_SESSION_ID",
            "WARP_TERMINAL_SESSION_UUID",
        ];
        let _guard = TEST_STATE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let saved: Vec<(&str, Option<String>)> = ENV_KEYS
            .iter()
            .map(|&k| (k, std::env::var(k).ok()))
            .collect();
        for &k in &ENV_KEYS {
            std::env::remove_var(k);
        }
        for (k, v) in overrides {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
        f();
        for (k, v) in saved {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }

    /// Guards tests that read/write the capabilities cache and cell dimension
    /// globals.
    fn with_globals(f: impl FnOnce()) {
        let _guard = TEST_STATE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        f();
    }

    fn b64(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    const KITTY_CAPS: TerminalCapabilities = TerminalCapabilities {
        images: Some(ImageProtocol::Kitty),
        true_color: true,
        hyperlinks: true,
    };

    // ── isImageLine ────────────────────────────────────────────────────────

    #[test]
    fn test_is_image_line_detects_iterm2_sequence_at_start_of_line() {
        // iTerm2 image escape sequence: ESC ]1337;File=...
        let iterm2_image_line = "\x1b]1337;File=size=100,100;inline=1:base64encodeddata==\x07";
        assert!(is_image_line(iterm2_image_line));
    }

    #[test]
    fn test_is_image_line_detects_iterm2_sequence_with_text_before_it() {
        // Simulating a line that has text then image data (bug scenario)
        let line_with_text_and_image =
            "Some text \x1b]1337;File=size=100,100;inline=1:base64data==\x07 more text";
        assert!(is_image_line(line_with_text_and_image));
    }

    #[test]
    fn test_is_image_line_detects_iterm2_sequence_in_middle_of_long_line() {
        // Simulate a very long line with image data in the middle
        let long_line_with_image =
            "Text before image...\x1b]1337;File=inline=1:verylongbase64data==...text after"
                .to_string();
        assert!(is_image_line(&long_line_with_image));
    }

    #[test]
    fn test_is_image_line_detects_iterm2_sequence_at_end_of_line() {
        let line_with_image_at_end =
            "Regular text ending with \x1b]1337;File=inline=1:base64data==\x07";
        assert!(is_image_line(line_with_image_at_end));
    }

    #[test]
    fn test_is_image_line_detects_minimal_iterm2_sequence() {
        let minimal_image_line = "\x1b]1337;File=:\x07";
        assert!(is_image_line(minimal_image_line));
    }

    #[test]
    fn test_is_image_line_detects_kitty_sequence_at_start_of_line() {
        // Kitty image escape sequence: ESC _G
        let kitty_image_line = "\x1b_Ga=T,f=100,t=f,d=base64data...\x1b\\\x1b_Gm=i=1;\x1b\\";
        assert!(is_image_line(kitty_image_line));
    }

    #[test]
    fn test_is_image_line_detects_kitty_sequence_with_text_before_it() {
        // Bug scenario: text + image data in same line
        let line_with_text_and_kitty_image =
            "Output: \x1b_Ga=T,f=100;data...\x1b\\\x1b_Gm=i=1;\x1b\\";
        assert!(is_image_line(line_with_text_and_kitty_image));
    }

    #[test]
    fn test_is_image_line_detects_kitty_sequence_with_padding() {
        // Kitty protocol adds padding to escape sequences
        let kitty_with_padding = "  \x1b_Ga=T,f=100...\x1b\\\x1b_Gm=i=1;\x1b\\  ";
        assert!(is_image_line(kitty_with_padding));
    }

    #[test]
    fn test_is_image_line_detects_sequences_in_very_long_lines() {
        // This simulates the crash scenario: a line with 304,401 chars
        // containing image escape sequences somewhere
        let base64_char = "A".repeat(100); // 100 chars of base64-like data
        let image_sequence = "\x1b]1337;File=size=800,600;inline=1:";

        // Build a long line with image sequence
        let long_line = format!(
            "Text prefix {image_sequence}{} suffix",
            base64_char.repeat(3000)
        ); // ~300,000 chars

        assert!(long_line.len() > 300000);
        assert!(is_image_line(&long_line));
    }

    #[test]
    fn test_is_image_line_detects_sequences_when_terminal_doesnt_support_images() {
        // The bug occurred when getImageEscapePrefix() returned null;
        // isImageLine should still detect image sequences regardless
        let line_with_image =
            "Read image file [image/jpeg]\x1b]1337;File=inline=1:base64data==\x07";
        assert!(is_image_line(line_with_image));
    }

    #[test]
    fn test_is_image_line_detects_sequences_with_ansi_codes_before_them() {
        // Text might have ANSI styling before image data
        let line_with_ansi_and_image = "\x1b[31mError output \x1b]1337;File=inline=1:image==\x07";
        assert!(is_image_line(line_with_ansi_and_image));
    }

    #[test]
    fn test_is_image_line_detects_sequences_with_ansi_codes_after_them() {
        let line_with_image_and_ansi =
            "\x1b_Ga=T,f=100:data...\x1b\\\x1b_Gm=i=1;\x1b\\\x1b[0m reset";
        assert!(is_image_line(line_with_image_and_ansi));
    }

    #[test]
    fn test_is_image_line_rejects_plain_text_lines() {
        let plain_text = "This is just a regular text line without any escape sequences";
        assert!(!is_image_line(plain_text));
    }

    #[test]
    fn test_is_image_line_rejects_lines_with_only_ansi_codes() {
        let ansi_text = "\x1b[31mRed text\x1b[0m and \x1b[32mgreen text\x1b[0m";
        assert!(!is_image_line(ansi_text));
    }

    #[test]
    fn test_is_image_line_rejects_lines_with_cursor_movement_codes() {
        let cursor_codes = "\x1b[1A\x1b[2KLine cleared and moved up";
        assert!(!is_image_line(cursor_codes));
    }

    #[test]
    fn test_is_image_line_rejects_partial_iterm2_sequences() {
        // Similar prefix but missing the complete sequence
        let partial_sequence = "Some text with ]1337;File but missing ESC at start";
        assert!(!is_image_line(partial_sequence));
    }

    #[test]
    fn test_is_image_line_rejects_partial_kitty_sequences() {
        // Similar prefix but missing the complete sequence
        let partial_sequence = "Some text with _G but missing ESC at start";
        assert!(!is_image_line(partial_sequence));
    }

    #[test]
    fn test_is_image_line_rejects_empty_lines() {
        assert!(!is_image_line(""));
    }

    #[test]
    fn test_is_image_line_rejects_newline_only_lines() {
        assert!(!is_image_line("\n"));
        assert!(!is_image_line("\n\n"));
    }

    #[test]
    fn test_is_image_line_detects_mixed_kitty_and_iterm2_sequences() {
        let mixed_line = "Kitty: \x1b_Ga=T...\x1b\\\x1b_Gm=i=1;\x1b\\ iTerm2: \x1b]1337;File=inline=1:data==\x07";
        assert!(is_image_line(mixed_line));
    }

    #[test]
    fn test_is_image_line_detects_multiple_text_and_image_segments() {
        let complex_line = "Start \x1b]1337;File=img1==\x07 middle \x1b]1337;File=img2==\x07 end";
        assert!(is_image_line(complex_line));
    }

    #[test]
    fn test_is_image_line_does_not_falsely_detect_file_paths() {
        // File path might contain "1337" or "File" but without escape sequences
        let file_path_line = "/path/to/File_1337_backup/image.jpg";
        assert!(!is_image_line(file_path_line));
    }

    // ── detectCapabilities ─────────────────────────────────────────────────

    #[test]
    fn test_detect_capabilities_defaults_to_hyperlinks_false_for_unknown_terminals() {
        with_env(&[], || {
            let caps = detect_capabilities();
            assert!(!caps.hyperlinks);
            assert_eq!(caps.images, None);
        });
    }

    #[test]
    fn test_detect_capabilities_enables_truecolor_for_unidentified_windows_consoles() {
        // terminal-image.ts:121-127 @ 4181f66 (fa07e7bd9): a Windows console
        // that matches no known terminal (no WT_SESSION, no COLORTERM hint)
        // still gets truecolor, with hyperlinks off.
        with_env(&[("TERM", Some("xterm-256color"))], || {
            let caps = detect_capabilities_inner(true, || false);
            assert!(caps.true_color);
            assert!(!caps.hyperlinks);
            assert_eq!(caps.images, None);
        });
    }

    #[test]
    fn test_detect_capabilities_enables_hyperlinks_under_tmux_when_client_forwards_them() {
        with_env(
            &[
                ("TMUX", Some("/tmp/tmux-1000/default,1234,0")),
                ("TERM_PROGRAM", Some("ghostty")),
            ],
            || {
                let caps = detect_capabilities_with(|| true);
                assert!(caps.hyperlinks);
                assert_eq!(caps.images, None);
            },
        );
    }

    #[test]
    fn test_detect_capabilities_disables_hyperlinks_under_tmux_when_client_does_not_forward() {
        with_env(
            &[
                ("TMUX", Some("/tmp/tmux-1000/default,1234,0")),
                ("TERM_PROGRAM", Some("ghostty")),
            ],
            || {
                let caps = detect_capabilities_with(|| false);
                assert!(!caps.hyperlinks);
                assert_eq!(caps.images, None);
            },
        );
    }

    #[test]
    fn test_detect_capabilities_checks_tmux_capability_when_term_starts_with_tmux() {
        with_env(
            &[
                ("TERM", Some("tmux-256color")),
                ("TERM_PROGRAM", Some("iterm.app")),
            ],
            || {
                let caps = detect_capabilities_with(|| true);
                assert!(caps.hyperlinks);
                assert_eq!(caps.images, None);

                let caps2 = detect_capabilities_with(|| false);
                assert!(!caps2.hyperlinks);
            },
        );
    }

    #[test]
    fn test_detect_capabilities_forces_hyperlinks_false_when_term_starts_with_screen() {
        with_env(&[("TERM", Some("screen-256color"))], || {
            let caps = detect_capabilities();
            assert!(!caps.hyperlinks);
            assert_eq!(caps.images, None);
        });
    }

    #[test]
    fn test_detect_capabilities_enables_hyperlinks_for_ghostty() {
        with_env(&[("TERM_PROGRAM", Some("ghostty"))], || {
            let caps = detect_capabilities();
            assert!(caps.hyperlinks);
        });
    }

    #[test]
    fn test_detect_capabilities_does_not_disable_ghostty_images_solely_because_cmux_is_present() {
        with_env(
            &[
                ("TERM_PROGRAM", Some("ghostty")),
                ("CMUX_WORKSPACE_ID", Some("workspace")),
            ],
            || {
                let caps = detect_capabilities();
                assert_eq!(caps.images, Some(ImageProtocol::Kitty));
                assert!(caps.hyperlinks);
            },
        );
    }

    #[test]
    fn test_detect_capabilities_enables_hyperlinks_for_kitty() {
        with_env(&[("KITTY_WINDOW_ID", Some("1"))], || {
            let caps = detect_capabilities();
            assert!(caps.hyperlinks);
        });
    }

    #[test]
    fn test_detect_capabilities_enables_hyperlinks_for_wezterm() {
        with_env(&[("WEZTERM_PANE", Some("0"))], || {
            let caps = detect_capabilities();
            assert!(caps.hyperlinks);
        });
    }

    #[test]
    fn test_detect_capabilities_enables_images_and_hyperlinks_for_warp_via_term_program() {
        with_env(&[("TERM_PROGRAM", Some("WarpTerminal"))], || {
            let caps = detect_capabilities();
            assert_eq!(caps.images, Some(ImageProtocol::Kitty));
            assert!(caps.true_color);
            assert!(caps.hyperlinks);
        });
    }

    #[test]
    fn test_detect_capabilities_enables_images_and_hyperlinks_for_warp_via_warp_session_id() {
        with_env(&[("WARP_SESSION_ID", Some("some-session-id"))], || {
            let caps = detect_capabilities();
            assert_eq!(caps.images, Some(ImageProtocol::Kitty));
            assert!(caps.true_color);
            assert!(caps.hyperlinks);
        });
    }

    #[test]
    fn test_detect_capabilities_enables_images_and_hyperlinks_for_warp_via_warp_terminal_session_uuid(
    ) {
        with_env(
            &[(
                "WARP_TERMINAL_SESSION_UUID",
                Some("d0e1a2e5-7ca7-44cd-9037-ac7222011161"),
            )],
            || {
                let caps = detect_capabilities();
                assert_eq!(caps.images, Some(ImageProtocol::Kitty));
                assert!(caps.true_color);
                assert!(caps.hyperlinks);
            },
        );
    }

    #[test]
    fn test_detect_capabilities_disables_images_for_warp_inside_tmux() {
        with_env(
            &[
                ("TERM_PROGRAM", Some("WarpTerminal")),
                ("TMUX", Some("/tmp/tmux-1000/default,1234,0")),
                ("TERM", Some("tmux-256color")),
            ],
            || {
                let caps = detect_capabilities_with(|| true);
                assert_eq!(caps.images, None);
                assert!(caps.hyperlinks);
            },
        );
    }

    #[test]
    fn test_detect_capabilities_enables_hyperlinks_for_iterm2() {
        with_env(&[("TERM_PROGRAM", Some("iterm.app"))], || {
            let caps = detect_capabilities();
            assert!(caps.hyperlinks);
        });
    }

    #[test]
    fn test_detect_capabilities_enables_hyperlinks_for_vscode() {
        with_env(&[("TERM_PROGRAM", Some("vscode"))], || {
            let caps = detect_capabilities();
            assert!(caps.hyperlinks);
        });
    }

    #[test]
    fn test_detect_capabilities_enables_truecolor_and_hyperlinks_for_windows_terminal_outside_multiplexers(
    ) {
        with_env(
            &[
                ("WT_SESSION", Some("session")),
                ("TERM", Some("xterm-256color")),
            ],
            || {
                let caps = detect_capabilities();
                assert!(caps.true_color);
                assert!(caps.hyperlinks);
                assert_eq!(caps.images, None);
            },
        );
    }

    #[test]
    fn test_detect_capabilities_enables_truecolor_without_hyperlinks_for_jetbrains_terminal() {
        with_env(
            &[
                ("TERMINAL_EMULATOR", Some("JetBrains-JediTerm")),
                ("TERM", Some("xterm-256color")),
            ],
            || {
                let caps = detect_capabilities();
                assert!(caps.true_color);
                assert!(!caps.hyperlinks);
                assert_eq!(caps.images, None);
            },
        );
    }

    #[test]
    fn test_detect_capabilities_does_not_inherit_windows_terminal_truecolor_through_tmux() {
        with_env(
            &[
                ("WT_SESSION", Some("session")),
                ("TMUX", Some("/tmp/tmux-1000/default,1234,0")),
                ("TERM", Some("tmux-256color")),
            ],
            || {
                let caps = detect_capabilities_with(|| false);
                assert!(!caps.true_color);
                assert!(!caps.hyperlinks);
                assert_eq!(caps.images, None);
            },
        );
    }

    #[test]
    fn test_detect_capabilities_trusts_explicit_truecolor_hints_through_tmux() {
        with_env(
            &[
                ("COLORTERM", Some("truecolor")),
                ("TMUX", Some("/tmp/tmux-1000/default,1234,0")),
                ("TERM", Some("tmux-256color")),
            ],
            || {
                let caps = detect_capabilities_with(|| false);
                assert!(caps.true_color);
                assert!(!caps.hyperlinks);
                assert_eq!(caps.images, None);
            },
        );
    }

    // ── Kitty image cursor movement / renderImage ──────────────────────────

    #[test]
    fn test_encode_kitty_can_request_no_terminal_side_cursor_movement() {
        let sequence = encode_kitty(
            "AAAA",
            &KittyEncodeOptions {
                columns: Some(2),
                rows: Some(2),
                move_cursor: Some(false),
                ..Default::default()
            },
        );
        assert!(sequence.starts_with("\x1b_Ga=T,f=100,q=2,C=1,c=2,r=2;"));
    }

    #[test]
    fn test_encode_kitty_suppresses_replies_for_delete_commands() {
        assert_eq!(delete_kitty_image(42), "\x1b_Ga=d,d=I,i=42,q=2\x1b\\");
        assert_eq!(delete_all_kitty_images(), "\x1b_Ga=d,d=A,q=2\x1b\\");
    }

    #[test]
    fn test_render_image_preserves_default_terminal_side_cursor_movement() {
        with_globals(|| {
            set_capabilities(KITTY_CAPS);
            set_cell_dimensions(CellDimensions {
                width_px: 10.0,
                height_px: 10.0,
            });
            let result = render_image(
                "AAAA",
                ImageDimensions {
                    width_px: 20.0,
                    height_px: 20.0,
                },
                &ImageRenderOptions {
                    max_width_cells: Some(2.0),
                    ..Default::default()
                },
            )
            .expect("renderImage should produce a sequence");
            assert!(!result.sequence.contains(",C=1,"));
            assert_eq!(result.rows, 2);
            reset_capabilities_cache();
            set_cell_dimensions(CellDimensions::default());
        });
    }

    #[test]
    fn test_render_image_can_opt_into_no_terminal_side_cursor_movement() {
        with_globals(|| {
            set_capabilities(KITTY_CAPS);
            set_cell_dimensions(CellDimensions {
                width_px: 10.0,
                height_px: 10.0,
            });
            let result = render_image(
                "AAAA",
                ImageDimensions {
                    width_px: 20.0,
                    height_px: 20.0,
                },
                &ImageRenderOptions {
                    max_width_cells: Some(2.0),
                    move_cursor: Some(false),
                    ..Default::default()
                },
            )
            .expect("renderImage should produce a sequence");
            assert!(result.sequence.contains(",C=1,"));
            assert_eq!(result.rows, 2);
            reset_capabilities_cache();
            set_cell_dimensions(CellDimensions::default());
        });
    }

    #[test]
    fn test_render_image_honors_max_height_cells_by_reducing_rendered_width() {
        with_globals(|| {
            set_capabilities(KITTY_CAPS);
            set_cell_dimensions(CellDimensions {
                width_px: 10.0,
                height_px: 10.0,
            });
            let result = render_image(
                "AAAA",
                ImageDimensions {
                    width_px: 10.0,
                    height_px: 100.0,
                },
                &ImageRenderOptions {
                    max_width_cells: Some(10.0),
                    max_height_cells: Some(5.0),
                    ..Default::default()
                },
            )
            .expect("renderImage should produce a sequence");
            assert_eq!(result.rows, 5);
            assert!(result.sequence.contains(",c=1,r=5"));
            reset_capabilities_cache();
            set_cell_dimensions(CellDimensions::default());
        });
    }

    #[test]
    fn test_render_image_returns_null_when_terminal_does_not_support_images() {
        with_globals(|| {
            set_capabilities(TerminalCapabilities {
                images: None,
                true_color: true,
                hyperlinks: true,
            });
            let result = render_image(
                "AAAA",
                ImageDimensions {
                    width_px: 20.0,
                    height_px: 20.0,
                },
                &ImageRenderOptions::default(),
            );
            assert!(result.is_none());
            reset_capabilities_cache();
        });
    }

    // ── hyperlink ──────────────────────────────────────────────────────────

    #[test]
    fn test_hyperlink_wraps_text_in_osc8_open_and_close_sequences() {
        let result = hyperlink("click me", "https://example.com");
        assert_eq!(
            result,
            "\x1b]8;;https://example.com\x1b\\click me\x1b]8;;\x1b\\"
        );
    }

    #[test]
    fn test_hyperlink_preserves_ansi_styling_inside_the_hyperlink() {
        let styled = "\x1b[4m\x1b[34mclick me\x1b[0m";
        let result = hyperlink(styled, "https://example.com");
        assert!(result.starts_with("\x1b]8;;https://example.com\x1b\\"));
        assert!(result.contains(styled));
        assert!(result.ends_with("\x1b]8;;\x1b\\"));
    }

    #[test]
    fn test_hyperlink_works_with_empty_text() {
        let result = hyperlink("", "https://example.com");
        assert_eq!(result, "\x1b]8;;https://example.com\x1b\\\x1b]8;;\x1b\\");
    }

    #[test]
    fn test_hyperlink_works_with_file_uris() {
        let result = hyperlink("README.md", "file:///home/user/README.md");
        assert!(result.contains("file:///home/user/README.md"));
        assert!(result.contains("README.md"));
    }

    // ── Supplementary coverage (protocol internals upstream only exercises
    // ── through the Image component / TUI core, ported in later phases) ─────

    #[test]
    fn test_encode_kitty_single_chunk_payload() {
        let sequence = encode_kitty(
            "AAAA",
            &KittyEncodeOptions {
                columns: Some(2),
                rows: Some(2),
                image_id: Some(7),
                ..Default::default()
            },
        );
        assert_eq!(sequence, "\x1b_Ga=T,f=100,q=2,c=2,r=2,i=7;AAAA\x1b\\");
    }

    #[test]
    fn test_encode_kitty_omits_zero_values_like_js_truthiness() {
        // `if (options.columns)` etc. — zero is falsy in JS (terminal-image.ts:180-182).
        let sequence = encode_kitty(
            "AAAA",
            &KittyEncodeOptions {
                columns: Some(0),
                rows: Some(0),
                image_id: Some(0),
                ..Default::default()
            },
        );
        assert_eq!(sequence, "\x1b_Ga=T,f=100,q=2;AAAA\x1b\\");
    }

    #[test]
    fn test_encode_kitty_chunks_large_payloads() {
        // 3 full chunks + a partial tail (terminal-image.ts:188-208).
        let data = "A".repeat(KITTY_CHUNK_SIZE * 3 + 100);
        let sequence = encode_kitty(&data, &KittyEncodeOptions::default());

        let header = "\x1b_Ga=T,f=100,q=2,m=1;";
        assert!(sequence.starts_with(header));
        let after_header = &sequence[header.len()..];
        assert!(after_header.starts_with(&"A".repeat(KITTY_CHUNK_SIZE)));

        let tail = "\x1b_Gm=0;";
        assert!(sequence.ends_with("\x1b\\"));
        let before_st = &sequence[..sequence.len() - 2];
        assert!(before_st.ends_with(&format!("{tail}{}", "A".repeat(100))));

        // Every chunk is transmitted as its own `ESC _G ... ESC \` sequence:
        // first with params + m=1, middles with m=1, last with m=0.
        let chunks: Vec<&str> = sequence.split("\x1b\\").filter(|c| !c.is_empty()).collect();
        assert_eq!(chunks.len(), 4);
        assert!(chunks[0].starts_with("\x1b_Ga=T,f=100,q=2,m=1;"));
        assert_eq!(
            chunks[0].len(),
            "\x1b_Ga=T,f=100,q=2,m=1;".len() + KITTY_CHUNK_SIZE
        );
        assert!(chunks[1].starts_with("\x1b_Gm=1;"));
        assert_eq!(chunks[1].len(), "\x1b_Gm=1;".len() + KITTY_CHUNK_SIZE);
        assert!(chunks[2].starts_with("\x1b_Gm=1;"));
        assert_eq!(chunks[2].len(), "\x1b_Gm=1;".len() + KITTY_CHUNK_SIZE);
        assert!(chunks[3].starts_with("\x1b_Gm=0;"));
        assert_eq!(chunks[3].len(), "\x1b_Gm=0;".len() + 100);
    }

    #[test]
    fn test_encode_iterm2_encodes_params_and_base64_name() {
        let sequence = encode_iterm2(
            "AA==",
            &ITerm2EncodeOptions {
                width: Some("2".to_string()),
                height: Some("auto".to_string()),
                name: Some("test.png".to_string()),
                preserve_aspect_ratio: Some(false),
                inline: Some(false),
            },
        );
        assert_eq!(
            sequence,
            "\x1b]1337;File=inline=0;width=2;height=auto;name=dGVzdC5wbmc=;preserveAspectRatio=0:AA==\x07"
        );
    }

    #[test]
    fn test_encode_iterm2_defaults_to_inline() {
        let sequence = encode_iterm2("AA==", &ITerm2EncodeOptions::default());
        assert_eq!(sequence, "\x1b]1337;File=inline=1:AA==\x07");
    }

    #[test]
    fn test_calculate_image_cell_size_scales_to_max_width() {
        let size = calculate_image_cell_size(
            ImageDimensions {
                width_px: 20.0,
                height_px: 20.0,
            },
            2.0,
            None,
            CellDimensions {
                width_px: 10.0,
                height_px: 10.0,
            },
        );
        assert_eq!(
            size,
            ImageCellSize {
                columns: 2,
                rows: 2
            }
        );
    }

    #[test]
    fn test_calculate_image_cell_size_caps_height_and_reduces_width() {
        let size = calculate_image_cell_size(
            ImageDimensions {
                width_px: 10.0,
                height_px: 100.0,
            },
            10.0,
            Some(5.0),
            CellDimensions {
                width_px: 10.0,
                height_px: 10.0,
            },
        );
        assert_eq!(
            size,
            ImageCellSize {
                columns: 1,
                rows: 5
            }
        );
    }

    #[test]
    fn test_calculate_image_cell_size_clamps_to_at_least_one_cell() {
        let size = calculate_image_cell_size(
            ImageDimensions {
                width_px: 10.0,
                height_px: 10.0,
            },
            0.0,
            None,
            CellDimensions {
                width_px: 10.0,
                height_px: 10.0,
            },
        );
        assert_eq!(
            size,
            ImageCellSize {
                columns: 1,
                rows: 1
            }
        );
    }

    #[test]
    fn test_calculate_image_rows_returns_rows_for_target_width() {
        // scale = 4 cells * 10px / 20px = 2 → rows = ceil(40 * 2 / 10) = 8.
        let rows = calculate_image_rows(
            ImageDimensions {
                width_px: 20.0,
                height_px: 40.0,
            },
            4.0,
            CellDimensions {
                width_px: 10.0,
                height_px: 10.0,
            },
        );
        assert_eq!(rows, 8);
    }

    #[test]
    fn test_get_png_dimensions_parses_ihdr() {
        let mut bytes = vec![0u8; 24];
        bytes[0..4].copy_from_slice(&[0x89, 0x50, 0x4e, 0x47]);
        bytes[16..20].copy_from_slice(&800u32.to_be_bytes());
        bytes[20..24].copy_from_slice(&600u32.to_be_bytes());
        assert_eq!(
            get_png_dimensions(&b64(&bytes)),
            Some(ImageDimensions {
                width_px: 800.0,
                height_px: 600.0
            })
        );
    }

    #[test]
    fn test_get_png_dimensions_rejects_short_or_mismatched_data() {
        assert_eq!(get_png_dimensions(&b64(&[0x89, 0x50, 0x4e])), None);
        assert_eq!(get_png_dimensions(&b64(&[0u8; 24])), None);
    }

    #[test]
    fn test_get_jpeg_dimensions_parses_sof0() {
        // SOI + SOF0 (FF C0, length 0x0011, precision 8, 120x75).
        let bytes: Vec<u8> = vec![
            0xff, 0xd8, 0xff, 0xc0, 0x00, 0x11, 0x08, 0x00, 0x78, 0x00, 0x4b, 0x00, 0x03,
        ];
        assert_eq!(
            get_jpeg_dimensions(&b64(&bytes)),
            Some(ImageDimensions {
                width_px: 75.0,
                height_px: 120.0
            })
        );
    }

    #[test]
    fn test_get_jpeg_dimensions_skips_segments_before_sof() {
        // SOI + DQT (length 4) + SOF0 — the scanner must skip the DQT segment.
        let bytes: Vec<u8> = vec![
            0xff, 0xd8, 0xff, 0xdb, 0x00, 0x04, 0x00, 0x00, 0xff, 0xc0, 0x00, 0x11, 0x08, 0x00,
            0x78, 0x00, 0x4b, 0x00,
        ];
        assert_eq!(
            get_jpeg_dimensions(&b64(&bytes)),
            Some(ImageDimensions {
                width_px: 75.0,
                height_px: 120.0
            })
        );
    }

    #[test]
    fn test_get_jpeg_dimensions_rejects_missing_sof_marker() {
        // SOI + DQT only — the scanner runs off the end of the buffer.
        let bytes: Vec<u8> = vec![0xff, 0xd8, 0xff, 0xdb, 0x00, 0x04, 0x00];
        assert_eq!(get_jpeg_dimensions(&b64(&bytes)), None);
        assert_eq!(get_jpeg_dimensions(&b64(&[0xff, 0xd8])), None);
    }

    #[test]
    fn test_get_gif_dimensions_parses_logical_screen_descriptor() {
        let mut bytes = vec![0u8; 10];
        bytes[0..6].copy_from_slice(b"GIF89a");
        bytes[6..8].copy_from_slice(&320u16.to_le_bytes());
        bytes[8..10].copy_from_slice(&240u16.to_le_bytes());
        assert_eq!(
            get_gif_dimensions(&b64(&bytes)),
            Some(ImageDimensions {
                width_px: 320.0,
                height_px: 240.0
            })
        );
        bytes[0..6].copy_from_slice(b"GIF87a");
        assert_eq!(
            get_gif_dimensions(&b64(&bytes)),
            Some(ImageDimensions {
                width_px: 320.0,
                height_px: 240.0
            })
        );
    }

    #[test]
    fn test_get_webp_dimensions_parses_vp8() {
        let mut bytes = vec![0u8; 30];
        bytes[0..4].copy_from_slice(b"RIFF");
        bytes[8..12].copy_from_slice(b"WEBP");
        bytes[12..16].copy_from_slice(b"VP8 ");
        bytes[26] = 0x3f;
        bytes[27] = 0x02; // 0x023f & 0x3fff = 575
        bytes[28] = 0x40;
        bytes[29] = 0x01; // 0x0140 & 0x3fff = 320
        assert_eq!(
            get_webp_dimensions(&b64(&bytes)),
            Some(ImageDimensions {
                width_px: 575.0,
                height_px: 320.0
            })
        );
    }

    #[test]
    fn test_get_webp_dimensions_parses_vp8l() {
        let mut bytes = vec![0u8; 30];
        bytes[0..4].copy_from_slice(b"RIFF");
        bytes[8..12].copy_from_slice(b"WEBP");
        bytes[12..16].copy_from_slice(b"VP8L");
        // width = (bits & 0x3fff) + 1, height = ((bits >> 14) & 0x3fff) + 1
        bytes[21..25].copy_from_slice(&0x0002_0001u32.to_le_bytes());
        assert_eq!(
            get_webp_dimensions(&b64(&bytes)),
            Some(ImageDimensions {
                width_px: 2.0,
                height_px: 9.0
            })
        );
    }

    #[test]
    fn test_get_webp_dimensions_parses_vp8x() {
        let mut bytes = vec![0u8; 30];
        bytes[0..4].copy_from_slice(b"RIFF");
        bytes[8..12].copy_from_slice(b"WEBP");
        bytes[12..16].copy_from_slice(b"VP8X");
        bytes[24] = 0x2f;
        bytes[25] = 0x03; // 0x032f + 1 = 816
        bytes[27] = 0x10;
        bytes[28] = 0x02; // 0x0210 + 1 = 529
        assert_eq!(
            get_webp_dimensions(&b64(&bytes)),
            Some(ImageDimensions {
                width_px: 816.0,
                height_px: 529.0
            })
        );
    }

    #[test]
    fn test_get_webp_dimensions_rejects_other_chunks_and_bad_magic() {
        let mut bytes = vec![0u8; 30];
        bytes[0..4].copy_from_slice(b"RIFF");
        bytes[8..12].copy_from_slice(b"WEBP");
        bytes[12..16].copy_from_slice(b"ANIM");
        assert_eq!(get_webp_dimensions(&b64(&bytes)), None);

        let mut bad = vec![0u8; 30];
        bad[0..4].copy_from_slice(b"RIFF");
        bad[8..12].copy_from_slice(b"JPEG");
        assert_eq!(get_webp_dimensions(&b64(&bad)), None);
    }

    #[test]
    fn test_get_image_dimensions_dispatches_on_mime_type() {
        let mut png = vec![0u8; 24];
        png[0..4].copy_from_slice(&[0x89, 0x50, 0x4e, 0x47]);
        png[16..20].copy_from_slice(&100u32.to_be_bytes());
        png[20..24].copy_from_slice(&50u32.to_be_bytes());
        let encoded = b64(&png);

        assert_eq!(
            get_image_dimensions(&encoded, "image/png"),
            Some(ImageDimensions {
                width_px: 100.0,
                height_px: 50.0
            })
        );
        assert_eq!(get_image_dimensions(&encoded, "image/jpeg"), None);
        assert_eq!(get_image_dimensions(&encoded, "image/gif"), None);
        assert_eq!(get_image_dimensions(&encoded, "image/webp"), None);
        assert_eq!(get_image_dimensions(&encoded, "application/pdf"), None);
        assert_eq!(
            get_image_dimensions("!!! not base64 !!!", "image/png"),
            None
        );
    }

    #[test]
    fn test_image_fallback_formats_placeholder() {
        assert_eq!(
            image_fallback("image/png", None, None),
            "[Image: [image/png]]"
        );
        assert_eq!(
            image_fallback(
                "image/jpeg",
                Some(ImageDimensions {
                    width_px: 800.0,
                    height_px: 600.0
                }),
                Some("photo.jpg"),
            ),
            "[Image: photo.jpg [image/jpeg] 800x600]"
        );
        // An empty filename is falsy upstream and must be skipped like `None`.
        assert_eq!(
            image_fallback("image/png", None, Some("")),
            "[Image: [image/png]]"
        );
    }

    #[test]
    fn test_allocate_image_id_returns_ids_in_range() {
        for _ in 0..1000 {
            let id = allocate_image_id();
            assert!(id >= 1);
            assert!(id <= 0xffff_fffe);
        }
    }

    #[test]
    fn test_get_set_cell_dimensions_round_trip() {
        with_globals(|| {
            set_cell_dimensions(CellDimensions {
                width_px: 12.0,
                height_px: 24.0,
            });
            assert_eq!(
                get_cell_dimensions(),
                CellDimensions {
                    width_px: 12.0,
                    height_px: 24.0
                }
            );
            set_cell_dimensions(CellDimensions::default());
        });
    }
}
