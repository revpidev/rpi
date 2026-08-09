//! Shared visual-line truncation — port of
//! `packages/coding-agent/src/modes/interactive/components/visual-truncate.ts`
//! @ pi 0.82.1 (2efa728).
//!
//! Intentional differences: none (the temp `Text` component is constructed
//! directly instead of upstream's `new Text(...)`; same rendering).

use rpi_tui::components::text::Text;
use rpi_tui::tui::Component;

/// `VisualTruncateResult` (visual-truncate.ts:8-13).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisualTruncateResult {
    /// The visual lines to display.
    pub visual_lines: Vec<String>,
    /// Number of visual lines that were skipped (hidden).
    pub skipped_count: usize,
}

/// `truncateToVisualLines` (visual-truncate.ts:27-50): truncate text to a
/// maximum number of visual lines (from the end), accounting for line
/// wrapping at `width`.
///
/// `padding_x` is the horizontal padding for the temp `Text` component:
/// use 0 when the result is placed in a `Box` (Box adds its own padding),
/// 1 when placed in a plain `Container`.
pub fn truncate_to_visual_lines(
    text: &str,
    max_visual_lines: usize,
    width: usize,
    padding_x: usize,
) -> VisualTruncateResult {
    if text.is_empty() {
        return VisualTruncateResult {
            visual_lines: Vec::new(),
            skipped_count: 0,
        };
    }

    let temp_text = Text::new(text, padding_x, 0, None);
    let all_visual_lines = temp_text.render(width);

    if all_visual_lines.len() <= max_visual_lines {
        return VisualTruncateResult {
            visual_lines: all_visual_lines,
            skipped_count: 0,
        };
    }

    let truncated_lines = all_visual_lines[all_visual_lines.len() - max_visual_lines..].to_vec();
    let skipped_count = all_visual_lines.len() - max_visual_lines;
    VisualTruncateResult {
        visual_lines: truncated_lines,
        skipped_count,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpi_tui::utils::visible_width;

    #[test]
    fn empty_text_returns_empty() {
        let result = truncate_to_visual_lines("", 5, 20, 0);
        assert!(result.visual_lines.is_empty());
        assert_eq!(result.skipped_count, 0);
    }

    #[test]
    fn short_text_is_returned_untouched() {
        // The temp Text component pads every line to the full width
        // (text.ts:45-104), so the returned lines are padded.
        let result = truncate_to_visual_lines("hello\nworld", 5, 20, 0);
        assert_eq!(
            result.visual_lines,
            vec![
                "hello               ".to_string(),
                "world               ".to_string()
            ]
        );
        assert_eq!(result.skipped_count, 0);
    }

    #[test]
    fn keeps_last_n_visual_lines_after_wrapping() {
        // "aaa bbb ccc ddd" at width 8 wraps: "aaa bbb" / "ccc ddd" — 2 lines.
        let result = truncate_to_visual_lines("aaa bbb ccc ddd", 1, 8, 0);
        assert_eq!(result.skipped_count, 1);
        assert_eq!(result.visual_lines.len(), 1);
        assert!(result.visual_lines[0].contains("ccc"));
    }

    #[test]
    fn padding_applies_to_rendered_lines() {
        let result = truncate_to_visual_lines("hi", 1, 10, 1);
        assert_eq!(result.visual_lines.len(), 1);
        // Text pads to full width: " hi " + padding.
        assert_eq!(visible_width(&result.visual_lines[0]), 10);
    }
}
