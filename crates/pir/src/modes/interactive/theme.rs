//! Interactive-mode theme helpers — the Rust slice of
//! `packages/coding-agent/src/modes/interactive/theme/theme.ts` @ pi 0.82.1
//! (2efa728) needed by the S4a components.
//!
//! Upstream exposes the active theme as a process global (`theme` getter over
//! `globalThis`, theme.ts:799-816) and builds a `MarkdownTheme` from it
//! (`getMarkdownTheme`, theme.ts:1230-1271). Following the workspace's
//! explicit-injection convention (coding-standards §1.2), pir components take
//! an `Arc<Theme>`/`Arc<MarkdownTheme>` in their constructors instead; the
//! interactive mode (S4b) owns the single shared instance.

use std::sync::Arc;

use pir_tui::components::markdown::{HighlightFn, MarkdownTheme, ThemeTextFn};

use crate::core::themes::Theme;

/// `getMarkdownTheme` (theme.ts:1230-1271): a `MarkdownTheme` whose style
/// callbacks resolve through the given `Theme`.
///
/// Intentional differences:
/// - `highlightCode` (theme.ts:1230-1255) is ported in
///   `crate::core::highlight` (syntect instead of highlight.js, ADR-0008 /
///   D-051); the hook is attached here so fenced code blocks render colored.
/// - The theme is passed explicitly instead of read from a global.
pub fn markdown_theme(theme: &Theme) -> Arc<MarkdownTheme> {
    let fg = |color: &'static str| -> ThemeTextFn {
        let theme = theme.clone();
        Box::new(move |text: &str| theme.fg(color, text))
    };
    let highlight_code: HighlightFn = {
        let theme = theme.clone();
        Box::new(move |code: &str, lang: Option<&str>| -> Vec<String> {
            crate::core::highlight::highlight_code(code, lang, &theme)
        })
    };
    Arc::new(MarkdownTheme {
        heading: fg("mdHeading"),
        link: fg("mdLink"),
        link_url: fg("mdLinkUrl"),
        code: fg("mdCode"),
        code_block: fg("mdCodeBlock"),
        code_block_border: fg("mdCodeBlockBorder"),
        quote: fg("mdQuote"),
        quote_border: fg("mdQuoteBorder"),
        hr: fg("mdHr"),
        list_bullet: fg("mdListBullet"),
        bold: Box::new(|text: &str| Theme::bold(text)),
        italic: Box::new(|text: &str| Theme::italic(text)),
        strikethrough: Box::new(|text: &str| Theme::strikethrough(text)),
        underline: Box::new(|text: &str| Theme::underline(text)),
        highlight_code: Some(highlight_code),
        code_block_indent: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::themes::load_theme;

    fn theme() -> Theme {
        load_theme("dark", None).expect("builtin dark theme must load")
    }

    #[test]
    fn markdown_theme_callbacks_resolve_theme_colors() {
        let md = markdown_theme(&theme());
        let heading = (md.heading)("Title");
        assert!(heading.starts_with("\x1b["));
        assert!(heading.ends_with("\x1b[39m"));
        assert!(heading.contains("Title"));
        // Bold callback is plain SGR bold (theme.ts:1262).
        assert_eq!((md.bold)("x"), "\x1b[1mx\x1b[22m");
        assert_eq!((md.italic)("x"), "\x1b[3mx\x1b[23m");
        assert_eq!((md.strikethrough)("x"), "\x1b[9mx\x1b[29m");
        assert_eq!((md.underline)("x"), "\x1b[4mx\x1b[24m");
        // The syntax-highlighting hook is wired (T17-W2, ADR-0008): valid
        // languages produce ANSI-colored lines, invalid ones fall back to the
        // mdCodeBlock color (theme.ts:1166-1168).
        let highlight = md.highlight_code.as_ref().expect("highlight hook wired");
        let rust_lines = highlight("fn main() {}", Some("rust"));
        assert!(rust_lines.len() == 1 && rust_lines[0].contains("\x1b["));
        let fallback = highlight("plain text", Some("notalang"));
        assert_eq!(fallback[0], theme().fg("mdCodeBlock", "plain text"));
        let no_lang = highlight("plain text", None);
        assert_eq!(no_lang[0], theme().fg("mdCodeBlock", "plain text"));
    }
}
