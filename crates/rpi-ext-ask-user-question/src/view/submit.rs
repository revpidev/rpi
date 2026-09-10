//! Submit-tab rendering (answer review + Submit/Cancel picker).
//!
//! Port of upstream `view/components/submit-picker.ts` +
//! `SubmitTabStrategy` (`view/tab-content-strategy.ts`) @ `338b264c`: the
//! picker is a fixed two-row `Submit answers` / `Cancel` list; the body is
//! the answered-question summary (`● header` + `→ answer`), and the footer
//! names the unanswered questions or declares readiness.
//!
//! Q2 scope note: the global-note review entry and the notes editor mount of
//! the upstream strategy belong to Q3 (FR-Q3-D); this module renders answers
//! only.

use std::collections::BTreeMap;

use crate::i18n::I18n;
use crate::state::reducer::QuestionnaireState;
use crate::tool::envelope::{format_answer_scalar, FormatAnswerVariant};
use crate::tool::types::{QuestionAnswer, QuestionData};
use crate::view::theme::Theme;
use crate::view::{truncate_line, visible_columns};

/// `SUBMIT_LABEL` (canonical-English fallback of `submit.label`).
pub const SUBMIT_LABEL: &str = "Submit answers";
/// `CANCEL_LABEL` (canonical-English fallback of `submit.cancel`).
pub const CANCEL_LABEL: &str = "Cancel";
/// `REVIEW_HEADING`.
pub const REVIEW_HEADING: &str = "Review your answers";
/// `READY_PROMPT`.
pub const READY_PROMPT: &str = "Ready to submit your answers?";
/// `INCOMPLETE_WARNING_PREFIX`.
pub const INCOMPLETE_WARNING_PREFIX: &str = "⚠ Answer remaining questions before submitting:";

/// Display label for one question (`header` or `Qn`).
fn question_label(question: &QuestionData, index: usize) -> String {
    if question.header.is_empty() {
        format!("Q{}", index + 1)
    } else {
        question.header.clone()
    }
}

/// Render the answer summary body (upstream `SubmitTabStrategy.bodyComponent`).
pub fn render_answers(
    state: &QuestionnaireState,
    questions: &[QuestionData],
    theme: &Theme,
    width: usize,
) -> Vec<String> {
    let _ = state;
    render_answer_rows(&state.answers, questions, theme, width)
}

fn render_answer_rows(
    answers: &BTreeMap<usize, QuestionAnswer>,
    questions: &[QuestionData],
    theme: &Theme,
    width: usize,
) -> Vec<String> {
    let mut lines = Vec::new();
    for (index, question) in questions.iter().enumerate() {
        let Some(answer) = answers.get(&index) else {
            continue;
        };
        lines.push(truncate_line(
            &format!(" ● {}", theme.muted(&question_label(question, index))),
            width,
        ));
        let answer_text = format_answer_scalar(answer, FormatAnswerVariant::Summary);
        lines.push(truncate_line(
            &format!("   → {}", theme.fg(theme.text, &answer_text)),
            width,
        ));
        if let Some(notes) = answer.notes.as_deref().filter(|notes| !notes.is_empty()) {
            lines.push(truncate_line(&format!("     notes: {notes}"), width));
        }
    }
    lines
}

/// The ready/incomplete prompt line.
pub fn render_prompt(
    state: &QuestionnaireState,
    questions: &[QuestionData],
    i18n: &I18n,
    theme: &Theme,
    width: usize,
) -> String {
    let missing: Vec<String> = questions
        .iter()
        .enumerate()
        .filter(|(index, _)| !state.answers.contains_key(index))
        .map(|(index, question)| question_label(question, index))
        .collect();
    let text = if missing.is_empty() {
        theme.muted(i18n.t("review.ready", READY_PROMPT))
    } else {
        theme.warning(&format!(
            "{} {}",
            i18n.t("review.incomplete", INCOMPLETE_WARNING_PREFIX),
            missing.join(", ")
        ))
    };
    truncate_line(&text, width)
}

