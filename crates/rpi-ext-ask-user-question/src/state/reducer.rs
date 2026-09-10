//! Canonical questionnaire state machine (the single mutation entry point).
//!
//! Port of upstream `packages/rpiv-ask-user-question/state/state-reducer.ts`
//! @ `338b264c` (v2.9.0+) plus the `QuestionnaireState` shape of
//! `state/state.ts`. [`apply`] is the only way state changes; every side
//! effect is returned declaratively as [`Effect`] and executed by
//! [`crate::state::session`] (or the parity harness).
//!
//! rpi adaptations:
//! - `Map<number, T>` → `BTreeMap<usize, T>`, `Set<number>` → `BTreeSet<usize>`
//!   (iteration order is canonicalised in [`QuestionnaireState::snapshot`]).
//! - `answer: string | null` → `Option<String>`; absent note/preview keys keep
//!   the upstream conditional-spread shape (`skip_serializing_if`).

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::state::build::QuestionItem;
use crate::state::key_router::Action;
use crate::state::row_intent::meta;
use crate::tool::types::{AnswerKind, QuestionAnswer, QuestionData, QuestionnaireResult};

/// Canonical questionnaire state (upstream `QuestionnaireState`).
#[derive(Clone, Debug, PartialEq, Default)]
pub struct QuestionnaireState {
    /// Active tab (`questions.len()` = Submit tab in multi-question mode).
    pub current_tab: usize,
    /// Focused row of the active tab.
    pub option_index: usize,
    /// Inline-input row focused and capturing keystrokes.
    pub input_mode: bool,
    /// Notes editor open.
    pub notes_visible: bool,
    /// Committed answers keyed by question index.
    pub answers: BTreeMap<usize, QuestionAnswer>,
    /// Multi-select checkbox state for the active tab.
    pub multi_select_checked: BTreeSet<usize>,
    /// In-flight custom drafts keyed by tab (a present empty string overrides
    /// an older answer).
    pub custom_drafts_by_tab: BTreeMap<usize, String>,
    /// Pre-answer notes side-band keyed by tab; the Submit tab's global note
    /// lives at the `questions.len()` pseudo-index.
    pub notes_by_tab: BTreeMap<usize, String>,
    /// Focused row of the Submit picker (`0` = Submit, `1` = Cancel).
    pub submit_choice_index: usize,
    /// Canonical mirror of the in-flight notes editor.
    pub notes_draft: String,
    /// Collapsed mode (the overlay gets out of the way).
    pub collapsed: bool,
}

impl QuestionnaireState {
    /// `initialState()` — every field at its upstream default.
    pub fn initial() -> Self {
        Self::default()
    }

    /// Canonical snapshot for tests and the parity harness.
    ///
    /// Maps become `{ "<key>": value }` objects and sets become sorted arrays,
    /// so the JS and Rust legs produce the same JSON shape.
    pub fn snapshot(&self) -> Value {
        let answers: serde_json::Map<String, Value> = self
            .answers
            .iter()
            .map(|(key, answer)| {
                (
                    key.to_string(),
                    serde_json::to_value(answer).unwrap_or(Value::Null),
                )
            })
            .collect();
        let drafts: serde_json::Map<String, Value> = self
            .custom_drafts_by_tab
            .iter()
            .map(|(key, value)| (key.to_string(), Value::String(value.clone())))
            .collect();
        let notes: serde_json::Map<String, Value> = self
            .notes_by_tab
            .iter()
            .map(|(key, value)| (key.to_string(), Value::String(value.clone())))
            .collect();
        json!({
            "currentTab": self.current_tab,
            "optionIndex": self.option_index,
            "inputMode": self.input_mode,
            "notesVisible": self.notes_visible,
            "answers": Value::Object(answers),
            "multiSelectChecked": self.multi_select_checked.iter().collect::<Vec<_>>(),
            "customDraftsByTab": Value::Object(drafts),
            "notesByTab": Value::Object(notes),
            "submitChoiceIndex": self.submit_choice_index,
            "notesDraft": self.notes_draft,
            "collapsed": self.collapsed,
        })
    }
}

