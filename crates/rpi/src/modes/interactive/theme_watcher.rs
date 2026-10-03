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
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use rpi_tui::terminal_colors::TerminalColorScheme;
use rpi_tui::tui_handle::TuiHandle;

use crate::core::themes::{TerminalTheme, get_theme_watch_path};
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
// Terminal appearance detection (theme.ts:697-716 @ a13d35a74)
// =============================================================================

/// Map between the rpi-tui scheme enum and the rpi theme enum.
fn to_theme(scheme: TerminalColorScheme) -> TerminalTheme {
    match scheme {
        TerminalColorScheme::Dark => TerminalTheme::Dark,
        TerminalColorScheme::Light => TerminalTheme::Light,
    }
}

fn to_scheme(theme: TerminalTheme) -> TerminalColorScheme {
    match theme {
        TerminalTheme::Dark => TerminalColorScheme::Dark,
        TerminalTheme::Light => TerminalColorScheme::Light,
    }
}

/// `detectColorFgBgTheme` (theme.ts:697-705 @ a13d35a74): `COLORFGBG`'s last
/// field is an ANSI index classified like Vim (0-6 and 8 dark, 7 and 9-15
/// light); missing/invalid values have no answer.
pub(crate) fn detect_color_fg_bg_theme(colorfgbg: Option<&str>) -> Option<TerminalColorScheme> {
    crate::core::themes::detect_color_fg_bg_theme(colorfgbg).map(to_scheme)
}

/// `detectTerminalBackgroundFromEnv` (theme.ts:734-753 @ 9841914, superseded
/// by `detectTerminalTheme` @ a13d35a74): `COLORFGBG`, falling back to dark.
pub(crate) fn detect_terminal_background_from_env(colorfgbg: Option<&str>) -> TerminalColorScheme {
    detect_color_fg_bg_theme(colorfgbg).unwrap_or(TerminalColorScheme::Dark)
}

/// `detectTerminalTheme` (theme.ts:707-716 @ a13d35a74): the reported
/// background (with the foreground as a tiebreaker) decides; without one the
/// terminal's light/dark report, then `COLORFGBG`, then dark.
pub(crate) fn detect_terminal_theme(
    colors: &rpi_tui::terminal_colors::TerminalColors,
    reported_scheme: Option<TerminalColorScheme>,
    colorfgbg: Option<&str>,
) -> TerminalColorScheme {
    to_scheme(crate::core::themes::detect_terminal_theme(
        colors,
        reported_scheme.map(to_theme),
        colorfgbg,
    ))
}

/// `detectTerminalThemeForAuto` (theme.ts:777-789 @ 9841914): the single-pass
/// OSC 10/11/4 query, then `detectTerminalTheme`. The Tui's own deadline
/// resolves the oneshot after `timeout_ms`; the tokio timeout is a backstop
/// for terminals that never reply (and tests without a pump).
pub(crate) async fn detect_terminal_theme_for_auto(
    ui: &TuiHandle,
    timeout_ms: u64,
) -> TerminalColorScheme {
    let timeout = Duration::from_millis(timeout_ms);
    let slack = Duration::from_millis(50);
    let colors = ui.query_terminal_colors(rpi_tui::tui::TerminalColorQueryOptions {
        timeout,
        on_late_reply: None,
    });
    let colors = tokio::time::timeout(timeout + slack, colors)
        .await
        .ok()
        .and_then(|reply| reply.ok())
        .unwrap_or_default();
    detect_terminal_theme(&colors, None, std::env::var("COLORFGBG").ok().as_deref())
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
            if let Some(mtime) = mtime
                && mtime != *last_mtime
            {
                *last_mtime = mtime;
                return true;
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
    _ui: TuiHandle,
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
    fn detect_color_fg_bg_theme_classifies_indices_like_vim() {
        // 0-6 and 8 are dark; 7 and 9-15 are light (theme.ts:697-705).
        assert_eq!(
            detect_color_fg_bg_theme(Some("15;0")),
            Some(TerminalColorScheme::Dark)
        );
        assert_eq!(
            detect_color_fg_bg_theme(Some("0;8")),
            Some(TerminalColorScheme::Dark)
        );
        assert_eq!(
            detect_color_fg_bg_theme(Some("0;7")),
            Some(TerminalColorScheme::Light)
        );
        assert_eq!(
            detect_color_fg_bg_theme(Some("0;15")),
            Some(TerminalColorScheme::Light)
        );
        assert_eq!(detect_color_fg_bg_theme(Some("0;16")), None);
        assert_eq!(detect_color_fg_bg_theme(Some("")), None);
        assert_eq!(detect_color_fg_bg_theme(None), None);
        // The rxvt three-field form uses the last field.
        assert_eq!(
            detect_color_fg_bg_theme(Some("0;0;1")),
            Some(TerminalColorScheme::Dark)
        );
    }

    #[test]
    fn detect_terminal_background_from_env_falls_back_to_dark() {
        assert_eq!(
            detect_terminal_background_from_env(Some("15;0")),
            TerminalColorScheme::Dark
        );
        assert_eq!(
            detect_terminal_background_from_env(Some("0;15")),
            TerminalColorScheme::Light
        );
        // Missing env falls back to dark (theme.ts:747-752).
        assert_eq!(
            detect_terminal_background_from_env(None),
            TerminalColorScheme::Dark
        );
    }

    #[test]
    fn detect_terminal_theme_prefers_reported_background_then_scheme_then_env() {
        use rpi_tui::terminal_colors::{RgbColor, TerminalColors};
        // Reported background wins.
        let colors = TerminalColors {
            background: Some(RgbColor {
                r: 255,
                g: 255,
                b: 255,
            }),
            ..TerminalColors::default()
        };
        assert_eq!(
            detect_terminal_theme(&colors, Some(TerminalColorScheme::Dark), Some("15;0")),
            TerminalColorScheme::Light
        );
        // Without a background, the light/dark report wins over COLORFGBG.
        assert_eq!(
            detect_terminal_theme(
                &TerminalColors::default(),
                Some(TerminalColorScheme::Light),
                Some("15;0")
            ),
            TerminalColorScheme::Light
        );
        // Then COLORFGBG, then dark.
        assert_eq!(
            detect_terminal_theme(&TerminalColors::default(), None, Some("0;15")),
            TerminalColorScheme::Light
        );
        assert_eq!(
            detect_terminal_theme(&TerminalColors::default(), None, None),
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
}
