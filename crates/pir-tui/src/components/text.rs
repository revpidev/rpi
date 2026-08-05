//! Port of `packages/tui/src/components/text.ts` @ pi 0.82.1 (2efa728).
//!
//! Intentional differences:
//! - The render cache uses interior mutability (`RefCell`) because
//!   `Component::render` takes `&self`; the component stays `Send` (but not
//!   `Sync`), which matches the single-threaded render loop.
//! - The color callback is `Box<dyn Fn(&str) -> String + Send + Sync>` instead
//!   of the upstream TS `(text: string) => string` type.

use std::cell::RefCell;

use crate::tui::Component;
use crate::utils::{apply_background_to_line, visible_width, wrap_text_with_ansi};

/// Color callback type, mirroring the upstream TS `(text: string) => string`
/// functions (e.g. `chalk.cyan`).
pub type ColorFn = Box<dyn Fn(&str) -> String + Send + Sync>;

/// Text component - displays multi-line text with word wrapping
/// (upstream `Text`, text.ts:7).
pub struct Text {
    text: String,
    padding_x: usize, // Left/right padding
    padding_y: usize, // Top/bottom padding
    custom_bg_fn: Option<ColorFn>,

    // Cache for rendered output
    cache: RefCell<Option<TextCache>>,
}

struct TextCache {
    text: String,
    width: usize,
    lines: Vec<String>,
}

impl Text {
    pub fn new(
        text: impl Into<String>,
        padding_x: usize,
        padding_y: usize,
        custom_bg_fn: Option<ColorFn>,
    ) -> Self {
        Self {
            text: text.into(),
            padding_x,
            padding_y,
            custom_bg_fn,
            cache: RefCell::new(None),
        }
    }

    pub fn set_text(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.invalidate();
    }

    pub fn set_custom_bg_fn(&mut self, custom_bg_fn: Option<ColorFn>) {
        self.custom_bg_fn = custom_bg_fn;
        self.invalidate();
    }

    fn invalidate_cache(&self) {
        *self.cache.borrow_mut() = None;
    }

    /// Shared render pipeline for an arbitrary text and width, with the same
    /// cache semantics as upstream `Text.render` (text.ts:45-104). `Loader`
    /// computes its display text at render time and reuses this (and the
    /// cache keyed on the text value).
    pub(crate) fn render_text(&self, text: &str, width: usize) -> Vec<String> {
        // Check cache
        if let Some(cache) = self.cache.borrow().as_ref() {
            if cache.text == text && cache.width == width {
                return cache.lines.clone();
            }
        }

        // Don't render anything if there's no actual text
        if text.trim().is_empty() {
            let result: Vec<String> = Vec::new();
            *self.cache.borrow_mut() = Some(TextCache {
                text: text.to_string(),
                width,
                lines: result.clone(),
            });
            return result;
        }

        // Replace tabs with 3 spaces
        let normalized_text = text.replace('\t', "   ");

        // Calculate content width (subtract left/right margins)
        let content_width = width.saturating_sub(self.padding_x * 2).max(1);

        // Wrap text (this preserves ANSI codes but does NOT pad)
        let wrapped_lines = wrap_text_with_ansi(&normalized_text, content_width);

        // Add margins and background to each line
        let left_margin = " ".repeat(self.padding_x);
        let right_margin = " ".repeat(self.padding_x);
        let mut content_lines: Vec<String> = Vec::new();

        for line in &wrapped_lines {
            // Add margins
            let line_with_margins = format!("{left_margin}{line}{right_margin}");

            // Apply background if specified (this also pads to full width)
            if let Some(bg_fn) = &self.custom_bg_fn {
                content_lines.push(apply_background_to_line(&line_with_margins, width, bg_fn));
            } else {
                // No background - just pad to width with spaces
                let visible_len = visible_width(&line_with_margins);
                let padding_needed = width.saturating_sub(visible_len);
                content_lines.push(format!("{line_with_margins}{}", " ".repeat(padding_needed)));
            }
        }

        // Add top/bottom padding (empty lines)
        let empty_line = " ".repeat(width);
        let mut empty_lines: Vec<String> = Vec::new();
        for _ in 0..self.padding_y {
            let line = if let Some(bg_fn) = &self.custom_bg_fn {
                apply_background_to_line(&empty_line, width, bg_fn)
            } else {
                empty_line.clone()
            };
            empty_lines.push(line);
        }

        let mut result = empty_lines.clone();
        result.extend(content_lines);
        result.extend(empty_lines);

        let lines = if result.is_empty() {
            vec![String::new()]
        } else {
            result
        };

        // Update cache
        *self.cache.borrow_mut() = Some(TextCache {
            text: text.to_string(),
            width,
            lines: lines.clone(),
        });

        lines
    }
}

