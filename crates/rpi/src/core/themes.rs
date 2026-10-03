//! Port of the theme system from
//! `packages/coding-agent/src/modes/interactive/theme/theme.ts` @ pi 0.82.1 (2efa728)
//! and `packages/tui/src/terminal-colors.ts`.
//!
//! Provides theme JSON schema validation, color variable resolution,
//! hex/256-colour → ANSI conversion, built-in dark/light themes, auto theme
//! parsing, and terminal-background detection logic as pure functions. The
//! actual terminal I/O (OSC 11 queries, fs watcher) lands in T12.
//!
//! Intentional differences:
//! - Built-in `dark.json` / `light.json` are embedded as string constants and
//!   parsed lazily (upstream reads files from disk via `getThemesDir()`).
//! - Terminal capabilities detection (`detectCapabilities` /
//!   `getCapabilities`) is not ported — it depends on the TUI runtime (T12).
//!   [`ColorMode`] is passed explicitly by the caller.
//! - The `Theme` struct's TUI helpers (`getMarkdownTheme`, `getEditorTheme`,
//!   `getSelectListTheme`, `getSettingsListTheme`) are not ported — they
//!   depend on TUI types from `pi-tui` that are not yet implemented (T11/T12).
//!   `highlightCode` / `getLanguageFromPath` are ported separately in
//!   `core::highlight` (syntect, T17-W2 / ADR-0008).
//! - Registered themes (`setRegisteredThemes` / `registeredThemes` Map) are
//!   not implemented — package/project theme registration comes from the
//!   resource loader (T17+). The load-priority chain still has a placeholder
//!   branch for registered themes.
//! - `chalk` text styling (bold/italic/underline/etc.) uses raw ANSI codes
//!   instead of a `chalk` equivalent.
//! - The global mutable singleton (`theme` proxy / `setGlobalTheme` /
//!   `currentThemeName` / `onThemeChangeCallback`) is not ported — the TUI
//!   runtime (T12) owns theme lifecycle.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::{Mutex, OnceLock};

use rpi_tui::colors as tui_colors;
use rpi_tui::terminal_colors::{RgbColor as TerminalRgb, TerminalColors};

use crate::config;
use crate::error::RpiError;

pub mod system;

pub use self::system::SYSTEM_THEME_NAME;

// ===========================================================================
// Terminal color state (theme.ts:406-417 @ a13d35a74)
// ===========================================================================

/// The terminal's reported colors; replaced (never mutated) on update, so
/// themes can cache resolved colors by identity.
static TERMINAL_COLORS: Mutex<TerminalColors> = Mutex::new(TerminalColors {
    foreground: None,
    background: None,
    palette: None,
});
/// Whether a report has ever been applied (upstream's `previous` check,
/// theme-controller.ts:114-116).
static TERMINAL_COLORS_REPORTED: AtomicBool = AtomicBool::new(false);
/// The terminal's last light/dark report (mode 2031); only used while it has
/// not reported a background (theme.ts:410-412).
static TERMINAL_COLOR_SCHEME: Mutex<Option<TerminalTheme>> = Mutex::new(None);
/// While the terminal color query is in flight, the system theme renders in
/// grayscale (theme.ts:408-409).
static TERMINAL_COLORS_PENDING: AtomicBool = AtomicBool::new(false);

fn lock_terminal<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// `setTerminalColors` (theme.ts:418-421): record the terminal's reported
/// colors and end the pending (grayscale) state.
pub fn set_terminal_colors(colors: TerminalColors) {
    *lock_terminal(&TERMINAL_COLORS) = colors;
    TERMINAL_COLORS_REPORTED.store(true, AtomicOrdering::Relaxed);
    TERMINAL_COLORS_PENDING.store(false, AtomicOrdering::Relaxed);
}

/// The terminal's last reported colors (clone of the shared snapshot).
pub fn get_terminal_colors() -> TerminalColors {
    lock_terminal(&TERMINAL_COLORS).clone()
}

/// Whether `set_terminal_colors` has ever been called (the controller's
/// first-report check).
pub fn has_terminal_colors() -> bool {
    TERMINAL_COLORS_REPORTED.load(AtomicOrdering::Relaxed)
}

/// `setTerminalColorScheme` (theme.ts:424-426): the fallback appearance for
/// terminals that do not report their background.
pub fn set_terminal_color_scheme(scheme: Option<TerminalTheme>) {
    *lock_terminal(&TERMINAL_COLOR_SCHEME) = scheme;
}

/// The terminal's last light/dark report.
pub fn get_terminal_color_scheme() -> Option<TerminalTheme> {
    *lock_terminal(&TERMINAL_COLOR_SCHEME)
}

/// `markTerminalColorsPending` (theme.ts:429-431): render the system theme
/// in grayscale until `set_terminal_colors` reports the terminal's colors.
pub fn mark_terminal_colors_pending() {
    TERMINAL_COLORS_PENDING.store(true, AtomicOrdering::Relaxed);
}

/// Whether the system theme is currently rendering in grayscale.
pub fn terminal_colors_pending() -> bool {
    TERMINAL_COLORS_PENDING.load(AtomicOrdering::Relaxed)
}

/// `detectColorFgBgTheme` (theme.ts:697-705 @ a13d35a74): the last numeric
/// `COLORFGBG` field is an ANSI index classified like Vim — 0-6 and 8 dark,
/// 7 and 9-15 light; anything else has no answer.
pub fn detect_color_fg_bg_theme(colorfgbg: Option<&str>) -> Option<TerminalTheme> {
    let background = colorfgbg?.rsplit(';').next()?.trim();
    if background.is_empty()
        || background.len() > 2
        || !background.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let index: u32 = background.parse().ok()?;
    if index > 15 {
        return None;
    }
    Some(if index <= 6 || index == 8 {
        TerminalTheme::Dark
    } else {
        TerminalTheme::Light
    })
}

/// `detectTerminalTheme` (theme.ts:707-716 @ a13d35a74): the reported
/// background (with the foreground as a tiebreaker) decides; without one the
/// terminal's light/dark report, then `COLORFGBG`, then dark.
pub fn detect_terminal_theme(
    colors: &TerminalColors,
    reported_scheme: Option<TerminalTheme>,
    colorfgbg: Option<&str>,
) -> TerminalTheme {
    if let Some(background) = colors.background {
        return match system::terminal_appearance(background, colors.foreground) {
            system::Appearance::Dark => TerminalTheme::Dark,
            system::Appearance::Light => TerminalTheme::Light,
        };
    }
    reported_scheme
        .or_else(|| detect_color_fg_bg_theme(colorfgbg))
        .unwrap_or(TerminalTheme::Dark)
}

/// `getTerminalTheme` (theme.ts:719-721): the appearance from everything the
/// terminal reported so far.
pub fn get_terminal_theme() -> TerminalTheme {
    detect_terminal_theme(
        &get_terminal_colors(),
        get_terminal_color_scheme(),
        std::env::var("COLORFGBG").ok().as_deref(),
    )
}

/// `createSystemTheme` (theme.ts:614-624): generate the `system` theme from
/// the terminal's reported colors (grayscale while they are pending).
pub fn create_system_theme(mode: Option<ColorMode>) -> Theme {
    let colors = get_terminal_colors();
    let generated = system::generate_system_theme_colors(&system::SystemThemeInput {
        foreground: colors.foreground,
        background: colors.background,
        palette: colors.palette,
        saturation: Some(if terminal_colors_pending() { 0.0 } else { 1.0 }),
        appearance_hint: Some(match get_terminal_theme() {
            TerminalTheme::Dark => system::Appearance::Dark,
            TerminalTheme::Light => system::Appearance::Light,
        }),
    });
    let resolved: HashMap<String, ResolvedColor> = generated
        .colors
        .iter()
        .map(|(token, value)| {
            let resolved = match value {
                system::SystemColorValue::Hex(hex) => ResolvedColor::Hex(hex.clone()),
                system::SystemColorValue::Index(index) => ResolvedColor::Index(*index),
                system::SystemColorValue::Default => ResolvedColor::Empty,
            };
            ((*token).to_string(), resolved)
        })
        .collect();
    let appearance = generated.appearance.map(|appearance| match appearance {
        system::Appearance::Dark => TerminalTheme::Dark,
        system::Appearance::Light => TerminalTheme::Light,
    });
    let dim: Vec<String> = generated
        .dim
        .iter()
        .map(|token| (*token).to_string())
        .collect();
    Theme::from_resolved(
        resolved,
        mode.unwrap_or(ColorMode::TrueColor),
        Some(SYSTEM_THEME_NAME.to_string()),
        None,
        appearance,
        dim,
    )
    // Invariant: every generated token is a valid hex/index/empty value;
    // `from_resolved` only fails on a malformed explicit color.
    .expect("generated system theme colors always build")
}

/// Map the rpi [ColorMode] onto the TUI color mode.
pub(crate) fn tui_color_mode(mode: ColorMode) -> tui_colors::TerminalColorMode {
    match mode {
        ColorMode::TrueColor => tui_colors::TerminalColorMode::TrueColor,
        ColorMode::Color256 => tui_colors::TerminalColorMode::Color256,
    }
}

/// `parseColor` on a theme color string plus the `#rgb` expansion
/// (theme.ts:171-183; colors.ts:121-140).
fn resolved_rgb(value: &str) -> Result<Rgb, RpiError> {
    if let Some(hex) = value.strip_prefix('#') {
        if hex.len() == 3 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            let mut expanded = String::from("#");
            for c in hex.chars() {
                expanded.push(c);
                expanded.push(c);
            }
            return hex_to_rgb(&expanded);
        }
        return hex_to_rgb(value);
    }
    let color =
        tui_colors::parse_color(value).map_err(|error| RpiError::Resource(error.to_string()))?;
    let rgb = tui_colors::color_to_rgb(&color);
    Ok(Rgb {
        r: rgb.r.round() as u32,
        g: rgb.g.round() as u32,
        b: rgb.b.round() as u32,
    })
}

/// `#rrggbb` for a resolved color string (hex passthrough/normalization, or
/// `parseColor` for OKLCH/OKHSL values).
pub fn resolved_color_to_hex(value: &str) -> String {
    if let Some(hex) = value.strip_prefix('#') {
        if hex.len() == 3 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            let mut expanded = String::from("#");
            for c in hex.chars() {
                expanded.push(c);
                expanded.push(c);
            }
            return expanded;
        }
        if hex.len() == 6 {
            return value.to_string();
        }
    }
    match tui_colors::parse_color(value) {
        Ok(color) => tui_colors::color_to_hex(&color),
        Err(_) => value.to_string(),
    }
}

/// Upstream `/^ok(lch|hsl)\(/i` literal-color check in `resolveVarRefs`
/// (theme.ts:234).
fn is_function_color(value: &str) -> bool {
    for prefix in ["oklch(", "okhsl("] {
        if value
            .get(..prefix.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
        {
            return true;
        }
    }
    false
}

// ===========================================================================
// Built-in theme JSON (verbatim values from upstream dark.json / light.json)
// ===========================================================================

/// Embedded `dark.json` (values ported verbatim from
/// `packages/coding-agent/src/modes/interactive/theme/dark.json`).
const DARK_THEME_JSON: &str = r##"{
	"$schema": "https://raw.githubusercontent.com/earendil-works/pi/main/packages/coding-agent/src/modes/interactive/theme/theme-schema.json",
	"name": "dark",
	"appearance": "dark",
	"vars": {
		"text": "okhsl(234 3% 89%)",
		"muted": "okhsl(229 6% 67%)",
		"violet": "okhsl(295 50% 67%)",
		"blue": "okhsl(232 54% 67%)",
		"green": "okhsl(159 59% 67%)",
		"red": "okhsl(20 72% 67%)",
		"yellow": "okhsl(83 88% 67%)",
		"blueBg": "okhsl(233 41% 24%)"
	},
	"colors": {
		"accent": "violet",
		"border": "okhsl(231 57% 65%)",
		"borderAccent": "okhsl(295 53% 64%)",
		"borderMuted": "okhsl(229 8% 53%)",
		"success": "green",
		"error": "red",
		"warning": "yellow",
		"muted": "muted",
		"dim": "okhsl(229 8% 56%)",
		"text": "text",
		"thinkingText": "okhsl(226 7% 65%)",
		"selectedBg": "blueBg",
		"scrollbarTrack": "okhsl(237 7% 33%)",
		"scrollbarThumb": "okhsl(232 7% 65%)",
		"searchMatchBg": "okhsl(53 51% 24%)",
		"searchMatchText": "muted",
		"userMessageBg": "blueBg",
		"userMessageText": "text",
		"customMessageBg": "okhsl(295 42% 24%)",
		"customMessageText": "muted",
		"customMessageLabel": "violet",
		"toolPendingBg": "okhsl(229 5% 24%)",
		"toolSuccessBg": "okhsl(158 46% 25%)",
		"toolErrorBg": "okhsl(19 54% 25%)",
		"toolTitle": "text",
		"toolOutput": "muted",
		"mdHeading": "yellow",
		"mdLink": "blue",
		"mdLinkUrl": "muted",
		"mdCode": "violet",
		"mdCodeBlock": "green",
		"mdCodeBlockBorder": "muted",
		"mdQuote": "muted",
		"mdQuoteBorder": "muted",
		"mdHr": "muted",
		"mdListBullet": "violet",
		"toolDiffAdded": "green",
		"toolDiffRemoved": "red",
		"toolDiffContext": "muted",
		"syntaxComment": "muted",
		"syntaxKeyword": "blue",
		"syntaxFunction": "yellow",
		"syntaxVariable": "okhsl(202 58% 67%)",
		"syntaxString": "okhsl(52 67% 67%)",
		"syntaxNumber": "green",
		"syntaxType": "violet",
		"syntaxOperator": "muted",
		"syntaxPunctuation": "muted",
		"thinkingOff": "okhsl(229 8% 49%)",
		"thinkingMinimal": "okhsl(232 20% 52%)",
		"thinkingLow": "okhsl(232 45% 54%)",
		"thinkingMedium": "okhsl(263 59% 56%)",
		"thinkingHigh": "okhsl(295 73% 59%)",
		"thinkingXhigh": "okhsl(337 81% 61%)",
		"thinkingMax": "okhsl(20 99% 63%)",
		"bashMode": "okhsl(159 64% 65%)"
	},
	"export": {
		"pageBg": "okhsl(262 14% 16%)",
		"cardBg": "okhsl(264 13% 19%)",
		"infoBg": "okhsl(53 51% 24%)"
	}
}"##;

