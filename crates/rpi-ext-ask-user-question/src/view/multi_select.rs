//! Multi-select question rows.
//!
//! Port of upstream `view/components/multi-select-view.ts` @ `338b264c` for
//! the Q2 surface: one row per option (`→ ` pointer + number + `[x]`/`[ ]`
//! checkbox + label, description on continuation lines indented by two
//! columns), the `Type something.` inline-input row, and the `Next` sentinel
//! (labelled `Submit` on the last question).
//!
//! Visual notes ([VARIANT], TE-D40): `[x]`/`[ ]` are the rpi checkbox glyphs
//! (upstream `[✔]`/`[ ]`); pointers and colours follow the single-select
//! design.

use crate::i18n::I18n;
use crate::state::reducer::QuestionnaireState;
use crate::state::row_intent::RowKind;
use crate::tool::types::QuestionData;
use crate::view::inline_input;
use crate::view::option_list::{ACTIVE_POINTER, INACTIVE_POINTER, NUMBER_SEPARATOR};
use crate::view::theme::Theme;
use crate::view::{pad_number, spaces, truncate_line, visible_columns};

/// Body rows plus an optional cursor position relative to the returned lines
/// (re-exported shape of [`crate::view::option_list::BodyRender`]).
pub use crate::view::option_list::BodyRender;

/// Checked checkbox glyph.
pub const CHECKED: &str = "[x]";
/// Unchecked checkbox glyph.
pub const UNCHECKED: &str = "[ ]";
/// Gap between the checkbox and the label.
pub const BOX_LABEL_GAP: &str = " ";
/// Description continuation indent (upstream `CONTINUATION_INDENT`).
pub const CONTINUATION_INDENT: &str = "  ";
/// Last-question `Next` label (`MULTI_SUBMIT_LABEL`).
pub const MULTI_SUBMIT_LABEL: &str = "Submit";

/// Render one multi-select question body.
#[allow(clippy::too_many_arguments)] // render context is threaded explicitly (no component graph)
pub fn render(
    state: &QuestionnaireState,
    question: &QuestionData,
    i18n: &I18n,
    theme: &Theme,
    input_text: &str,
    input_cursor: Option<usize>,
    width: usize,
    is_last_question: bool,
) -> BodyRender {
    let focused = !state.notes_visible;
    let number_width = (question.options.len() + 1).max(1).to_string().len();
    let mut lines: Vec<String> = Vec::new();
    let mut cursor = None;
    let mut focused_range: Option<(usize, usize)> = None;

    for (index, option) in question.options.iter().enumerate() {
        let active = focused && index == state.option_index;
        let row_start = lines.len();
        let pointer = if active {
            ACTIVE_POINTER
        } else {
            INACTIVE_POINTER
        };
        let checked = state.multi_select_checked.contains(&index);
        let box_glyph = if checked { CHECKED } else { UNCHECKED };
        let box_style = if checked {
            theme.accent(box_glyph)
        } else {
            theme.muted(box_glyph)
        };
        let prefix = format!(
            "{}{}{NUMBER_SEPARATOR}{box_style}{BOX_LABEL_GAP}",
            if active {
                theme.accent(pointer)
            } else {
                pointer.to_owned()
            },
            pad_number(index + 1, number_width),
        );
        let prefix_width = visible_columns(&format!(
            "{pointer}{}{NUMBER_SEPARATOR}{box_glyph}{BOX_LABEL_GAP}",
            pad_number(index + 1, number_width)
        ));
        let content_width = width.saturating_sub(prefix_width).max(1);
        let label = truncate_line(&option.label, content_width);
        let styled = if active {
            theme.accent_bold(&label)
        } else {
            label
        };
        lines.push(truncate_line(&format!("{prefix}{styled}"), width));
        if !option.description.is_empty() {
            for segment in rpi_tui::utils::wrap_text_with_ansi(&option.description, content_width) {
                lines.push(truncate_line(
                    &format!("{CONTINUATION_INDENT}{}", theme.muted(&segment)),
                    width,
                ));
            }
        }
        if active {
            focused_range = Some((row_start, lines.len()));
        }
    }

    // `Type something.` row (always present on multi-select).
    let other_index = question.options.len();
    let other_row_start = lines.len();
    let other_active = focused && state.option_index == other_index;
    let other_pointer = if other_active {
        ACTIVE_POINTER
    } else {
        INACTIVE_POINTER
    };
    let other_prefix = format!(
        "{}{}{NUMBER_SEPARATOR}{}{BOX_LABEL_GAP}",
        if other_active {
            theme.accent(other_pointer)
        } else {
            other_pointer.to_owned()
        },
        pad_number(other_index + 1, number_width),
        theme.muted(UNCHECKED),
    );
    let other_prefix_width = visible_columns(&format!(
        "{other_pointer}{}{NUMBER_SEPARATOR}{UNCHECKED}{BOX_LABEL_GAP}",
        pad_number(other_index + 1, number_width)
    ));
    let other_content_width = width.saturating_sub(other_prefix_width).max(1);
    if other_active && state.input_mode {
        let rendered = inline_input::render_inline_input(
            input_text,
            input_cursor,
            &other_prefix,
            &spaces(other_prefix_width),
            other_content_width,
            |line| theme.accent_bold(line),
        );
        if let Some((line_index, column)) = rendered.cursor {
            cursor = Some((lines.len() + line_index, column));
        }
        lines.extend(rendered.lines);
    } else {
        let text = if input_text.is_empty() {
            i18n.display_label(RowKind::Other)
        } else {
            input_text.to_owned()
        };
        for (line_index, segment) in rpi_tui::utils::wrap_text_with_ansi(&text, other_content_width)
            .into_iter()
            .enumerate()
        {
            let line = if line_index == 0 {
                format!("{other_prefix}{segment}")
            } else {
                format!("{}{segment}", spaces(other_prefix_width))
            };
            lines.push(if other_active {
                theme.accent_bold(&line)
            } else {
                line
            });
        }
    }
    let other_row_end = lines.len();

    // `Next` sentinel (no number/checkbox; `Submit` on the last question).
    let next_index = question.options.len() + 1;
    let next_row_start = lines.len();
    let next_active = focused && state.option_index == next_index;
    let next_label = if is_last_question {
        MULTI_SUBMIT_LABEL.to_owned()
    } else {
        i18n.display_label(RowKind::Next)
    };
    let next_pointer = if next_active {
        theme.accent(ACTIVE_POINTER)
    } else {
        INACTIVE_POINTER.to_owned()
    };
    let next_label = if next_active {
        theme.accent_bold(&next_label)
    } else {
        next_label
    };
    lines.push(truncate_line(&format!("{next_pointer}{next_label}"), width));
    if other_active {
        focused_range = Some((other_row_start, other_row_end));
    }
    if next_active {
        focused_range = Some((next_row_start, lines.len()));
    }

    BodyRender {
        lines,
        cursor,
        focused_range,
    }
}