/// Build a state from a partial JSON snapshot (parity harness / tests).
///
/// Missing keys take the [`QuestionnaireState::initial`] defaults; maps are
/// read from JSON objects, sets from arrays. Malformed entries are skipped
/// (the harness fixtures are trusted, but this stays panic-free).
pub fn state_from_json(value: &Value) -> QuestionnaireState {
    let mut state = QuestionnaireState::initial();
    if let Some(current_tab) = value.get("currentTab").and_then(Value::as_u64) {
        state.current_tab = current_tab as usize;
    }
    if let Some(option_index) = value.get("optionIndex").and_then(Value::as_u64) {
        state.option_index = option_index as usize;
    }
    if let Some(input_mode) = value.get("inputMode").and_then(Value::as_bool) {
        state.input_mode = input_mode;
    }
    if let Some(notes_visible) = value.get("notesVisible").and_then(Value::as_bool) {
        state.notes_visible = notes_visible;
    }
    if let Some(submit_choice_index) = value.get("submitChoiceIndex").and_then(Value::as_u64) {
        state.submit_choice_index = submit_choice_index as usize;
    }
    if let Some(notes_draft) = value.get("notesDraft").and_then(Value::as_str) {
        state.notes_draft = notes_draft.to_owned();
    }
    if let Some(collapsed) = value.get("collapsed").and_then(Value::as_bool) {
        state.collapsed = collapsed;
    }
    if let Some(answers) = value.get("answers").and_then(Value::as_object) {
        for (key, answer) in answers {
            let (Ok(index), Ok(answer)) = (
                key.parse::<usize>(),
                serde_json::from_value::<QuestionAnswer>(answer.clone()),
            ) else {
                continue;
            };
            state.answers.insert(index, answer);
        }
    }
    if let Some(checked) = value.get("multiSelectChecked").and_then(Value::as_array) {
        state.multi_select_checked = checked
            .iter()
            .filter_map(Value::as_u64)
            .map(|index| index as usize)
            .collect();
    }
    if let Some(drafts) = value.get("customDraftsByTab").and_then(Value::as_object) {
        for (key, draft) in drafts {
            let (Ok(index), Some(draft)) = (key.parse::<usize>(), draft.as_str()) else {
                continue;
            };
            state.custom_drafts_by_tab.insert(index, draft.to_owned());
        }
    }
    if let Some(notes) = value.get("notesByTab").and_then(Value::as_object) {
        for (key, note) in notes {
            let (Ok(index), Some(note)) = (key.parse::<usize>(), note.as_str()) else {
                continue;
            };
            state.notes_by_tab.insert(index, note.to_owned());
        }
    }
    state
}

/// Session-lifetime context the reducer reads (upstream `ApplyContext`).
#[derive(Clone, Debug)]
pub struct ApplyContext<'a> {
    /// All questions.
    pub questions: &'a [QuestionData],
    /// Per-tab row lists (built once per session).
    pub items_by_tab: &'a [Vec<QuestionItem>],
}

/// Declarative side effects emitted by [`apply`]; the runtime executes them
/// after committing the new state.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum Effect {
    /// Replace the inline-input buffer.
    SetInputBuffer {
        /// New buffer contents.
        value: String,
    },
    /// Empty the inline-input buffer.
    ClearInputBuffer,
    /// Open the host external editor seeded with `value`.
    OpenInputEditor {
        /// Seed text.
        value: String,
    },
    /// Replace the notes editor buffer.
    SetNotesValue {
        /// New notes text.
        value: String,
    },
    /// Focus/unfocus the notes editor.
    SetNotesFocused {
        /// New focus flag.
        focused: bool,
    },
    /// Forward one raw keystroke to the notes editor.
    ForwardNotesKeystroke {
        /// Raw key data.
        data: String,
    },
    /// Hide/show the underlying overlay.
    SetOverlayHidden {
        /// `true` = hide.
        hidden: bool,
    },
    /// Finish the questionnaire with `result`.
    Done {
        /// Final result (verbatim).
        result: QuestionnaireResult,
    },
}

/// One reduction result (upstream `ApplyResult`).
#[derive(Clone, Debug, PartialEq)]
pub struct ApplyResult {
    /// New state.
    pub state: QuestionnaireState,
    /// Effects to execute, in order.
    pub effects: Vec<Effect>,
}

fn ordered_answers(state: &QuestionnaireState, questions: &[QuestionData]) -> Vec<QuestionAnswer> {
    let mut out = Vec::new();
    for index in 0..questions.len() {
        if let Some(answer) = state.answers.get(&index) {
            out.push(answer.clone());
        }
    }
    out
}

/// `syncMultiSelectFromAnswers` — rebuild the checkbox set for `tab` from the
/// saved multi answer (labels → indices).
fn sync_multi_select_from_answers(
    answers: &BTreeMap<usize, QuestionAnswer>,
    questions: &[QuestionData],
    tab: usize,
) -> BTreeSet<usize> {
    let Some(question) = questions.get(tab) else {
        return BTreeSet::new();
    };
    if question.multi_select != Some(true) {
        return BTreeSet::new();
    }
    let saved_labels = answers
        .get(&tab)
        .and_then(|answer| answer.selected.as_ref())
        .cloned()
        .unwrap_or_default();
    question
        .options
        .iter()
        .enumerate()
        .filter(|(_, option)| saved_labels.contains(&option.label))
        .map(|(index, _)| index)
        .collect()
}