/// Embedded `light.json` (values ported verbatim from
/// `packages/coding-agent/src/modes/interactive/theme/light.json`).
const LIGHT_THEME_JSON: &str = r##"{
	"$schema": "https://raw.githubusercontent.com/earendil-works/pi/main/packages/coding-agent/src/modes/interactive/theme/theme-schema.json",
	"name": "light",
	"appearance": "light",
	"vars": {
		"text": "okhsl(225 5% 27%)",
		"muted": "okhsl(229 8% 47%)",
		"violet": "okhsl(295 60% 46%)",
		"blue": "okhsl(231 68% 47%)",
		"green": "okhsl(159 75% 46%)",
		"red": "okhsl(20 91% 47%)",
		"yellow": "okhsl(83 99% 47%)",
		"blueBg": "okhsl(235 19% 91%)"
	},
	"colors": {
		"accent": "violet",
		"border": "okhsl(231 67% 55%)",
		"borderAccent": "okhsl(295 59% 55%)",
		"borderMuted": "okhsl(235 7% 66%)",
		"success": "green",
		"error": "red",
		"warning": "yellow",
		"muted": "muted",
		"dim": "okhsl(229 7% 59%)",
		"text": "text",
		"thinkingText": "okhsl(234 8% 55%)",
		"selectedBg": "blueBg",
		"scrollbarTrack": "okhsl(248 3% 90%)",
		"scrollbarThumb": "okhsl(226 7% 65%)",
		"searchMatchBg": "okhsl(56 22% 91%)",
		"searchMatchText": "muted",
		"userMessageBg": "blueBg",
		"userMessageText": "text",
		"customMessageBg": "okhsl(295 25% 91%)",
		"customMessageText": "muted",
		"customMessageLabel": "violet",
		"toolPendingBg": "okhsl(248 3% 91%)",
		"toolSuccessBg": "okhsl(156 21% 91%)",
		"toolErrorBg": "okhsl(24 23% 91%)",
		"toolTitle": "text",
		"toolOutput": "muted",
		"mdHeading": "yellow",
		"mdLink": "blue",
		"mdLinkUrl": "muted",
		"mdCode": "violet",
		"mdCodeBlock": "green",
		"mdCodeBlockBorder": "muted",
		"mdQuote": "muted",
		"mdQuoteBorder": "muted",
		"mdHr": "muted",
		"mdListBullet": "violet",
		"toolDiffAdded": "green",
		"toolDiffRemoved": "red",
		"toolDiffContext": "muted",
		"syntaxComment": "muted",
		"syntaxKeyword": "blue",
		"syntaxFunction": "yellow",
		"syntaxVariable": "okhsl(203 73% 46%)",
		"syntaxString": "okhsl(52 84% 46%)",
		"syntaxNumber": "green",
		"syntaxType": "violet",
		"syntaxOperator": "muted",
		"syntaxPunctuation": "muted",
		"thinkingOff": "okhsl(223 5% 80%)",
		"thinkingMinimal": "okhsl(229 14% 78%)",
		"thinkingLow": "okhsl(232 33% 76%)",
		"thinkingMedium": "okhsl(264 48% 74%)",
		"thinkingHigh": "okhsl(295 62% 72%)",
		"thinkingXhigh": "okhsl(337 74% 70%)",
		"thinkingMax": "okhsl(20 98% 68%)",
		"bashMode": "okhsl(159 74% 55%)"
	},
	"export": {
		"pageBg": "okhsl(17 3% 94%)",
		"cardBg": "okhsl(17 5% 97%)",
		"infoBg": "okhsl(56 22% 91%)"
	}
}"##;

// ===========================================================================
// Constants
// ===========================================================================

/// The 51 required colour keys in `colors` (theme-schema.json:38-89, in
/// schema required-array order).
pub const REQUIRED_COLOR_KEYS: &[&str] = &[
    // Core UI (11)
    "accent",
    "border",
    "borderAccent",
    "borderMuted",
    "success",
    "error",
    "warning",
    "muted",
    "dim",
    "text",
    "thinkingText",
    // Backgrounds & Content Text (11)
    "selectedBg",
    "userMessageBg",
    "userMessageText",
    "customMessageBg",
    "customMessageText",
    "customMessageLabel",
    "toolPendingBg",
    "toolSuccessBg",
    "toolErrorBg",
    "toolTitle",
    "toolOutput",
    // Markdown (10)
    "mdHeading",
    "mdLink",
    "mdLinkUrl",
    "mdCode",
    "mdCodeBlock",
    "mdCodeBlockBorder",
    "mdQuote",
    "mdQuoteBorder",
    "mdHr",
    "mdListBullet",
    // Tool Diffs (3)
    "toolDiffAdded",
    "toolDiffRemoved",
    "toolDiffContext",
    // Syntax Highlighting (9)
    "syntaxComment",
    "syntaxKeyword",
    "syntaxFunction",
    "syntaxVariable",
    "syntaxString",
    "syntaxNumber",
    "syntaxType",
    "syntaxOperator",
    "syntaxPunctuation",
    // Thinking Level Borders (6)
    "thinkingOff",
    "thinkingMinimal",
    "thinkingLow",
    "thinkingMedium",
    "thinkingHigh",
    "thinkingXhigh",
    // Bash Mode (1)
    "bashMode",
];

/// All allowed keys in the `colors` object (51 required + `thinkingMax`
/// + `scrollbarTrack`/`scrollbarThumb` + `searchMatchBg`/`searchMatchText`).
pub const ALLOWED_COLOR_KEYS: &[&str] = &[
    // Same 51 required keys
    "accent",
    "border",
    "borderAccent",
    "borderMuted",
    "success",
    "error",
    "warning",
    "muted",
    "dim",
    "text",
    "thinkingText",
    "selectedBg",
    "userMessageBg",
    "userMessageText",
    "customMessageBg",
    "customMessageText",
    "customMessageLabel",
    "toolPendingBg",
    "toolSuccessBg",
    "toolErrorBg",
    "toolTitle",
    "toolOutput",
    "mdHeading",
    "mdLink",
    "mdLinkUrl",
    "mdCode",
    "mdCodeBlock",
    "mdCodeBlockBorder",
    "mdQuote",
    "mdQuoteBorder",
    "mdHr",
    "mdListBullet",
    "toolDiffAdded",
    "toolDiffRemoved",
    "toolDiffContext",
    "syntaxComment",
    "syntaxKeyword",
    "syntaxFunction",
    "syntaxVariable",
    "syntaxString",
    "syntaxNumber",
    "syntaxType",
    "syntaxOperator",
    "syntaxPunctuation",
    "thinkingOff",
    "thinkingMinimal",
    "thinkingLow",
    "thinkingMedium",
    "thinkingHigh",
    "thinkingXhigh",
    "bashMode",
    // Optional 52nd key
    "thinkingMax",
    // Optional 53rd key (commit for fullscreen scrollbar, R3.2.3 / theme.ts:53)
    "scrollbarThumb",
    // Optional 54th key (457ae8c79 scrollbar redesign: the track token,
    // a foreground color falling back to `muted`, theme.ts:53 @ 9841914)
    "scrollbarTrack",
    // Optional 55th/56th keys (transcript search, 00121ed99 /
    // theme-schema.json:148-155 @ 9841914)
    "searchMatchBg",
    "searchMatchText",
];

/// Background-colour keys — separated from foreground colours in
/// `create_theme` (theme.ts:602-609 @ 9841914; searchMatchBg in theme.ts:531;
/// `scrollbarThumb` moved to the FOREGROUND key set by the 457ae8c79
/// scrollbar redesign — the thumb glyph is foreground-styled now).
pub const BG_COLOR_KEYS: &[&str] = &[
    "selectedBg",
    "searchMatchBg",
    "userMessageBg",
    "customMessageBg",
    "toolPendingBg",
    "toolSuccessBg",
    "toolErrorBg",
];

/// 6×6×6 colour cube channel values (theme.ts:186).
const CUBE_VALUES: [u32; 6] = [0, 95, 135, 175, 215, 255];

/// Grayscale ramp values (indices 232-255, 24 shades from 8 to 238,
/// theme.ts:189).
const GRAY_VALUES: [u32; 24] = [
    8, 18, 28, 38, 48, 58, 68, 78, 88, 98, 108, 118, 128, 138, 148, 158, 168, 178, 188, 198, 208,
    218, 228, 238,
];

// ===========================================================================
// Terminal introspection byte sequences (actual send/receive is T12)
// ===========================================================================

/// CSI 16t — queries the terminal cell dimensions in pixels (written at
/// `tui.ts:686`, only used by image-capable terminals).
pub const CSI_16T_QUERY: &[u8] = b"\x1b[16t";

/// OSC 9;4;3 — indeterminate progress indicator (iTerm2/WezTerm protocol,
/// `terminal.ts:12`). Sent as `OSC 9;4;3 ST`.
pub const OSC_9_4_INDETERMINATE: &[u8] = b"\x1b]9;4;3\x07";

/// OSC 9;4;0 — clear progress indicator (`terminal.ts:13` @ 4181f66,
/// e8a17822d: no parameter separator after the `0`).
pub const OSC_9_4_CLEAR: &[u8] = b"\x1b]9;4;0\x07";

/// CSI ?2031h — enable terminal colour-scheme change notifications
/// (`tui.ts:675`). The terminal pushes asynchronous `\x1b[?997;Nn` reports
/// when the OS dark/light preference changes.
pub const CSI_2031H_ENABLE: &[u8] = b"\x1b[?2031h";

/// CSI ?2031l — disable colour-scheme change notifications (`tui.ts:675`).
pub const CSI_2031L_DISABLE: &[u8] = b"\x1b[?2031l";

// ===========================================================================
// Types
// ===========================================================================

/// A raw colour value from theme JSON: hex string (`"#ff0000"`), variable
/// reference (`"primary"`), empty string (`""` = terminal default), or 256
/// -colour palette index (0-255).
///
/// Port of `ColorValueSchema` / `ColorValue` (theme.ts:24-29). The
/// [`Raw`](ColorValue::Raw) arm models the eb3e9feed lenient cast: without an
/// installed validator (see [`set_theme_json_validator`]),
/// `loadThemeFromPath` accepts the JSON as-is and any non-string/non-number
/// value flows into `resolveVarRefs`, which crashes with the JS TypeError
/// wording (pinned byte-for-byte by the themes golden).
#[derive(Debug, Clone, PartialEq)]
pub enum ColorValue {
    /// Hex `"#RRGGBB"`, variable reference name, or empty string.
    Str(String),
    /// 256-colour palette index (0-255).
    Index(u32),
    /// Lenient passthrough of an unvalidated JSON value.
    Raw(RawColorValue),
}

/// The unvalidated shapes carried by [`ColorValue::Raw`] (only reachable
/// through the lenient parse path — the installed validator rejects them
/// with structured diagnostics instead).
#[derive(Debug, Clone, PartialEq)]
pub enum RawColorValue {
    /// `undefined` — never parsed from JSON; only arises from the `??`
    /// fallback keys in `with_color_fallbacks` when the source color is
    /// missing (upstream `colors.muted ?? colors.muted` stays `undefined`).
    Undefined,
    /// JSON `null`.
    Null,
    /// JSON `true`/`false`.
    Bool(bool),
    /// A number outside 0-255 (in-range integers parse as
    /// [`ColorValue::Index`]).
    Number(f64),
    /// Arrays and objects.
    Other(serde_json::Value),
}

impl serde::Serialize for ColorValue {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            ColorValue::Str(s) => serializer.serialize_str(s),
            ColorValue::Index(i) => serializer.serialize_u32(*i),
            // Raw values only round-trip on the lenient path; `undefined`
            // has no JSON form and serializes as null.
            ColorValue::Raw(RawColorValue::Undefined) | ColorValue::Raw(RawColorValue::Null) => {
                serializer.serialize_none()
            }
            ColorValue::Raw(RawColorValue::Bool(b)) => serializer.serialize_bool(*b),
            ColorValue::Raw(RawColorValue::Number(n)) => serializer.serialize_f64(*n),
            ColorValue::Raw(RawColorValue::Other(v)) => v.serialize(serializer),
        }
    }
}

impl<'de> serde::Deserialize<'de> for ColorValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        match value {
            serde_json::Value::String(s) => Ok(ColorValue::Str(s)),
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_u64()
                    && i <= 255
                {
                    return Ok(ColorValue::Index(i as u32));
                }
                // Out-of-range/fractional numbers pass through leniently
                // (eb3e9feed: the unvalidated cast keeps them; resolveVarRefs
                // returns numbers as-is and fgAnsi prints the raw index).
                Ok(ColorValue::Raw(RawColorValue::Number(
                    n.as_f64().unwrap_or_default(),
                )))
            }
            // Bools/null/arrays/objects stay raw instead of failing the
            // deserialization — without an installed validator upstream's
            // cast accepts them and the crash happens in resolveVarRefs.
            serde_json::Value::Bool(b) => Ok(ColorValue::Raw(RawColorValue::Bool(b))),
            serde_json::Value::Null => Ok(ColorValue::Raw(RawColorValue::Null)),
            other => Ok(ColorValue::Raw(RawColorValue::Other(other))),
        }
    }
}

/// A resolved colour value after variable-reference resolution.
#[derive(Debug, Clone, PartialEq)]
pub enum ResolvedColor {
    /// Hex colour string `"#RRGGBB"`.
    Hex(String),
    /// 256-colour palette index.
    Index(u32),
    /// Empty string = terminal default colour.
    Empty,
}

/// Colour output mode (theme.ts:165).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorMode {
    TrueColor,
    Color256,
}

/// The colour mode for the current terminal: truecolor unless the terminal
/// (or the `terminal.trueColor` override applied to the rpi-tui
/// capabilities) reports otherwise (`getTerminalColorMode`,
/// terminal-image.ts:172; theme construction consumes it,
/// resource-loader.ts @ ddba59618 #9973).
pub fn terminal_color_mode() -> ColorMode {
    // Single detection source (terminal-image.ts:172 @ a13d35a74): reuse the
    // capability-based decision instead of re-reading the flags.
    match rpi_tui::terminal_image::get_terminal_color_mode() {
        tui_colors::TerminalColorMode::TrueColor => ColorMode::TrueColor,
        tui_colors::TerminalColorMode::Color256 => ColorMode::Color256,
    }
}

