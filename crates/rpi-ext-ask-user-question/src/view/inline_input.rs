//! Inline multiline input row (`Type something.` / notes drafts).
//!
//! Port of the rendering half of upstream
//! `view/components/inline-input.ts` @ `338b264c`: the buffer occupies the
//! row prefix on its first line and a continuation prefix on wrapped/logical
//! continuation lines; the cursor cell is reported to the host (explicit
//! `cursor:{row,col}`, R-U3.2) instead of carrying upstream's reverse-video +
//! `CURSOR_MARKER` pair.
//!
//! rpi adaptation: cursor offsets are **character** offsets (the session's
//! `InputBuffer` is char-indexed), not pi-tui's UTF-16 offsets; the host
//! cursor snap in `component_registry` already protects wide graphemes.

use rpi_tui::utils::visible_width;

/// One wrapped segment together with its source character range.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WrappedSegment {
    /// Rendered segment text (no prefix).
    pub text: String,
    /// Inclusive start char offset in the source line.
    pub start: usize,
    /// Exclusive end char offset in the source line.
    pub end: usize,
    /// The segment ends at an explicit newline (not a visual wrap).
    pub hard_break: bool,
}

/// Character-based wrapper that keeps source offsets, so the cursor can be
/// located inside the wrapped output. Words are not re-flowed (input drafts
/// break at the width boundary), which keeps cursor math exact.
pub fn wrap_with_offsets(line: &str, width: usize) -> Vec<WrappedSegment> {
    let width = width.max(1);
    let mut segments: Vec<WrappedSegment> = Vec::new();
    let mut current = String::new();
    let mut current_start = 0usize;
    let mut offset = 0usize;

    for character in line.chars() {
        if character == '\n' {
            segments.push(WrappedSegment {
                text: std::mem::take(&mut current),
                start: current_start,
                end: offset,
                hard_break: true,
            });
            offset += 1;
            current_start = offset;
            continue;
        }
        let character_width = visible_width(&character.to_string());
        if !current.is_empty() && visible_width(&current) + character_width > width {
            segments.push(WrappedSegment {
                text: std::mem::take(&mut current),
                start: current_start,
                end: offset,
                hard_break: false,
            });
            current_start = offset;
        }
        current.push(character);
        offset += 1;
    }
    // Trailing segment (also for an empty buffer: one empty segment exists so
    // the cursor has a home).
    segments.push(WrappedSegment {
        text: current,
        start: current_start,
        end: offset,
        hard_break: false,
    });
    segments
}

/// Locate `cursor` (char offset) in the wrapped segments, returning
/// `(segment index, column within the segment)`.
fn cursor_in_segments(segments: &[WrappedSegment], cursor: usize) -> (usize, usize) {
    for (index, segment) in segments.iter().enumerate() {
        if cursor < segment.end || (cursor == segment.end && segment.hard_break) {
            let column = visible_width(&slice_chars(&segment.text, 0, cursor - segment.start));
            return (index, column);
        }
        if cursor == segment.end {
            // Cursor sits on a visual wrap boundary: prefer the start of the
            // next segment (the next char is rendered there).
            if segments.get(index + 1).is_some() {
                return (index + 1, 0);
            }
            return (index, visible_width(&segment.text));
        }
    }
    let last = segments.len().saturating_sub(1);
    (
        last,
        visible_width(segments.get(last).map(|s| s.text.as_str()).unwrap_or("")),
    )
}

fn slice_chars(text: &str, start: usize, count: usize) -> String {
    text.chars().skip(start).take(count).collect()
}

/// Rendered inline-input row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InlineInputLines {
    /// One or more rendered lines (prefix included, ANSI styled the same way
    /// on every line).
    pub lines: Vec<String>,
    /// Cursor position inside `lines` (`(segment index, visible column)`) when
    /// a cursor offset was supplied.
    pub cursor: Option<(usize, usize)>,
}

/// Render `text` across the logical/wrapped lines of one row.
///
/// `row_prefix` heads the first line; `continuation_prefix` heads every
/// continuation line. `style` wraps each finished line (the caller chooses
/// selected/plain). `content_width` excludes the prefix columns.
pub fn render_inline_input(
    text: &str,
    cursor: Option<usize>,
    row_prefix: &str,
    continuation_prefix: &str,
    content_width: usize,
    style: impl Fn(&str) -> String,
) -> InlineInputLines {
    let segments = wrap_with_offsets(text, content_width);
    let located = cursor.map(|offset| cursor_in_segments(&segments, offset));
    let prefix_for = |index: usize| {
        if index == 0 {
            row_prefix
        } else {
            continuation_prefix
        }
    };
    let lines = segments
        .iter()
        .enumerate()
        .map(|(index, segment)| style(&format!("{}{}", prefix_for(index), segment.text)))
        .collect();
    // The reported column is absolute within the rendered line (prefix
    // included) so the dialog can use it as a frame column verbatim.
    let cursor = located.map(|(index, column)| (index, visible_width(prefix_for(index)) + column));
    InlineInputLines { lines, cursor }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_with_offsets_keeps_char_ranges_across_logical_and_visual_breaks() {
        let segments = wrap_with_offsets("abcdef\ngh", 3);
        assert_eq!(
            segments,
            vec![
                WrappedSegment {
                    text: "abc".to_owned(),
                    start: 0,
                    end: 3,
                    hard_break: false
                },
                WrappedSegment {
                    text: "def".to_owned(),
                    start: 3,
                    end: 6,
                    hard_break: true
                },
                WrappedSegment {
                    text: "gh".to_owned(),
                    start: 7,
                    end: 9,
                    hard_break: false
                },
            ]
        );
    }

    #[test]
    fn cursor_in_segments_prefers_next_segment_on_visual_wrap() {
        let segments = wrap_with_offsets("abcdef", 3);
        assert_eq!(cursor_in_segments(&segments, 0), (0, 0));
        assert_eq!(cursor_in_segments(&segments, 2), (0, 2));
        assert_eq!(cursor_in_segments(&segments, 3), (1, 0));
        assert_eq!(cursor_in_segments(&segments, 6), (1, 3));
    }

    #[test]
    fn cursor_in_segments_stays_on_line_before_hard_break() {
        let segments = wrap_with_offsets("abc\ndef", 10);
        assert_eq!(cursor_in_segments(&segments, 3), (0, 3));
        assert_eq!(cursor_in_segments(&segments, 4), (1, 0));
        assert_eq!(cursor_in_segments(&segments, 5), (1, 1));
    }

    #[test]
    fn render_inline_input_prefixes_and_wraps() {
        let rendered =
            render_inline_input("abcdef", Some(6), "> ", "  ", 3, |line| line.to_owned());
        assert_eq!(rendered.lines, vec!["> abc", "  def"]);
        assert_eq!(rendered.cursor, Some((1, 2 + 3)));
    }

    #[test]
    fn render_inline_input_empty_buffer_has_one_line_and_cursor() {
        let rendered = render_inline_input("", Some(0), "> ", "  ", 10, |line| line.to_owned());
        assert_eq!(rendered.lines, vec!["> "]);
        assert_eq!(rendered.cursor, Some((0, 2)));
    }

    #[test]
    fn render_inline_input_wide_chars_count_visible_columns() {
        let rendered =
            render_inline_input("汉字", Some(2), "> ", "  ", 10, |line| line.to_owned());
        assert_eq!(rendered.lines, vec!["> 汉字"]);
        assert_eq!(rendered.cursor, Some((0, 6)));
    }
}
