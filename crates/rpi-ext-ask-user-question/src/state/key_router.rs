//! Raw key bytes → questionnaire action.
//!
//! Port of upstream `packages/rpiv-ask-user-question/state/key-router.ts` @
//! `338b264c` (v2.9.0+). The router is a pure function over `(data, state,
//! runtime)`: it never mutates state — every transition is expressed as an
//! [`Action`] consumed by [`crate::state::reducer::apply`].
//!
//! rpi adaptations (no behavior divergence on the ported surface):
//! - `matchesKey(data, name)` is `rpi_tui::keys::matches_key` (the rpi port of
//!   the same `packages/tui/src/keys.ts` source the upstream package uses).
//! - Upstream resolves `tui.*` keybinding names through the host keybinding
//!   manager (which honors user overrides). The interactive-UI ABI exposes no
//!   keybinding channel, so [`Keybindings::pi_defaults`] pins the upstream
//!   default key ids; see the TE30 landing note in
//!   `rpi-docs/extensions/rpiv-ask-user-question/02-design.md` §4.
//! - `QuestionnaireRuntime.currentItem`/`items` borrow the per-tab item list
//!   (`crate::state::build::QuestionItem`) instead of the TS structural union.

use std::collections::BTreeMap;

use rpi_tui::keys::matches_key;
use serde::{Deserialize, Serialize};

use crate::state::build::QuestionItem;
use crate::state::reducer::QuestionnaireState;
use crate::state::row_intent::{meta, RowKind};
use crate::tool::types::{AnswerKind, QuestionAnswer, QuestionData};

/// `KEYBIND_UP` (`tui.select.up`).
pub const KEYBIND_UP: &str = "tui.select.up";
/// `KEYBIND_DOWN` (`tui.select.down`).
pub const KEYBIND_DOWN: &str = "tui.select.down";
/// `KEYBIND_CONFIRM` (`tui.select.confirm`).
pub const KEYBIND_CONFIRM: &str = "tui.select.confirm";
/// `KEYBIND_SUBMIT` (`tui.input.submit`).
pub const KEYBIND_SUBMIT: &str = "tui.input.submit";
/// `KEYBIND_CANCEL` (`tui.select.cancel`).
pub const KEYBIND_CANCEL: &str = "tui.select.cancel";
/// `KEYBIND_NEW_LINE` (`tui.input.newLine`).
pub const KEYBIND_NEW_LINE: &str = "tui.input.newLine";
/// `KEYBIND_EDITOR_UP` (`tui.editor.cursorUp`).
pub const KEYBIND_EDITOR_UP: &str = "tui.editor.cursorUp";
/// `KEYBIND_EDITOR_DOWN` (`tui.editor.cursorDown`).
pub const KEYBIND_EDITOR_DOWN: &str = "tui.editor.cursorDown";
/// `KEYBIND_CLEAR` (`tui.editor.deleteToLineStart`).
pub const KEYBIND_CLEAR: &str = "tui.editor.deleteToLineStart";
/// `KEYBIND_EXTERNAL_EDITOR` (`app.editor.external`).
pub const KEYBIND_EXTERNAL_EDITOR: &str = "app.editor.external";

/// `NOTES_ACTIVATE_KEY`.
pub const NOTES_ACTIVATE_KEY: &str = "n";
/// `SPACE_KEY`.
pub const SPACE_KEY: &str = " ";

/// Name → key-id table used by [`route_key`].
///
/// Production uses [`Self::pi_defaults`] (the upstream keybinding defaults);
/// the parity harness injects the same ids from `state-vectors.json`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Keybindings {
    entries: BTreeMap<String, Vec<String>>,
}

impl Default for Keybindings {
    fn default() -> Self {
        Self::pi_defaults()
    }
}

