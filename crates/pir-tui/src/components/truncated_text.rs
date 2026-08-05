//! Port of `packages/tui/src/components/truncated-text.ts` @ pi 0.82.1 (2efa728).
//!
//! Intentional differences: none.

use crate::tui::Component;
use crate::utils::{truncate_to_width, visible_width};

/// Text component that truncates to fit viewport width (upstream
/// `TruncatedText`, truncated-text.ts:7).
pub struct TruncatedText {
    text: String,
    padding_x: usize,
    padding_y: usize,
}

impl TruncatedText {
    pub fn new(text: impl Into<String>, padding_x: usize, padding_y: usize) -> Self {
        Self {
            text: text.into(),
            padding_x,
            padding_y,
        }
    }
}

impl Component for TruncatedText {
    fn render(&self, width: usize) -> Vec<String> {
        let mut result: Vec<String> = Vec::new();

        // Empty line padded to width
        let empty_line = " ".repeat(width);

        // Add vertical padding above
        for _ in 0..self.padding_y {
            result.push(empty_line.clone());
        }

        // Calculate available width after horizontal padding
        let available_width = width.saturating_sub(self.padding_x * 2).max(1);

        // Take only the first line (stop at newline)
        let single_line_text = self.text.split('\n').next().unwrap_or("");

        // Truncate text if needed (accounting for ANSI codes)
        let display_text = truncate_to_width(single_line_text, available_width, "...", false);

        // Add horizontal padding
        let line_with_padding = format!(
            "{}{}{}",
            " ".repeat(self.padding_x),
            display_text,
            " ".repeat(self.padding_x)
        );

        // Pad line to exactly width characters
        let line_visible_width = visible_width(&line_with_padding);
        let padding_needed = width.saturating_sub(line_visible_width);
        let final_line = format!("{line_with_padding}{}", " ".repeat(padding_needed));

        result.push(final_line);

        // Add vertical padding below
        for _ in 0..self.padding_y {
            result.push(empty_line.clone());
        }

        result
    }
}

#[cfg(test)]
mod tests {
    //! Ports of `test/truncated-text.test.ts` @ pi 0.82.1 (2efa728), all 8
    //! cases (the 9th upstream case is covered by `truncates_styled_text_...`
    //! and `truncates_first_line_even_with_newlines_in_text`).

    use super::*;

    /// Strip SGR escape sequences, mirroring the upstream test regex
    /// `/\x1b\[[0-9;]*m/g` (truncated-text.test.ts:47).
    fn strip_sgr(input: &str) -> String {
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
    fn pads_output_lines_to_exactly_match_width() {
        let text = TruncatedText::new("Hello world", 1, 0);
        let lines = text.render(50);

        // Should have exactly one content line (no vertical padding)
        assert_eq!(lines.len(), 1);

        // Line should be exactly 50 visible characters
        assert_eq!(visible_width(&lines[0]), 50);
    }

    #[test]
    fn pads_output_with_vertical_padding_lines_to_width() {
        let text = TruncatedText::new("Hello", 0, 2);
        let lines = text.render(40);

        // Should have 2 padding lines + 1 content line + 2 padding lines = 5 total
        assert_eq!(lines.len(), 5);

        // All lines should be exactly 40 characters
        for line in &lines {
            assert_eq!(visible_width(line), 40);
        }
    }

    #[test]
    fn truncates_long_text_and_pads_to_width() {
        let long_text =
            "This is a very long piece of text that will definitely exceed the available width";
        let text = TruncatedText::new(long_text, 1, 0);
        let lines = text.render(30);

        assert_eq!(lines.len(), 1);

        // Should be exactly 30 characters
        assert_eq!(visible_width(&lines[0]), 30);

        // Should contain ellipsis
        let stripped = strip_sgr(&lines[0]);
        assert!(stripped.contains("..."));
    }

    #[test]
    fn preserves_ansi_codes_in_output_and_pads_correctly() {
        let styled_text = "\x1b[31mHello\x1b[39m \x1b[34mworld\x1b[39m";
        let text = TruncatedText::new(styled_text, 1, 0);
        let lines = text.render(40);

        assert_eq!(lines.len(), 1);

        // Should be exactly 40 visible characters (ANSI codes don't count)
        assert_eq!(visible_width(&lines[0]), 40);

        // Should preserve the color codes
        assert!(lines[0].contains("\x1b["));
    }

    #[test]
    fn truncates_styled_text_and_adds_reset_code_before_ellipsis() {
        let long_styled_text = format!(
            "\x1b[31m{}\x1b[39m",
            "This is a very long red text that will be truncated"
        );
        let text = TruncatedText::new(long_styled_text, 1, 0);
        let lines = text.render(20);

        assert_eq!(lines.len(), 1);

        // Should be exactly 20 visible characters
        assert_eq!(visible_width(&lines[0]), 20);

        // Should contain reset code before ellipsis
        assert!(lines[0].contains("\x1b[0m..."));
    }

    #[test]
    fn handles_text_that_fits_exactly() {
        // With paddingX=1, available width is 30-2=28
        // "Hello world" is 11 chars, fits comfortably
        let text = TruncatedText::new("Hello world", 1, 0);
        let lines = text.render(30);

        assert_eq!(lines.len(), 1);
        assert_eq!(visible_width(&lines[0]), 30);

        // Should NOT contain ellipsis
        let stripped = strip_sgr(&lines[0]);
        assert!(!stripped.contains("..."));
    }

    #[test]
    fn handles_empty_text() {
        let text = TruncatedText::new("", 1, 0);
        let lines = text.render(30);

        assert_eq!(lines.len(), 1);
        assert_eq!(visible_width(&lines[0]), 30);
    }

    #[test]
    fn stops_at_newline_and_only_shows_first_line() {
        let multiline_text = "First line\nSecond line\nThird line";
        let text = TruncatedText::new(multiline_text, 1, 0);
        let lines = text.render(40);

        assert_eq!(lines.len(), 1);
        assert_eq!(visible_width(&lines[0]), 40);

        // Should only contain "First line"
        let stripped = strip_sgr(&lines[0]);
        assert!(stripped.contains("First line"));
        assert!(!stripped.contains("Second line"));
        assert!(!stripped.contains("Third line"));
    }

    #[test]
    fn truncates_first_line_even_with_newlines_in_text() {
        let long_multiline_text =
            "This is a very long first line that needs truncation\nSecond line";
        let text = TruncatedText::new(long_multiline_text, 1, 0);
        let lines = text.render(25);

        assert_eq!(lines.len(), 1);
        assert_eq!(visible_width(&lines[0]), 25);

        // Should contain ellipsis and not second line
        let stripped = strip_sgr(&lines[0]);
        assert!(stripped.contains("..."));
        assert!(!stripped.contains("Second line"));
    }
}