/// Render the two-row picker (row 0 = Submit, row 1 = Cancel).
pub fn render_picker(
    state: &QuestionnaireState,
    i18n: &I18n,
    theme: &Theme,
    width: usize,
) -> Vec<String> {
    let labels = [
        i18n.t("submit.label", SUBMIT_LABEL),
        i18n.t("submit.cancel", CANCEL_LABEL),
    ];
    let mut lines = Vec::new();
    for (index, label) in labels.iter().enumerate() {
        let active = state.submit_choice_index == index;
        let pointer = if active { "→ " } else { "  " };
        let pointer_width = visible_columns(pointer);
        let text = if active {
            theme.accent_bold(label)
        } else {
            (*label).to_owned()
        };
        lines.push(truncate_line(
            &format!("{pointer}{}. {text}", index + 1),
            width,
        ));
        let _ = pointer_width;
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::types::{AnswerKind, OptionData};

    fn questions() -> Vec<QuestionData> {
        vec![
            QuestionData {
                question: "Pick one".to_owned(),
                header: "H1".to_owned(),
                options: vec![OptionData {
                    label: "A".to_owned(),
                    description: "a".to_owned(),
                    preview: None,
                }],
                multi_select: None,
            },
            QuestionData {
                question: "Pick two".to_owned(),
                header: String::new(),
                options: vec![OptionData {
                    label: "B".to_owned(),
                    description: "b".to_owned(),
                    preview: None,
                }],
                multi_select: Some(true),
            },
        ]
    }

    #[test]
    fn answers_render_label_scalar_and_notes() {
        let theme = Theme::dark();
        let questions = questions();
        let mut state = QuestionnaireState::initial();
        state.answers.insert(
            0,
            QuestionAnswer {
                question_index: 0,
                question: "Pick one".to_owned(),
                kind: AnswerKind::Option,
                answer: Some("A".to_owned()),
                selected: None,
                notes: Some("my note".to_owned()),
                preview: None,
            },
        );
        state.answers.insert(
            1,
            QuestionAnswer {
                question_index: 1,
                question: "Pick two".to_owned(),
                kind: AnswerKind::Multi,
                answer: None,
                selected: Some(vec!["B".to_owned()]),
                notes: None,
                preview: None,
            },
        );
        let plain: Vec<String> = render_answers(&state, &questions, &theme, 80)
            .iter()
            .map(|line| strip_ansi(line))
            .collect();
        assert_eq!(plain[0], " ● H1");
        assert_eq!(plain[1], "   → A");
        assert_eq!(plain[2], "     notes: my note");
        assert_eq!(plain[3], " ● Q2");
        assert_eq!(plain[4], "   → B");
    }

    #[test]
    fn prompt_names_missing_questions_and_greens_when_complete() {
        let i18n = I18n::for_locale("en");
        let theme = Theme::dark();
        let questions = questions();
        let state = QuestionnaireState::initial();
        let incomplete = strip_ansi(&render_prompt(&state, &questions, &i18n, &theme, 80));
        assert!(incomplete.contains("H1, Q2"), "{incomplete}");

        let mut complete = QuestionnaireState::initial();
        for index in 0..questions.len() {
            complete.answers.insert(
                index,
                QuestionAnswer {
                    question_index: index,
                    question: "q".to_owned(),
                    kind: AnswerKind::Option,
                    answer: Some("A".to_owned()),
                    selected: None,
                    notes: None,
                    preview: None,
                },
            );
        }
        let ready = strip_ansi(&render_prompt(&complete, &questions, &i18n, &theme, 80));
        assert_eq!(ready, READY_PROMPT);
    }

    #[test]
    fn picker_marks_the_active_row() {
        let i18n = I18n::for_locale("en");
        let theme = Theme::dark();
        let state = QuestionnaireState::initial();
        let plain: Vec<String> = render_picker(&state, &i18n, &theme, 80)
            .iter()
            .map(|line| strip_ansi(line))
            .collect();
        assert_eq!(plain, vec!["→ 1. Submit answers", "  2. Cancel"]);
        let mut cancel = QuestionnaireState::initial();
        cancel.submit_choice_index = 1;
        let plain: Vec<String> = render_picker(&cancel, &i18n, &theme, 80)
            .iter()
            .map(|line| strip_ansi(line))
            .collect();
        assert_eq!(plain, vec!["  1. Submit answers", "→ 2. Cancel"]);
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
