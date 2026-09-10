//! Tab bar (multi-question mode).
//!
//! Port of upstream `view/components/tab-bar.ts` @ `338b264c`: a `← … →` row
//! of header chips (`■` answered / `□` open) plus the Submit chip. Hidden in
//! single-question mode (the dialog skips it entirely).

use crate::i18n::I18n;
use crate::state::reducer::QuestionnaireState;
use crate::tool::types::QuestionData;
use crate::view::theme::Theme;
use crate::view::truncate_line;

/// Render the tab-bar row (empty when there is only one question).
pub fn render(
    state: &QuestionnaireState,
    questions: &[QuestionData],
    i18n: &I18n,
    theme: &Theme,
    width: usize,
) -> Vec<String> {
    let _ = i18n; // chips carry author headers / `Qn`; no locale keys today
    if questions.len() <= 1 {
        return Vec::new();
    }
    let mut line = String::from(" ← ");
    for (index, question) in questions.iter().enumerate() {
        let label = if question.header.is_empty() {
            format!("Q{}", index + 1)
        } else {
            question.header.clone()
        };
        let box_glyph = if state.answers.contains_key(&index) {
            "■"
        } else {
            "□"
        };
        let segment = format!(" {box_glyph} {label} ");
        if index == state.current_tab {
            line.push_str(&theme.selected(&segment));
        } else if state.answers.contains_key(&index) {
            line.push_str(&theme.success(&segment));
        } else {
            line.push_str(&theme.muted(&segment));
        }
        line.push(' ');
    }
    let submit_segment = " ✓ Submit ";
    if state.current_tab == questions.len() {
        line.push_str(&theme.selected(submit_segment));
    } else {
        let all_answered = !questions.is_empty()
            && questions
                .iter()
                .enumerate()
                .all(|(index, _)| state.answers.contains_key(&index));
        line.push_str(&theme.fg(
            if all_answered {
                theme.success
            } else {
                theme.dim
            },
            submit_segment,
        ));
    }
    line.push_str(" →");
    vec![truncate_line(&line, width)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::types::OptionData;

    fn questions(count: usize) -> Vec<QuestionData> {
        (0..count)
            .map(|index| QuestionData {
                question: "Pick one".to_owned(),
                header: format!("H{}", index + 1),
                options: vec![OptionData {
                    label: "A".to_owned(),
                    description: "a".to_owned(),
                    preview: None,
                }],
                multi_select: None,
            })
            .collect()
    }

    #[test]
    fn tab_bar_hidden_for_a_single_question() {
        let i18n = I18n::for_locale("en");
        let theme = Theme::dark();
        assert!(render(
            &QuestionnaireState::initial(),
            &questions(1),
            &i18n,
            &theme,
            80
        )
        .is_empty());
    }

    #[test]
    fn tab_bar_marks_answered_active_and_submit_chips() {
        let i18n = I18n::for_locale("en");
        let theme = Theme::dark();
        let questions = questions(3);
        let mut state = QuestionnaireState::initial();
        state.current_tab = 1;
        state.answers.insert(
            0,
            crate::tool::types::QuestionAnswer {
                question_index: 0,
                question: "Pick one".to_owned(),
                kind: crate::tool::types::AnswerKind::Option,
                answer: Some("A".to_owned()),
                selected: None,
                notes: None,
                preview: None,
            },
        );
        let line = render(&state, &questions, &i18n, &theme, 200).remove(0);
        assert!(line.contains("■ H1"), "{line}");
        assert!(line.contains("□ H2"), "{line}");
        assert!(line.contains("✓ Submit"), "{line}");
        assert!(line.contains('←') && line.contains('→'));
        // The answered chip (tab 0, inactive) uses success; the active chip
        // (tab 1) uses the selected background.
        assert!(line.contains("\u{1b}[38;5;108m ■ H1"), "{line}");
        assert!(line.contains("\u{1b}[38;5;252;48;5;237m □ H2"), "{line}");
    }

    #[test]
    fn tab_bar_truncates_to_width() {
        let i18n = I18n::for_locale("en");
        let theme = Theme::dark();
        let line = render(
            &QuestionnaireState::initial(),
            &questions(4),
            &i18n,
            &theme,
            16,
        )
        .remove(0);
        assert!(rpi_tui::utils::visible_width(&line) <= 16, "{line}");
    }
}
