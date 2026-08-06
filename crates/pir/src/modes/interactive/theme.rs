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

use pir_tui::components::markdown::{MarkdownTheme, ThemeTextFn};

use crate::core::themes::Theme;

/// `getMarkdownTheme` (theme.ts:1230-1271): a `MarkdownTheme` whose style
/// callbacks resolve through the given `Theme`.
///
/// Intentional differences:
/// - `highlightCode` is not ported: pir has no syntax highlighter yet, so the
///   hook is left `None`, which makes the Markdown component fall back to the
///   `mdCodeBlock` color — identical to upstream's "no valid language" path
///   (theme.ts:1251-1255). A highlighter can be attached here later.
/// - The theme is passed explicitly instead of read from a global.
pub fn markdown_theme(theme: &Theme) -> Arc<MarkdownTheme> {
    let fg = |color: &'static str| -> ThemeTextFn {
        let theme = theme.clone();
        Box::new(move |text: &str| theme.fg(color, text))
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
        highlight_code: None,
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
        assert!(md.highlight_code.is_none());
    }
}