impl Keybindings {
    /// Upstream default key ids for every action the questionnaire reads
    /// (`packages/tui/src/keybindings.ts` +
    /// `packages/coding-agent/src/core/keybindings.ts` defaults; `escape` and
    /// `ctrl+c` both cancel, `shift+enter`/`ctrl+j` both insert a newline).
    pub fn pi_defaults() -> Self {
        let mut entries: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut entry = |name: &str, keys: &[&str]| {
            entries.insert(
                name.to_owned(),
                keys.iter().map(|key| (*key).to_owned()).collect(),
            );
        };
        entry(KEYBIND_UP, &["up"]);
        entry(KEYBIND_DOWN, &["down"]);
        entry(KEYBIND_CONFIRM, &["enter"]);
        entry(KEYBIND_SUBMIT, &["enter"]);
        entry(KEYBIND_CANCEL, &["escape", "ctrl+c"]);
        entry(KEYBIND_NEW_LINE, &["shift+enter", "ctrl+j"]);
        entry(KEYBIND_EDITOR_UP, &["up"]);
        entry(KEYBIND_EDITOR_DOWN, &["down"]);
        entry(KEYBIND_CLEAR, &["ctrl+u"]);
        entry(KEYBIND_EXTERNAL_EDITOR, &["ctrl+g"]);
        Self { entries }
    }

    /// Replace one name's key ids (test/parity seam).
    pub fn set(&mut self, name: &str, keys: Vec<String>) {
        self.entries.insert(name.to_owned(), keys);
    }

    /// Whether `data` matches any key id bound to `name`. An unknown name
    /// matches nothing (the upstream manager returns false for it).
    pub fn matches(&self, data: &str, name: &str) -> bool {
        self.entries
            .get(name)
            .is_some_and(|keys| keys.iter().any(|key| matches_key(data, key)))
    }
}

/// Upstream `QuestionnaireAction` (identical wire shape).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum Action {
    /// Move the option focus, snapshotting the live input draft.
    Nav {
        /// Target row index (already wrapped by the router).
        next_index: usize,
        /// Live inline-input buffer.
        input_value: String,
    },
    /// `Ctrl+U` / `deleteToLineStart` — clear the whole draft.
    InputClear,
    /// `Ctrl+G` — open the host external editor with the current draft.
    InputEdit {
        /// Current draft.
        value: String,
    },
    /// External-editor replacement committed back into the draft.
    InputReplace {
        /// Replacement text.
        value: String,
    },
    /// `Tab`/`Shift+Tab`/left/right — switch the active tab.
    TabSwitch {
        /// Target tab index (already wrapped).
        next_tab: usize,
    },
    /// Commit one question answer.
    Confirm {
        /// The answer to record.
        answer: QuestionAnswer,
        /// Multi-question auto-advance target (absent in single-question mode).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        auto_advance_tab: Option<usize>,
    },
    /// Multi-select checkbox toggle.
    Toggle {
        /// Row index toggled.
        index: usize,
    },
    /// Multi-select `Next` commit.
    MultiConfirm {
        /// Selected labels (declaration order; empty = empty selection).
        selected: Vec<String>,
        /// Multi-question auto-advance target.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        auto_advance_tab: Option<usize>,
    },
    /// `Esc` — abandon the whole questionnaire.
    Cancel,
    /// `n` on a question tab — open the notes editor.
    NotesEnter,
    /// `Esc`/`Enter` in the notes editor — commit and close.
    NotesExit,
    /// Submit-tab `Enter` on the Submit row.
    Submit,
    /// Submit-tab arrow navigation.
    SubmitNav {
        /// `0` = Submit row, `1` = Cancel row.
        next_index: usize,
    },
    /// Forward one raw keystroke to the notes editor.
    NotesForward {
        /// Raw key data.
        data: String,
    },
    /// Collapse/expand toggle (configured `collapseKey`).
    ToggleCollapsed,
    /// Swallow the keystroke.
    Ignore,
}

impl Action {
    /// Wire discriminator string (parity report readability).
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Nav { .. } => "nav",
            Self::InputClear => "input_clear",
            Self::InputEdit { .. } => "input_edit",
            Self::InputReplace { .. } => "input_replace",
            Self::TabSwitch { .. } => "tab_switch",
            Self::Confirm { .. } => "confirm",
            Self::Toggle { .. } => "toggle",
            Self::MultiConfirm { .. } => "multi_confirm",
            Self::Cancel => "cancel",
            Self::NotesEnter => "notes_enter",
            Self::NotesExit => "notes_exit",
            Self::Submit => "submit",
            Self::SubmitNav { .. } => "submit_nav",
            Self::NotesForward { .. } => "notes_forward",
            Self::ToggleCollapsed => "toggle_collapsed",
            Self::Ignore => "ignore",
        }
    }

    /// Whether this action is the router's explicit `ignore`.
    pub fn is_ignore(&self) -> bool {
        matches!(self, Self::Ignore)
    }
}

