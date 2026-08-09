//! Theme hot-reload watcher and terminal-appearance detection — the Rust
//! slice of `theme/theme.ts` @ pi 0.82.1 (2efa728) not covered by the S4a
//! `theme.rs` module: custom-theme file watching (theme.ts:886-957) and auto
//! theme resolution (theme.ts:648-677, 718-789).
//!
//! Intentional differences:
//! - File watching uses a 100ms polling loop instead of `fs.watch`
//!   (theme.ts:936-956): no `notify` dependency, identical debounce
//!   semantics (theme.ts:904-934). Detection is mtime-based.
//! - The watcher tracks the plain theme name from settings
//!   (`SettingsManager::get_theme`); automatic `light/dark`-style pairs
//!   resolve to a fixed theme at apply time and are not watched per branch
//!   (upstream watches the resolved active theme name).
//! - `detectTerminalBackgroundFromEnv` (theme.ts:734-753) returns only the
//!   resolved theme, not the `{theme, source, detail, confidence}` record —
//!   the detail fields have no local consumers.
//! - The plain `"auto"` setting value is treated as built-in light/dark
//!   following (upstream only understands slash pairs, theme.ts:648-662).

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use rpi_tui::terminal_colors::{RgbColor, TerminalColorScheme};
use rpi_tui::tui::Tui;

use crate::core::themes::get_theme_watch_path;
use crate::modes::interactive::interactive_mode::{InteractiveUi, UiCommand};

/// Poll interval for the theme watcher (upstream debounces reloads by 100ms,
/// theme.ts:933).
const THEME_WATCH_POLL_INTERVAL: Duration = Duration::from_millis(100);

// =============================================================================
// Auto theme resolution (theme.ts:648-677)
// =============================================================================

/// `parseAutoThemeSetting` (theme.ts:648-662): `"light/dark"`-style pairs;
/// anything without exactly one non-empty slash is not an automatic setting.
pub(crate) fn parse_auto_theme_setting(theme_setting: Option<&str>) -> Option<(String, String)> {
    let theme_setting = theme_setting?;
    let mut slashes = theme_setting.match_indices('/');
    let (slash_index, _) = slashes.next()?;
    if slashes.next().is_some() {
        return None;
    }
    let light = theme_setting[..slash_index].trim();
    let dark = theme_setting[slash_index + 1..].trim();
    if light.is_empty() || dark.is_empty() {
        return None;
    }
    Some((light.to_string(), dark.to_string()))
}

/// The automatic theme pair for a setting: slash pairs (theme.ts:648-662)
/// plus the plain `"auto"` shorthand for built-in light/dark following
/// (local extension, see module header).
pub(crate) fn auto_theme_pair(setting: Option<&str>) -> Option<(String, String)> {
    match setting {
        Some("auto") => Some(("light".to_string(), "dark".to_string())),
        other => parse_auto_theme_setting(other),
    }
}

// =============================================================================
// Terminal appearance detection (theme.ts:718-789)
// =============================================================================

