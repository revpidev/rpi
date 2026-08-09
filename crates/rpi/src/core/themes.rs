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
use std::sync::OnceLock;

use crate::config;
use crate::error::RpiError;

// ===========================================================================
// Built-in theme JSON (verbatim values from upstream dark.json / light.json)
// ===========================================================================

/// Embedded `dark.json` (values ported verbatim from
/// `packages/coding-agent/src/modes/interactive/theme/dark.json`).
const DARK_THEME_JSON: &str = r##"{"name":"dark","vars":{"cyan":"#00d7ff","blue":"#5f87ff","green":"#b5bd68","red":"#cc6666","yellow":"#ffff00","text":"#d4d4d4","gray":"#808080","dimGray":"#666666","darkGray":"#505050","accent":"#8abeb7","selectedBg":"#3a3a4a","userMsgBg":"#343541","toolPendingBg":"#282832","toolSuccessBg":"#283228","toolErrorBg":"#3c2828","customMsgBg":"#2d2838"},"colors":{"accent":"accent","border":"blue","borderAccent":"cyan","borderMuted":"darkGray","success":"green","error":"red","warning":"yellow","muted":"gray","dim":"dimGray","text":"text","thinkingText":"gray","selectedBg":"selectedBg","userMessageBg":"userMsgBg","userMessageText":"text","customMessageBg":"customMsgBg","customMessageText":"text","customMessageLabel":"#9575cd","toolPendingBg":"toolPendingBg","toolSuccessBg":"toolSuccessBg","toolErrorBg":"toolErrorBg","toolTitle":"text","toolOutput":"gray","mdHeading":"#f0c674","mdLink":"#81a2be","mdLinkUrl":"dimGray","mdCode":"accent","mdCodeBlock":"green","mdCodeBlockBorder":"gray","mdQuote":"gray","mdQuoteBorder":"gray","mdHr":"gray","mdListBullet":"accent","toolDiffAdded":"green","toolDiffRemoved":"red","toolDiffContext":"gray","syntaxComment":"#6A9955","syntaxKeyword":"#569CD6","syntaxFunction":"#DCDCAA","syntaxVariable":"#9CDCFE","syntaxString":"#CE9178","syntaxNumber":"#B5CEA8","syntaxType":"#4EC9B0","syntaxOperator":"#D4D4D4","syntaxPunctuation":"#D4D4D4","thinkingOff":"darkGray","thinkingMinimal":"#6e6e6e","thinkingLow":"#5f87af","thinkingMedium":"#81a2be","thinkingHigh":"#b294bb","thinkingXhigh":"#d183e8","thinkingMax":"#ff5fff","bashMode":"green"},"export":{"pageBg":"#18181e","cardBg":"#1e1e24","infoBg":"#3c3728"}}"##;