/// Apply-context-free runtime values the router reads every keystroke
/// (upstream `QuestionnaireRuntime`; `keybindings` is injected separately so
/// the canonical state stays free of host configuration).
#[derive(Clone, Debug)]
pub struct QuestionnaireRuntime<'a> {
    /// Name → key-id resolver.
    pub keybindings: &'a Keybindings,
    /// Live inline-input draft.
    pub input_buffer: String,
    /// Editor cursor can move up (input mode only).
    pub can_move_input_up: bool,
    /// Editor cursor can move down (input mode only).
    pub can_move_input_down: bool,
    /// All questions.
    pub questions: &'a [QuestionData],
    /// Multi-question mode (`questions.len() > 1`).
    pub is_multi: bool,
    /// Focused row of the active tab.
    pub current_item: Option<&'a QuestionItem>,
    /// Rows of the active tab.
    pub items: &'a [QuestionItem],
    /// Resolved collapse key id (or `"off"`).
    pub collapse_key: String,
}

/// `isConfirm`: `tui.select.confirm` or `tui.input.submit` (a Slack-style
/// config folds `enter` into new-line and moves submit elsewhere; matching
/// either name keeps confirm reachable — upstream #156).
fn is_confirm(kb: &Keybindings, data: &str) -> bool {
    kb.matches(data, KEYBIND_CONFIRM) || kb.matches(data, KEYBIND_SUBMIT)
}

/// `wrapTab` — modulo wrap that tolerates a zero total.
pub fn wrap_tab(index: i64, total: usize) -> usize {
    if total == 0 {
        return 0;
    }
    let total = total as i64;
    (((index % total) + total) % total) as usize
}

/// `allAnswered` — every question has an answer.
pub fn all_answered(state: &QuestionnaireState, runtime: &QuestionnaireRuntime<'_>) -> bool {
    if runtime.questions.is_empty() {
        return false;
    }
    (0..runtime.questions.len()).all(|index| state.answers.contains_key(&index))
}

/// `totalTabs` — questions plus the Submit tab in multi-question mode.
fn total_tabs(runtime: &QuestionnaireRuntime<'_>) -> usize {
    if runtime.is_multi {
        runtime.questions.len() + 1
    } else {
        1
    }
}

/// `computeAutoAdvanceTab`.
fn compute_auto_advance_tab(
    state: &QuestionnaireState,
    runtime: &QuestionnaireRuntime<'_>,
) -> Option<usize> {
    if !runtime.is_multi {
        return None;
    }
    if state.current_tab + 1 < runtime.questions.len() {
        return Some(state.current_tab + 1);
    }
    Some(runtime.questions.len())
}

/// `buildSingleSelectAnswer`.
fn build_single_select_answer(
    state: &QuestionnaireState,
    runtime: &QuestionnaireRuntime<'_>,
) -> Option<QuestionAnswer> {
    let question = runtime.questions.get(state.current_tab)?;
    if state.input_mode {
        let label = runtime.input_buffer.clone();
        return Some(QuestionAnswer {
            question_index: state.current_tab,
            question: question.question.clone(),
            kind: AnswerKind::Custom,
            answer: (!label.is_empty()).then_some(label),
            selected: None,
            notes: None,
            preview: None,
        });
    }
    let item = runtime.current_item?;
    match item.kind {
        RowKind::Other | RowKind::Next => None,
        RowKind::Option => Some(QuestionAnswer {
            question_index: state.current_tab,
            question: question.question.clone(),
            kind: AnswerKind::Option,
            answer: Some(item.label.clone()),
            selected: None,
            notes: None,
            preview: None,
        }),
    }
}

/// `buildMultiSelected`.
fn build_multi_selected(
    state: &QuestionnaireState,
    runtime: &QuestionnaireRuntime<'_>,
) -> Vec<String> {
    let Some(question) = runtime.questions.get(state.current_tab) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (index, option) in question.options.iter().enumerate() {
        if state.multi_select_checked.contains(&index) {
            out.push(option.label.clone());
        }
    }
    out
}