/// RGB colour triple.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb {
    pub r: u32,
    pub g: u32,
    pub b: u32,
}

/// Terminal colour scheme: `"dark"` or `"light"` (theme.ts:646).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalTheme {
    Dark,
    Light,
}

impl TerminalTheme {
    /// Returns `"dark"` or `"light"`.
    pub fn as_str(&self) -> &'static str {
        match self {
            TerminalTheme::Dark => "dark",
            TerminalTheme::Light => "light",
        }
    }
}

/// Source of a terminal-theme detection result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalThemeSource {
    TerminalBackground,
    ColorFgBg,
    Fallback,
}

impl TerminalThemeSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            TerminalThemeSource::TerminalBackground => "terminal background",
            TerminalThemeSource::ColorFgBg => "COLORFGBG",
            TerminalThemeSource::Fallback => "fallback",
        }
    }
}

/// Confidence level for terminal-theme detection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalThemeConfidence {
    High,
    Low,
}

/// Result of terminal background colour detection (theme.ts:678-683).
#[derive(Debug, Clone)]
pub struct TerminalThemeDetection {
    pub theme: TerminalTheme,
    pub source: TerminalThemeSource,
    pub detail: String,
    pub confidence: TerminalThemeConfidence,
}

/// Parsed `light/dark` auto-theme setting (theme.ts:648-663).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoThemeSetting {
    pub light_theme: String,
    pub dark_theme: String,
}

/// The `export` section of a theme JSON (theme.ts:96-102).
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThemeExport {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_bg: Option<ColorValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub card_bg: Option<ColorValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub info_bg: Option<ColorValue>,
}

/// Parsed theme JSON (theme.ts:31-103). `name` defaults to an empty string
/// on the lenient path (upstream's unchecked cast leaves it `undefined`;
/// both are only reachable without an installed validator — the validator
/// requires the string).
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ThemeJson {
    #[serde(default)]
    pub name: String,
    /// `appearance` (theme.ts:96-100 @ a13d35a74): the background the theme
    /// is designed for; detected from the theme colors when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub appearance: Option<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub vars: HashMap<String, ColorValue>,
    pub colors: HashMap<String, ColorValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub export: Option<ThemeExport>,
}

/// Resolved export colours for HTML rendering (theme.ts:1057-1085).
#[derive(Debug, Clone, Default)]
pub struct ThemeExportColors {
    pub page_bg: Option<String>,
    pub card_bg: Option<String>,
    pub info_bg: Option<String>,
}

/// Discovered theme metadata (theme.ts:457-460).
#[derive(Debug, Clone)]
pub struct ThemeInfo {
    pub name: String,
    pub path: Option<PathBuf>,
}

/// `ThemeStyle` (theme.ts:123-128 @ a13d35a74): a token name or a concrete
/// color for either slot plus text attributes.
#[derive(Debug, Clone, Default)]
pub struct ThemeStyle {
    pub fg: Option<ThemeStyleColor>,
    pub bg: Option<ThemeStyleColor>,
    pub attributes: tui_colors::TextAttributes,
}

/// Either a theme token name (`ThemeColor`/`ThemeBg`) or a concrete color.
#[derive(Debug, Clone, PartialEq)]
pub enum ThemeStyleColor {
    Token(String),
    Color(tui_colors::Color),
}

/// A constructed theme with pre-computed ANSI escape sequences.
///
/// Port of `class Theme` (theme.ts:330-432 @ a13d35a74). Foreground and
/// background colours are separated because they use different ANSI reset
/// codes (39 vs 49). `colors` holds the concrete color per token (terminal
/// defaults resolved at construction) and `dim_tokens` the tokens rendered
/// faint (SGR 2).
#[derive(Debug, Clone)]
pub struct Theme {
    pub name: Option<String>,
    pub source_path: Option<PathBuf>,
    fg_colors: HashMap<String, String>,
    bg_colors: HashMap<String, String>,
    mode: ColorMode,
    own_appearance: Option<TerminalTheme>,
    colors: HashMap<String, tui_colors::Color>,
    dim_tokens: HashSet<String>,
}

// ===========================================================================
// Colour Utilities
// ===========================================================================

/// Parse `"#RRGGBB"` into [`Rgb`] (theme.ts:171-183).
fn hex_to_rgb(hex: &str) -> Result<Rgb, RpiError> {
    let cleaned = hex.strip_prefix('#').unwrap_or(hex);
    if cleaned.len() != 6 {
        return Err(RpiError::Resource(format!("Invalid hex color: {}", hex)));
    }
    let r = u32::from_str_radix(&cleaned[0..2], 16)
        .map_err(|_| RpiError::Resource(format!("Invalid hex color: {}", hex)))?;
    let g = u32::from_str_radix(&cleaned[2..4], 16)
        .map_err(|_| RpiError::Resource(format!("Invalid hex color: {}", hex)))?;
    let b = u32::from_str_radix(&cleaned[4..6], 16)
        .map_err(|_| RpiError::Resource(format!("Invalid hex color: {}", hex)))?;
    Ok(Rgb { r, g, b })
}

/// Find the index of the nearest cube channel value (theme.ts:191-202).
fn find_closest_cube_index(value: u32) -> usize {
    let mut min_dist = u32::MAX;
    let mut min_idx = 0;
    for (i, cube) in CUBE_VALUES.iter().enumerate() {
        let dist = value.abs_diff(*cube);
        if dist < min_dist {
            min_dist = dist;
            min_idx = i;
        }
    }
    min_idx
}

/// Find the index of the nearest gray ramp value (theme.ts:204-215).
fn find_closest_gray_index(gray: u32) -> usize {
    let mut min_dist = u32::MAX;
    let mut min_idx = 0;
    for (i, g) in GRAY_VALUES.iter().enumerate() {
        let dist = gray.abs_diff(*g);
        if dist < min_dist {
            min_dist = dist;
            min_idx = i;
        }
    }
    min_idx
}

/// Weighted Euclidean colour distance (theme.ts:217-223).
fn color_distance(r1: u32, g1: u32, b1: u32, r2: u32, g2: u32, b2: u32) -> f64 {
    let dr = r1 as f64 - r2 as f64;
    let dg = g1 as f64 - g2 as f64;
    let db = b1 as f64 - b2 as f64;
    dr * dr * 0.299 + dg * dg * 0.587 + db * db * 0.114
}

/// Approximate an RGB colour as a 256-colour palette index (theme.ts:225-256).
///
/// Uses the 6×6×6 colour cube (indices 16-231) and the 24-shade grayscale
/// ramp (indices 232-255). When the colour is nearly neutral (`spread < 10`)
/// and the gray match is closer, the gray index wins; otherwise the cube
/// index preserves tint.
pub fn rgb_to_256(r: u32, g: u32, b: u32) -> u32 {
    let r_idx = find_closest_cube_index(r);
    let g_idx = find_closest_cube_index(g);
    let b_idx = find_closest_cube_index(b);
    let cube_r = CUBE_VALUES[r_idx];
    let cube_g = CUBE_VALUES[g_idx];
    let cube_b = CUBE_VALUES[b_idx];
    let cube_index = 16 + 36 * r_idx as u32 + 6 * g_idx as u32 + b_idx as u32;
    let cube_dist = color_distance(r, g, b, cube_r, cube_g, cube_b);

    let gray = (0.299 * r as f64 + 0.587 * g as f64 + 0.114 * b as f64).round() as u32;
    let gray_idx = find_closest_gray_index(gray);
    let gray_value = GRAY_VALUES[gray_idx];
    let gray_index = 232 + gray_idx as u32;
    let gray_dist = color_distance(r, g, b, gray_value, gray_value, gray_value);

    let max_c = r.max(g).max(b);
    let min_c = r.min(g).min(b);
    let spread = max_c - min_c;

    if spread < 10 && gray_dist < cube_dist {
        gray_index
    } else {
        cube_index
    }
}

/// Convert `"#RRGGBB"` to a 256-colour index (theme.ts:258-261).
pub fn hex_to_256(hex: &str) -> Result<u32, RpiError> {
    let rgb = hex_to_rgb(hex)?;
    Ok(rgb_to_256(rgb.r, rgb.g, rgb.b))
}

/// Convert a 256-colour index to a hex string (theme.ts:978-1016).
pub fn ansi256_to_hex(index: u32) -> String {
    const BASIC_COLORS: [&str; 16] = [
        "#000000", "#800000", "#008000", "#808000", "#000080", "#800080", "#008080", "#c0c0c0",
        "#808080", "#ff0000", "#00ff00", "#ffff00", "#0000ff", "#ff00ff", "#00ffff", "#ffffff",
    ];
    if index < 16 {
        return BASIC_COLORS[index as usize].to_string();
    }
    if index < 232 {
        let cube_index = index - 16;
        let r = cube_index / 36;
        let g = (cube_index % 36) / 6;
        let b = cube_index % 6;
        let to_hex = |n: u32| -> String {
            let val = if n == 0 { 0 } else { 55 + n * 40 };
            format!("{:02x}", val)
        };
        return format!("#{}{}{}", to_hex(r), to_hex(g), to_hex(b));
    }
    let gray = 8 + (index - 232) * 10;
    let gray_hex = format!("{:02x}", gray);
    format!("#{}{}{}", gray_hex, gray_hex, gray_hex)
}

/// Generate the ANSI foreground escape for a resolved colour
/// (theme.ts:263-276).
fn fg_ansi(color: &ResolvedColor, mode: ColorMode) -> Result<String, RpiError> {
    match color {
        ResolvedColor::Empty => Ok("\x1b[39m".to_string()),
        ResolvedColor::Index(i) => Ok(format!("\x1b[38;5;{}m", i)),
        ResolvedColor::Hex(h) => {
            let rgb = resolved_rgb(h)?;
            match mode {
                ColorMode::TrueColor => Ok(format!("\x1b[38;2;{};{};{}m", rgb.r, rgb.g, rgb.b)),
                ColorMode::Color256 => {
                    let idx = rgb_to_256(rgb.r, rgb.g, rgb.b);
                    Ok(format!("\x1b[38;5;{}m", idx))
                }
            }
        }
    }
}

/// Generate the ANSI background escape for a resolved colour
/// (theme.ts:278-291).
fn bg_ansi(color: &ResolvedColor, mode: ColorMode) -> Result<String, RpiError> {
    match color {
        ResolvedColor::Empty => Ok("\x1b[49m".to_string()),
        ResolvedColor::Index(i) => Ok(format!("\x1b[48;5;{}m", i)),
        ResolvedColor::Hex(h) => {
            let rgb = resolved_rgb(h)?;
            match mode {
                ColorMode::TrueColor => Ok(format!("\x1b[48;2;{};{};{}m", rgb.r, rgb.g, rgb.b)),
                ColorMode::Color256 => {
                    let idx = rgb_to_256(rgb.r, rgb.g, rgb.b);
                    Ok(format!("\x1b[48;5;{}m", idx))
                }
            }
        }
    }
}

// ===========================================================================
// Variable Resolution (theme.ts:293-324)
// ===========================================================================

/// Resolve a colour value, following variable references recursively with
/// cycle detection (theme.ts:293-309).
fn resolve_var_refs(
    value: &ColorValue,
    vars: &HashMap<String, ColorValue>,
    visited: &mut HashSet<String>,
) -> Result<ResolvedColor, RpiError> {
    match value {
        ColorValue::Index(i) => Ok(ResolvedColor::Index(*i)),
        ColorValue::Str(s) => {
            if s.is_empty() {
                return Ok(ResolvedColor::Empty);
            }
            if s.starts_with('#') || is_function_color(s) {
                return Ok(ResolvedColor::Hex(s.clone()));
            }
            // Variable reference
            if visited.contains(s) {
                return Err(RpiError::Resource(format!(
                    "Circular variable reference detected: {}",
                    s
                )));
            }
            let referenced = vars.get(s).ok_or_else(|| {
                RpiError::Resource(format!("Variable reference not found: {}", s))
            })?;
            visited.insert(s.clone());
            resolve_var_refs(referenced, vars, visited)
        }
        // Lenient-cast values (eb3e9feed): upstream `resolveVarRefs` reads
        // `value.startsWith("#")` before any string check, so `undefined`/
        // `null` raise "Cannot read properties of … (reading 'startsWith')"
        // and any other non-number/non-string type raises
        // "value.startsWith is not a function" — the JS TypeError wording
        // is pinned byte-for-byte by the themes golden (unvalidated path).
        ColorValue::Raw(RawColorValue::Undefined) => Err(RpiError::Resource(
            "Cannot read properties of undefined (reading 'startsWith')".to_string(),
        )),
        ColorValue::Raw(RawColorValue::Null) => Err(RpiError::Resource(
            "Cannot read properties of null (reading 'startsWith')".to_string(),
        )),
        ColorValue::Raw(RawColorValue::Bool(_)) | ColorValue::Raw(RawColorValue::Other(_)) => Err(
            RpiError::Resource("value.startsWith is not a function".to_string()),
        ),
        // `typeof value === "number"` returns as-is (theme.ts:234); the raw
        // index prints through the 256-colour escape (fgAnsi typeof number,
        // theme.ts:204). Negative/fractional values saturate to the integer
        // index (upstream would print the raw number — an unpinned edge of
        // the lenient path).
        ColorValue::Raw(RawColorValue::Number(n)) => Ok(ResolvedColor::Index(n.max(0.0) as u32)),
    }
}

/// Resolve all colour values in a map (theme.ts:311-320).
fn resolve_theme_colors(
    colors: &HashMap<String, ColorValue>,
    vars: &HashMap<String, ColorValue>,
) -> Result<HashMap<String, ResolvedColor>, RpiError> {
    let mut resolved = HashMap::new();
    for (key, value) in colors {
        let mut visited = HashSet::new();
        resolved.insert(key.clone(), resolve_var_refs(value, vars, &mut visited)?);
    }
    Ok(resolved)
}

