//! Single-select option rows.
//!
//! Port of upstream `view/components/option-list-view.ts` +
//! `view/components/wrapping-select.ts` @ `338b264c` reduced to the Q2
//! surface: numbered rows with a `→ ` active pointer, the `Type something.`
//! inline-input sentinel, a one-line wrapped description, and the `✔`
//! previously-confirmed marker.
//!
//! Visual notes ([VARIANT], TE-D40): the active pointer is the rpi `→ `
//! design (upstream `❯ `), and the row styling is the rpi theme palette. Row
//! *semantics* (numbering, sentinel behavior, confirmed marker, wrap) follow
//! upstream.

use crate::i18n::I18n;
use crate::state::build::QuestionItem;
use crate::state::reducer::QuestionnaireState;
use crate::state::row_intent::RowKind;
use crate::state::selectors::select_confirmed_indicator;
use crate::tool::types::{QuestionAnswer, QuestionData};
use crate::view::inline_input;
use crate::view::theme::Theme;
use crate::view::{pad_number, spaces, visible_columns};

use std::collections::BTreeMap;

/// Body rows plus an optional cursor position relative to the returned lines.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct BodyRender {
    /// Rendered lines (no trailing blank).
    pub lines: Vec<String>,
    /// `(line index, visible column)` of the input cursor.
    pub cursor: Option<(usize, usize)>,
}

/// Active-row pointer.
pub const ACTIVE_POINTER: &str = "→ ";
/// Inactive-row pointer (same visible width).
pub const INACTIVE_POINTER: &str = "  ";
/// Confirmed-row marker.
pub const CONFIRMED_MARK: &str = " ✔";
/// Number separator.
pub const NUMBER_SEPARATOR: &str = ". ";

/// Render one single-select question body.
#[allow(clippy::too_many_arguments)] // render context is threaded explicitly (no component graph)
pub fn render(
    state: &QuestionnaireState,
    question: &QuestionData,
    items: &[QuestionItem],
    _i18n: &I18n,
    theme: &Theme,
    input_text: &str,
    input_cursor: Option<usize>,
    width: usize,
) -> BodyRender {
    let focused = !state.notes_visible;
    let confirmed = select_confirmed_indicator(
        std::slice::from_ref(question),
        0,
        &strip_index(&state.answers, state.current_tab),
        items,
    );
    let number_width = items.len().max(1).to_string().len();
    let mut lines: Vec<String> = Vec::new();
    let mut cursor = None;

    for (index, item) in items.iter().enumerate() {
        let active = focused && index == state.option_index;
        let pointer = if active {
            ACTIVE_POINTER
        } else {
            INACTIVE_POINTER
        };
        let prefix = format!(
            "{pointer}{}{NUMBER_SEPARATOR}",
            pad_number(index + 1, number_width)
        );
        let prefix_width = visible_columns(&prefix);
        let content_width = width.saturating_sub(prefix_width).max(1);
        let continuation = spaces(prefix_width);

        let is_confirmed = confirmed
            .as_ref()
            .is_some_and(|indicator| indicator.index == index);
        let override_label = confirmed
            .as_ref()
            .filter(|indicator| indicator.index == index)
            .and_then(|indicator| indicator.label_override.clone());

        if item.kind == RowKind::Other && active && state.input_mode {
            let rendered = inline_input::render_inline_input(
                input_text,
                input_cursor,
                &prefix,
                &continuation,
                content_width,
                |line| theme.accent_bold(line),
            );
            if let Some((line_index, column)) = rendered.cursor {
                cursor = Some((lines.len() + line_index, column));
            }
            lines.extend(rendered.lines);
            continue;
        }

        let base_label = if item.kind == RowKind::Other && !input_text.is_empty() {
            input_text.to_owned()
        } else {
            item.label.clone()
        };
        let label = if is_confirmed {
            format!("{}{CONFIRMED_MARK}", override_label.unwrap_or(base_label))
        } else {
            base_label
        };
        let apply_selected = active || is_confirmed;
        for (line_index, segment) in rpi_tui::utils::wrap_text_with_ansi(&label, content_width)
            .into_iter()
            .enumerate()
        {
            let line = if line_index == 0 {
                format!("{prefix}{segment}")
            } else {
                format!("{continuation}{segment}")
            };
            lines.push(if apply_selected {
                if active {
                    theme.accent_bold(&line)
                } else {
                    theme.accent(&line)
                }
            } else {
                line
            });
        }

        if let Some(description) = item.description.as_deref().filter(|text| !text.is_empty()) {
            for segment in rpi_tui::utils::wrap_text_with_ansi(description, content_width) {
                lines.push(format!("{continuation}{}", theme.muted(&segment)));
            }
        }
    }

    BodyRender { lines, cursor }
}

/// The confirmed-indicator selector is keyed by `state.current_tab`; this
/// borrows the single-question map without cloning.
fn strip_index(
    answers: &BTreeMap<usize, QuestionAnswer>,
    current_tab: usize,
) -> BTreeMap<usize, QuestionAnswer> {
    let mut out = BTreeMap::new();
    if let Some(answer) = answers.get(&current_tab) {
        out.insert(0, answer.clone());
    }
    out
}