/// `persistMultiSelectAnswer` — mirror the live checkbox set into `answers`
/// (an empty selection deletes the entry).
fn persist_multi_select_answer(
    state: &QuestionnaireState,
    ctx: &ApplyContext<'_>,
) -> BTreeMap<usize, QuestionAnswer> {
    let Some(question) = ctx.questions.get(state.current_tab) else {
        return state.answers.clone();
    };
    if question.multi_select != Some(true) {
        return state.answers.clone();
    }
    let selected: Vec<String> = question
        .options
        .iter()
        .enumerate()
        .filter(|(index, _)| state.multi_select_checked.contains(index))
        .map(|(_, option)| option.label.clone())
        .collect();
    let mut out = state.answers.clone();
    if selected.is_empty() {
        out.remove(&state.current_tab);
        return out;
    }
    let mut answer = QuestionAnswer {
        question_index: state.current_tab,
        question: question.question.clone(),
        kind: AnswerKind::Multi,
        answer: None,
        selected: Some(selected),
        notes: None,
        preview: None,
    };
    if let Some(notes) = state
        .notes_by_tab
        .get(&state.current_tab)
        .filter(|notes| !notes.is_empty())
    {
        answer.notes = Some(notes.clone());
    }
    out.insert(state.current_tab, answer);
    out
}

/// `notesValueFor` — in-flight side-band first, answer mirror second.
fn notes_value_for(state: &QuestionnaireState, tab: usize) -> String {
    state
        .notes_by_tab
        .get(&tab)
        .cloned()
        .or_else(|| {
            state
                .answers
                .get(&tab)
                .and_then(|answer| answer.notes.clone())
        })
        .unwrap_or_default()
}

/// `customDraftValueFor` — in-flight draft first, confirmed custom answer second.
fn custom_draft_value_for(state: &QuestionnaireState, tab: usize) -> String {
    if let Some(draft) = state.custom_drafts_by_tab.get(&tab) {
        return draft.clone();
    }
    state
        .answers
        .get(&tab)
        .filter(|answer| answer.kind == AnswerKind::Custom)
        .and_then(|answer| answer.answer.clone())
        .unwrap_or_default()
}

fn set_custom_draft(
    state: &QuestionnaireState,
    tab: usize,
    value: String,
) -> BTreeMap<usize, String> {
    let mut drafts = state.custom_drafts_by_tab.clone();
    drafts.insert(tab, value);
    drafts
}

fn without_custom_draft(state: &QuestionnaireState, tab: usize) -> BTreeMap<usize, String> {
    let mut drafts = state.custom_drafts_by_tab.clone();
    drafts.remove(&tab);
    drafts
}

/// `switchTabResult` — reset per-tab transients and rehydrate the target
/// question's draft/notes.
fn switch_tab_result(
    state: &QuestionnaireState,
    next_tab: usize,
    ctx: &ApplyContext<'_>,
) -> ApplyResult {
    let notes_value = notes_value_for(state, next_tab);
    let transitioned = QuestionnaireState {
        current_tab: next_tab,
        option_index: 0,
        input_mode: false,
        notes_visible: false,
        multi_select_checked: sync_multi_select_from_answers(
            &state.answers,
            ctx.questions,
            next_tab,
        ),
        notes_draft: notes_value.clone(),
        submit_choice_index: 0,
        ..state.clone()
    };
    ApplyResult {
        state: transitioned,
        effects: vec![
            Effect::SetNotesFocused { focused: false },
            Effect::SetNotesValue { value: notes_value },
            Effect::SetInputBuffer {
                value: custom_draft_value_for(state, next_tab),
            },
        ],
    }
}

/// `orderedAnswers` + global-note lift (`doneFor`'s result shape). Public so
/// the session can synthesize the dispose/cancel result without a reducer
/// round-trip.
pub fn result_for(
    state: &QuestionnaireState,
    questions: &[QuestionData],
    cancelled: bool,
) -> QuestionnaireResult {
    QuestionnaireResult {
        answers: ordered_answers(state, questions),
        cancelled,
        global_note: state
            .notes_by_tab
            .get(&questions.len())
            .filter(|note| !note.is_empty())
            .cloned(),
        error: None,
    }
}

/// `doneFor` — lift the Submit tab's global note and finish.
fn done_for(state: &QuestionnaireState, ctx: &ApplyContext<'_>, cancelled: bool) -> ApplyResult {
    let result = result_for(state, ctx.questions, cancelled);
    ApplyResult {
        state: state.clone(),
        effects: vec![Effect::Done { result }],
    }
}