/// Apply colour fallbacks (theme.ts:268-277 @ 9841914): `thinkingMax` →
/// `thinkingXhigh`; `scrollbarTrack` → `muted` and `scrollbarThumb` → `text`
/// (457ae8c79: the scrollbar tokens became optional foreground colors);
/// `searchMatchBg`/`searchMatchText` → `selectedBg`/`text` (00121ed99).
fn with_color_fallbacks(mut colors: HashMap<String, ColorValue>) -> HashMap<String, ColorValue> {
    // `colors.<key> ?? colors.<source>` spread semantics (theme.ts:261-277
    // @ 9841914): the key is always created — when the source is missing the
    // value stays `undefined` (only reachable on the lenient path; the
    // installed validator requires every source color).
    fn fallback_to(colors: &mut HashMap<String, ColorValue>, key: &str, source: &str) {
        if colors.contains_key(key) {
            return;
        }
        let value = colors
            .get(source)
            .cloned()
            .unwrap_or(ColorValue::Raw(RawColorValue::Undefined));
        colors.insert(key.to_string(), value);
    }
    fallback_to(&mut colors, "thinkingMax", "thinkingXhigh");
    fallback_to(&mut colors, "scrollbarTrack", "muted");
    fallback_to(&mut colors, "scrollbarThumb", "text");
    fallback_to(&mut colors, "searchMatchBg", "selectedBg");
    fallback_to(&mut colors, "searchMatchText", "text");
    colors
}

// ===========================================================================
// Theme JSON Parsing & Validation (theme.ts:516-595)
// ===========================================================================

/// Check whether a JSON value is a valid colour value (string or integer
/// 0-255).
fn is_valid_color_value(v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::String(_) => true,
        serde_json::Value::Number(n) => n.as_u64().is_some_and(|i| i <= 255),
        _ => false,
    }
}

/// Validate a theme name — must not contain `/` (theme.ts:516-522,
/// schema pattern `^[^/]+$`).
pub fn assert_theme_name_is_valid(name: &str) -> Result<(), RpiError> {
    if name == SYSTEM_THEME_NAME {
        return Err(RpiError::Resource(format!(
            "Invalid theme name \"{}\": \"{}\" is reserved for the generated system theme.",
            name, SYSTEM_THEME_NAME
        )));
    }
    if name.contains('/') {
        return Err(RpiError::Resource(format!(
            "Invalid theme name \"{}\": theme names cannot contain \"/\" because it is reserved for automatic light/dark theme settings.",
            name
        )));
    }
    Ok(())
}

/// Parse and validate a theme JSON value (theme.ts:524-563).
///
/// Collects structured diagnostics for missing colour tokens and other schema
/// errors, then deserialises into [`ThemeJson`].
/// The validator installed through [`set_theme_json_validator`]
/// (theme.ts:29-42, eb3e9feed): full theme-JSON validation with the
/// structured "Invalid theme" diagnostics. A plain `fn` pointer is enough
/// (upstream passes the module function itself, no closures).
pub type ThemeJsonValidator = fn(&str, &serde_json::Value) -> Result<ThemeJson, RpiError>;

static THEME_JSON_VALIDATOR: std::sync::RwLock<Option<ThemeJsonValidator>> =
    std::sync::RwLock::new(None);

/// Install full theme validation (theme.ts:37-42). Without it, documents
/// are accepted as-is, which is what built-in themes already do: validating
/// user-authored JSON is an app-level decision (upstream `pi` installs the
/// validator in `main.ts:1004` during startup, before the first theme
/// loads; the library paths — and the parity goldens — keep the lenient
/// fallback).
pub fn set_theme_json_validator(validator: ThemeJsonValidator) {
    *THEME_JSON_VALIDATOR
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(validator);
}

/// Parse a theme JSON value (theme.ts:488-494 @ 9841914, eb3e9feed):
/// installed validator first; otherwise the lenient fallback only checks
/// for an object with a `"colors"` map and casts the rest through as-is
/// (invalid shapes surface later, in [`resolve_var_refs`], with the JS
/// TypeError wording).
pub fn parse_theme_json(label: &str, value: &serde_json::Value) -> Result<ThemeJson, RpiError> {
    if let Some(validate) = *THEME_JSON_VALIDATOR
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
    {
        return validate(label, value);
    }
    if !value.is_object() || !value.as_object().is_some_and(|o| o.contains_key("colors")) {
        return Err(RpiError::Resource(format!(
            "Invalid theme \"{label}\": expected an object with a \"colors\" map."
        )));
    }
    let theme_json: ThemeJson = serde_json::from_value(value.clone())
        .map_err(|e| RpiError::Resource(format!("Invalid theme \"{label}\": {e}")))?;
    Ok(theme_json)
}

/// Validate one theme document, throwing a message that names the offending
/// tokens (theme-json.ts:103-146 @ 9841914, eb3e9feed — the typebox schema
/// and its error formatting moved out of `theme.ts`; the diagnostic text is
/// unchanged from the pre-split inline validation).
pub fn validate_theme_json(label: &str, value: &serde_json::Value) -> Result<ThemeJson, RpiError> {
    let mut missing_colors: Vec<String> = Vec::new();
    let mut other_errors: Vec<String> = Vec::new();

    // Check top-level allowed keys (additionalProperties: false)
    const ALLOWED_TOP_KEYS: &[&str] = &["$schema", "name", "vars", "colors", "export"];
    if let Some(obj) = value.as_object() {
        for key in obj.keys() {
            if !ALLOWED_TOP_KEYS.contains(&key.as_str()) {
                other_errors.push(format!("  - /{}: additional property not allowed", key));
            }
        }
    }

    // Check name
    match value.get("name") {
        Some(serde_json::Value::String(_)) => {}
        Some(_) => other_errors.push("  - /name: expected string".to_string()),
        None => other_errors.push("  - : missing required property: name".to_string()),
    }

    // Check colors
    match value.get("colors") {
        None => {
            other_errors.push("  - : missing required property: colors".to_string());
            for key in REQUIRED_COLOR_KEYS {
                missing_colors.push((*key).to_string());
            }
        }
        Some(_) if !value["colors"].is_object() => {
            other_errors.push("  - /colors: expected object".to_string());
            for key in REQUIRED_COLOR_KEYS {
                missing_colors.push((*key).to_string());
            }
        }
        Some(colors_val) => {
            // Invariant: the `!is_object()` arm above already rejected
            // non-object `colors` values, so this is an object.
            let obj = colors_val.as_object().unwrap();
            // Check required keys
            for key in REQUIRED_COLOR_KEYS {
                if !obj.contains_key(*key) {
                    missing_colors.push((*key).to_string());
                }
            }
            // Check allowed keys (additionalProperties: false)
            for key in obj.keys() {
                if !ALLOWED_COLOR_KEYS.contains(&key.as_str()) {
                    other_errors.push(format!(
                        "  - /colors/{}: additional property not allowed",
                        key
                    ));
                }
            }
            // Check value types
            for (key, val) in obj {
                if !is_valid_color_value(val) {
                    other_errors.push(format!(
                        "  - /colors/{}: expected string or integer 0-255",
                        key
                    ));
                }
            }
        }
    }

    // Check vars types if present
    if let Some(vars) = value.get("vars").and_then(|v| v.as_object()) {
        for (key, val) in vars {
            if !is_valid_color_value(val) {
                other_errors.push(format!(
                    "  - /vars/{}: expected string or integer 0-255",
                    key
                ));
            }
        }
    }

    // Check export if present
    if let Some(export) = value.get("export") {
        if !export.is_object() {
            other_errors.push("  - /export: expected object".to_string());
        } else if let Some(obj) = export.as_object() {
            const ALLOWED_EXPORT_KEYS: &[&str] = &["pageBg", "cardBg", "infoBg"];
            for key in obj.keys() {
                if !ALLOWED_EXPORT_KEYS.contains(&key.as_str()) {
                    other_errors.push(format!(
                        "  - /export/{}: additional property not allowed",
                        key
                    ));
                }
            }
            for (key, val) in obj {
                if !is_valid_color_value(val) {
                    other_errors.push(format!(
                        "  - /export/{}: expected string or integer 0-255",
                        key
                    ));
                }
            }
        }
    }

    if !missing_colors.is_empty() || !other_errors.is_empty() {
        let mut msg = format!("Invalid theme \"{}\":\n", label);
        if !missing_colors.is_empty() {
            msg.push_str("\nMissing required color tokens:\n");
            let mut sorted = missing_colors.clone();
            sorted.sort();
            for color in &sorted {
                msg.push_str(&format!("  - {}\n", color));
            }
            msg.push_str("\nPlease add these colors to your theme's \"colors\" object.");
            msg.push_str("\nSee the built-in themes (dark.json, light.json) for reference values.");
        }
        if !other_errors.is_empty() {
            msg.push_str(&format!("\n\nOther errors:\n{}", other_errors.join("\n")));
        }
        return Err(RpiError::Resource(msg));
    }

    let theme_json: ThemeJson = serde_json::from_value(value.clone())
        .map_err(|e| RpiError::Resource(format!("Invalid theme \"{}\": {}", label, e)))?;

    assert_theme_name_is_valid(&theme_json.name)?;
    Ok(theme_json)
}

/// Parse theme JSON from a string (theme.ts:565-573).
pub fn parse_theme_json_content(label: &str, content: &str) -> Result<ThemeJson, RpiError> {
    let json: serde_json::Value = serde_json::from_str(content)
        .map_err(|e| RpiError::Resource(format!("Failed to parse theme {}: {}", label, e)))?;
    parse_theme_json(label, &json)
}

// ===========================================================================
// Theme Construction & Loading (theme.ts:440-636)
// ===========================================================================

static BUILTIN_THEMES: OnceLock<HashMap<String, ThemeJson>> = OnceLock::new();

/// Lazily parsed built-in dark/light themes (theme.ts:440-451). Built-in
/// themes are never validated (eb3e9feed: upstream `loadThemeJson` returns
/// them raw; validation is only for user-authored documents), so the JSON
/// deserializes directly — no `parse_theme_json`/validator round-trip.
pub fn get_builtin_themes() -> &'static HashMap<String, ThemeJson> {
    BUILTIN_THEMES.get_or_init(|| {
        let mut themes = HashMap::new();
        let dark_val: serde_json::Value = serde_json::from_str(DARK_THEME_JSON)
            .expect("built-in dark theme JSON is verified valid at development time");
        themes.insert(
            "dark".to_string(),
            serde_json::from_value(dark_val)
                .expect("built-in dark theme is verified valid at development time"),
        );
        let light_val: serde_json::Value = serde_json::from_str(LIGHT_THEME_JSON)
            .expect("built-in light theme JSON is verified valid at development time");
        themes.insert(
            "light".to_string(),
            serde_json::from_value(light_val)
                .expect("built-in light theme is verified valid at development time"),
        );
        themes
    })
}

/// Construct a [`Theme`] from parsed JSON (theme.ts:597-621).
pub fn create_theme(
    theme_json: &ThemeJson,
    mode: Option<ColorMode>,
    source_path: Option<&Path>,
) -> Result<Theme, RpiError> {
    let color_mode = mode.unwrap_or(ColorMode::TrueColor);
    let colors = with_color_fallbacks(theme_json.colors.clone());

    let resolved = resolve_theme_colors(&colors, &theme_json.vars)?;
    let appearance = match theme_json.appearance.as_deref() {
        Some("dark") => Some(TerminalTheme::Dark),
        Some("light") => Some(TerminalTheme::Light),
        Some(other) => {
            return Err(RpiError::Resource(format!(
                "Invalid theme \"{}\": appearance must be \"dark\" or \"light\", got \"{}\". See the built-in themes (dark.json, light.json) for reference values.",
                theme_json.name, other
            )));
        }
        None => None,
    };
    Theme::from_resolved(
        resolved,
        color_mode,
        Some(theme_json.name.clone()),
        source_path.map(Path::to_path_buf),
        appearance,
        Vec::new(),
    )
}

/// Load and construct a theme from a file path (theme.ts:623-627).
pub fn load_theme_from_path(path: &Path, mode: Option<ColorMode>) -> Result<Theme, RpiError> {
    let content = std::fs::read_to_string(path).map_err(|e| {
        RpiError::Resource(format!("Failed to read theme {}: {}", path.display(), e))
    })?;
    let theme_json = parse_theme_json_content(&path.display().to_string(), &content)?;
    create_theme(&theme_json, mode, Some(path))
}

/// Load raw [`ThemeJson`] by name from built-in or global custom themes
/// (theme.ts:575-595).
///
/// Priority: built-in → global custom themes dir.
/// Registered themes (packages) will be added by the resource loader (T17+).
pub fn load_theme_json(name: &str) -> Result<ThemeJson, RpiError> {
    // 1. Built-in themes
    if let Some(json) = get_builtin_themes().get(name) {
        return Ok(json.clone());
    }
    // 2. (placeholder for registered themes — T17+ resource loader)
    // 3. Custom themes from global themes dir
    let theme_path = config::get_global_themes_dir().join(format!("{}.json", name));
    if !theme_path.exists() {
        return Err(RpiError::Resource(format!("Theme not found: {}", name)));
    }
    let content = std::fs::read_to_string(&theme_path)
        .map_err(|e| RpiError::Resource(format!("Failed to read theme {}: {}", name, e)))?;
    parse_theme_json_content(name, &content)
}

/// Load and construct a [`Theme`] by name (theme.ts:629-636).
pub fn load_theme(name: &str, mode: Option<ColorMode>) -> Result<Theme, RpiError> {
    // The system theme name is reserved: it takes precedence over custom
    // themes of the same name (theme.ts:629-636 @ a13d35a74).
    if name == SYSTEM_THEME_NAME {
        return Ok(create_system_theme(mode));
    }
    let theme_json = load_theme_json(name)?;
    create_theme(&theme_json, mode, None)
}

/// Load a theme by name, returning `None` on any error (theme.ts:638-644).
pub fn get_theme_by_name(name: &str) -> Option<Theme> {
    load_theme(name, None).ok()
}