/// Number of body rows a question produces for the Next label decision
/// (unused outside rendering; kept for tests/documentation symmetry).
pub fn numbered_rows(question: &QuestionData) -> usize {
    question.options.len() + 1
}

/// Body rows as plain text (test helper; strips SGR sequences).
#[cfg(test)]
fn plain_lines(body: &BodyRender) -> Vec<String> {
    body.lines
        .iter()
        .map(|line| {
            let mut out = String::new();
            let mut chars = line.chars().peekable();
            while let Some(character) = chars.next() {
                if character == '\u{1b}' {
                    if chars.peek() == Some(&'[') {
                        chars.next();
                        for next in chars.by_ref() {
                            if next.is_ascii_alphabetic() {
                                break;
                            }
                        }
                    }
                    continue;
                }
                out.push(character);
            }
            out
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::types::OptionData;
    use rpi_tui::utils::visible_width;

    fn question() -> QuestionData {
        QuestionData {
            question: "Pick many".to_owned(),
            header: "H".to_owned(),
            options: vec![
                OptionData {
                    label: "A".to_owned(),
                    description: "first".to_owned(),
                    preview: None,
                },
                OptionData {
                    label: "B".to_owned(),
                    description: "second".to_owned(),
                    preview: None,
                },
            ],
            multi_select: Some(true),
        }
    }

    #[test]
    fn rows_have_checkboxes_and_a_next_sentinel() {
        let question = question();
        let i18n = I18n::for_locale("en");
        let theme = Theme::dark();
        let body = render(
            &QuestionnaireState::initial(),
            &question,
            &i18n,
            &theme,
            "",
            None,
            40,
            false,
        );
        let plain = plain_lines(&body);
        assert_eq!(
            plain,
            vec![
                "→ 1. [ ] A",
                "  first",
                "  2. [ ] B",
                "  second",
                "  3. [ ] Type something.",
                "  Next",
            ]
        );
    }

    #[test]
    fn checked_rows_use_the_checked_box_and_last_question_submits() {
        let question = question();
        let i18n = I18n::for_locale("en");
        let theme = Theme::dark();
        let mut state = QuestionnaireState::initial();
        state.multi_select_checked.insert(1);
        let body = render(&state, &question, &i18n, &theme, "", None, 40, true);
        let plain = plain_lines(&body);
        assert!(plain[2].contains("[x] B"), "{plain:?}");
        assert_eq!(plain[5], "  Submit", "last question uses the Submit label");
        assert_eq!(state.multi_select_checked.len(), 1);
    }

    #[test]
    fn active_other_row_renders_the_draft_with_cursor() {
        let question = question();
        let i18n = I18n::for_locale("en");
        let theme = Theme::dark();
        let mut state = QuestionnaireState::initial();
        state.option_index = 2;
        state.input_mode = true;
        let body = render(&state, &question, &i18n, &theme, "abc", Some(3), 40, false);
        let plain = plain_lines(&body);
        assert!(plain[4].contains("abc"), "{plain:?}");
        assert_eq!(
            body.cursor,
            Some((4, crate::view::visible_columns("→ 3. [ ] ") + 3))
        );
    }

    #[test]
    fn every_line_fits_the_width() {
        let question = question();
        let i18n = I18n::for_locale("en");
        let theme = Theme::dark();
        let body = render(
            &QuestionnaireState::initial(),
            &question,
            &i18n,
            &theme,
            "a long custom draft that wraps",
            None,
            18,
            false,
        );
        for line in &body.lines {
            assert!(visible_width(line) <= 18, "{line}");
        }
    }

    #[test]
    fn numbered_rows_counts_options_plus_other() {
        assert_eq!(numbered_rows(&question()), 3);
    }
}