impl Component for Text {
    fn render(&self, width: usize) -> Vec<String> {
        self.render_text(&self.text, width)
    }

    fn invalidate(&mut self) {
        self.invalidate_cache();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::visible_width;

    fn strip_ansi(input: &str) -> String {
        let mut out = String::with_capacity(input.len());
        let mut chars = input.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' && chars.peek() == Some(&'[') {
                chars.next();
                for c in chars.by_ref() {
                    if c == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    #[test]
    fn empty_or_whitespace_text_renders_no_lines() {
        assert!(Text::new("", 1, 1, None).render(10).is_empty());
        assert!(Text::new("   \n  ", 1, 1, None).render(10).is_empty());
    }

    #[test]
    fn pads_content_to_full_width() {
        let text = Text::new("hi", 1, 0, None);
        let lines = text.render(10);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0], format!(" hi {}", " ".repeat(6)));
        assert_eq!(visible_width(&lines[0]), 10);
    }

    #[test]
    fn wraps_long_text_to_content_width() {
        let text = Text::new("hello world", 1, 0, None);
        let lines = text.render(6); // content width 6 - 2 = 4
        assert_eq!(lines.len(), 4);
        for line in &lines {
            assert_eq!(visible_width(line), 6);
        }
        assert!(lines[0].contains("hell"));
    }

    #[test]
    fn replaces_tabs_with_three_spaces() {
        let text = Text::new("a\tb", 0, 0, None);
        let lines = text.render(20);
        assert_eq!(lines.len(), 1);
        assert_eq!(strip_ansi(&lines[0]).trim_end(), "a   b");
    }

    #[test]
    fn adds_vertical_padding_lines() {
        let text = Text::new("hi", 1, 2, None);
        let lines = text.render(10);
        assert_eq!(lines.len(), 5); // 2 top + 1 content + 2 bottom
        assert_eq!(lines[0], " ".repeat(10));
        assert_eq!(lines[4], " ".repeat(10));
        assert_eq!(visible_width(&lines[2]), 10);
    }

    #[test]
    fn applies_custom_background_to_all_lines() {
        let text = Text::new(
            "hi",
            1,
            1,
            Some(Box::new(|line: &str| format!("\x1b[48;5;1m{line}\x1b[0m"))),
        );
        let lines = text.render(10);
        assert_eq!(lines.len(), 3); // top pad + content + bottom pad
        for line in &lines {
            assert!(line.starts_with("\x1b[48;5;1m"));
            assert!(line.ends_with("\x1b[0m"));
            assert_eq!(visible_width(line), 10);
        }
    }

    #[test]
    fn render_cache_returns_same_output_for_same_text_and_width() {
        let text = Text::new("hello", 1, 0, None);
        assert_eq!(text.render(20), text.render(20));
        assert_eq!(text.render(10), text.render(10));
    }

    #[test]
    fn set_text_invalidates_cache() {
        let mut text = Text::new("hello", 1, 0, None);
        let before = text.render(20);
        text.set_text("world");
        let after = text.render(20);
        assert_ne!(before, after);
        assert!(strip_ansi(&after[0]).trim_end().ends_with("world"));
    }

    #[test]
    fn set_custom_bg_fn_invalidates_cache() {
        let mut text = Text::new("hi", 1, 0, None);
        let before = text.render(10);
        assert!(!before[0].contains('\x1b'));
        text.set_custom_bg_fn(Some(Box::new(|line: &str| {
            format!("\x1b[48;5;1m{line}\x1b[0m")
        })));
        let after = text.render(10);
        assert!(after[0].starts_with("\x1b[48;5;1m"));
    }

    #[test]
    fn invalidate_rebuilds_cache() {
        let mut text = Text::new("hi", 1, 0, None);
        let before = text.render(10);
        text.invalidate();
        assert_eq!(before, text.render(10));
    }
}