/// Discover all available themes (theme.ts:462-514).
///
/// Priority: built-in → global custom themes.
pub fn get_available_themes() -> Vec<ThemeInfo> {
    let mut result: Vec<ThemeInfo> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    // The system theme is generated, so it has no file; it comes first
    // (theme.ts:470-514 @ a13d35a74).
    seen.insert(SYSTEM_THEME_NAME.to_string());
    result.push(ThemeInfo {
        name: SYSTEM_THEME_NAME.to_string(),
        path: None,
    });

    // Built-in themes
    for name in get_builtin_themes().keys() {
        if seen.insert(name.clone()) {
            result.push(ThemeInfo {
                name: name.clone(),
                path: None,
            });
        }
    }

    // Custom themes from global themes dir
    let themes_dir = config::get_global_themes_dir();
    if themes_dir.exists()
        && let Ok(entries) = std::fs::read_dir(&themes_dir)
    {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Ok(content) = std::fs::read_to_string(&path)
                && let Ok(theme_json) =
                    parse_theme_json_content(&path.display().to_string(), &content)
                && seen.insert(theme_json.name.clone())
            {
                result.push(ThemeInfo {
                    name: theme_json.name,
                    path: Some(path),
                });
            }
        }
    }

    // The system theme comes first: it is the default and adapts to every
    // terminal.
    result.sort_by(
        |a, b| match (a.name == SYSTEM_THEME_NAME, b.name == SYSTEM_THEME_NAME) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.name.cmp(&b.name),
        },
    );
    result
}

/// Default theme name based on environment detection (theme.ts:791-793).
pub fn get_default_theme() -> TerminalTheme {
    detect_terminal_background_from_env().theme
}

/// The raw default (dark) theme JSON — the value `ctx.ui.theme` returns on
/// bridges without theme access (upstream returns the statically imported
/// default theme object, rpc-mode.ts:283-285 / runner.ts:256-258).
pub fn default_theme_json() -> serde_json::Value {
    serde_json::from_str(DARK_THEME_JSON).expect("built-in dark theme JSON is valid")
}

/// A theme's raw JSON value by name (for `ctx.ui.theme` / `getTheme`).
pub fn theme_json_value(name: &str) -> Option<serde_json::Value> {
    load_theme_json(name)
        .ok()
        .and_then(|theme| serde_json::to_value(theme).ok())
}

// ===========================================================================
// HTML Export Helpers (theme.ts:1022-1085)
// ===========================================================================

/// Resolve theme colours as CSS-compatible hex strings
/// (theme.ts:1022-1043).
pub fn get_resolved_theme_colors(theme_name: &str) -> Result<HashMap<String, String>, RpiError> {
    let is_light = theme_name == "light";
    let theme_json = load_theme_json(theme_name)?;
    let colors = with_color_fallbacks(theme_json.colors.clone());
    let resolved = resolve_theme_colors(&colors, &theme_json.vars)?;
    let default_text = if is_light { "#000000" } else { "#e5e5e7" };
    let mut css_colors = HashMap::new();
    for (key, color) in &resolved {
        let css = match color {
            ResolvedColor::Index(i) => ansi256_to_hex(*i),
            ResolvedColor::Empty => default_text.to_string(),
            // CSS custom properties take hex; OKLCH/OKHSL values (and #rgb)
            // go through `colorToHex(parseColor(...))` (theme.ts:1022-1043).
            ResolvedColor::Hex(h) => resolved_color_to_hex(h),
        };
        css_colors.insert(key.clone(), css);
    }
    Ok(css_colors)
}

/// Check if a theme is a light theme (theme.ts:1048-1051 @ a13d35a74):
/// loads the theme and compares its appearance.
pub fn is_light_theme(theme_name: &str) -> bool {
    get_theme_by_name(theme_name)
        .map(|theme| theme.appearance() == TerminalTheme::Light)
        .unwrap_or(false)
}

/// Get explicit export colours from a theme (theme.ts:1057-1085).
pub fn get_theme_export_colors(theme_name: &str) -> ThemeExportColors {
    // The generated system theme has no export colors (theme.ts:1057-1085).
    if theme_name == SYSTEM_THEME_NAME {
        return ThemeExportColors::default();
    }
    let theme_json = match load_theme_json(theme_name) {
        Ok(j) => j,
        Err(_) => return ThemeExportColors::default(),
    };
    let export_section = match &theme_json.export {
        Some(e) => e,
        None => return ThemeExportColors::default(),
    };
    let vars = &theme_json.vars;
    let resolve = |value: Option<&ColorValue>| -> Option<String> {
        let value = value?;
        let mut visited = HashSet::new();
        let resolved = resolve_var_refs(value, vars, &mut visited).ok()?;
        match resolved {
            ResolvedColor::Index(i) => Some(ansi256_to_hex(i)),
            ResolvedColor::Empty => None,
            // Export colors end up in CSS, which understands hex and
            // oklch() directly but not okhsl() (theme.ts:1070-1082).
            ResolvedColor::Hex(h) => Some(resolved_color_to_hex(&h)),
        }
    };
    ThemeExportColors {
        page_bg: resolve(export_section.page_bg.as_ref()),
        card_bg: resolve(export_section.card_bg.as_ref()),
        info_bg: resolve(export_section.info_bg.as_ref()),
    }
}

// ===========================================================================
// Auto Theme (theme.ts:648-676)
// ===========================================================================

/// Parse a `"light/dark"` auto-theme setting (theme.ts:648-663).
///
/// The setting must contain exactly one `/`. Both halves are trimmed and must
/// be non-empty. Returns `None` for zero, two, or more slashes, or for empty
/// halves.
pub fn parse_auto_theme_setting(theme_setting: Option<&str>) -> Option<AutoThemeSetting> {
    let setting = theme_setting?;
    let slash_index = setting.find('/')?;
    // Must have exactly one '/'
    if setting[slash_index + 1..].contains('/') {
        return None;
    }
    let light_theme = setting[..slash_index].trim().to_string();
    let dark_theme = setting[slash_index + 1..].trim().to_string();
    if light_theme.is_empty() || dark_theme.is_empty() {
        return None;
    }
    Some(AutoThemeSetting {
        light_theme,
        dark_theme,
    })
}

/// Resolve a theme setting against the detected terminal theme
/// (theme.ts:665-676).
pub fn resolve_theme_setting(
    theme_setting: Option<&str>,
    terminal_theme: TerminalTheme,
) -> Option<String> {
    if let Some(auto) = parse_auto_theme_setting(theme_setting) {
        return Some(if terminal_theme == TerminalTheme::Light {
            auto.light_theme
        } else {
            auto.dark_theme
        });
    }
    // If the setting contains '/' but wasn't valid auto format → None
    if theme_setting.is_some_and(|s| s.contains('/')) {
        return None;
    }
    theme_setting.map(|s| s.to_string())
}

// ===========================================================================
// Terminal Background Detection (pure functions)
// ===========================================================================

/// sRGB → linear conversion for one channel (theme.ts:719-722).
/// `detectTerminalBackgroundFromEnv` (theme.ts:734-753 @ 9841914, superseded
/// by `detectTerminalTheme` @ a13d35a74): the `COLORFGBG` index classified
/// like Vim, falling back to dark.
pub fn detect_terminal_background_from_env() -> TerminalThemeDetection {
    let colorfgbg = std::env::var("COLORFGBG").unwrap_or_default();
    detect_terminal_background_from_env_str(&colorfgbg)
}

/// Pure form of [`detect_terminal_background_from_env`]; does not read
/// `std::env`.
pub fn detect_terminal_background_from_env_str(colorfgbg: &str) -> TerminalThemeDetection {
    if let Some(theme) = detect_color_fg_bg_theme(Some(colorfgbg)) {
        return TerminalThemeDetection {
            theme,
            source: TerminalThemeSource::ColorFgBg,
            detail: format!("COLORFGBG {}", colorfgbg),
            confidence: TerminalThemeConfidence::High,
        };
    }
    TerminalThemeDetection {
        theme: TerminalTheme::Dark,
        source: TerminalThemeSource::Fallback,
        detail: "no terminal background hint found".to_string(),
        confidence: TerminalThemeConfidence::Low,
    }
}

// ===========================================================================
// Hot Reload Path (theme.ts:886-957)
// ===========================================================================

/// Determine which file path to watch for hot-reload (theme.ts:886-902).
///
/// Only custom (non-built-in) themes in the global themes directory are
/// watched. Returns `None` for built-in themes (`dark`, `light`) or when the
/// file does not exist. The watcher itself is wired up in T12.
pub fn get_theme_watch_path(theme_name: &str) -> Option<PathBuf> {
    if theme_name == "dark" || theme_name == "light" {
        return None;
    }
    let theme_file = config::get_global_themes_dir().join(format!("{}.json", theme_name));
    if theme_file.exists() {
        Some(theme_file)
    } else {
        None
    }
}

// ===========================================================================
// Theme struct methods
// ===========================================================================

/// Convert a resolved color into the concrete color model.
fn resolved_to_color(color: &ResolvedColor) -> Option<tui_colors::Color> {
    match color {
        ResolvedColor::Empty => None,
        ResolvedColor::Index(index) => tui_colors::indexed_color(i64::from(*index)).ok(),
        ResolvedColor::Hex(value) => {
            let value = if value.starts_with('#') && value.len() == 4 {
                resolved_color_to_hex(value)
            } else {
                value.clone()
            };
            tui_colors::parse_color(&value).ok()
        }
    }
}

/// A terminal-reported RGB color as the concrete color model.
fn terminal_rgb_color(rgb: TerminalRgb) -> tui_colors::Color {
    tui_colors::Color::Rgb(tui_colors::Rgb {
        r: f64::from(rgb.r),
        g: f64::from(rgb.g),
        b: f64::from(rgb.b),
    })
}

/// `detectAppearance` (theme.ts:436-445 @ a13d35a74): the background a theme
/// is designed for, from the lightness of its own colors. Palette indices
/// 0-15 follow the user's terminal and say nothing about the theme.
fn detect_appearance(
    foregrounds: &[tui_colors::Color],
    backgrounds: &[tui_colors::Color],
) -> Option<TerminalTheme> {
    fn average_lightness(colors: &[tui_colors::Color]) -> Option<f64> {
        let fixed: Vec<&tui_colors::Color> = colors
            .iter()
            .filter(|color| !matches!(color, tui_colors::Color::Indexed(index) if *index < 16))
            .collect();
        if fixed.is_empty() {
            return None;
        }
        Some(
            fixed
                .iter()
                .map(|color| tui_colors::color_to_oklch(color).l)
                .sum::<f64>()
                / fixed.len() as f64,
        )
    }
    let foreground = average_lightness(foregrounds);
    let background = average_lightness(backgrounds);
    match (foreground, background) {
        (Some(foreground), Some(background)) => Some(if background < foreground {
            TerminalTheme::Dark
        } else {
            TerminalTheme::Light
        }),
        (None, Some(background)) => Some(if background < 0.5 {
            TerminalTheme::Dark
        } else {
            TerminalTheme::Light
        }),
        (Some(foreground), None) => Some(if foreground > 0.5 {
            TerminalTheme::Dark
        } else {
            TerminalTheme::Light
        }),
        (None, None) => None,
    }
}

impl Theme {
    /// Build a theme from resolved colors (shared by JSON themes and the
    /// generated system theme). Terminal defaults for `""` tokens and the
    /// faint (SGR 2) mixing happen here (theme.ts:322-337).
    fn from_resolved(
        resolved: HashMap<String, ResolvedColor>,
        mode: ColorMode,
        name: Option<String>,
        source_path: Option<PathBuf>,
        appearance: Option<TerminalTheme>,
        dim: Vec<String>,
    ) -> Result<Theme, RpiError> {
        let bg_set: HashSet<&str> = BG_COLOR_KEYS.iter().copied().collect();
        let mut fg_colors: HashMap<String, String> = HashMap::new();
        let mut bg_colors: HashMap<String, String> = HashMap::new();
        let mut colors: HashMap<String, tui_colors::Color> = HashMap::new();
        let mut concrete_foregrounds: Vec<tui_colors::Color> = Vec::new();
        let mut concrete_backgrounds: Vec<tui_colors::Color> = Vec::new();
        let mut default_foreground_tokens: Vec<String> = Vec::new();
        let mut default_background_tokens: Vec<String> = Vec::new();
        for (key, color) in &resolved {
            let is_background = bg_set.contains(key.as_str());
            if is_background {
                bg_colors.insert(key.clone(), bg_ansi(color, mode)?);
            } else {
                fg_colors.insert(key.clone(), fg_ansi(color, mode)?);
            }
            match color {
                ResolvedColor::Empty => {
                    if is_background {
                        default_background_tokens.push(key.clone());
                    } else {
                        default_foreground_tokens.push(key.clone());
                    }
                }
                _ => {
                    if let Some(concrete) = resolved_to_color(color) {
                        if is_background {
                            concrete_backgrounds.push(concrete);
                        } else {
                            concrete_foregrounds.push(concrete);
                        }
                        colors.insert(key.clone(), concrete);
                    }
                }
            }
        }
        let own_appearance =
            appearance.or_else(|| detect_appearance(&concrete_foregrounds, &concrete_backgrounds));
        // Terminal defaults for "" tokens: the reported colors, or a guess
        // based on the appearance (theme.ts:306-317).
        let terminal = get_terminal_colors();
        let guessed_appearance = own_appearance.unwrap_or_else(get_terminal_theme);
        let (guessed_foreground, guessed_background) = match guessed_appearance {
            TerminalTheme::Dark => (
                tui_colors::Color::Rgb(tui_colors::Rgb {
                    r: 229.0,
                    g: 229.0,
                    b: 231.0,
                }),
                tui_colors::Color::Rgb(tui_colors::Rgb {
                    r: 0.0,
                    g: 0.0,
                    b: 0.0,
                }),
            ),
            TerminalTheme::Light => (
                tui_colors::Color::Rgb(tui_colors::Rgb {
                    r: 0.0,
                    g: 0.0,
                    b: 0.0,
                }),
                tui_colors::Color::Rgb(tui_colors::Rgb {
                    r: 255.0,
                    g: 255.0,
                    b: 255.0,
                }),
            ),
        };
        let foreground = terminal
            .foreground
            .map(terminal_rgb_color)
            .unwrap_or(guessed_foreground);
        let background = terminal
            .background
            .map(terminal_rgb_color)
            .unwrap_or(guessed_background);
        for token in default_foreground_tokens {
            colors.insert(token, foreground);
        }
        for token in default_background_tokens {
            colors.insert(token, background);
        }
        let dim_tokens: HashSet<String> = dim.into_iter().collect();
        for token in &dim_tokens {
            if let Some(color) = colors.get(token).copied()
                && let Ok(mixed) = tui_colors::mix_colors(
                    &color,
                    &background,
                    0.4,
                    tui_colors::ColorMixSpace::Oklch,
                )
            {
                colors.insert(token.clone(), mixed);
            }
        }
        Ok(Theme {
            name,
            source_path,
            fg_colors,
            bg_colors,
            mode,
            own_appearance,
            colors,
            dim_tokens,
        })
    }

