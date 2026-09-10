//! Derived projections over [`QuestionnaireState`].
//!
//! Port of upstream `packages/rpiv-ask-user-question/state/selectors/` @
//! `338b264c`: `derivations.ts` (`selectConfirmedIndicator`,
//! `selectActivePreviewPaneIndex`) and `focus.ts` (`selectActiveView`). The
//! per-props selectors of `projections.ts` are folded into
//! [`crate::view::dialog`] because the rpi renderer computes rows directly
//! from state (no component-binding graph).

use crate::state::build::QuestionItem;
use crate::state::reducer::QuestionnaireState;
use crate::tool::types::{AnswerKind, QuestionAnswer, QuestionData};

/// Which view owns focus this tick (upstream `ActiveView`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActiveView {
    /// The notes editor is open.
    Notes,
    /// The Submit tab picker owns focus.
    Submit,
    /// The option list owns focus.
    Options,
}

/// `selectActiveView` — priority notes > submit > options.
pub fn select_active_view(state: &QuestionnaireState, total_questions: usize) -> ActiveView {
    if state.notes_visible {
        return ActiveView::Notes;
    }
    if state.current_tab == total_questions {
        return ActiveView::Submit;
    }
    ActiveView::Options
}

/// A previously-confirmed row marker (upstream `selectConfirmedIndicator`).
#[derive(Clone, Debug, PartialEq)]
pub struct ConfirmedIndicator {
    /// Row index carrying the `✔` mark.
    pub index: usize,
    /// Replacement label for the `Type something.` row (the custom answer).
    pub label_override: Option<String>,
}

/// `selectConfirmedIndicator` — which row should be marked as previously
/// confirmed. Multi-select draws its own `[x]` boxes, so it always returns
/// `None`; a missing/non-matching answer skips the marker.
pub fn select_confirmed_indicator(
    questions: &[QuestionData],
    current_tab: usize,
    answers: &std::collections::BTreeMap<usize, QuestionAnswer>,
    items: &[QuestionItem],
) -> Option<ConfirmedIndicator> {
    let question = questions.get(current_tab)?;
    if question.multi_select == Some(true) {
        return None;
    }
    let prior = answers.get(&current_tab)?;
    if prior.kind == AnswerKind::Custom {
        let index = items
            .iter()
            .position(|item| item.kind == crate::state::row_intent::RowKind::Other)?;
        return Some(ConfirmedIndicator {
            index,
            label_override: Some(prior.answer.clone().unwrap_or_default()),
        });
    }
    if prior.kind != AnswerKind::Option {
        return None;
    }
    let label = prior.answer.as_ref()?;
    let index = items.iter().position(|item| {
        item.kind == crate::state::row_intent::RowKind::Option && &item.label == label
    })?;
    Some(ConfirmedIndicator {
        index,
        label_override: None,
    })
}

/// `selectActivePreviewPaneIndex` — the Submit tab reuses the last question's
/// pane purely for layout.
pub fn select_active_preview_pane_index(current_tab: usize, total_questions: usize) -> usize {
    if total_questions == 0 {
        return 0;
    }
    current_tab.min(total_questions - 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::I18n;
    use crate::state::build::build_items_for_question;
    use crate::tool::types::OptionData;

    fn question(multi_select: bool) -> QuestionData {
        QuestionData {
            question: "Pick one".to_owned(),
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

    fn answer(kind: AnswerKind, value: Option<&str>) -> QuestionAnswer {
        QuestionAnswer {
            question_index: 0,
            question: "Pick one".to_owned(),
            kind,
            answer: value.map(str::to_owned),
            selected: None,
            notes: None,
            preview: None,
        }
    }

    #[test]
    fn active_view_priority_is_notes_then_submit_then_options() {
        let mut state = QuestionnaireState::initial();
        assert_eq!(select_active_view(&state, 2), ActiveView::Options);
        state.current_tab = 2;
        assert_eq!(select_active_view(&state, 2), ActiveView::Submit);
        state.notes_visible = true;
        assert_eq!(select_active_view(&state, 2), ActiveView::Notes);
    }

    #[test]
    fn confirmed_indicator_maps_option_and_custom_answers() {
        let i18n = I18n::for_locale("en");
        let questions = vec![question(false)];
        let items = build_items_for_question(&questions[0], &i18n);
        let mut answers = std::collections::BTreeMap::new();

        answers.insert(0, answer(AnswerKind::Option, Some("B")));
        assert_eq!(
            select_confirmed_indicator(&questions, 0, &answers, &items),
            Some(ConfirmedIndicator {
                index: 1,
                label_override: None
            })
        );

        answers.insert(0, answer(AnswerKind::Custom, Some("typed")));
        assert_eq!(
            select_confirmed_indicator(&questions, 0, &answers, &items),
            Some(ConfirmedIndicator {
                index: 2,
                label_override: Some("typed".to_owned())
            })
        );

        answers.insert(0, answer(AnswerKind::Option, Some("missing")));
        assert_eq!(
            select_confirmed_indicator(&questions, 0, &answers, &items),
            None
        );
    }

    #[test]
    fn confirmed_indicator_skips_multi_select() {
        let i18n = I18n::for_locale("en");
        let questions = vec![question(true)];
        let items = build_items_for_question(&questions[0], &i18n);
        let mut answers = std::collections::BTreeMap::new();
        answers.insert(0, answer(AnswerKind::Multi, None));
        assert_eq!(
            select_confirmed_indicator(&questions, 0, &answers, &items),
            None
        );
    }

    #[test]
    fn active_preview_pane_index_clamps_to_the_last_question() {
        assert_eq!(select_active_preview_pane_index(0, 0), 0);
        assert_eq!(select_active_preview_pane_index(1, 3), 1);
        assert_eq!(select_active_preview_pane_index(3, 3), 2);
    }
}