fn nav_handler(state: &QuestionnaireState, action: &Action, ctx: &ApplyContext<'_>) -> ApplyResult {
    let Action::Nav {
        next_index,
        input_value,
    } = action
    else {
        return ApplyResult {
            state: state.clone(),
            effects: Vec::new(),
        };
    };
    let items = ctx.items_by_tab.get(state.current_tab);
    let input_mode = items
        .and_then(|items| items.get(*next_index))
        .is_some_and(|item| meta(item.kind).activates_input_mode);
    let custom_drafts_by_tab = if state.input_mode {
        set_custom_draft(state, state.current_tab, input_value.clone())
    } else {
        state.custom_drafts_by_tab.clone()
    };
    let next = QuestionnaireState {
        option_index: *next_index,
        input_mode,
        custom_drafts_by_tab,
        ..state.clone()
    };
    let effects = if input_mode {
        vec![Effect::SetInputBuffer {
            value: custom_draft_value_for(&next, state.current_tab),
        }]
    } else {
        Vec::new()
    };
    ApplyResult {
        state: next,
        effects,
    }
}

fn input_clear_handler(
    state: &QuestionnaireState,
    _action: &Action,
    _ctx: &ApplyContext<'_>,
) -> ApplyResult {
    ApplyResult {
        state: QuestionnaireState {
            custom_drafts_by_tab: set_custom_draft(state, state.current_tab, String::new()),
            ..state.clone()
        },
        effects: vec![Effect::ClearInputBuffer],
    }
}

fn input_edit_handler(
    state: &QuestionnaireState,
    action: &Action,
    _ctx: &ApplyContext<'_>,
) -> ApplyResult {
    let Action::InputEdit { value } = action else {
        return ApplyResult {
            state: state.clone(),
            effects: Vec::new(),
        };
    };
    ApplyResult {
        state: state.clone(),
        effects: vec![Effect::OpenInputEditor {
            value: value.clone(),
        }],
    }
}

fn input_replace_handler(
    state: &QuestionnaireState,
    action: &Action,
    _ctx: &ApplyContext<'_>,
) -> ApplyResult {
    let Action::InputReplace { value } = action else {
        return ApplyResult {
            state: state.clone(),
            effects: Vec::new(),
        };
    };
    ApplyResult {
        state: QuestionnaireState {
            custom_drafts_by_tab: set_custom_draft(state, state.current_tab, value.clone()),
            ..state.clone()
        },
        effects: vec![Effect::SetInputBuffer {
            value: value.clone(),
        }],
    }
}

fn tab_switch_handler(
    state: &QuestionnaireState,
    action: &Action,
    ctx: &ApplyContext<'_>,
) -> ApplyResult {
    let Action::TabSwitch { next_tab } = action else {
        return ApplyResult {
            state: state.clone(),
            effects: Vec::new(),
        };
    };
    switch_tab_result(state, *next_tab, ctx)
}

fn confirm_handler(
    state: &QuestionnaireState,
    action: &Action,
    ctx: &ApplyContext<'_>,
) -> ApplyResult {
    let Action::Confirm {
        answer,
        auto_advance_tab,
    } = action
    else {
        return ApplyResult {
            state: state.clone(),
            effects: Vec::new(),
        };
    };
    let mut answer = answer.clone();
    if answer.kind == AnswerKind::Option {
        if let Some(labels) = answer.answer.as_ref() {
            if let Some(question) = ctx.questions.get(answer.question_index) {
                if let Some(matched) = question
                    .options
                    .iter()
                    .find(|option| &option.label == labels)
                {
                    if let Some(preview) = matched.preview.as_ref().filter(|p| !p.is_empty()) {
                        answer.preview = Some(preview.clone());
                    }
                }
            }
        }
    }
    if let Some(pending) = state
        .notes_by_tab
        .get(&answer.question_index)
        .filter(|notes| !notes.is_empty())
    {
        answer.notes = Some(pending.clone());
    }
    let mut answers = state.answers.clone();
    answers.insert(answer.question_index, answer.clone());
    // Custom free-text on a multi-select tab is mutually exclusive with
    // checkbox selections: clear the checked set so the boxes vanish on Enter.
    let is_custom_multi = answer.kind == AnswerKind::Custom
        && ctx
            .questions
            .get(answer.question_index)
            .is_some_and(|question| question.multi_select == Some(true));
    let custom_drafts_by_tab = if answer.kind == AnswerKind::Custom {
        without_custom_draft(state, answer.question_index)
    } else {
        state.custom_drafts_by_tab.clone()
    };
    let next = QuestionnaireState {
        answers,
        custom_drafts_by_tab,
        multi_select_checked: if is_custom_multi {
            BTreeSet::new()
        } else {
            state.multi_select_checked.clone()
        },
        ..state.clone()
    };
    if let Some(auto_advance) = auto_advance_tab {
        return switch_tab_result(&next, *auto_advance, ctx);
    }
    done_for(&next, ctx, false)
}