    /// Wrap text in a foreground colour (theme.ts:359-363); faint tokens add
    /// SGR 2 and close it with the color reset.
    pub fn fg(&self, color: &str, text: &str) -> String {
        let ansi = self.fg_colors.get(color).map(|s| s.as_str()).unwrap_or("");
        if self.dim_tokens.contains(color) {
            format!("{ansi}\x1b[2m{text}\x1b[22;39m")
        } else {
            format!("{ansi}{text}\x1b[39m")
        }
    }
    /// Wrap text in a background colour (theme.ts:365-369).
    pub fn bg(&self, color: &str, text: &str) -> String {
        let ansi = self.bg_colors.get(color).map(|s| s.as_str()).unwrap_or("");
        format!("{}{}\x1b[49m", ansi, text)
    }

    /// Raw ANSI foreground prefix for a colour (theme.ts:391-395); faint
    /// tokens include SGR 2.
    pub fn get_fg_ansi(&self, color: &str) -> String {
        let ansi = self.fg_colors.get(color).cloned().unwrap_or_default();
        if self.dim_tokens.contains(color) {
            format!("{ansi}\x1b[2m")
        } else {
            ansi
        }
    }

    /// Raw ANSI background prefix for a colour (theme.ts:397-401).
    pub fn get_bg_ansi(&self, color: &str) -> &str {
        self.bg_colors.get(color).map(|s| s.as_str()).unwrap_or("")
    }

    /// Current colour mode (theme.ts:403-405).
    pub fn get_color_mode(&self) -> ColorMode {
        self.mode
    }

    /// `theme.appearance` (theme.ts:313-315): declared in the theme JSON or
    /// detected from its colors, falling back to the terminal's appearance.
    pub fn appearance(&self) -> TerminalTheme {
        self.own_appearance.unwrap_or_else(get_terminal_theme)
    }

    /// `theme.colors` (theme.ts:322): concrete colors for all tokens.
    pub fn colors(&self) -> &HashMap<String, tui_colors::Color> {
        &self.colors
    }

    /// `theme.style(text, options)` (theme.ts:342-357): resolve tokens or
    /// concrete colors and apply the attributes with reverse-order resets.
    pub fn style(&self, text: &str, options: &ThemeStyle) -> String {
        let mut attributes = options.attributes;
        let fg_ansi = match &options.fg {
            Some(ThemeStyleColor::Token(token)) => {
                if self.dim_tokens.contains(token) {
                    attributes.dim = true;
                }
                Some(self.get_fg_ansi(token))
            }
            Some(ThemeStyleColor::Color(color)) => Some(tui_colors::foreground_ansi(
                color,
                tui_color_mode(self.mode),
            )),
            None => None,
        };
        let bg_ansi = match &options.bg {
            Some(ThemeStyleColor::Token(token)) => Some(self.get_bg_ansi(token).to_string()),
            Some(ThemeStyleColor::Color(color)) => Some(tui_colors::background_ansi(
                color,
                tui_color_mode(self.mode),
            )),
            None => None,
        };
        tui_colors::style_text_with_ansi(text, fg_ansi.as_deref(), bg_ansi.as_deref(), &attributes)
    }