/// `getRgbColorLuminance` (theme.ts:718-724): WCAG-relative-luminance-style
/// linearization of an sRGB channel.
fn rgb_luminance(rgb: RgbColor) -> f64 {
    let RgbColor { r, g, b } = rgb;
    let to_linear = |channel: u8| {
        let value = f64::from(channel) / 255.0;
        if value <= 0.03928 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * to_linear(r) + 0.7152 * to_linear(g) + 0.0722 * to_linear(b)
}

/// `getThemeForRgbColor` (theme.ts:730-732): luminance >= 0.5 is light.
pub(crate) fn theme_for_rgb(rgb: &RgbColor) -> TerminalColorScheme {
    if rgb_luminance(*rgb) >= 0.5 {
        TerminalColorScheme::Light
    } else {
        TerminalColorScheme::Dark
    }
}

/// `ansi256ToHex` (theme.ts:978-1034): 256-color index → `#rrggbb`.
fn ansi256_to_hex(index: u8) -> String {
    // Basic colors (0-15) - approximate common terminal values.
    const BASIC_COLORS: [&str; 16] = [
        "#000000", "#800000", "#008000", "#808000", "#000080", "#800080", "#008080", "#c0c0c0",
        "#808080", "#ff0000", "#00ff00", "#ffff00", "#0000ff", "#ff00ff", "#00ffff", "#ffffff",
    ];
    let index = index as u16;
    if index < 16 {
        return BASIC_COLORS[index as usize].to_string();
    }
    // Color cube (16-231): 6x6x6 = 216 colors.
    if index < 232 {
        let cube_index = index - 16;
        let r = cube_index / 36;
        let g = (cube_index % 36) / 6;
        let b = cube_index % 6;
        let to_hex = |n: u16| -> String {
            let value = if n == 0 { 0 } else { 55 + n * 40 };
            format!("{value:02x}")
        };
        return format!("#{}{}{}", to_hex(r), to_hex(g), to_hex(b));
    }
    // Grayscale (232-255): 24 shades.
    let gray = 8 + (index - 232) * 10;
    format!("#{gray:02x}{gray:02x}{gray:02x}")
}

/// `getColorFgBgBackgroundIndex` (theme.ts:707-716): the last numeric
/// segment of `COLORFGBG` (the background index).
fn colorfgbg_background_index(colorfgbg: &str) -> Option<u8> {
    for part in colorfgbg.rsplit(';') {
        if let Ok(bg) = part.trim().parse::<u8>() {
            return Some(bg);
        }
    }
    None
}

/// `detectTerminalBackgroundFromEnv` (theme.ts:734-753): the `COLORFGBG`
/// background index luminance, falling back to `dark` when absent.
pub(crate) fn detect_terminal_background_from_env(colorfgbg: Option<&str>) -> TerminalColorScheme {
    if let Some(index) = colorfgbg.and_then(colorfgbg_background_index) {
        let hex = ansi256_to_hex(index);
        let rgb = hex_to_rgb(&hex).expect("ansi256_to_hex always yields #rrggbb");
        return theme_for_rgb(&rgb);
    }
    TerminalColorScheme::Dark
}

/// Parse a `#rrggbb` hex color (the ansi256 table emits these directly).
fn hex_to_rgb(hex: &str) -> Option<RgbColor> {
    let hex = hex.strip_prefix('#')?;
    Some(RgbColor {
        r: u8::from_str_radix(hex.get(0..2)?, 16).ok()?,
        g: u8::from_str_radix(hex.get(2..4)?, 16).ok()?,
        b: u8::from_str_radix(hex.get(4..6)?, 16).ok()?,
    })
}

/// `detectTerminalThemeForAuto` (theme.ts:777-789): DSR color-scheme query,
/// then OSC 11 background query, then `COLORFGBG`/fallback.
pub(crate) async fn detect_terminal_theme_for_auto(
    ui: &Tui,
    timeout_ms: u64,
) -> TerminalColorScheme {
    let timeout = Duration::from_millis(timeout_ms);
    // The Tui's own deadline resolves the oneshot; the tokio timeout is a
    // backstop for terminals that never reply (and tests without a pump).
    let slack = Duration::from_millis(50);
    let scheme = ui.query_terminal_color_scheme(timeout);
    if let Some(scheme) = tokio::time::timeout(timeout + slack, scheme)
        .await
        .ok()
        .and_then(|reply| reply.ok())
        .flatten()
    {
        return scheme;
    }
    let rgb = ui.query_terminal_background_color(timeout);
    if let Some(rgb) = tokio::time::timeout(timeout + slack, rgb)
        .await
        .ok()
        .and_then(|reply| reply.ok())
        .flatten()
    {
        return theme_for_rgb(&rgb);
    }
    detect_terminal_background_from_env(std::env::var("COLORFGBG").ok().as_deref())
}

// =============================================================================
// Theme file watcher (theme.ts:886-957)
// =============================================================================

/// One watcher tick: resolve the current custom-theme watch path from
/// settings and compare its mtime to the last seen value. Returns `true`
/// when the file changed since the last tick. Built-in themes (`dark`,
/// `light`), automatic pairs and missing files are not watched
/// (theme.ts:889-902).
fn poll_theme_change(
    ui_state: &InteractiveUi,
    last_seen: &mut Option<(PathBuf, SystemTime)>,
) -> bool {
    let name = ui_state
        .session()
        .settings_manager(|settings| settings.get_theme());
    let Some(path) = name.as_deref().and_then(get_theme_watch_path) else {
        *last_seen = None;
        return false;
    };
    let mtime = std::fs::metadata(&path)
        .and_then(|metadata| metadata.modified())
        .ok();
    match last_seen {
        Some((last_path, last_mtime)) if *last_path == path => {
            if let Some(mtime) = mtime {
                if mtime != *last_mtime {
                    *last_mtime = mtime;
                    return true;
                }
            }
            false
        }
        // New watch path (theme switched or first tick): register the file
        // state without firing (theme.ts:912-914 guards stale timers).
        _ => {
            if let Some(mtime) = mtime {
                *last_seen = Some((path, mtime));
            } else {
                *last_seen = None;
            }
            false
        }
    }
}

/// `startThemeWatcher` (theme.ts:886-957): a polling thread that watches the
/// current custom theme file and queues a [`UiCommand::ThemeChanged`] for the
/// drain whenever it changes. The drain performs the reload so the theme
/// swap happens on the driver thread (the theme fields' only writer). Stop
/// by setting the shared `stop` flag. The `ui` handle is kept for signature
/// symmetry with the drain-side apply (the reload itself runs on the drain,
/// which owns `ui_state.ui`).
pub(crate) fn spawn_theme_watcher(
    _ui: Tui,
    ui_state: Arc<InteractiveUi>,
    stop: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("rpi-theme-watcher".to_string())
        .spawn(move || {
            let mut last_seen: Option<(PathBuf, SystemTime)> = None;
            while !stop.load(Ordering::Relaxed) {
                if poll_theme_change(&ui_state, &mut last_seen) {
                    ui_state.push(UiCommand::ThemeChanged);
                    ui_state.render_handle.request_render();
                }
                std::thread::sleep(THEME_WATCH_POLL_INTERVAL);
            }
        })
        .expect("spawn theme watcher thread")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_for_rgb_uses_relative_luminance_threshold() {
        assert_eq!(
            theme_for_rgb(&RgbColor { r: 0, g: 0, b: 0 }),
            TerminalColorScheme::Dark
        );
        assert_eq!(
            theme_for_rgb(&RgbColor {
                r: 255,
                g: 255,
                b: 255
            }),
            TerminalColorScheme::Light
        );
        // #808080: luminance ≈ 0.216 < 0.5 → dark.
        assert_eq!(
            theme_for_rgb(&RgbColor {
                r: 0x80,
                g: 0x80,
                b: 0x80
            }),
            TerminalColorScheme::Dark
        );
        // #cccccc: luminance ≈ 0.604 ≥ 0.5 → light.
        assert_eq!(
            theme_for_rgb(&RgbColor {
                r: 0xcc,
                g: 0xcc,
                b: 0xcc
            }),
            TerminalColorScheme::Light
        );
        // #999999: luminance ≈ 0.319 < 0.5 → dark.
        assert_eq!(
            theme_for_rgb(&RgbColor {
                r: 0x99,
                g: 0x99,
                b: 0x99
            }),
            TerminalColorScheme::Dark
        );
    }

    #[test]
    fn parse_auto_theme_setting_accepts_exactly_one_slash_pair() {
        assert_eq!(
            parse_auto_theme_setting(Some("light/dark")),
            Some(("light".to_string(), "dark".to_string()))
        );
        assert_eq!(
            parse_auto_theme_setting(Some("nord/rose-pine")),
            Some(("nord".to_string(), "rose-pine".to_string()))
        );
        assert_eq!(parse_auto_theme_setting(Some("dark")), None);
        assert_eq!(parse_auto_theme_setting(Some("auto")), None);
        assert_eq!(parse_auto_theme_setting(Some("a/b/c")), None);
        assert_eq!(parse_auto_theme_setting(Some("/dark")), None);
        assert_eq!(parse_auto_theme_setting(Some("light/")), None);
        assert_eq!(parse_auto_theme_setting(Some("")), None);
        assert_eq!(parse_auto_theme_setting(None), None);
    }

    #[test]
    fn auto_theme_pair_accepts_plain_auto_shorthand() {
        assert_eq!(
            auto_theme_pair(Some("auto")),
            Some(("light".to_string(), "dark".to_string()))
        );
        assert_eq!(
            auto_theme_pair(Some("nord/rose-pine")),
            Some(("nord".to_string(), "rose-pine".to_string()))
        );
        assert_eq!(auto_theme_pair(Some("dark")), None);
        assert_eq!(auto_theme_pair(None), None);
    }

    #[test]
    fn colorfgbg_background_index_reads_last_numeric_segment() {
        assert_eq!(colorfgbg_background_index("15;0"), Some(0));
        assert_eq!(colorfgbg_background_index("0;15"), Some(15));
        assert_eq!(colorfgbg_background_index("4"), Some(4));
        assert_eq!(colorfgbg_background_index("x;y"), None);
        assert_eq!(colorfgbg_background_index(""), None);
    }

    #[test]
    fn detect_terminal_background_from_env_uses_colorfgbg_luminance() {
        // Index 0 = black → dark.
        assert_eq!(
            detect_terminal_background_from_env(Some("15;0")),
            TerminalColorScheme::Dark
        );
        // Index 15 = white → light.
        assert_eq!(
            detect_terminal_background_from_env(Some("0;15")),
            TerminalColorScheme::Light
        );
        // Index 4 = blue (#0000ff): luminance ≈ 0.072 → dark.
        assert_eq!(
            detect_terminal_background_from_env(Some("4")),
            TerminalColorScheme::Dark
        );
        // Index 7 = light gray (#c0c0c0): luminance ≈ 0.527 ≥ 0.5 → light.
        assert_eq!(
            detect_terminal_background_from_env(Some("7")),
            TerminalColorScheme::Light
        );
        // Index 232 = #080808 → dark; 255 = #fefefe → light (grayscale ramp).
        assert_eq!(
            detect_terminal_background_from_env(Some("232")),
            TerminalColorScheme::Dark
        );
        assert_eq!(
            detect_terminal_background_from_env(Some("255")),
            TerminalColorScheme::Light
        );
        // Missing env falls back to dark (theme.ts:747-752).
        assert_eq!(
            detect_terminal_background_from_env(None),
            TerminalColorScheme::Dark
        );
    }
}