fn toggle_handler(
    state: &QuestionnaireState,
    action: &Action,
    ctx: &ApplyContext<'_>,
) -> ApplyResult {
    let Action::Toggle { index } = action else {
        return ApplyResult {
            state: state.clone(),
            effects: Vec::new(),
        };
    };
    let mut checked = state.multi_select_checked.clone();
    if !checked.remove(index) {
        checked.insert(*index);
    }
    let intermediate = QuestionnaireState {
        multi_select_checked: checked,
        ..state.clone()
    };
    let answers = persist_multi_select_answer(&intermediate, ctx);
    ApplyResult {
        state: QuestionnaireState {
            answers,
            ..intermediate
        },
        effects: Vec::new(),
    }
}

fn multi_confirm_handler(
    state: &QuestionnaireState,
    action: &Action,
    ctx: &ApplyContext<'_>,
) -> ApplyResult {
    let Action::MultiConfirm {
        selected,
        auto_advance_tab,
    } = action
    else {
        return ApplyResult {
            state: state.clone(),
            effects: Vec::new(),
        };
    };
    let Some(question) = ctx.questions.get(state.current_tab) else {
        return ApplyResult {
            state: state.clone(),
            effects: Vec::new(),
        };
    };
    let mut answer = QuestionAnswer {
        question_index: state.current_tab,
        question: question.question.clone(),
        kind: AnswerKind::Multi,
        answer: None,
        selected: Some(selected.clone()),
        notes: None,
        preview: None,
    };
    if let Some(pending) = state
        .notes_by_tab
        .get(&state.current_tab)
        .filter(|notes| !notes.is_empty())
    {
        answer.notes = Some(pending.clone());
    }
    let mut answers = state.answers.clone();
    answers.insert(state.current_tab, answer);
    let synced = QuestionnaireState {
        answers: answers.clone(),
        multi_select_checked: sync_multi_select_from_answers(
            &answers,
            ctx.questions,
            state.current_tab,
        ),
        ..state.clone()
    };
    if let Some(auto_advance) = auto_advance_tab {
        return switch_tab_result(&synced, *auto_advance, ctx);
    }
    done_for(&synced, ctx, false)
}

fn notes_enter_handler(
    state: &QuestionnaireState,
    _action: &Action,
    _ctx: &ApplyContext<'_>,
) -> ApplyResult {
    let value = notes_value_for(state, state.current_tab);
    ApplyResult {
        state: QuestionnaireState {
            notes_visible: true,
            notes_draft: value.clone(),
            ..state.clone()
        },
        effects: vec![
            Effect::SetNotesValue { value },
            Effect::SetNotesFocused { focused: true },
        ],
    }
}

fn notes_exit_handler(
    state: &QuestionnaireState,
    _action: &Action,
    _ctx: &ApplyContext<'_>,
) -> ApplyResult {
    let trimmed = state.notes_draft.trim().to_owned();
    let mut notes = state.notes_by_tab.clone();
    let mut answers = state.answers.clone();
    if trimmed.is_empty() {
        notes.remove(&state.current_tab);
        if let Some(previous) = answers.get(&state.current_tab).cloned() {
            if previous.notes.is_some() {
                let stripped = QuestionAnswer {
                    notes: None,
                    ..previous
                };
                answers.insert(state.current_tab, stripped);
            }
        }
    } else {
        notes.insert(state.current_tab, trimmed.clone());
        if let Some(previous) = answers.get(&state.current_tab).cloned() {
            answers.insert(
                state.current_tab,
                QuestionAnswer {
                    notes: Some(trimmed),
                    ..previous
                },
            );
        }
    }
    ApplyResult {
        state: QuestionnaireState {
            notes_by_tab: notes,
            answers,
            notes_visible: false,
            ..state.clone()
        },
        effects: vec![Effect::SetNotesFocused { focused: false }],
    }
}

fn submit_nav_handler(
    state: &QuestionnaireState,
    action: &Action,
    _ctx: &ApplyContext<'_>,
) -> ApplyResult {
    let Action::SubmitNav { next_index } = action else {
        return ApplyResult {
            state: state.clone(),
            effects: Vec::new(),
        };
    };
    ApplyResult {
        state: QuestionnaireState {
            submit_choice_index: *next_index,
            ..state.clone()
        },
        effects: Vec::new(),
    }
}

fn notes_forward_handler(
    state: &QuestionnaireState,
    action: &Action,
    _ctx: &ApplyContext<'_>,
) -> ApplyResult {
    let Action::NotesForward { data } = action else {
        return ApplyResult {
            state: state.clone(),
            effects: Vec::new(),
        };
    };
    ApplyResult {
        state: state.clone(),
        effects: vec![Effect::ForwardNotesKeystroke { data: data.clone() }],
    }
}