/// `tabSwitchAction` — `Tab`/`→` forward, `Shift+Tab`/`←` backward.
fn tab_switch_action(
    data: &str,
    state: &QuestionnaireState,
    runtime: &QuestionnaireRuntime<'_>,
) -> Option<Action> {
    if !runtime.is_multi {
        return None;
    }
    let total = total_tabs(runtime);
    if matches_key(data, "tab") || matches_key(data, "right") {
        return Some(Action::TabSwitch {
            next_tab: wrap_tab(state.current_tab as i64 + 1, total),
        });
    }
    if matches_key(data, "shift+tab") || matches_key(data, "left") {
        return Some(Action::TabSwitch {
            next_tab: wrap_tab(state.current_tab as i64 - 1, total),
        });
    }
    None
}

/// `nextNavOnDown` — wrap to the first row at the bottom.
fn next_nav_on_down(state: &QuestionnaireState, runtime: &QuestionnaireRuntime<'_>) -> Action {
    Action::Nav {
        next_index: wrap_tab(state.option_index as i64 + 1, runtime.items.len().max(1)),
        input_value: runtime.input_buffer.clone(),
    }
}

/// `prevNavOnUp` — wrap to the last row at the top.
fn prev_nav_on_up(state: &QuestionnaireState, runtime: &QuestionnaireRuntime<'_>) -> Action {
    Action::Nav {
        next_index: wrap_tab(state.option_index as i64 - 1, runtime.items.len().max(1)),
        input_value: runtime.input_buffer.clone(),
    }
}

/// `routeCollapsed` — swallow every key except cancel while collapsed.
fn route_collapsed(kb: &Keybindings, data: &str) -> Action {
    if kb.matches(data, KEYBIND_CANCEL) {
        return Action::Cancel;
    }
    Action::Ignore
}

/// `routeNotesMode` — everything is editor text except close/commit keys.
fn route_notes_mode(kb: &Keybindings, data: &str) -> Action {
    if kb.matches(data, KEYBIND_CANCEL) {
        return Action::NotesExit;
    }
    if kb.matches(data, KEYBIND_NEW_LINE) {
        return Action::NotesForward {
            data: data.to_owned(),
        };
    }
    if is_confirm(kb, data) {
        return Action::NotesExit;
    }
    Action::NotesForward {
        data: data.to_owned(),
    }
}

/// `routeInputMode`.
fn route_input_mode(
    kb: &Keybindings,
    data: &str,
    state: &QuestionnaireState,
    runtime: &QuestionnaireRuntime<'_>,
) -> Action {
    // Newline takes precedence over confirmation when a user configuration
    // binds the same physical key to both semantics.
    if kb.matches(data, KEYBIND_NEW_LINE) {
        return Action::Ignore;
    }
    if is_confirm(kb, data) {
        let Some(answer) = build_single_select_answer(state, runtime) else {
            return Action::Ignore;
        };
        return Action::Confirm {
            answer,
            auto_advance_tab: compute_auto_advance_tab(state, runtime),
        };
    }
    if kb.matches(data, KEYBIND_CLEAR) {
        return Action::InputClear;
    }
    if kb.matches(data, KEYBIND_EXTERNAL_EDITOR) {
        return Action::InputEdit {
            value: runtime.input_buffer.clone(),
        };
    }
    if kb.matches(data, KEYBIND_CANCEL) {
        return Action::Cancel;
    }
    if kb.matches(data, KEYBIND_EDITOR_UP) && runtime.can_move_input_up {
        return Action::Ignore;
    }
    if kb.matches(data, KEYBIND_EDITOR_DOWN) && runtime.can_move_input_down {
        return Action::Ignore;
    }
    if kb.matches(data, KEYBIND_UP) {
        return prev_nav_on_up(state, runtime);
    }
    if kb.matches(data, KEYBIND_DOWN) {
        return next_nav_on_down(state, runtime);
    }
    Action::Ignore
}