/// Embedded `light.json` (values ported verbatim from
/// `packages/coding-agent/src/modes/interactive/theme/light.json`).
const LIGHT_THEME_JSON: &str = r##"{"name":"light","vars":{"teal":"#5a8080","blue":"#547da7","green":"#588458","red":"#aa5555","yellow":"#9a7326","text":"#1f2328","mediumGray":"#6c6c6c","dimGray":"#767676","lightGray":"#b0b0b0","selectedBg":"#d0d0e0","userMsgBg":"#e8e8e8","toolPendingBg":"#e8e8f0","toolSuccessBg":"#e8f0e8","toolErrorBg":"#f0e8e8","customMsgBg":"#ede7f6"},"colors":{"accent":"teal","border":"blue","borderAccent":"teal","borderMuted":"lightGray","success":"green","error":"red","warning":"yellow","muted":"mediumGray","dim":"dimGray","text":"text","thinkingText":"mediumGray","selectedBg":"selectedBg","userMessageBg":"userMsgBg","userMessageText":"text","customMessageBg":"customMsgBg","customMessageText":"text","customMessageLabel":"#7e57c2","toolPendingBg":"toolPendingBg","toolSuccessBg":"toolSuccessBg","toolErrorBg":"toolErrorBg","toolTitle":"text","toolOutput":"mediumGray","mdHeading":"yellow","mdLink":"blue","mdLinkUrl":"dimGray","mdCode":"teal","mdCodeBlock":"green","mdCodeBlockBorder":"mediumGray","mdQuote":"mediumGray","mdQuoteBorder":"mediumGray","mdHr":"mediumGray","mdListBullet":"green","toolDiffAdded":"green","toolDiffRemoved":"red","toolDiffContext":"mediumGray","syntaxComment":"#008000","syntaxKeyword":"#0000FF","syntaxFunction":"#795E26","syntaxVariable":"#001080","syntaxString":"#A31515","syntaxNumber":"#098658","syntaxType":"#267F99","syntaxOperator":"#000000","syntaxPunctuation":"#000000","thinkingOff":"lightGray","thinkingMinimal":"#767676","thinkingLow":"blue","thinkingMedium":"teal","thinkingHigh":"#875f87","thinkingXhigh":"#8b008b","thinkingMax":"#af005f","bashMode":"green"},"export":{"pageBg":"#f8f8f8","cardBg":"#ffffff","infoBg":"#fffae6"}}"##;

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

/// All allowed keys in the `colors` object (51 required + `thinkingMax`).
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
];

