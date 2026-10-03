//! Shared visual-line truncation — port of
//! `packages/coding-agent/src/modes/interactive/components/visual-truncate.ts`
//! @ pi 0.82.1 (2efa728) with the `keep` option and `VisualLinePreview` from
//! 0.99.2 (0582d9c11).
//!
//! Intentional differences: none (the temp `Text` component is constructed
//! directly instead of upstream's `new Text(...)`; same rendering).

use std::sync::Mutex;

use rpi_tui::components::text::Text;
use rpi_tui::tui::Component;
use rpi_tui::utils::truncate_to_width;

/// `VisualTruncateResult` (visual-truncate.ts:8-13).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisualTruncateResult {
    /// The visual lines to display.
    pub visual_lines: Vec<String>,
    /// Number of visual lines that were skipped (hidden).
    pub skipped_count: usize,
}

/// Which visual lines to keep when truncating (`keep`, visual-truncate.ts
/// @ 0582d9c11).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisualKeep {
    /// Keep the first visual lines (previews that show the start).
    Start,
    /// Keep the last visual lines (bash output, which tails).
    End,
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
    truncate_to_visual_lines_keep(
        text,
        max_visual_lines,
        width,
        padding_x,
        VisualKeep::End,
    )
}

/// [`truncate_to_visual_lines`] with an explicit `keep` end
/// (visual-truncate.ts:30-52 @ 0582d9c11).
pub fn truncate_to_visual_lines_keep(
    text: &str,
    max_visual_lines: usize,
    width: usize,
    padding_x: usize,
    keep: VisualKeep,
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

    let truncated_lines = match keep {
        VisualKeep::Start => all_visual_lines[..max_visual_lines].to_vec(),
        VisualKeep::End => all_visual_lines[all_visual_lines.len() - max_visual_lines..].to_vec(),
    };
    let skipped_count = all_visual_lines.len() - max_visual_lines;
    VisualTruncateResult {
        visual_lines: truncated_lines,
        skipped_count,
    }
}

/// `VisualLinePreview` (visual-truncate.ts:54-90 @ 0582d9c11): collapsed
/// tool output limited to a number of visual lines. Limiting logical lines
/// instead lets a single long line (such as minified JSON) wrap across the
/// whole screen. Caches its lines per width, since it renders on every
/// frame for every result in the transcript.
///
/// The hint goes before kept end lines and after kept start lines, like
/// upstream (`keep`-dependent placement).
pub struct VisualLinePreview {
    text: String,
    max_visual_lines: usize,
    keep: VisualKeep,
    hint: Option<Box<dyn Fn(usize) -> String + Send + Sync>>,
    cache: Mutex<Option<(usize, Vec<String>)>>,
}

impl VisualLinePreview {
    pub fn new(text: impl Into<String>, max_visual_lines: usize, keep: VisualKeep) -> Self {
        Self {
            text: text.into(),
            max_visual_lines,
            keep,
            hint: None,
            cache: Mutex::new(None),
        }
    }

    /// Sets the styled hint line for the given number of hidden visual
    /// lines (`formatHint`, visual-truncate.ts:63).
    pub fn with_hint(
        mut self,
        hint: Box<dyn Fn(usize) -> String + Send + Sync>,
    ) -> Self {
        self.hint = Some(hint);
        self
    }
}

impl Component for VisualLinePreview {
    fn render(&self, width: usize) -> Vec<String> {
        if let Some((cached_width, lines)) = self
            .cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            && *cached_width == width
        {
            return lines.clone();
        }
        let preview = truncate_to_visual_lines_keep(
            &self.text,
            self.max_visual_lines,
            width,
            0,
            self.keep,
        );
        let mut lines = preview.visual_lines;
        if preview.skipped_count > 0 {
            let hint = match &self.hint {
                Some(format) => format(preview.skipped_count),
                None => format!("... ({} more lines)", preview.skipped_count),
            };
            let hint = truncate_to_width(&hint, width, "...", false);
            match self.keep {
                VisualKeep::Start => lines.push(hint),
                VisualKeep::End => lines.insert(0, hint),
            }
        }
        *self
            .cache
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some((width, lines.clone()));
        lines
    }

    fn invalidate(&mut self) {
        *self.cache.lock().unwrap_or_else(|e| e.into_inner()) = None;
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

    #[test]
    fn keep_start_keeps_the_first_visual_lines() {
        let result = truncate_to_visual_lines_keep(
            "aaa bbb ccc ddd",
            1,
            8,
            0,
            VisualKeep::Start,
        );
        assert_eq!(result.skipped_count, 1);
        assert_eq!(result.visual_lines.len(), 1);
        assert!(result.visual_lines[0].contains("aaa"));
    }

    /// Regression for #10192 (upstream codemode-renderer.test.ts @ 0582d9c11):
    /// collapsed output is limited to wrapped lines, not logical lines.
    #[test]
    fn visual_line_preview_limits_wrapped_lines_and_appends_start_hint() {
        let long = "x".repeat(1000);
        let preview = VisualLinePreview::new(long, 5, VisualKeep::Start)
            .with_hint(Box::new(|hidden| format!("... ({hidden} more lines)")));
        let lines = preview.render(50);
        assert_eq!(lines.len(), 6, "5 kept + hint");
        // The hint is the last line and names the hidden visual lines.
        assert_eq!(lines[5], "... (15 more lines)");
        assert!(lines[0].starts_with("xxx"));
    }

    #[test]
    fn visual_line_preview_places_the_hint_before_kept_end_lines() {
        let text = (1..=20)
            .map(|i| format!("line-{i:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        let preview = VisualLinePreview::new(text, 3, VisualKeep::End);
        let lines = preview.render(40);
        assert_eq!(lines.len(), 4);
        assert!(lines[0].contains("... (17 more lines)"));
        assert!(lines[1].contains("line-18"));
        assert!(lines[3].contains("line-20"));
    }

    #[test]
    fn visual_line_preview_caches_per_width() {
        let text = "a".repeat(100);
        let preview = VisualLinePreview::new(text, 2, VisualKeep::Start);
        let first = preview.render(20);
        assert_eq!(preview.render(20), first, "same width reuses the cache");
        let second = preview.render(30);
        assert_ne!(second, first, "a new width re-truncates");
    }
}