/// `routeSubmitTab`.
fn route_submit_tab(
    kb: &Keybindings,
    data: &str,
    state: &QuestionnaireState,
    runtime: &QuestionnaireRuntime<'_>,
) -> Action {
    if kb.matches(data, KEYBIND_CANCEL) {
        return Action::Cancel;
    }
    if let Some(tab) = tab_switch_action(data, state, runtime) {
        return tab;
    }
    let up = kb.matches(data, KEYBIND_UP);
    let down = kb.matches(data, KEYBIND_DOWN);
    if up || down {
        let delta: i64 = if down { 1 } else { -1 };
        let next = wrap_tab(state.submit_choice_index as i64 + delta, 2);
        return Action::SubmitNav { next_index: next };
    }
    if is_confirm(kb, data) {
        // D1 (revised): Submit always submits; Cancel always cancels. Partial
        // answers flow through the envelope.
        return if state.submit_choice_index == 1 {
            Action::Cancel
        } else {
            Action::Submit
        };
    }
    if data == NOTES_ACTIVATE_KEY {
        return Action::NotesEnter;
    }
    Action::Ignore
}

/// `routeMultiSelectTab`.
fn route_multi_select_tab(
    kb: &Keybindings,
    data: &str,
    state: &QuestionnaireState,
    runtime: &QuestionnaireRuntime<'_>,
) -> Action {
    let focused_meta = runtime.current_item.map(|item| meta(item.kind));
    if data == SPACE_KEY {
        if focused_meta.is_some_and(|meta| meta.blocks_multi_toggle) {
            return Action::Ignore;
        }
        if focused_meta.is_some_and(|meta| meta.activates_input_mode) {
            return Action::Ignore;
        }
        return Action::Toggle {
            index: state.option_index,
        };
    }
    if is_confirm(kb, data) {
        if focused_meta.is_some_and(|meta| meta.activates_input_mode) {
            return Action::Ignore;
        }
        if !focused_meta.is_some_and(|meta| meta.auto_submits_in_multi) {
            return Action::Toggle {
                index: state.option_index,
            };
        }
        return Action::MultiConfirm {
            selected: build_multi_selected(state, runtime),
            auto_advance_tab: compute_auto_advance_tab(state, runtime),
        };
    }
    if kb.matches(data, KEYBIND_CANCEL) {
        return Action::Cancel;
    }
    Action::Ignore
}

/// `routeSingleSelectTab`.
fn route_single_select_tab(
    kb: &Keybindings,
    data: &str,
    state: &QuestionnaireState,
    runtime: &QuestionnaireRuntime<'_>,
) -> Action {
    if is_confirm(kb, data) {
        let Some(answer) = build_single_select_answer(state, runtime) else {
            return Action::Ignore;
        };
        return Action::Confirm {
            answer,
            auto_advance_tab: compute_auto_advance_tab(state, runtime),
        };
    }
    if kb.matches(data, KEYBIND_CANCEL) {
        return Action::Cancel;
    }
    Action::Ignore
}