fn toggle_collapsed_handler(
    state: &QuestionnaireState,
    _action: &Action,
    _ctx: &ApplyContext<'_>,
) -> ApplyResult {
    let collapsed = !state.collapsed;
    ApplyResult {
        state: QuestionnaireState {
            collapsed,
            ..state.clone()
        },
        effects: vec![Effect::SetOverlayHidden { hidden: collapsed }],
    }
}

/// Reduce one action. Unhandled variants are no-ops (the enum is exhaustive
/// at the router; `apply` keeps an explicit dispatch so every handler is
/// individually testable).
pub fn apply(state: &QuestionnaireState, action: &Action, ctx: &ApplyContext<'_>) -> ApplyResult {
    match action {
        Action::Nav { .. } => nav_handler(state, action, ctx),
        Action::InputClear => input_clear_handler(state, action, ctx),
        Action::InputEdit { .. } => input_edit_handler(state, action, ctx),
        Action::InputReplace { .. } => input_replace_handler(state, action, ctx),
        Action::TabSwitch { .. } => tab_switch_handler(state, action, ctx),
        Action::Confirm { .. } => confirm_handler(state, action, ctx),
        Action::Toggle { .. } => toggle_handler(state, action, ctx),
        Action::MultiConfirm { .. } => multi_confirm_handler(state, action, ctx),
        Action::Cancel => done_for(state, ctx, true),
        Action::NotesEnter => notes_enter_handler(state, action, ctx),
        Action::NotesExit => notes_exit_handler(state, action, ctx),
        Action::Submit => done_for(state, ctx, false),
        Action::SubmitNav { .. } => submit_nav_handler(state, action, ctx),
        Action::NotesForward { .. } => notes_forward_handler(state, action, ctx),
        Action::ToggleCollapsed => toggle_collapsed_handler(state, action, ctx),
        Action::Ignore => ApplyResult {
            state: state.clone(),
            effects: Vec::new(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::I18n;
    use crate::state::build::build_items_for_question;
    use crate::state::key_router::{route_key, Keybindings, QuestionnaireRuntime};
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

    fn context<'a>(
        questions: &'a [QuestionData],
        items: &'a [Vec<QuestionItem>],
    ) -> ApplyContext<'a> {
        ApplyContext {
            questions,
            items_by_tab: items,
        }
    }

    fn fixture(multi_select: bool, count: usize) -> (Vec<QuestionData>, Vec<Vec<QuestionItem>>) {
        let i18n = I18n::for_locale("en");
        let questions: Vec<QuestionData> = (0..count).map(|_| question(multi_select)).collect();
        let items = questions
            .iter()
            .map(|question| build_items_for_question(question, &i18n))
            .collect();
        (questions, items)
    }

    #[test]
    fn nav_regular_keeps_the_active_draft_buffer_intact() {
        let (questions, items) = fixture(false, 1);
        let state = QuestionnaireState::initial();
        let result = apply(
            &state,
            &Action::Nav {
                next_index: 1,
                input_value: String::new(),
            },
            &context(&questions, &items),
        );
        assert_eq!(result.state.option_index, 1);
        assert!(!result.state.input_mode);
        assert!(result.effects.is_empty());
    }

    #[test]
    fn nav_onto_other_row_restores_the_in_flight_draft_ahead_of_a_confirmed_answer() {
        let (questions, items) = fixture(false, 1);
        let mut state = QuestionnaireState::initial();
        state.answers.insert(
            0,
            QuestionAnswer {
                question_index: 0,
                question: "Pick one".to_owned(),
                kind: AnswerKind::Custom,
                answer: Some("confirmed".to_owned()),
                selected: None,
                notes: None,
                preview: None,
            },
        );
        state.custom_drafts_by_tab.insert(0, "draft".to_owned());
        let result = apply(
            &state,
            &Action::Nav {
                next_index: 2,
                input_value: String::new(),
            },
            &context(&questions, &items),
        );
        assert!(result.state.input_mode);
        assert_eq!(
            result.effects,
            vec![Effect::SetInputBuffer {
                value: "draft".to_owned()
            }]
        );
    }

    #[test]
    fn nav_onto_other_row_with_prior_custom_answer_restores_the_buffer() {
        let (questions, items) = fixture(false, 1);
        let mut state = QuestionnaireState::initial();
        state.answers.insert(
            0,
            QuestionAnswer {
                question_index: 0,
                question: "Pick one".to_owned(),
                kind: AnswerKind::Custom,
                answer: Some("Hello".to_owned()),
                selected: None,
                notes: None,
                preview: None,
            },
        );
        let result = apply(
            &state,
            &Action::Nav {
                next_index: 2,
                input_value: String::new(),
            },
            &context(&questions, &items),
        );
        assert_eq!(
            result.effects,
            vec![Effect::SetInputBuffer {
                value: "Hello".to_owned()
            }]
        );
    }

    #[test]
    fn tab_switch_emits_notes_and_buffer_effects_and_resets_transients() {
        let (questions, items) = fixture(false, 2);
        let mut state = QuestionnaireState::initial();
        state.notes_visible = true;
        state.submit_choice_index = 1;
        state.custom_drafts_by_tab.insert(1, "second".to_owned());
        let result = apply(
            &state,
            &Action::TabSwitch { next_tab: 1 },
            &context(&questions, &items),
        );
        assert_eq!(result.state.current_tab, 1);
        assert_eq!(result.state.option_index, 0);
        assert!(!result.state.notes_visible);
        assert_eq!(result.state.submit_choice_index, 0);
        assert_eq!(
            result.effects,
            vec![
                Effect::SetNotesFocused { focused: false },
                Effect::SetNotesValue {
                    value: String::new()
                },
                Effect::SetInputBuffer {
                    value: "second".to_owned()
                },
            ]
        );
    }

    #[test]
    fn confirm_regular_option_emits_done_with_the_answer() {
        let (questions, items) = fixture(false, 1);
        let state = QuestionnaireState::initial();
        let answer = QuestionAnswer {
            question_index: 0,
            question: "Pick one".to_owned(),
            kind: AnswerKind::Option,
            answer: Some("A".to_owned()),
            selected: None,
            notes: None,
            preview: None,
        };
        let result = apply(
            &state,
            &Action::Confirm {
                answer: answer.clone(),
                auto_advance_tab: None,
            },
            &context(&questions, &items),
        );
        assert_eq!(result.state.answers.get(&0), Some(&answer));
        assert_eq!(
            result.effects,
            vec![Effect::Done {
                result: QuestionnaireResult {
                    answers: vec![answer],
                    cancelled: false,
                    global_note: None,
                    error: None,
                }
            }]
        );
    }

    #[test]
    fn confirm_merges_pending_notes_and_preview() {
        let i18n = I18n::for_locale("en");
        let mut question = question(false);
        question.options[0].preview = Some("code".to_owned());
        let items = vec![build_items_for_question(&question, &i18n)];
        let questions = vec![question];
        let mut state = QuestionnaireState::initial();
        state.notes_by_tab.insert(0, "note".to_owned());
        let result = apply(
            &state,
            &Action::Confirm {
                answer: QuestionAnswer {
                    question_index: 0,
                    question: "Pick one".to_owned(),
                    kind: AnswerKind::Option,
                    answer: Some("A".to_owned()),
                    selected: None,
                    notes: None,
                    preview: None,
                },
                auto_advance_tab: None,
            },
            &context(&questions, &items),
        );
        let answer = result.state.answers.get(&0).expect("answer");
        assert_eq!(answer.preview.as_deref(), Some("code"));
        assert_eq!(answer.notes.as_deref(), Some("note"));
    }

    #[test]
    fn toggle_persists_and_clears_multi_answers() {
        let (questions, items) = fixture(true, 1);
        let state = QuestionnaireState::initial();
        let toggled = apply(
            &state,
            &Action::Toggle { index: 0 },
            &context(&questions, &items),
        );
        assert_eq!(
            toggled
                .state
                .multi_select_checked
                .iter()
                .collect::<Vec<_>>(),
            vec![&0]
        );
        let answer = toggled.state.answers.get(&0).expect("multi answer");
        assert_eq!(answer.kind, AnswerKind::Multi);
        assert_eq!(answer.selected.as_deref(), Some(&["A".to_owned()][..]));
        let untoggled = apply(
            &toggled.state,
            &Action::Toggle { index: 0 },
            &context(&questions, &items),
        );
        assert!(untoggled.state.multi_select_checked.is_empty());
        assert!(
            untoggled.state.answers.is_empty(),
            "empty selection deletes"
        );
    }

    #[test]
    fn confirm_custom_on_multi_clears_checkboxes_and_draft() {
        let (questions, items) = fixture(true, 1);
        let mut state = QuestionnaireState::initial();
        state.multi_select_checked.insert(0);
        state.custom_drafts_by_tab.insert(0, "typed".to_owned());
        let result = apply(
            &state,
            &Action::Confirm {
                answer: QuestionAnswer {
                    question_index: 0,
                    question: "Pick one".to_owned(),
                    kind: AnswerKind::Custom,
                    answer: Some("typed".to_owned()),
                    selected: None,
                    notes: None,
                    preview: None,
                },
                auto_advance_tab: None,
            },
            &context(&questions, &items),
        );
        assert!(result.state.multi_select_checked.is_empty());
        assert!(!result.state.custom_drafts_by_tab.contains_key(&0));
    }

    #[test]
    fn multi_confirm_commits_an_empty_selection() {
        let (questions, items) = fixture(true, 1);
        let state = QuestionnaireState::initial();
        let result = apply(
            &state,
            &Action::MultiConfirm {
                selected: Vec::new(),
                auto_advance_tab: None,
            },
            &context(&questions, &items),
        );
        let answer = result.state.answers.get(&0).expect("answer");
        assert_eq!(answer.kind, AnswerKind::Multi);
        assert_eq!(answer.selected.as_deref(), Some(&[][..]));
    }

    #[test]
    fn notes_enter_exit_keeps_notes_out_of_answers_until_confirm() {
        let (questions, items) = fixture(false, 1);
        let mut state = QuestionnaireState::initial();
        let entered = apply(&state, &Action::NotesEnter, &context(&questions, &items));
        assert!(entered.state.notes_visible);
        assert_eq!(
            entered.effects,
            vec![
                Effect::SetNotesValue {
                    value: String::new()
                },
                Effect::SetNotesFocused { focused: true }
            ]
        );
        state = entered.state;
        state.notes_draft = "  hello  ".to_owned();
        let exited = apply(&state, &Action::NotesExit, &context(&questions, &items));
        assert!(!exited.state.notes_visible);
        assert_eq!(
            exited.state.notes_by_tab.get(&0).map(String::as_str),
            Some("hello")
        );
        assert!(
            !exited.state.answers.contains_key(&0),
            "notes never mark a question answered"
        );
    }

    #[test]
    fn done_lifts_the_submit_tab_global_note() {
        let (questions, items) = fixture(false, 2);
        let mut state = QuestionnaireState::initial();
        state.current_tab = 2;
        state.notes_by_tab.insert(2, "global".to_owned());
        let result = apply(&state, &Action::Submit, &context(&questions, &items));
        let Effect::Done { result } = &result.effects[0] else {
            panic!("expected done");
        };
        assert_eq!(result.global_note.as_deref(), Some("global"));
        assert!(!result.cancelled);
    }

    #[test]
    fn cancel_reports_cancelled_with_partial_answers() {
        let (questions, items) = fixture(false, 2);
        let mut state = QuestionnaireState::initial();
        state.answers.insert(
            0,
            QuestionAnswer {
                question_index: 0,
                question: "Pick one".to_owned(),
                kind: AnswerKind::Option,
                answer: Some("A".to_owned()),
                selected: None,
                notes: None,
                preview: None,
            },
        );
        let result = apply(&state, &Action::Cancel, &context(&questions, &items));
        let Effect::Done { result } = &result.effects[0] else {
            panic!("expected done");
        };
        assert!(result.cancelled);
        assert_eq!(result.answers.len(), 1, "collected answers survive cancel");
    }

    #[test]
    fn toggle_collapsed_emits_overlay_hidden_effect() {
        let (questions, items) = fixture(false, 1);
        let state = QuestionnaireState::initial();
        let result = apply(
            &state,
            &Action::ToggleCollapsed,
            &context(&questions, &items),
        );
        assert!(result.state.collapsed);
        assert_eq!(
            result.effects,
            vec![Effect::SetOverlayHidden { hidden: true }]
        );
        let back = apply(
            &result.state,
            &Action::ToggleCollapsed,
            &context(&questions, &items),
        );
        assert!(!back.state.collapsed);
        assert_eq!(
            back.effects,
            vec![Effect::SetOverlayHidden { hidden: false }]
        );
    }

    /// The full router → reducer loop drives one answered question from the
    /// canonical initial state to `done`.
    #[test]
    fn router_reducer_loop_answers_a_single_question() {
        let (questions, items) = fixture(false, 1);
        let keybindings = Keybindings::pi_defaults();
        let ctx = context(&questions, &items);
        let mut state = QuestionnaireState::initial();
        let runtime = QuestionnaireRuntime {
            keybindings: &keybindings,
            input_buffer: String::new(),
            can_move_input_up: false,
            can_move_input_down: false,
            questions: &questions,
            is_multi: false,
            current_item: items[0].first(),
            items: &items[0],
            collapse_key: "ctrl+]".to_owned(),
        };
        let action = route_key("\r", &state, &runtime);
        let result = apply(&state, &action, &ctx);
        state = result.state;
        let Effect::Done { result } = &result.effects[0] else {
            panic!("expected done");
        };
        assert_eq!(result.answers.len(), 1);
        assert!(!result.cancelled);
        assert_eq!(
            state
                .answers
                .get(&0)
                .and_then(|a| a.answer.clone())
                .as_deref(),
            Some("A")
        );
    }
}
