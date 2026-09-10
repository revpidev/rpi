//! Dialog colour palette.
//!
//! The interactive-UI protocol delivers the host theme as JSON
//! (`ui.theme`, mirrored on the `theme` component event); the guest maps the
//! tokens it paints with onto [`AnsiStyle`] colours and falls back to a
//! built-in dark palette when a token is absent (first frame before the
//! `theme` event arrives, custom themes with missing keys).
//!
//! Visuals are the rpi-specific design ([VARIANT], TE-D40); behavior and key
//! handling stay upstream-aligned.

use rpi_ext_host::interactive_ui::{AnsiColor, AnsiStyle};
use serde_json::Value;

/// Colours the dialog paints with (a subset of the host theme tokens).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Theme {
    /// Primary text.
    pub text: AnsiColor,
    /// Secondary text (descriptions, answer labels).
    pub muted: AnsiColor,
    /// Tertiary text (hints).
    pub dim: AnsiColor,
    /// Selection/highlight accent.
    pub accent: AnsiColor,
    /// Answered/positive state.
    pub success: AnsiColor,
    /// Incomplete/unanswered warning.
    pub warning: AnsiColor,
    /// Selected-row background.
    pub selected_bg: AnsiColor,
}

impl Default for Theme {
    fn default() -> Self {
        Self::dark()
    }
}

impl Theme {
    /// Built-in dark palette (256-colour approximations of the rpi dark
    /// theme; used until `ui.theme`/`theme` arrives or a token is missing).
    pub fn dark() -> Self {
        Self {
            text: AnsiColor::Indexed(252),
            muted: AnsiColor::Indexed(245),
            dim: AnsiColor::Indexed(240),
            accent: AnsiColor::Indexed(109),
            success: AnsiColor::Indexed(108),
            warning: AnsiColor::Indexed(179),
            selected_bg: AnsiColor::Indexed(237),
        }
    }

    /// Build a palette from `ui.theme` JSON; each token falls back to
    /// [`Self::dark`] independently.
    pub fn from_json(theme: &Value) -> Self {
        let fallback = Self::dark();
        let Some(colors) = theme.get("colors").and_then(Value::as_object) else {
            return fallback;
        };
        let vars = theme.get("vars").and_then(Value::as_object);
        let resolve = |token: &str, fallback: AnsiColor| -> AnsiColor {
            colors
                .get(token)
                .and_then(|value| resolve_color(value, vars))
                .unwrap_or(fallback)
        };
        Self {
            text: resolve("text", fallback.text),
            muted: resolve("muted", fallback.muted),
            dim: resolve("dim", fallback.dim),
            accent: resolve("accent", fallback.accent),
            success: resolve("success", fallback.success),
            warning: resolve("warning", fallback.warning),
            selected_bg: resolve("selectedBg", fallback.selected_bg),
        }
    }

    /// Foreground paint with a plain style.
    pub fn fg(&self, color: AnsiColor, text: &str) -> String {
        AnsiStyle::new().fg(color).apply(text)
    }

    /// Muted paint.
    pub fn muted(&self, text: &str) -> String {
        self.fg(self.muted, text)
    }

    /// Dim paint.
    pub fn dim(&self, text: &str) -> String {
        self.fg(self.dim, text)
    }

    /// Accent paint.
    pub fn accent(&self, text: &str) -> String {
        self.fg(self.accent, text)
    }

    /// Bold accent paint (active labels).
    pub fn accent_bold(&self, text: &str) -> String {
        AnsiStyle::new().fg(self.accent).bold().apply(text)
    }

    /// Success paint.
    pub fn success(&self, text: &str) -> String {
        self.fg(self.success, text)
    }

    /// Warning paint.
    pub fn warning(&self, text: &str) -> String {
        self.fg(self.warning, text)
    }

    /// Bold text paint (question/heading lines).
    pub fn bold(&self, text: &str) -> String {
        AnsiStyle::new().fg(self.text).bold().apply(text)
    }

    /// Selected-row paint (background + primary text).
    pub fn selected(&self, text: &str) -> String {
        AnsiStyle::new()
            .bg(self.selected_bg)
            .fg(self.text)
            .apply(text)
    }
}

/// Resolve one theme colour value: `"#rrggbb"` hex, a `vars` reference, or a
/// numeric 256-colour index.
fn resolve_color(
    value: &Value,
    vars: Option<&serde_json::Map<String, Value>>,
) -> Option<AnsiColor> {
    if let Some(text) = value.as_str() {
        if let Some(hex) = text.strip_prefix('#') {
            return parse_hex(hex);
        }
        // Theme colours may reference a `vars` entry by name.
        return vars
            .and_then(|vars| vars.get(text))
            .and_then(|value| resolve_color(value, vars));
    }
    let index = value.as_u64()?;
    u8::try_from(index).ok().map(AnsiColor::Indexed)
}

fn parse_hex(hex: &str) -> Option<AnsiColor> {
    if hex.len() != 6 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let red = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let green = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let blue = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some(AnsiColor::Rgb(red, green, blue))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn theme_json_resolves_hex_vars_and_indices() {
        let theme = Theme::from_json(&json!({
            "vars": {"accent": "#8abeb7", "muted": "#808080"},
            "colors": {
                "text": "#d4d4d4",
                "muted": "muted",
                "accent": "accent",
                "success": 108,
                "warning": "not-a-color",
            }
        }));
        let fallback = Theme::dark();
        assert_eq!(theme.text, AnsiColor::Rgb(0xd4, 0xd4, 0xd4));
        assert_eq!(theme.muted, AnsiColor::Rgb(0x80, 0x80, 0x80));
        assert_eq!(theme.accent, AnsiColor::Rgb(0x8a, 0xbe, 0xb7));
        assert_eq!(theme.success, AnsiColor::Indexed(108));
        assert_eq!(
            theme.warning, fallback.warning,
            "unresolvable token falls back"
        );
    }

    #[test]
    fn theme_json_without_colors_falls_back_entirely() {
        assert_eq!(Theme::from_json(&json!({"name": "dark"})), Theme::dark());
        assert_eq!(Theme::from_json(&Value::Null), Theme::dark());
    }

    #[test]
    fn paint_helpers_emit_sgr_and_plain_passthrough() {
        let theme = Theme::dark();
        assert_eq!(theme.accent("x"), format!("\u{1b}[38;5;109mx\u{1b}[0m"));
        assert_eq!(
            theme.accent_bold("x"),
            format!("\u{1b}[1;38;5;109mx\u{1b}[0m")
        );
        assert_eq!(theme.bold("x"), format!("\u{1b}[1;38;5;252mx\u{1b}[0m"));
        assert!(theme.selected("x").starts_with("\u{1b}[38;5;252;48;5;237m"));
    }
}