/// `routeKey` — the full cascade (collapse intercept → collapsed/notes/input
/// modes → Submit tab → tab switch → `n` gate → arrows → per-mode routing).
pub fn route_key(
    data: &str,
    state: &QuestionnaireState,
    runtime: &QuestionnaireRuntime<'_>,
) -> Action {
    let kb = runtime.keybindings;

    // Collapse/expand is a UI-level affordance intercepted first so it works
    // from every inner state. A missing/empty/"off" spec disables it.
    if !runtime.collapse_key.is_empty()
        && runtime.collapse_key != "off"
        && matches_key(data, &runtime.collapse_key)
    {
        return Action::ToggleCollapsed;
    }

    if state.collapsed {
        return route_collapsed(kb, data);
    }
    if state.notes_visible {
        return route_notes_mode(kb, data);
    }
    if state.input_mode {
        return route_input_mode(kb, data, state, runtime);
    }
    if runtime.is_multi && state.current_tab == runtime.questions.len() {
        return route_submit_tab(kb, data, state, runtime);
    }

    if let Some(tab) = tab_switch_action(data, state, runtime) {
        return tab;
    }

    if runtime.questions.get(state.current_tab).is_none() {
        return Action::Ignore;
    }

    // Universal `n` activation on every question tab (above the per-mode
    // blocks: notes/input modes already returned, the Submit tab has its own
    // branch, and tab-switch ignores `n`).
    if data == NOTES_ACTIVATE_KEY {
        return Action::NotesEnter;
    }

    if kb.matches(data, KEYBIND_UP) {
        return prev_nav_on_up(state, runtime);
    }
    if kb.matches(data, KEYBIND_DOWN) {
        return next_nav_on_down(state, runtime);
    }

    let is_multi_question = runtime
        .questions
        .get(state.current_tab)
        .is_some_and(|question| question.multi_select == Some(true));
    if is_multi_question {
        return route_multi_select_tab(kb, data, state, runtime);
    }
    route_single_select_tab(kb, data, state, runtime)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::I18n;
    use crate::state::build::build_items_for_question;
    use crate::tool::types::{OptionData, QuestionData};

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

    struct Fixture {
        questions: Vec<QuestionData>,
        items: Vec<Vec<QuestionItem>>,
        keybindings: Keybindings,
    }

    impl Fixture {
        fn new(multi_select: bool, count: usize) -> Self {
            let questions: Vec<QuestionData> = (0..count).map(|_| question(multi_select)).collect();
            let i18n = I18n::for_locale("en");
            let items = questions
                .iter()
                .map(|question| build_items_for_question(question, &i18n))
                .collect();
            Self {
                questions,
                items,
                keybindings: Keybindings::pi_defaults(),
            }
        }

        fn route(&self, data: &str, state: &QuestionnaireState) -> Action {
            self.route_at(data, state, state.current_tab, state.option_index)
        }

        fn route_at(
            &self,
            data: &str,
            state: &QuestionnaireState,
            tab: usize,
            option: usize,
        ) -> Action {
            let runtime = QuestionnaireRuntime {
                keybindings: &self.keybindings,
                input_buffer: String::new(),
                can_move_input_up: false,
                can_move_input_down: false,
                questions: &self.questions,
                is_multi: self.questions.len() > 1,
                current_item: self.items.get(tab).and_then(|items| items.get(option)),
                items: self.items.get(tab).map(Vec::as_slice).unwrap_or(&[]),
                collapse_key: "ctrl+]".to_owned(),
            };
            route_key(data, state, &runtime)
        }
    }

    #[test]
    fn tab_switch_wraps_forward_and_backward_including_submit() {
        let fixture = Fixture::new(false, 2);
        let state = QuestionnaireState::initial();
        assert_eq!(
            fixture.route("\t", &state),
            Action::TabSwitch { next_tab: 1 }
        );
        assert_eq!(
            fixture.route("\x1b[Z", &state),
            Action::TabSwitch { next_tab: 2 },
            "shift+tab from tab 0 wraps backward onto the Submit tab"
        );
        assert_eq!(
            fixture.route("\x1b[C", &state),
            Action::TabSwitch { next_tab: 1 },
            "right arrow is a forward tab switch"
        );
        let mut submit = state.clone();
        submit.current_tab = 2;
        assert_eq!(
            fixture.route("\t", &submit),
            Action::TabSwitch { next_tab: 0 },
            "Submit tab wraps back to the first question"
        );
    }

    #[test]
    fn single_select_confirm_and_cancel() {
        let fixture = Fixture::new(false, 1);
        let state = QuestionnaireState::initial();
        let action = fixture.route("\r", &state);
        let Action::Confirm {
            answer,
            auto_advance_tab,
        } = action
        else {
            panic!("expected confirm, got {action:?}");
        };
        assert_eq!(answer.kind, AnswerKind::Option);
        assert_eq!(answer.answer.as_deref(), Some("A"));
        assert_eq!(
            auto_advance_tab, None,
            "single question never auto-advances"
        );
        assert_eq!(fixture.route("\x1b", &state), Action::Cancel);
    }

    #[test]
    fn nav_wraps_at_both_ends() {
        let fixture = Fixture::new(false, 1);
        let state = QuestionnaireState::initial();
        // items = [A, B, other] → 3 rows.
        assert_eq!(
            fixture.route("\x1b[B", &state),
            Action::Nav {
                next_index: 1,
                input_value: String::new()
            }
        );
        let mut last = QuestionnaireState::initial();
        last.option_index = 2;
        assert_eq!(
            fixture.route("\x1b[B", &last),
            Action::Nav {
                next_index: 0,
                input_value: String::new()
            }
        );
        assert_eq!(
            fixture.route("\x1b[A", &state),
            Action::Nav {
                next_index: 2,
                input_value: String::new()
            }
        );
    }

    #[test]
    fn multi_select_space_enter_and_next_matrix() {
        let fixture = Fixture::new(true, 1);
        let state = QuestionnaireState::initial();
        assert_eq!(
            fixture.route(" ", &state),
            Action::Toggle { index: 0 },
            "Space toggles a regular option"
        );
        assert_eq!(
            fixture.route("\r", &state),
            Action::Toggle { index: 0 },
            "Enter on a regular option toggles like Space"
        );
        // Type-something row (index 2) suppresses Space and confirm.
        assert_eq!(
            fixture.route_at(" ", &state, 0, 2),
            Action::Ignore,
            "Space is suppressed on the inline-input row"
        );
        // Next row (index 3) suppresses Space and submits.
        assert_eq!(
            fixture.route_at(" ", &state, 0, 3),
            Action::Ignore,
            "Space is suppressed on the Next sentinel"
        );
        assert_eq!(
            fixture.route_at("\r", &state, 0, 3),
            Action::MultiConfirm {
                selected: Vec::new(),
                auto_advance_tab: None
            },
            "empty selection is a valid commit"
        );
    }

    #[test]
    fn input_mode_newline_precedes_confirm_and_arrows_exit_at_edges() {
        let fixture = Fixture::new(false, 1);
        let mut state = QuestionnaireState::initial();
        state.input_mode = true;
        state.option_index = 2;
        assert_eq!(
            fixture.route("\n", &state),
            Action::Ignore,
            "newline is forwarded to the editor, never a confirm"
        );
        assert_eq!(
            fixture.route("\x7f", &state),
            Action::Ignore,
            "plain text keys fall through to the editor fast path"
        );
        assert_eq!(
            fixture.route("\x1b[A", &state),
            Action::Nav {
                next_index: 1,
                input_value: String::new()
            },
            "single-line buffer: up exits to row navigation"
        );
        assert_eq!(
            fixture.route("\x15", &state),
            Action::InputClear,
            "ctrl+u clears the draft"
        );
        assert_eq!(fixture.route("\x1b", &state), Action::Cancel);
    }

    #[test]
    fn notes_mode_forwards_and_closes() {
        let fixture = Fixture::new(false, 1);
        let mut state = QuestionnaireState::initial();
        state.notes_visible = true;
        assert_eq!(
            fixture.route("x", &state),
            Action::NotesForward {
                data: "x".to_owned()
            }
        );
        assert_eq!(fixture.route("\x1b", &state), Action::NotesExit);
        assert_eq!(fixture.route("\r", &state), Action::NotesExit);
    }

    #[test]
    fn collapsed_swallows_everything_except_cancel() {
        let fixture = Fixture::new(false, 1);
        let mut state = QuestionnaireState::initial();
        state.collapsed = true;
        assert_eq!(fixture.route("x", &state), Action::Ignore);
        assert_eq!(fixture.route("\x1b", &state), Action::Cancel);
        // The collapse key still routes while collapsed.
        assert_eq!(fixture.route("\x1d", &state), Action::ToggleCollapsed);
    }

    #[test]
    fn submit_tab_navigation_and_choice() {
        let fixture = Fixture::new(false, 2);
        let mut state = QuestionnaireState::initial();
        state.current_tab = 2; // Submit tab (2 questions)
        assert_eq!(fixture.route("\r", &state), Action::Submit);
        state.submit_choice_index = 1;
        assert_eq!(fixture.route("\r", &state), Action::Cancel);
        state.submit_choice_index = 0;
        assert_eq!(
            fixture.route("\x1b[B", &state),
            Action::SubmitNav { next_index: 1 }
        );
        let mut on_cancel = state.clone();
        on_cancel.submit_choice_index = 1;
        assert_eq!(
            fixture.route("\x1b[A", &on_cancel),
            Action::SubmitNav { next_index: 0 },
            "arrows wrap across the two Submit rows"
        );
    }

    #[test]
    fn collapse_key_off_disables_toggle() {
        let mut fixture = Fixture::new(false, 1);
        fixture.keybindings = Keybindings::pi_defaults();
        let state = QuestionnaireState::initial();
        let runtime = QuestionnaireRuntime {
            keybindings: &fixture.keybindings,
            input_buffer: String::new(),
            can_move_input_up: false,
            can_move_input_down: false,
            questions: &fixture.questions,
            is_multi: false,
            current_item: fixture.items[0].first(),
            items: &fixture.items[0],
            collapse_key: "off".to_owned(),
        };
        assert_eq!(route_key("\x1d", &state, &runtime), Action::Ignore);
    }
}
