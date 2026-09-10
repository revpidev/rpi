//! Dialog layout assembly + footer hints.
//!
//! Port of upstream `view/dialog-builder.ts` @ `338b264c` for the Q2 surface:
//! sticky question heading, tab bar (multi-question), the active body
//! (single-select options / multi-select / Submit review), and one clipped
//! footer hint line. The bordered `DialogView` chrome, terminal-height scroll
//! math and overflow indicators are Q3 (FR-Q3-B/C).

use crate::config::format_key_spec_for_display;
use crate::i18n::I18n;
use crate::state::build::QuestionItem;
use crate::state::reducer::QuestionnaireState;
use crate::state::row_intent::RowKind;
use crate::tool::types::QuestionData;
use crate::view::theme::Theme;
use crate::view::{multi_select, option_list, submit, tab_bar, truncate_line, RenderedFrame};

/// Hint literals (canonical-English fallbacks of the `hint.*` locale keys).
pub const HINT_PART_ENTER: &str = "Enter to select";
/// `↑/↓ to navigate`.
pub const HINT_PART_NAV: &str = "↑/↓ to navigate";
/// `Shift+Enter for newline`.
pub const HINT_PART_NEW_LINE: &str = "Shift+Enter for newline";
/// `Ctrl+U to clear`.
pub const HINT_PART_CLEAR: &str = "Ctrl+U to clear";
/// `Space to toggle`.
pub const HINT_PART_TOGGLE: &str = "Space to toggle";
/// `n to add notes`.
pub const HINT_PART_NOTES: &str = "n to add notes";
/// `Tab to switch questions`.
pub const HINT_PART_TAB: &str = "Tab to switch questions";
/// `Esc to cancel`.
pub const HINT_PART_CANCEL: &str = "Esc to cancel";
/// `{key} to collapse`.
pub const HINT_PART_COLLAPSE_TEMPLATE: &str = "{key} to collapse";
/// `n to add a note` (Submit-tab hint).
pub const REVIEW_GLOBAL_HINT: &str = "n to add a note";
/// Collapse key sentinel meaning "no shortcut".
pub const COLLAPSE_KEY_OFF: &str = "off";

/// Inputs for one dialog frame.
pub struct DialogModel<'a> {
    /// Canonical state.
    pub state: &'a QuestionnaireState,
    /// All questions.
    pub questions: &'a [QuestionData],
    /// Per-tab row lists.
    pub items_by_tab: &'a [Vec<QuestionItem>],
    /// Locale table.
    pub i18n: &'a I18n,
    /// Theme palette.
    pub theme: &'a Theme,
    /// Live inline-input draft.
    pub input_text: &'a str,
    /// Cursor char offset inside `input_text`.
    pub input_cursor: Option<usize>,
    /// Resolved collapse key spec (`"ctrl+]"`, `"alt+o"`, or `"off"`).
    pub collapse_key: &'a str,
    /// Frame width in columns.
    pub width: usize,
}

/// `buildHintText` — the question-tab footer.
pub fn build_hint_text(
    question: Option<&QuestionData>,
    is_multi: bool,
    state: &QuestionnaireState,
    collapse_key: &str,
    i18n: &I18n,
) -> String {
    let mut parts = vec![
        i18n.t("hint.enter", HINT_PART_ENTER).to_owned(),
        i18n.t("hint.navigate", HINT_PART_NAV).to_owned(),
    ];
    if question.is_some_and(|question| question.multi_select == Some(true)) {
        parts.push(i18n.t("hint.toggle", HINT_PART_TOGGLE).to_owned());
    }
    if question.is_some() && !state.notes_visible && !state.input_mode {
        parts.push(i18n.t("hint.notes", HINT_PART_NOTES).to_owned());
    }
    if is_multi {
        parts.push(i18n.t("hint.tab", HINT_PART_TAB).to_owned());
    }
    parts.push(i18n.t("hint.cancel", HINT_PART_CANCEL).to_owned());
    if collapse_key != COLLAPSE_KEY_OFF && !collapse_key.is_empty() {
        parts.push(
            i18n.t("hint.collapse", HINT_PART_COLLAPSE_TEMPLATE)
                .replace("{key}", &format_key_spec_for_display(collapse_key)),
        );
    }
    if state.notes_visible || state.input_mode {
        parts.push(i18n.t("hint.newline", HINT_PART_NEW_LINE).to_owned());
    }
    if state.input_mode {
        parts.push(i18n.t("hint.clear", HINT_PART_CLEAR).to_owned());
    }
    parts.join(" · ")
}