/// Render the localized sentinel label for a row (`Type something.`).
pub fn sentinel_label(i18n: &I18n, kind: RowKind) -> String {
    i18n.display_label(kind)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::build::build_items_for_question;
    use crate::tool::types::{AnswerKind, OptionData};
    use rpi_tui::utils::visible_width;

    fn question() -> QuestionData {
        QuestionData {
            question: "Pick one".to_owned(),
            header: "H".to_owned(),
            options: vec![
                OptionData {
                    label: "A".to_owned(),
                    description: "first option".to_owned(),
                    preview: None,
                },
                OptionData {
                    label: "B".to_owned(),
                    description: "second option".to_owned(),
                    preview: None,
                },
            ],
            multi_select: None,
        }
    }

    fn fixture() -> (QuestionData, Vec<QuestionItem>) {
        let i18n = I18n::for_locale("en");
        let question = question();
        let items = build_items_for_question(&question, &i18n);
        (question, items)
    }

    #[test]
    fn rows_carry_pointer_number_label_and_description() {
        let (question, items) = fixture();
        let i18n = I18n::for_locale("en");
        let theme = Theme::dark();
        let body = render(
            &QuestionnaireState::initial(),
            &question,
            &items,
            &i18n,
            &theme,
            "",
            None,
            40,
        );
        let plain: Vec<String> = body.lines.iter().map(|line| strip_ansi(line)).collect();
        assert_eq!(
            plain,
            vec![
                "→ 1. A",
                "     first option",
                "  2. B",
                "     second option",
                "  3. Type something.",
            ]
        );
    }

    #[test]
    fn active_other_row_renders_the_draft_and_reports_the_cursor() {
        let (question, items) = fixture();
        let i18n = I18n::for_locale("en");
        let theme = Theme::dark();
        let mut state = QuestionnaireState::initial();
        state.option_index = 2;
        state.input_mode = true;
        let body = render(
            &state,
            &question,
            &items,
            &i18n,
            &theme,
            "draft",
            Some(5),
            40,
        );
        assert!(body.lines[4].contains("draft"), "{:?}", body.lines);
        assert_eq!(
            body.cursor,
            Some((4, crate::view::visible_columns("→ 3. ") + 5))
        );
    }

    #[test]
    fn confirmed_answer_marks_the_row() {
        let (question, items) = fixture();
        let i18n = I18n::for_locale("en");
        let theme = Theme::dark();
        let mut state = QuestionnaireState::initial();
        state.answers.insert(
            0,
            QuestionAnswer {
                question_index: 0,
                question: "Pick one".to_owned(),
                kind: AnswerKind::Option,
                answer: Some("B".to_owned()),
                selected: None,
                notes: None,
                preview: None,
            },
        );
        let body = render(&state, &question, &items, &i18n, &theme, "", None, 40);
        let plain: Vec<String> = body.lines.iter().map(|line| strip_ansi(line)).collect();
        assert!(plain[2].contains("B ✔"), "{plain:?}");
    }

    #[test]
    fn description_wraps_and_lines_fit_the_width() {
        let i18n = I18n::for_locale("en");
        let theme = Theme::dark();
        let question = QuestionData {
            question: "Pick one".to_owned(),
            header: "H".to_owned(),
            options: vec![
                OptionData {
                    label: "A".to_owned(),
                    description: "a very long description that must wrap at narrow widths"
                        .to_owned(),
                    preview: None,
                },
                OptionData {
                    label: "B".to_owned(),
                    description: "b".to_owned(),
                    preview: None,
                },
            ],
            multi_select: None,
        };
        let items = build_items_for_question(&question, &i18n);
        let body = render(
            &QuestionnaireState::initial(),
            &question,
            &items,
            &i18n,
            &theme,
            "",
            None,
            24,
        );
        assert!(
            body.lines.len() > 3,
            "description wrapped: {:?}",
            body.lines
        );
        for line in &body.lines {
            assert!(visible_width(line) <= 24, "{line}");
        }
    }

    /// Local helper: strip SGR sequences for readable assertions.
    fn strip_ansi(text: &str) -> String {
        let mut out = String::new();
        let mut chars = text.chars().peekable();
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
    }

    #[test]
    fn confirmed_custom_answer_overrides_the_other_row_label() {
        let (question, items) = fixture();
        let i18n = I18n::for_locale("en");
        let theme = Theme::dark();
        let mut state = QuestionnaireState::initial();
        state.answers.insert(
            0,
            QuestionAnswer {
                question_index: 0,
                question: "Pick one".to_owned(),
                kind: AnswerKind::Custom,
                answer: Some("own words".to_owned()),
                selected: None,
                notes: None,
                preview: None,
            },
        );
        let body = render(
            &state,
            &question,
            &items,
            &i18n,
            &theme,
            "own words",
            None,
            40,
        );
        let plain: Vec<String> = body.lines.iter().map(|line| strip_ansi(line)).collect();
        assert!(plain[4].contains("own words ✔"), "{plain:?}");
    }
}