/// Background-colour keys — separated from foreground colours in
/// `create_theme` (theme.ts:602-609).
pub const BG_COLOR_KEYS: &[&str] = &[
    "selectedBg",
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

/// OSC 11 query — queries the terminal default background colour
/// (written at `tui.ts:1689`). Response: `\x1b]11;rgb:RRRR/GGGG/BBBB\x07`
/// parsed by [`parse_osc11_background_color`].
pub const OSC_11_QUERY: &[u8] = b"\x1b]11;?\x07";

/// CSI ?996n — queries the terminal colour-scheme preference (dark/light)
/// (written at `tui.ts:1716`). Response: `\x1b[?997;Nn` parsed by
/// [`parse_terminal_color_scheme_report`].
pub const CSI_996_QUERY: &[u8] = b"\x1b[?996n";

/// CSI 16t — queries the terminal cell dimensions in pixels (written at
/// `tui.ts:686`, only used by image-capable terminals).
pub const CSI_16T_QUERY: &[u8] = b"\x1b[16t";

/// OSC 9;4;3 — indeterminate progress indicator (iTerm2/WezTerm protocol,
/// `terminal.ts:12`). Sent as `OSC 9;4;3 ST`.
pub const OSC_9_4_INDETERMINATE: &[u8] = b"\x1b]9;4;3\x07";

/// OSC 9;4;0; — clear progress indicator (`terminal.ts:13`).
pub const OSC_9_4_CLEAR: &[u8] = b"\x1b]9;4;0;\x07";

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
/// Port of `ColorValueSchema` / `ColorValue` (theme.ts:24-29).
#[derive(Debug, Clone, PartialEq)]
pub enum ColorValue {
    /// Hex `"#RRGGBB"`, variable reference name, or empty string.
    Str(String),
    /// 256-colour palette index (0-255).
    Index(u32),
}

impl serde::Serialize for ColorValue {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            ColorValue::Str(s) => serializer.serialize_str(s),
            ColorValue::Index(i) => serializer.serialize_u32(*i),
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
                let i = n.as_u64().ok_or_else(|| {
                    serde::de::Error::custom(format!("expected integer 0-255, got {}", n))
                })?;
                if i > 255 {
                    return Err(serde::de::Error::custom(format!(
                        "expected integer 0-255, got {}",
                        i
                    )));
                }
                Ok(ColorValue::Index(i as u32))
            }
            _ => Err(serde::de::Error::custom("expected string or integer 0-255")),
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

/// Parsed theme JSON (theme.ts:31-103).
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ThemeJson {
    pub name: String,
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

/// A constructed theme with pre-computed ANSI escape sequences.
///
/// Port of `class Theme` (theme.ts:330-432). Foreground and background
/// colours are separated because they use different ANSI reset codes (39 vs
/// 49).
#[derive(Debug, Clone)]
pub struct Theme {
    pub name: Option<String>,
    pub source_path: Option<PathBuf>,
    fg_colors: HashMap<String, String>,
    bg_colors: HashMap<String, String>,
    mode: ColorMode,
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
            let rgb = hex_to_rgb(h)?;
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
            let rgb = hex_to_rgb(h)?;
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
            if s.starts_with('#') {
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

/// Apply the `thinkingMax` fallback: if absent, use `thinkingXhigh`
/// (theme.ts:322-324).
fn with_thinking_max_fallback(
    mut colors: HashMap<String, ColorValue>,
) -> HashMap<String, ColorValue> {
    if !colors.contains_key("thinkingMax") {
        if let Some(xhigh) = colors.get("thinkingXhigh") {
            colors.insert("thinkingMax".to_string(), xhigh.clone());
        }
    }
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
pub fn parse_theme_json(label: &str, value: &serde_json::Value) -> Result<ThemeJson, RpiError> {
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

/// Lazily parsed built-in dark/light themes (theme.ts:440-451).
pub fn get_builtin_themes() -> &'static HashMap<String, ThemeJson> {
    BUILTIN_THEMES.get_or_init(|| {
        let mut themes = HashMap::new();
        let dark_val: serde_json::Value = serde_json::from_str(DARK_THEME_JSON)
            .expect("built-in dark theme JSON is verified valid at development time");
        themes.insert(
            "dark".to_string(),
            parse_theme_json("dark", &dark_val)
                .expect("built-in dark theme is verified valid at development time"),
        );
        let light_val: serde_json::Value = serde_json::from_str(LIGHT_THEME_JSON)
            .expect("built-in light theme JSON is verified valid at development time");
        themes.insert(
            "light".to_string(),
            parse_theme_json("light", &light_val)
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
    let colors = with_thinking_max_fallback(theme_json.colors.clone());
    let resolved = resolve_theme_colors(&colors, &theme_json.vars)?;

    let bg_set: HashSet<&str> = BG_COLOR_KEYS.iter().copied().collect();
    let mut fg_colors: HashMap<String, String> = HashMap::new();
    let mut bg_colors: HashMap<String, String> = HashMap::new();

    for (key, color) in &resolved {
        if bg_set.contains(key.as_str()) {
            bg_colors.insert(key.clone(), bg_ansi(color, color_mode)?);
        } else {
            fg_colors.insert(key.clone(), fg_ansi(color, color_mode)?);
        }
    }

    Ok(Theme {
        name: Some(theme_json.name.clone()),
        source_path: source_path.map(Path::to_path_buf),
        fg_colors,
        bg_colors,
        mode: color_mode,
    })
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
    if themes_dir.exists() {
        if let Ok(entries) = std::fs::read_dir(&themes_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                if let Ok(content) = std::fs::read_to_string(&path) {
                    if let Ok(theme_json) =
                        parse_theme_json_content(&path.display().to_string(), &content)
                    {
                        if seen.insert(theme_json.name.clone()) {
                            result.push(ThemeInfo {
                                name: theme_json.name,
                                path: Some(path),
                            });
                        }
                    }
                }
            }
        }
    }

    result.sort_by(|a, b| a.name.cmp(&b.name));
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
    let colors = with_thinking_max_fallback(theme_json.colors.clone());
    let resolved = resolve_theme_colors(&colors, &theme_json.vars)?;
    let default_text = if is_light { "#000000" } else { "#e5e5e7" };
    let mut css_colors = HashMap::new();
    for (key, color) in &resolved {
        let css = match color {
            ResolvedColor::Index(i) => ansi256_to_hex(*i),
            ResolvedColor::Empty => default_text.to_string(),
            ResolvedColor::Hex(h) => h.clone(),
        };
        css_colors.insert(key.clone(), css);
    }
    Ok(css_colors)
}

/// Check if a theme name is the light theme (theme.ts:1048-1051).
pub fn is_light_theme(theme_name: &str) -> bool {
    theme_name == "light"
}

/// Get explicit export colours from a theme (theme.ts:1057-1085).
pub fn get_theme_export_colors(theme_name: &str) -> ThemeExportColors {
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
            ResolvedColor::Hex(h) => Some(h),
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
fn to_linear(channel: u32) -> f64 {
    let value = channel as f64 / 255.0;
    if value <= 0.03928 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

/// Relative luminance of an RGB colour (theme.ts:718-724).
pub fn get_rgb_color_luminance(rgb: &Rgb) -> f64 {
    0.2126 * to_linear(rgb.r) + 0.7152 * to_linear(rgb.g) + 0.0722 * to_linear(rgb.b)
}

/// Luminance from a 256-colour index (theme.ts:726-728).
pub fn get_ansi_color_luminance(index: u32) -> f64 {
    let hex = ansi256_to_hex(index);
    let rgb = hex_to_rgb(&hex).unwrap_or(Rgb { r: 0, g: 0, b: 0 });
    get_rgb_color_luminance(&rgb)
}

/// Classify an RGB colour as light or dark (theme.ts:730-732).
pub fn get_theme_for_rgb_color(rgb: &Rgb) -> TerminalTheme {
    if get_rgb_color_luminance(rgb) >= 0.5 {
        TerminalTheme::Light
    } else {
        TerminalTheme::Dark
    }
}

/// Parse the background index from a COLORFGBG env var (theme.ts:707-716).
///
/// Format is typically `"fg;bg"` or `"fg;bg;0"`. Scans from the right for the
/// first valid 0-255 integer.
pub fn get_color_fg_bg_background_index(colorfgbg: &str) -> Option<u32> {
    let parts: Vec<&str> = colorfgbg.split(';').collect();
    for part in parts.iter().rev() {
        let trimmed = part.trim();
        if let Ok(bg) = trimmed.parse::<u32>() {
            if bg <= 255 {
                return Some(bg);
            }
        }
    }
    None
}

/// Detect terminal theme from environment (COLORFGBG or fallback)
/// (theme.ts:734-753).
///
/// Pure function — does not read `std::env`. Pass the COLORFGBG string
/// explicitly.
pub fn detect_terminal_background_from_env_str(colorfgbg: &str) -> TerminalThemeDetection {
    if let Some(bg) = get_color_fg_bg_background_index(colorfgbg) {
        let luminance = get_ansi_color_luminance(bg);
        return TerminalThemeDetection {
            theme: if luminance >= 0.5 {
                TerminalTheme::Light
            } else {
                TerminalTheme::Dark
            },
            source: TerminalThemeSource::ColorFgBg,
            detail: format!("background color index {}", bg),
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

/// Detect terminal theme from environment (theme.ts:734-753).
///
/// Reads `COLORFGBG` from the process environment.
pub fn detect_terminal_background_from_env() -> TerminalThemeDetection {
    let colorfgbg = std::env::var("COLORFGBG").unwrap_or_default();
    detect_terminal_background_from_env_str(&colorfgbg)
}

// ===========================================================================
// OSC / CSI Response Parsing (terminal-colors.ts)
// ===========================================================================

/// Check whether `data` is an OSC 11 background-colour response
/// (terminal-colors.ts:31-33).
pub fn is_osc11_background_color_response(data: &str) -> bool {
    parse_osc11_background_color(data).is_some()
}

/// Parse an OSC 11 background-colour response into [`Rgb`]
/// (terminal-colors.ts:35-65).
///
/// Supports `#RRGGBB` (6 hex), `#RRRRGGGGBBBB` (12 hex), and
/// `rgb:RRRR/GGGG/BBBB` formats. Multi-digit channels are normalised to 0-255
/// by linear scaling.
pub fn parse_osc11_background_color(data: &str) -> Option<Rgb> {
    let bytes = data.as_bytes();
    if bytes.len() < 7 {
        return None;
    }
    // Must start with ESC ] 1 1 ;
    if &bytes[..5] != b"\x1b]11;" {
        return None;
    }
    // Must end with BEL or ESC-backslash (ST)
    let body_end = if bytes[bytes.len() - 1] == 0x07 {
        bytes.len() - 1
    } else if bytes.len() >= 2 && &bytes[bytes.len() - 2..] == b"\x1b\\" {
        bytes.len() - 2
    } else {
        return None;
    };
    let body = std::str::from_utf8(&bytes[5..body_end]).ok()?.trim();
    parse_osc_color_value(body)
}

/// Parse a colour value from an OSC 11 response body.
fn parse_osc_color_value(value: &str) -> Option<Rgb> {
    if let Some(hex) = value.strip_prefix('#') {
        if hex.len() == 6 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
            let r = u32::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u32::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u32::from_str_radix(&hex[4..6], 16).ok()?;
            return Some(Rgb { r, g, b });
        }
        if hex.len() == 12 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
            let r = parse_osc_hex_channel(&hex[0..4])?;
            let g = parse_osc_hex_channel(&hex[4..8])?;
            let b = parse_osc_hex_channel(&hex[8..12])?;
            return Some(Rgb { r, g, b });
        }
        return None;
    }
    // rgb:RRRR/GGGG/BBBB or rgba:RRRR/GGGG/BBBB format
    let lower = value.to_ascii_lowercase();
    let rest = lower
        .strip_prefix("rgba:")
        .or_else(|| lower.strip_prefix("rgb:"))?;
    let parts: Vec<&str> = rest.split('/').collect();
    if parts.len() != 3 {
        return None;
    }
    let r = parse_osc_hex_channel(parts[0])?;
    let g = parse_osc_hex_channel(parts[1])?;
    let b = parse_osc_hex_channel(parts[2])?;
    Some(Rgb { r, g, b })
}

/// Normalise a variable-length hex channel to 0-255 (terminal-colors.ts:17-26).
fn parse_osc_hex_channel(channel: &str) -> Option<u32> {
    if channel.is_empty() || !channel.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let len = channel.len();
    let max = 16_u32.pow(len as u32).saturating_sub(1);
    if max == 0 {
        return None;
    }
    let val = u32::from_str_radix(channel, 16).ok()?;
    Some(((val as f64 / max as f64) * 255.0).round() as u32)
}

/// Parse a CSI ?997 colour-scheme report (terminal-colors.ts:67-72).
///
/// `\x1b[?997;1n` → Dark, `\x1b[?997;2n` → Light.
pub fn parse_terminal_color_scheme_report(data: &str) -> Option<TerminalTheme> {
    if data == "\x1b[?997;1n" {
        return Some(TerminalTheme::Dark);
    }
    if data == "\x1b[?997;2n" {
        return Some(TerminalTheme::Light);
    }
    None
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

impl Theme {
    /// Wrap text in a foreground colour (theme.ts:359-363).
    pub fn fg(&self, color: &str, text: &str) -> String {
        let ansi = self.fg_colors.get(color).map(|s| s.as_str()).unwrap_or("");
        format!("{}{}\x1b[39m", ansi, text)
    }

    /// Wrap text in a background colour (theme.ts:365-369).
    pub fn bg(&self, color: &str, text: &str) -> String {
        let ansi = self.bg_colors.get(color).map(|s| s.as_str()).unwrap_or("");
        format!("{}{}\x1b[49m", ansi, text)
    }

    /// Raw ANSI foreground prefix for a colour (theme.ts:391-395).
    pub fn get_fg_ansi(&self, color: &str) -> &str {
        self.fg_colors.get(color).map(|s| s.as_str()).unwrap_or("")
    }

    /// Raw ANSI background prefix for a colour (theme.ts:397-401).
    pub fn get_bg_ansi(&self, color: &str) -> &str {
        self.bg_colors.get(color).map(|s| s.as_str()).unwrap_or("")
    }

    /// Current colour mode (theme.ts:403-405).
    pub fn get_color_mode(&self) -> ColorMode {
        self.mode
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
        // vars
        assert_eq!(
            dark.vars.get("cyan"),
            Some(&ColorValue::Str("#00d7ff".to_string()))
        );
        assert_eq!(
            dark.vars.get("accent"),
            Some(&ColorValue::Str("#8abeb7".to_string()))
        );
        // colors (var refs)
        assert_eq!(
            dark.colors.get("accent"),
            Some(&ColorValue::Str("accent".to_string()))
        );
        assert_eq!(
            dark.colors.get("border"),
            Some(&ColorValue::Str("blue".to_string()))
        );
        // colors (direct hex)
        assert_eq!(
            dark.colors.get("customMessageLabel"),
            Some(&ColorValue::Str("#9575cd".to_string()))
        );
        // export
        let export = dark.export.as_ref().unwrap();
        assert_eq!(export.page_bg, Some(ColorValue::Str("#18181e".to_string())));
        assert_eq!(export.card_bg, Some(ColorValue::Str("#1e1e24".to_string())));
        assert_eq!(export.info_bg, Some(ColorValue::Str("#3c3728".to_string())));
    }

    #[test]
    fn test_builtin_light_specific_values() {
        let themes = get_builtin_themes();
        let light = &themes["light"];
        assert_eq!(
            light.vars.get("teal"),
            Some(&ColorValue::Str("#5a8080".to_string()))
        );
        assert_eq!(
            light.colors.get("accent"),
            Some(&ColorValue::Str("teal".to_string()))
        );
        assert_eq!(
            light.colors.get("customMessageLabel"),
            Some(&ColorValue::Str("#7e57c2".to_string()))
        );
        let export = light.export.as_ref().unwrap();
        assert_eq!(export.page_bg, Some(ColorValue::Str("#f8f8f8".to_string())));
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
        assert_eq!(ALLOWED_COLOR_KEYS.len(), 52); // 51 + thinkingMax
    }

    #[test]
    fn test_thinking_max_fallback() {
        let mut colors = HashMap::new();
        colors.insert(
            "thinkingXhigh".to_string(),
            ColorValue::Str("#aabbcc".to_string()),
        );
        let result = with_thinking_max_fallback(colors);
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
        let result = with_thinking_max_fallback(colors);
        assert_eq!(
            result.get("thinkingMax"),
            Some(&ColorValue::Str("#ff0000".to_string()))
        );
    }

    #[test]
    fn test_parse_theme_json_missing_colors() {
        let json: serde_json::Value = serde_json::json!({
            "name": "test",
            "colors": {}
        });
        let result = parse_theme_json("test", &json);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("Missing required color tokens"));
        assert!(msg.contains("accent"));
    }

    #[test]
    fn test_parse_theme_json_invalid_name_slash() {
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
        let result = parse_theme_json("test", &json);
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
    fn test_parse_theme_json_integer_out_of_range() {
        let mut colors = serde_json::Map::new();
        for key in REQUIRED_COLOR_KEYS {
            colors.insert((*key).to_string(), serde_json::json!(300));
        }
        let json = serde_json::json!({
            "name": "bad256",
            "colors": serde_json::Value::Object(colors),
        });
        let result = parse_theme_json("test", &json);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_theme_json_content_valid() {
        let content = r#"{"name":"test","colors":{},"vars":{}}"#.to_string();
        // This should fail because colors is empty (missing required keys)
        let result = parse_theme_json_content("test", &content);
        assert!(result.is_err());
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
            vars: HashMap::new(),
            colors,
            export: None,
        };
        let theme = create_theme(&theme_json, Some(ColorMode::TrueColor), None).unwrap();
        let max_ansi = theme.get_fg_ansi("thinkingMax");
        assert!(max_ansi.contains("171")); // 0xab = 171
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
    fn test_colorfgbg_parsing() {
        assert_eq!(get_color_fg_bg_background_index("0;15"), Some(15));
        assert_eq!(get_color_fg_bg_background_index("7;0;0"), Some(0));
        assert_eq!(get_color_fg_bg_background_index("15;235"), Some(235));
        assert_eq!(get_color_fg_bg_background_index(""), None);
        assert_eq!(get_color_fg_bg_background_index("abc"), None);
    }

    #[test]
    fn test_luminance_black() {
        let rgb = Rgb { r: 0, g: 0, b: 0 };
        let lum = get_rgb_color_luminance(&rgb);
        assert!(lum < 0.5);
        assert_eq!(get_theme_for_rgb_color(&rgb), TerminalTheme::Dark);
    }

    #[test]
    fn test_luminance_white() {
        let rgb = Rgb {
            r: 255,
            g: 255,
            b: 255,
        };
        let lum = get_rgb_color_luminance(&rgb);
        assert!(lum >= 0.5);
        assert_eq!(get_theme_for_rgb_color(&rgb), TerminalTheme::Light);
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

    // --- OSC/CSI parsing --------------------------------------------------

    #[test]
    fn test_parse_osc11_hex_6() {
        let data = "\x1b]11;#ff0000\x07";
        let rgb = parse_osc11_background_color(data).unwrap();
        assert_eq!(rgb, Rgb { r: 255, g: 0, b: 0 });
    }

    #[test]
    fn test_parse_osc11_rgb_format() {
        let data = "\x1b]11;rgb:ffff/0000/0000\x07";
        let rgb = parse_osc11_background_color(data).unwrap();
        assert_eq!(rgb, Rgb { r: 255, g: 0, b: 0 });
    }

    #[test]
    fn test_parse_osc11_hex_12() {
        let data = "\x1b]11;#ffff00000000\x07";
        let rgb = parse_osc11_background_color(data).unwrap();
        assert_eq!(rgb, Rgb { r: 255, g: 0, b: 0 });
    }

    #[test]
    fn test_parse_osc11_st_terminator() {
        let data = "\x1b]11;#00ff00\x1b\\";
        let rgb = parse_osc11_background_color(data).unwrap();
        assert_eq!(rgb, Rgb { r: 0, g: 255, b: 0 });
    }

    #[test]
    fn test_parse_osc11_invalid() {
        assert!(parse_osc11_background_color("not a response").is_none());
        assert!(parse_osc11_background_color("\x1b]11;?\x07").is_none()); // query, not response
    }

    #[test]
    fn test_parse_color_scheme_report() {
        assert_eq!(
            parse_terminal_color_scheme_report("\x1b[?997;1n"),
            Some(TerminalTheme::Dark)
        );
        assert_eq!(
            parse_terminal_color_scheme_report("\x1b[?997;2n"),
            Some(TerminalTheme::Light)
        );
        assert!(parse_terminal_color_scheme_report("garbage").is_none());
    }

    #[test]
    fn test_is_osc11_response() {
        assert!(is_osc11_background_color_response("\x1b]11;#000000\x07"));
        assert!(!is_osc11_background_color_response("hello"));
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
        assert_eq!(BG_COLOR_KEYS.len(), 6);
    }

    // --- Terminal introspection constants ---------------------------------

    #[test]
    fn test_osc_11_query_bytes() {
        assert_eq!(OSC_11_QUERY, b"\x1b]11;?\x07");
    }

    #[test]
    fn test_csi_996_query_bytes() {
        assert_eq!(CSI_996_QUERY, b"\x1b[?996n");
    }

    #[test]
    fn test_csi_16t_query_bytes() {
        assert_eq!(CSI_16T_QUERY, b"\x1b[16t");
    }
}