/// `buildSubmitHintText` — the Submit-tab footer.
pub fn build_submit_hint_text(state: &QuestionnaireState, i18n: &I18n) -> String {
    let mut parts = vec![
        i18n.t("hint.enter", HINT_PART_ENTER).to_owned(),
        i18n.t("hint.navigate", HINT_PART_NAV).to_owned(),
    ];
    if !state.notes_visible {
        parts.push(i18n.t("review.global_hint", REVIEW_GLOBAL_HINT).to_owned());
    }
    parts.push(i18n.t("hint.cancel", HINT_PART_CANCEL).to_owned());
    if state.notes_visible {
        parts.push(i18n.t("hint.newline", HINT_PART_NEW_LINE).to_owned());
    }
    parts.join(" · ")
}

/// Render one dialog frame at `width`.
pub fn render(model: &DialogModel<'_>) -> RenderedFrame {
    let is_multi = model.questions.len() > 1;
    let active_question = model.state.current_tab;
    let mut lines: Vec<String> = Vec::new();
    let mut cursor = None;

    if is_multi {
        lines.extend(tab_bar::render(
            model.state,
            model.questions,
            model.i18n,
            model.theme,
            model.width,
        ));
        lines.push(String::new());
    }

    if is_multi && active_question == model.questions.len() {
        // Submit tab.
        lines.push(truncate_line(
            &model
                .theme
                .accent_bold(model.i18n.t("review.heading", submit::REVIEW_HEADING)),
            model.width,
        ));
        lines.push(String::new());
        lines.extend(submit::render_answers(
            model.state,
            model.questions,
            model.theme,
            model.width,
        ));
        lines.push(String::new());
        lines.push(submit::render_prompt(
            model.state,
            model.questions,
            model.i18n,
            model.theme,
            model.width,
        ));
        lines.extend(submit::render_picker(
            model.state,
            model.i18n,
            model.theme,
            model.width,
        ));
        lines.push(truncate_line(
            &model
                .theme
                .dim(&build_submit_hint_text(model.state, model.i18n)),
            model.width,
        ));
        return RenderedFrame { lines, cursor };
    }

    let Some(question) = model.questions.get(active_question) else {
        return RenderedFrame { lines, cursor };
    };

    if !is_multi && !question.header.is_empty() {
        lines.push(truncate_line(
            &model.theme.selected(&format!(" {} ", question.header)),
            model.width,
        ));
        lines.push(String::new());
    }
    for segment in rpi_tui::utils::wrap_text_with_ansi(&question.question, model.width.max(1)) {
        lines.push(truncate_line(&model.theme.bold(&segment), model.width));
    }
    lines.push(String::new());

    let items = model
        .items_by_tab
        .get(active_question)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let body = if question.multi_select == Some(true) {
        multi_select::render(
            model.state,
            question,
            model.i18n,
            model.theme,
            model.input_text,
            model.input_cursor,
            model.width,
            active_question + 1 == model.questions.len(),
        )
    } else {
        option_list::render(
            model.state,
            question,
            items,
            model.i18n,
            model.theme,
            model.input_text,
            model.input_cursor,
            model.width,
        )
    };
    let body_start = lines.len();
    if let Some((row, column)) = body.cursor {
        cursor = Some((body_start + row, column));
    }
    lines.extend(body.lines);
    lines.push(String::new());
    lines.push(truncate_line(
        &model.theme.dim(&build_hint_text(
            Some(question),
            is_multi,
            model.state,
            model.collapse_key,
            model.i18n,
        )),
        model.width,
    ));

    RenderedFrame { lines, cursor }
}

