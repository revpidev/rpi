//! Line-frame renderer for the questionnaire dialog (route C).
//!
//! The guest owns the UX: [`dialog::render`] turns the canonical state into
//! `Vec<String>` frames with ANSI SGR styling, and the interactive-UI ABI
//! composites them into the host's overlay (ADR-0024 / R-U3). Visuals are the
//! rpi design ([VARIANT], TE-D40); behavior and key handling stay aligned
//! with upstream `packages/rpiv-ask-user-question` @ `338b264c`.
//!
//! Every emitted line is width-clipped here (ANSI-aware), so the host's own
//! clipping is a no-op and golden frames are deterministic across widths.

pub mod dialog;
pub mod inline_input;
pub mod multi_select;
pub mod option_list;
pub mod preview;
pub mod submit;
pub mod tab_bar;
pub mod theme;

use rpi_tui::utils::{truncate_to_width, visible_width};

/// One rendered frame: lines plus the hardware-cursor position.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct RenderedFrame {
    /// Frame lines (ANSI SGR may appear).
    pub lines: Vec<String>,
    /// `(row, col)` of the input cursor when a text editor owns focus.
    pub cursor: Option<(usize, usize)>,
}

/// Visible column count of `text` (ANSI-aware).
pub fn visible_columns(text: &str) -> usize {
    visible_width(text)
}

/// Clip `line` to `width` visible columns, appending `…` when truncated.
pub fn truncate_line(line: &str, width: usize) -> String {
    if visible_width(line) <= width {
        return line.to_owned();
    }
    truncate_to_width(line, width, "…", false)
}

/// Zero-padded row number (`padStart`) used by the option rows.
pub fn pad_number(value: usize, width: usize) -> String {
    let text = value.to_string();
    let padding = width.saturating_sub(text.len());
    format!("{}{text}", " ".repeat(padding))
}

/// `n` spaces.
pub fn spaces(count: usize) -> String {
    " ".repeat(count)
}