    /// Map a thinking-level string to its border colour name
    /// (theme.ts:407-427).
    pub fn thinking_border_color_name(level: &str) -> &'static str {
        match level {
            "off" => "thinkingOff",
            "minimal" => "thinkingMinimal",
            "low" => "thinkingLow",
            "medium" => "thinkingMedium",
            "high" => "thinkingHigh",
            "xhigh" => "thinkingXhigh",
            "max" => "thinkingMax",
            _ => "thinkingOff",
        }
    }

    /// Bash-mode border colour name (theme.ts:429-431).
    pub fn bash_mode_border_color_name() -> &'static str {
        "bashMode"
    }

    /// Bold text (chalk.bold equivalent).
    pub fn bold(text: &str) -> String {
        format!("\x1b[1m{}\x1b[22m", text)
    }

    /// Italic text (chalk.italic equivalent).
    pub fn italic(text: &str) -> String {
        format!("\x1b[3m{}\x1b[23m", text)
    }

    /// Underlined text (chalk.underline equivalent).
    pub fn underline(text: &str) -> String {
        format!("\x1b[4m{}\x1b[24m", text)
    }

    /// Inverse video (chalk.inverse equivalent).
    pub fn inverse(text: &str) -> String {
        format!("\x1b[7m{}\x1b[27m", text)
    }

    /// Strikethrough (chalk.strikethrough equivalent).
    pub fn strikethrough(text: &str) -> String {
        format!("\x1b[9m{}\x1b[29m", text)
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // --- Colour utilities -------------------------------------------------

    #[test]
    fn test_hex_to_rgb() {
        let rgb = hex_to_rgb("#ff0000").unwrap();
        assert_eq!(rgb, Rgb { r: 255, g: 0, b: 0 });

        let rgb = hex_to_rgb("#00d7ff").unwrap();
        assert_eq!(
            rgb,
            Rgb {
                r: 0,
                g: 215,
                b: 255
            }
        );
    }

    #[test]
    fn test_hex_to_rgb_invalid() {
        assert!(hex_to_rgb("#ff").is_err());
        assert!(hex_to_rgb("#gggggg").is_err());
        assert!(hex_to_rgb("").is_err());
    }

    #[test]
    fn test_rgb_to_256_white() {
        // #ffffff → cube index 231 (255,255,255)
        assert_eq!(rgb_to_256(255, 255, 255), 231);
    }

    #[test]
    fn test_rgb_to_256_black() {
        // #000000 → cube index 16 (0,0,0)
        assert_eq!(rgb_to_256(0, 0, 0), 16);
    }

    #[test]
    fn test_rgb_to_256_saturated_red() {
        // #cc6666 → cube (spread >= 10, no gray consideration)
        // r=204→215(idx4), g=102→95(idx1), b=102→95(idx1)
        // cubeIndex = 16 + 36*4 + 6*1 + 1 = 167
        assert_eq!(rgb_to_256(204, 102, 102), 167);
    }

    #[test]
    fn test_rgb_to_256_near_neutral_gray() {
        // #969696 → gray (spread=0 < 10, gray closer than cube)
        // gray=150, closest gray=148(idx14), grayIndex=246
        assert_eq!(rgb_to_256(150, 150, 150), 246);
    }

    #[test]
    fn test_ansi256_to_hex_roundtrip_basic() {
        assert_eq!(ansi256_to_hex(0), "#000000");
        assert_eq!(ansi256_to_hex(15), "#ffffff");
        assert_eq!(ansi256_to_hex(7), "#c0c0c0");
    }

    #[test]
    fn test_ansi256_to_hex_cube() {
        // Index 16 = cube 0,0,0 → #000000
        assert_eq!(ansi256_to_hex(16), "#000000");
        // Index 231 = cube 5,5,5 → #ffffff
        assert_eq!(ansi256_to_hex(231), "#ffffff");
        // Index 196 = cube 5,0,0 → #ff0000
        assert_eq!(ansi256_to_hex(196), "#ff0000");
    }

    #[test]
    fn test_ansi256_to_hex_grayscale() {
        // Index 232 = gray 8 → #080808
        assert_eq!(ansi256_to_hex(232), "#080808");
        // Index 255 = gray 238 → #eeeeee
        assert_eq!(ansi256_to_hex(255), "#eeeeee");
    }

    #[test]
    fn test_fg_ansi_truecolor() {
        let hex = ResolvedColor::Hex("#ff0000".to_string());
        assert_eq!(
            fg_ansi(&hex, ColorMode::TrueColor).unwrap(),
            "\x1b[38;2;255;0;0m"
        );
    }

    #[test]
    fn test_fg_ansi_256() {
        let hex = ResolvedColor::Hex("#ffffff".to_string());
        assert_eq!(
            fg_ansi(&hex, ColorMode::Color256).unwrap(),
            "\x1b[38;5;231m"
        );
    }

    #[test]
    fn test_fg_ansi_empty() {
        assert_eq!(
            fg_ansi(&ResolvedColor::Empty, ColorMode::TrueColor).unwrap(),
            "\x1b[39m"
        );
    }

    #[test]
    fn test_fg_ansi_index() {
        assert_eq!(
            fg_ansi(&ResolvedColor::Index(39), ColorMode::TrueColor).unwrap(),
            "\x1b[38;5;39m"
        );
    }

    #[test]
    fn test_bg_ansi_truecolor() {
        let hex = ResolvedColor::Hex("#3a3a4a".to_string());
        assert_eq!(
            bg_ansi(&hex, ColorMode::TrueColor).unwrap(),
            "\x1b[48;2;58;58;74m"
        );
    }

    #[test]
    fn test_bg_ansi_empty() {
        assert_eq!(
            bg_ansi(&ResolvedColor::Empty, ColorMode::TrueColor).unwrap(),
            "\x1b[49m"
        );
    }

    // --- Variable resolution ----------------------------------------------

    #[test]
    fn test_resolve_var_refs_hex() {
        let result = resolve_var_refs(
            &ColorValue::Str("#ff0000".to_string()),
            &HashMap::new(),
            &mut HashSet::new(),
        )
        .unwrap();
        assert_eq!(result, ResolvedColor::Hex("#ff0000".to_string()));
    }

    #[test]
    fn test_resolve_var_refs_empty() {
        let result = resolve_var_refs(
            &ColorValue::Str(String::new()),
            &HashMap::new(),
            &mut HashSet::new(),
        )
        .unwrap();
        assert_eq!(result, ResolvedColor::Empty);
    }

    #[test]
    fn test_resolve_var_refs_index() {
        let result =
            resolve_var_refs(&ColorValue::Index(39), &HashMap::new(), &mut HashSet::new()).unwrap();
        assert_eq!(result, ResolvedColor::Index(39));
    }

    #[test]
    fn test_resolve_var_refs_var_ref() {
        let mut vars = HashMap::new();
        vars.insert(
            "primary".to_string(),
            ColorValue::Str("#ff0000".to_string()),
        );
        let result = resolve_var_refs(
            &ColorValue::Str("primary".to_string()),
            &vars,
            &mut HashSet::new(),
        )
        .unwrap();
        assert_eq!(result, ResolvedColor::Hex("#ff0000".to_string()));
    }

    #[test]
    fn test_resolve_var_refs_chained() {
        let mut vars = HashMap::new();
        vars.insert("a".to_string(), ColorValue::Str("b".to_string()));
        vars.insert("b".to_string(), ColorValue::Str("#00ff00".to_string()));
        let result = resolve_var_refs(
            &ColorValue::Str("a".to_string()),
            &vars,
            &mut HashSet::new(),
        )
        .unwrap();
        assert_eq!(result, ResolvedColor::Hex("#00ff00".to_string()));
    }

    #[test]
    fn test_resolve_var_refs_circular() {
        let mut vars = HashMap::new();
        vars.insert("a".to_string(), ColorValue::Str("b".to_string()));
        vars.insert("b".to_string(), ColorValue::Str("a".to_string()));
        let result = resolve_var_refs(
            &ColorValue::Str("a".to_string()),
            &vars,
            &mut HashSet::new(),
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Circular"));
    }

    #[test]
    fn test_resolve_var_refs_not_found() {
        let result = resolve_var_refs(
            &ColorValue::Str("nonexistent".to_string()),
            &HashMap::new(),
            &mut HashSet::new(),
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    // --- Built-in themes --------------------------------------------------

    #[test]
    fn test_builtin_themes_exist() {
        let themes = get_builtin_themes();
        assert!(themes.contains_key("dark"));
        assert!(themes.contains_key("light"));
    }

    #[test]
    fn test_builtin_dark_has_51_plus_1_colors() {
        let themes = get_builtin_themes();
        let dark = &themes["dark"];
        for key in REQUIRED_COLOR_KEYS {
            assert!(
                dark.colors.contains_key(*key),
                "dark theme missing required color: {}",
                key
            );
        }
        // thinkingMax is explicitly defined in dark.json
        assert!(dark.colors.contains_key("thinkingMax"));
    }

    #[test]
    fn test_builtin_light_has_51_plus_1_colors() {
        let themes = get_builtin_themes();
        let light = &themes["light"];
        for key in REQUIRED_COLOR_KEYS {
            assert!(
                light.colors.contains_key(*key),
                "light theme missing required color: {}",
                key
            );
        }
        assert!(light.colors.contains_key("thinkingMax"));
    }

    #[test]
    fn test_builtin_dark_specific_values() {
        let themes = get_builtin_themes();
        let dark = &themes["dark"];
        // vars (bf8e4b953 rewrite: OKHSL values from the pin's dark.json)
        assert_eq!(dark.appearance.as_deref(), Some("dark"));
        assert_eq!(
            dark.vars.get("text"),
            Some(&ColorValue::Str("okhsl(234 3% 89%)".to_string()))
        );
        assert_eq!(
            dark.vars.get("blue"),
            Some(&ColorValue::Str("okhsl(232 54% 67%)".to_string()))
        );
        // colors (var refs)
        assert_eq!(
            dark.colors.get("accent"),
            Some(&ColorValue::Str("violet".to_string()))
        );
        assert_eq!(
            dark.colors.get("border"),
            Some(&ColorValue::Str("okhsl(231 57% 65%)".to_string()))
        );
        // colors (var refs)
        assert_eq!(
            dark.colors.get("customMessageLabel"),
            Some(&ColorValue::Str("violet".to_string()))
        );
        // export (okhsl values; converted to hex on export)
        let export = dark.export.as_ref().unwrap();
        assert_eq!(
            export.page_bg,
            Some(ColorValue::Str("okhsl(262 14% 16%)".to_string()))
        );
        assert_eq!(
            export.card_bg,
            Some(ColorValue::Str("okhsl(264 13% 19%)".to_string()))
        );
        assert_eq!(
            export.info_bg,
            Some(ColorValue::Str("okhsl(53 51% 24%)".to_string()))
        );
    }

    #[test]
    fn test_builtin_light_specific_values() {
        let themes = get_builtin_themes();
        let light = &themes["light"];
        assert_eq!(light.appearance.as_deref(), Some("light"));
        assert_eq!(
            light.vars.get("text"),
            Some(&ColorValue::Str("okhsl(225 5% 27%)".to_string()))
        );
        assert_eq!(
            light.colors.get("accent"),
            Some(&ColorValue::Str("violet".to_string()))
        );
        assert_eq!(
            light.colors.get("customMessageLabel"),
            Some(&ColorValue::Str("violet".to_string()))
        );
        let export = light.export.as_ref().unwrap();
        assert_eq!(
            export.page_bg,
            Some(ColorValue::Str("okhsl(17 3% 94%)".to_string()))
        );
    }

    #[test]
    fn test_builtin_dark_thinking_max_differs_from_xhigh() {
        let themes = get_builtin_themes();
        let dark = &themes["dark"];
        assert_ne!(
            dark.colors.get("thinkingMax"),
            dark.colors.get("thinkingXhigh")
        );
    }

    // --- Theme JSON parsing & validation ----------------------------------

    #[test]
    fn test_required_color_keys_count() {
        assert_eq!(REQUIRED_COLOR_KEYS.len(), 51);
    }

    #[test]
    fn test_allowed_color_keys_count() {
        // 53 → 55: searchMatchBg/searchMatchText added (theme-schema.json:148-155
        // @ 9841914, 00121ed99).
        assert_eq!(ALLOWED_COLOR_KEYS.len(), 56); // 51 + thinkingMax + scrollbarTrack/Thumb + searchMatchBg/Text
    }

    #[test]
    fn test_thinking_max_fallback() {
        let mut colors = HashMap::new();
        colors.insert(
            "thinkingXhigh".to_string(),
            ColorValue::Str("#aabbcc".to_string()),
        );
        let result = with_color_fallbacks(colors);
        assert_eq!(
            result.get("thinkingMax"),
            Some(&ColorValue::Str("#aabbcc".to_string()))
        );
    }

    #[test]
    fn test_thinking_max_no_override() {
        let mut colors = HashMap::new();
        colors.insert(
            "thinkingXhigh".to_string(),
            ColorValue::Str("#aabbcc".to_string()),
        );
        colors.insert(
            "thinkingMax".to_string(),
            ColorValue::Str("#ff0000".to_string()),
        );
        let result = with_color_fallbacks(colors);
        assert_eq!(
            result.get("thinkingMax"),
            Some(&ColorValue::Str("#ff0000".to_string()))
        );
    }

    #[test]
    fn test_validate_theme_json_missing_colors() {
        let json: serde_json::Value = serde_json::json!({
            "name": "test",
            "colors": {}
        });
        let result = validate_theme_json("test", &json);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("Missing required color tokens"));
        assert!(msg.contains("accent"));
    }

    #[test]
    fn test_validate_theme_json_invalid_name_slash() {
        let mut colors = serde_json::Map::new();
        for key in REQUIRED_COLOR_KEYS {
            colors.insert(
                (*key).to_string(),
                serde_json::Value::String("#000000".to_string()),
            );
        }
        let json = serde_json::json!({
            "name": "foo/bar",
            "colors": serde_json::Value::Object(colors),
        });
        let result = validate_theme_json("test", &json);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("cannot contain"));
    }

    #[test]
    fn test_parse_theme_json_integer_color() {
        let mut colors = serde_json::Map::new();
        for key in REQUIRED_COLOR_KEYS {
            colors.insert((*key).to_string(), serde_json::json!(42));
        }
        let json = serde_json::json!({
            "name": "test256",
            "colors": serde_json::Value::Object(colors),
        });
        let result = parse_theme_json("test", &json).unwrap();
        assert_eq!(result.colors.get("accent"), Some(&ColorValue::Index(42)));
    }

    #[test]
    fn test_validate_theme_json_integer_out_of_range() {
        let mut colors = serde_json::Map::new();
        for key in REQUIRED_COLOR_KEYS {
            colors.insert((*key).to_string(), serde_json::json!(300));
        }
        let json = serde_json::json!({
            "name": "bad256",
            "colors": serde_json::Value::Object(colors),
        });
        let result = validate_theme_json("test", &json);
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_theme_json_content_missing_colors() {
        let content = r#"{"name":"test","colors":{},"vars":{}}"#.to_string();
        // Under the installed validator this fails because colors is empty
        // (missing required keys); the lenient fallback accepts it.
        let value: serde_json::Value = serde_json::from_str(&content).expect("valid json");
        assert!(validate_theme_json("test", &value).is_err());
        assert!(parse_theme_json("test", &value).is_ok());
    }

    // --- eb3e9feed split: the lenient fallback (no validator installed) ---

    /// `parseThemeJson` fallback (theme.ts:488-494 @ 9841914): without an
    /// installed validator only "an object with a \"colors\" map" is
    /// checked.
    #[test]
    fn test_lenient_parse_requires_only_a_colors_map() {
        for bad in [
            serde_json::json!("just a string"),
            serde_json::json!(42),
            serde_json::json!({ "name": "no-colors" }),
            serde_json::json!([1, 2]),
        ] {
            let result = parse_theme_json("test", &bad);
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("expected an object with a \"colors\" map."),
                "minimal check must reject {bad}"
            );
        }
        // A document with a colors map passes through unvalidated — the
        // name-slash check lives in the validator only.
        let json = serde_json::json!({ "name": "foo/bar", "colors": {} });
        let parsed = parse_theme_json("test", &json).expect("lenient cast");
        assert_eq!(parsed.name, "foo/bar");
    }

    /// Lenient load of a partial theme: the `??` fallback keys stay
    /// `undefined` and resolution crashes with the JS TypeError wording
    /// pinned by the themes golden (invalid-missing-colors).
    #[test]
    fn test_lenient_partial_theme_undefined_fallback_error() {
        let json = serde_json::json!({
            "name": "invalid-missing-colors",
            "colors": { "accent": "#123456", "border": 7 }
        });
        let theme_json = parse_theme_json("test", &json).expect("lenient cast");
        let error = create_theme(&theme_json, Some(ColorMode::TrueColor), None).unwrap_err();
        let message = match error {
            RpiError::Resource(inner) => inner,
            other => other.to_string(),
        };
        assert_eq!(
            message,
            "Cannot read properties of undefined (reading 'startsWith')"
        );
    }

    /// Lenient load with a boolean color: `value.startsWith` is not a
    /// function (invalid-color-value-type golden, errorPrefix). A full
    /// required map keeps the undefined fallback keys out of the picture so
    /// the boolean is what resolution trips over.
    #[test]
    fn test_lenient_boolean_color_error() {
        let mut colors: HashMap<String, ColorValue> = REQUIRED_COLOR_KEYS
            .iter()
            .map(|k| ((*k).to_string(), ColorValue::Str("#000000".to_string())))
            .collect();
        colors.insert(
            "accent".to_string(),
            ColorValue::Raw(RawColorValue::Bool(true)),
        );
        let theme_json = ThemeJson {
            name: "bool-color".to_string(),
            appearance: None,
            vars: HashMap::new(),
            colors,
            export: None,
        };
        let error = create_theme(&theme_json, Some(ColorMode::TrueColor), None).unwrap_err();
        let message = match error {
            RpiError::Resource(inner) => inner,
            other => other.to_string(),
        };
        assert_eq!(message, "value.startsWith is not a function");
    }

    /// Out-of-range numbers pass through leniently and render the raw index
    /// (theme.ts:204 `typeof color === "number"`).
    #[test]
    fn test_lenient_out_of_range_number_renders_raw_index() {
        let mut colors: HashMap<String, ColorValue> = REQUIRED_COLOR_KEYS
            .iter()
            .map(|k| ((*k).to_string(), ColorValue::Str("#000000".to_string())))
            .collect();
        colors.insert(
            "accent".to_string(),
            ColorValue::Raw(RawColorValue::Number(300.0)),
        );
        let theme_json = ThemeJson {
            name: "wide-index".to_string(),
            appearance: None,
            vars: HashMap::new(),
            colors,
            export: None,
        };
        let theme = create_theme(&theme_json, Some(ColorMode::TrueColor), None).unwrap();
        assert_eq!(theme.get_fg_ansi("accent"), "\x1b[38;5;300m");
    }

    // --- create_theme -----------------------------------------------------

    #[test]
    fn test_create_theme_from_builtin_dark() {
        let themes = get_builtin_themes();
        let dark = &themes["dark"];
        let theme = create_theme(dark, Some(ColorMode::TrueColor), None).unwrap();
        assert_eq!(theme.name.as_deref(), Some("dark"));
        // fg_ansi for accent should be a truecolor sequence
        let accent_ansi = theme.get_fg_ansi("accent");
        assert!(accent_ansi.starts_with("\x1b[38;2;"));
        // bg key
        let bg_ansi = theme.get_bg_ansi("selectedBg");
        assert!(bg_ansi.starts_with("\x1b[48;2;"));
    }

    #[test]
    fn test_create_theme_thinking_max_fallback() {
        let mut colors: HashMap<String, ColorValue> = REQUIRED_COLOR_KEYS
            .iter()
            .map(|k| ((*k).to_string(), ColorValue::Str("#000000".to_string())))
            .collect();
        // Remove thinkingMax if present (it's not in REQUIRED_COLOR_KEYS)
        colors.remove("thinkingMax");
        // thinkingXhigh will be used as fallback
        colors.insert(
            "thinkingXhigh".to_string(),
            ColorValue::Str("#abcdef".to_string()),
        );
        let theme_json = ThemeJson {
            name: "test".to_string(),
            appearance: None,
            vars: HashMap::new(),
            colors,
            export: None,
        };
        let theme = create_theme(&theme_json, Some(ColorMode::TrueColor), None).unwrap();
        let max_ansi = theme.get_fg_ansi("thinkingMax");
        assert!(max_ansi.contains("171")); // 0xab = 171
    }

    // scrollbar-theme.test.ts:37-57 @ 9841914 (457ae8c79): the scrollbar
    // tokens are optional FOREGROUND colors — track falls back to `muted`,
    // thumb to `text`; explicit values pass through as truecolor.
    #[test]
    fn test_scrollbar_color_fallbacks() {
        let base_colors: HashMap<String, ColorValue> = REQUIRED_COLOR_KEYS
            .iter()
            .map(|k| ((*k).to_string(), ColorValue::Str("#000000".to_string())))
            .collect();
        let make_theme = |colors: HashMap<String, ColorValue>, name: &str| {
            let theme_json = ThemeJson {
                name: name.to_string(),
                appearance: None,
                vars: HashMap::new(),
                colors,
                export: None,
            };
            create_theme(&theme_json, Some(ColorMode::TrueColor), None).unwrap()
        };

        // Missing both → muted / text.
        let mut missing = base_colors.clone();
        missing.remove("scrollbarTrack");
        missing.remove("scrollbarThumb");
        let theme = make_theme(missing, "missing-scrollbar-theme");
        assert_eq!(
            theme.get_fg_ansi("scrollbarTrack"),
            theme.get_fg_ansi("muted")
        );
        assert_eq!(
            theme.get_fg_ansi("scrollbarThumb"),
            theme.get_fg_ansi("text")
        );
        // scrollbarThumb is no longer a background key (457ae8c79).
        assert_eq!(theme.get_bg_ansi("scrollbarThumb"), "");

        // Explicit values resolve to truecolor sequences.
        let mut explicit = base_colors;
        explicit.insert(
            "scrollbarTrack".to_string(),
            ColorValue::Str("#654321".to_string()),
        );
        explicit.insert(
            "scrollbarThumb".to_string(),
            ColorValue::Str("#123456".to_string()),
        );
        let theme = make_theme(explicit, "custom-scrollbar-theme");
        assert_eq!(theme.get_fg_ansi("scrollbarTrack"), "\x1b[38;2;101;67;33m");
        assert_eq!(theme.get_fg_ansi("scrollbarThumb"), "\x1b[38;2;18;52;86m");
    }

    // dark.json:36-37 / light.json:35-36 @ 9841914: the built-in themes set
    // both scrollbar tokens explicitly; the v1.0.0 rewrite gives them their
    // own OKHSL values, so they no longer alias borderMuted/text.
    fn truecolor_fg(value: &str) -> String {
        let color = rpi_tui::colors::parse_color(value).expect("valid theme color");
        rpi_tui::colors::foreground_ansi(&color, rpi_tui::colors::TerminalColorMode::TrueColor)
    }

    #[test]
    fn test_builtin_themes_carry_explicit_scrollbar_colors() {
        let themes = get_builtin_themes();
        let dark = create_theme(&themes["dark"], Some(ColorMode::TrueColor), None).unwrap();
        assert_eq!(
            dark.get_fg_ansi("scrollbarTrack"),
            truecolor_fg("okhsl(237 7% 33%)")
        );
        assert_eq!(
            dark.get_fg_ansi("scrollbarThumb"),
            truecolor_fg("okhsl(232 7% 65%)")
        );
        let light = create_theme(&themes["light"], Some(ColorMode::TrueColor), None).unwrap();
        assert_eq!(
            light.get_fg_ansi("scrollbarTrack"),
            truecolor_fg("okhsl(248 3% 90%)")
        );
        assert_eq!(
            light.get_fg_ansi("scrollbarThumb"),
            truecolor_fg("okhsl(226 7% 65%)")
        );
    }

    // --- Auto theme -------------------------------------------------------

    #[test]
    fn test_parse_auto_theme_setting_valid() {
        let result = parse_auto_theme_setting(Some("mylight/mydark"));
        assert_eq!(
            result,
            Some(AutoThemeSetting {
                light_theme: "mylight".to_string(),
                dark_theme: "mydark".to_string(),
            })
        );
    }

    #[test]
    fn test_parse_auto_theme_setting_with_spaces() {
        let result = parse_auto_theme_setting(Some("  light  /  dark  "));
        assert_eq!(
            result,
            Some(AutoThemeSetting {
                light_theme: "light".to_string(),
                dark_theme: "dark".to_string(),
            })
        );
    }

    #[test]
    fn test_parse_auto_theme_setting_none() {
        assert_eq!(parse_auto_theme_setting(None), None);
    }

    #[test]
    fn test_parse_auto_theme_setting_no_slash() {
        assert_eq!(parse_auto_theme_setting(Some("dark")), None);
    }

    #[test]
    fn test_parse_auto_theme_setting_two_slashes() {
        assert_eq!(parse_auto_theme_setting(Some("a/b/c")), None);
    }

    #[test]
    fn test_parse_auto_theme_setting_empty_half() {
        assert_eq!(parse_auto_theme_setting(Some("/dark")), None);
        assert_eq!(parse_auto_theme_setting(Some("light/")), None);
        assert_eq!(parse_auto_theme_setting(Some(" / ")), None);
    }

    #[test]
    fn test_resolve_theme_setting_auto() {
        assert_eq!(
            resolve_theme_setting(Some("light/dark"), TerminalTheme::Light),
            Some("light".to_string())
        );
        assert_eq!(
            resolve_theme_setting(Some("light/dark"), TerminalTheme::Dark),
            Some("dark".to_string())
        );
    }

    #[test]
    fn test_resolve_theme_setting_plain() {
        assert_eq!(
            resolve_theme_setting(Some("mytheme"), TerminalTheme::Dark),
            Some("mytheme".to_string())
        );
    }

    #[test]
    fn test_resolve_theme_setting_none() {
        assert_eq!(resolve_theme_setting(None, TerminalTheme::Dark), None);
    }

    #[test]
    fn test_resolve_theme_setting_invalid_auto() {
        // Contains '/' but invalid → None
        assert_eq!(
            resolve_theme_setting(Some("a/b/c"), TerminalTheme::Dark),
            None
        );
    }

    // --- Terminal detection -----------------------------------------------

    #[test]
    fn test_detect_color_fg_bg_theme_classifies_indices_like_vim() {
        // 0-6 and 8 dark; 7 and 9-15 light (theme.ts:697-705 @ a13d35a74).
        assert_eq!(
            detect_color_fg_bg_theme(Some("15;0")),
            Some(TerminalTheme::Dark)
        );
        assert_eq!(
            detect_color_fg_bg_theme(Some("0;8")),
            Some(TerminalTheme::Dark)
        );
        assert_eq!(
            detect_color_fg_bg_theme(Some("0;7")),
            Some(TerminalTheme::Light)
        );
        assert_eq!(
            detect_color_fg_bg_theme(Some("0;15")),
            Some(TerminalTheme::Light)
        );
        // Out-of-range/absent/invalid values have no answer.
        assert_eq!(detect_color_fg_bg_theme(Some("0;16")), None);
        assert_eq!(detect_color_fg_bg_theme(Some("0;235")), None);
        assert_eq!(detect_color_fg_bg_theme(Some("")), None);
        assert_eq!(detect_color_fg_bg_theme(Some("abc;def")), None);
        assert_eq!(detect_color_fg_bg_theme(None), None);
    }

    #[test]
    fn test_detect_terminal_theme_prefers_reported_colors_then_scheme_then_env() {
        let colors = TerminalColors {
            background: Some(TerminalRgb {
                r: 255,
                g: 255,
                b: 255,
            }),
            ..TerminalColors::default()
        };
        // Reported background wins over the scheme and COLORFGBG.
        assert_eq!(
            detect_terminal_theme(&colors, Some(TerminalTheme::Dark), Some("15;0")),
            TerminalTheme::Light
        );
        // Without a background: the light/dark report, then COLORFGBG, then dark.
        assert_eq!(
            detect_terminal_theme(
                &TerminalColors::default(),
                Some(TerminalTheme::Light),
                Some("15;0")
            ),
            TerminalTheme::Light
        );
        assert_eq!(
            detect_terminal_theme(&TerminalColors::default(), None, Some("0;15")),
            TerminalTheme::Light
        );
        assert_eq!(
            detect_terminal_theme(&TerminalColors::default(), None, None),
            TerminalTheme::Dark
        );
    }

    #[test]
    fn test_detect_from_env_colorfgbg() {
        let det = detect_terminal_background_from_env_str("0;0");
        assert_eq!(det.theme, TerminalTheme::Dark); // index 0 = black = dark
        assert_eq!(det.source, TerminalThemeSource::ColorFgBg);
        assert_eq!(det.confidence, TerminalThemeConfidence::High);
    }

    #[test]
    fn test_detect_from_env_colorfgbg_light() {
        let det = detect_terminal_background_from_env_str("0;15"); // bg=15=white
        assert_eq!(det.theme, TerminalTheme::Light);
        assert_eq!(det.confidence, TerminalThemeConfidence::High);
    }

    #[test]
    fn test_detect_from_env_fallback() {
        let det = detect_terminal_background_from_env_str("");
        assert_eq!(det.theme, TerminalTheme::Dark);
        assert_eq!(det.source, TerminalThemeSource::Fallback);
        assert_eq!(det.confidence, TerminalThemeConfidence::Low);
    }

    // --- Theme file format (V16-10 FR-D) ----------------------------------

    fn all_required_colors(value: &str) -> HashMap<String, ColorValue> {
        REQUIRED_COLOR_KEYS
            .iter()
            .map(|key| ((*key).to_string(), ColorValue::Str(value.to_string())))
            .collect()
    }

    #[test]
    fn test_theme_file_six_color_forms_and_var_refs() {
        let mut colors = all_required_colors("#101010");
        // `#rgb` (expands to #aabbcc), oklch(), okhsl(), 0-255 index, var ref.
        colors.insert("accent".to_string(), ColorValue::Str("#abc".to_string()));
        colors.insert(
            "border".to_string(),
            ColorValue::Str("oklch(62% 0.1 200)".to_string()),
        );
        colors.insert(
            "success".to_string(),
            ColorValue::Str("okhsl(159 59% 67%)".to_string()),
        );
        colors.insert("error".to_string(), ColorValue::Index(9));
        colors.insert("warning".to_string(), ColorValue::Str("brand".to_string()));
        let mut vars = HashMap::new();
        vars.insert("brand".to_string(), ColorValue::Str("#123456".to_string()));
        let json = ThemeJson {
            name: "extended".to_string(),
            appearance: None,
            vars,
            colors,
            export: None,
        };
        let theme = create_theme(&json, Some(ColorMode::TrueColor), None).expect("theme");
        assert_eq!(theme.get_fg_ansi("accent"), "\x1b[38;2;170;187;204m");
        assert!(theme.get_fg_ansi("border").starts_with("\x1b[38;2;"));
        assert!(theme.get_fg_ansi("success").starts_with("\x1b[38;2;"));
        assert_eq!(theme.get_fg_ansi("warning"), "\x1b[38;2;18;52;86m");
        assert_eq!(
            theme.colors().get("error"),
            Some(&tui_colors::Color::Indexed(9))
        );
        // Omitted appearance is detected from the theme's own colors
        // (theme.ts:436-445); the fixture's foregrounds average above 0.5.
        assert_eq!(theme.appearance(), TerminalTheme::Dark);
        // Explicit appearance wins over detection.
        let mut light_json = ThemeJson {
            name: "explicit-light".to_string(),
            appearance: Some("light".to_string()),
            vars: HashMap::new(),
            colors: all_required_colors("#eeeeee"),
            export: None,
        };
        let light = create_theme(&light_json, Some(ColorMode::TrueColor), None).expect("theme");
        assert_eq!(light.appearance(), TerminalTheme::Light);
        // Invalid appearance values are rejected.
        light_json.appearance = Some("dusk".to_string());
        assert!(create_theme(&light_json, Some(ColorMode::TrueColor), None).is_err());
    }

    #[test]
    fn test_theme_style_resolves_tokens_and_attributes() {
        let mut colors = all_required_colors("#101010");
        colors.insert("accent".to_string(), ColorValue::Str("#010203".to_string()));
        let json = ThemeJson {
            name: "style".to_string(),
            appearance: Some("dark".to_string()),
            vars: HashMap::new(),
            colors,
            export: None,
        };
        let theme = create_theme(&json, Some(ColorMode::TrueColor), None).expect("theme");
        let style = ThemeStyle {
            fg: Some(ThemeStyleColor::Token("accent".to_string())),
            bg: None,
            attributes: tui_colors::TextAttributes {
                bold: true,
                ..Default::default()
            },
        };
        assert_eq!(
            theme.style("x", &style),
            "\x1b[38;2;1;2;3m\x1b[1mx\x1b[22m\x1b[39m"
        );
        let concrete = ThemeStyle {
            fg: Some(ThemeStyleColor::Color(
                tui_colors::parse_color("#0a0b0c").expect("color"),
            )),
            bg: None,
            attributes: tui_colors::TextAttributes::default(),
        };
        assert_eq!(theme.style("y", &concrete), "\x1b[38;2;10;11;12my\x1b[39m");
    }

    // --- System theme + terminal state (V16-10 FR-C, OSC mock) ------------

    static TERMINAL_STATE_LOCK: Mutex<()> = Mutex::new(());

    fn terminal_state_lock() -> std::sync::MutexGuard<'static, ()> {
        TERMINAL_STATE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn reset_terminal_state() {
        *lock_terminal(&TERMINAL_COLORS) = TerminalColors::default();
        *lock_terminal(&TERMINAL_COLOR_SCHEME) = None;
        TERMINAL_COLORS_REPORTED.store(false, AtomicOrdering::Relaxed);
        TERMINAL_COLORS_PENDING.store(false, AtomicOrdering::Relaxed);
    }

    fn dracula_colors() -> TerminalColors {
        let palette = [
            "#21222c", "#ff5555", "#50fa7b", "#f1fa8c", "#bd93f9", "#ff79c6", "#8be9fd", "#f8f8f2",
            "#6272a4", "#ff6e6e", "#69ff94", "#ffffa5", "#d6acff", "#ff92df", "#a4ffff", "#ffffff",
        ]
        .iter()
        .map(|hex| {
            let color = rpi_tui::colors::parse_color(hex).expect("palette color");
            let rgb = rpi_tui::colors::color_to_rgb(&color);
            TerminalRgb {
                r: rgb.r.round() as u8,
                g: rgb.g.round() as u8,
                b: rgb.b.round() as u8,
            }
        })
        .collect();
        TerminalColors {
            background: Some(TerminalRgb {
                r: 40,
                g: 42,
                b: 54,
            }),
            foreground: Some(TerminalRgb {
                r: 248,
                g: 248,
                b: 242,
            }),
            palette: Some(palette),
        }
    }

    #[test]
    fn test_system_theme_lists_first_and_is_reserved() {
        assert_eq!(get_available_themes()[0].name, SYSTEM_THEME_NAME);
        let export = get_theme_export_colors("system");
        assert!(export.page_bg.is_none() && export.card_bg.is_none() && export.info_bg.is_none());
        assert!(assert_theme_name_is_valid(SYSTEM_THEME_NAME).is_err());
        assert!(assert_theme_name_is_valid("my-system").is_ok());
    }

    #[test]
    fn test_system_theme_generates_from_reported_colors() {
        let _guard = terminal_state_lock();
        reset_terminal_state();
        set_terminal_colors(dracula_colors());
        let theme = load_theme("system", Some(ColorMode::TrueColor)).expect("system theme");
        assert_eq!(theme.appearance(), TerminalTheme::Dark);
        // Body text uses the terminal foreground (OSC 10 default).
        assert_eq!(theme.get_fg_ansi("text"), "\x1b[39m");
        // Palette-derived colors are concrete.
        assert!(theme.get_fg_ansi("error").starts_with("\x1b[38;"));
        let error = theme.colors().get("error").expect("error color");
        assert!(tui_colors::color_to_oklch(error).c > 0.01);
        reset_terminal_state();
    }

    #[test]
    fn test_system_theme_renders_grayscale_while_pending() {
        let _guard = terminal_state_lock();
        reset_terminal_state();
        mark_terminal_colors_pending();
        let pending = create_system_theme(Some(ColorMode::TrueColor));
        let pending_error = pending.colors().get("error").expect("error color");
        assert!(
            tui_colors::color_to_oklch(pending_error).c < 0.005,
            "pending system theme is grayscale"
        );
        // A late report replaces the grayscale frame.
        set_terminal_colors(dracula_colors());
        let reported = create_system_theme(Some(ColorMode::TrueColor));
        let error = reported.colors().get("error").expect("error color");
        assert!(tui_colors::color_to_oklch(error).c > 0.01);
        reset_terminal_state();
    }

    #[test]
    fn test_terminal_scheme_change_drives_system_theme_without_background() {
        let _guard = terminal_state_lock();
        reset_terminal_state();
        set_terminal_color_scheme(Some(TerminalTheme::Light));
        assert_eq!(get_terminal_theme(), TerminalTheme::Light);
        let theme = create_system_theme(Some(ColorMode::TrueColor));
        assert_eq!(theme.appearance(), TerminalTheme::Light);
        // No background/palette: the ANSI palette indices tier.
        assert_eq!(
            theme.colors().get("error"),
            Some(&tui_colors::Color::Indexed(1))
        );
        reset_terminal_state();
    }

    // --- Text styles ------------------------------------------------------

    #[test]
    fn test_bold() {
        assert_eq!(Theme::bold("hi"), "\x1b[1mhi\x1b[22m");
    }

    #[test]
    fn test_thinking_border_color_name() {
        assert_eq!(Theme::thinking_border_color_name("off"), "thinkingOff");
        assert_eq!(
            Theme::thinking_border_color_name("minimal"),
            "thinkingMinimal"
        );
        assert_eq!(Theme::thinking_border_color_name("low"), "thinkingLow");
        assert_eq!(
            Theme::thinking_border_color_name("medium"),
            "thinkingMedium"
        );
        assert_eq!(Theme::thinking_border_color_name("high"), "thinkingHigh");
        assert_eq!(Theme::thinking_border_color_name("xhigh"), "thinkingXhigh");
        assert_eq!(Theme::thinking_border_color_name("max"), "thinkingMax");
        assert_eq!(Theme::thinking_border_color_name("unknown"), "thinkingOff");
    }

    #[test]
    fn test_bg_color_keys_count() {
        // 7 → 8: searchMatchBg added (theme.ts:531 @ 9841914).
        assert_eq!(BG_COLOR_KEYS.len(), 7); // scrollbarThumb moved to fg (457ae8c79)
    }

    // --- Terminal introspection constants ---------------------------------

    #[test]
    fn test_csi_16t_query_bytes() {
        assert_eq!(CSI_16T_QUERY, b"\x1b[16t");
    }
}