/// Sentinel label lookup used by tests (dialog re-exports the table lookup).
pub fn sentinel_label(i18n: &I18n, kind: RowKind) -> String {
    i18n.display_label(kind)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::build::build_items_for_question;
    use crate::tool::types::OptionData;

    fn question(multi_select: bool) -> QuestionData {
        QuestionData {
            question: "Pick one?".to_owned(),
            header: "H".to_owned(),
            options: vec![
                OptionData {
                    label: "A".to_owned(),
                    description: "a".to_owned(),
                    preview: None,
                },
                OptionData {
                    label: "B".to_owned(),
                    description: "b".to_owned(),
                    preview: None,
                },
            ],
            multi_select: multi_select.then_some(true),
        }
    }

    #[allow(clippy::too_many_arguments)] // test helper mirrors the render context
    fn model<'a>(
        state: &'a QuestionnaireState,
        questions: &'a [QuestionData],
        items: &'a [Vec<QuestionItem>],
        i18n: &'a I18n,
        theme: &'a Theme,
        input_text: &'a str,
        collapse_key: &'a str,
        width: usize,
    ) -> DialogModel<'a> {
        DialogModel {
            state,
            questions,
            items_by_tab: items,
            i18n,
            theme,
            input_text,
            input_cursor: None,
            collapse_key,
            width,
        }
    }

    #[test]
    fn single_question_layout_has_header_heading_options_and_hint() {
        let i18n = I18n::for_locale("en");
        let theme = Theme::dark();
        let questions = vec![question(false)];
        let items = vec![build_items_for_question(&questions[0], &i18n)];
        let state = QuestionnaireState::initial();
        let frame = render(&model(
            &state, &questions, &items, &i18n, &theme, "", "ctrl+]", 100,
        ));
        let plain: Vec<String> = frame.lines.iter().map(|line| strip_ansi(line)).collect();
        assert_eq!(plain[0], " H ");
        assert_eq!(plain[1], "");
        assert_eq!(plain[2], "Pick one?");
        assert_eq!(plain[4], "→ 1. A");
        assert_eq!(plain[8], "  3. Type something.");
        assert!(plain[10].starts_with("Enter to select"), "{plain:?}");
        assert!(plain[10].contains("Ctrl+] to collapse"), "{plain:?}");
    }

    #[test]
    fn multi_question_layout_has_tab_bar_and_submit_tab() {
        let i18n = I18n::for_locale("en");
        let theme = Theme::dark();
        let questions = vec![question(false), question(false)];
        let items = vec![
            build_items_for_question(&questions[0], &i18n),
            build_items_for_question(&questions[1], &i18n),
        ];
        let mut state = QuestionnaireState::initial();
        state.current_tab = 2;
        let frame = render(&model(
            &state, &questions, &items, &i18n, &theme, "", "off", 60,
        ));
        let plain: Vec<String> = frame.lines.iter().map(|line| strip_ansi(line)).collect();
        assert!(plain[0].contains("□ H"), "{plain:?}");
        assert_eq!(plain[2], "Review your answers");
        assert_eq!(
            plain[5],
            "⚠ Answer remaining questions before submitting: H, H"
        );
        assert!(plain[6].starts_with("→ 1. Submit answers"), "{plain:?}");
        assert!(plain[7].starts_with("  2. Cancel"), "{plain:?}");
        assert!(plain[8].starts_with("Enter to select"), "{plain:?}");
        assert!(!plain[8].contains("collapse"), "off disables the hint");
    }

    #[test]
    fn hint_drops_notes_and_adds_newline_and_clear_in_input_mode() {
        let i18n = I18n::for_locale("en");
        let mut state = QuestionnaireState::initial();
        state.input_mode = true;
        let hint = build_hint_text(None, false, &state, "ctrl+]", &i18n);
        assert!(!hint.contains("n to add notes"), "{hint}");
        assert!(hint.contains("Shift+Enter for newline"), "{hint}");
        assert!(hint.contains("Ctrl+U to clear"), "{hint}");
    }

    #[test]
    fn submit_hint_swaps_the_note_part_when_the_editor_is_open() {
        let i18n = I18n::for_locale("en");
        let state = QuestionnaireState::initial();
        let resting = build_submit_hint_text(&state, &i18n);
        assert!(resting.contains("n to add a note"), "{resting}");
        assert!(!resting.contains("Shift+Enter"), "{resting}");
        let mut editing = QuestionnaireState::initial();
        editing.notes_visible = true;
        let editing = build_submit_hint_text(&editing, &i18n);
        assert!(!editing.contains("n to add a note"), "{editing}");
        assert!(editing.contains("Shift+Enter for newline"), "{editing}");
    }

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
}
