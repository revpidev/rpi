//! Component session driver: the guest side of the interactive-UI ABI.
//!
//! Port of upstream `state/questionnaire-session.ts` @ `338b264c` for route C:
//! owns the canonical state cell, the headless inline-input buffer, the
//! router/reducer loop and the effect runner, and exposes the
//! [`Component`] surface the ABI drives (`render` / `handle_input` /
//! `on_resize` / `on_dispose`, plus the optional focus/theme hooks).
//!
//! rpi adaptations:
//! - `ctx.ui.custom()` + `OverlayHandle`/`onTerminalInput` are replaced by
//!   `mountComponent` / `pollComponent` / `renderComponent` (R-U1–R-U4); the
//!   session runs synchronously on the tool-execute thread inside
//!   [`run`] and never touches the terminal directly.
//! - The headless pi-tui `Editor` pair becomes [`InputBuffer`], a char-indexed
//!   multiline buffer (insert/newline/backspace/delete/arrows/home/end). Paste
//!   and undo history are out of Q2 scope.
//! - Q2 stages the notes and collapse actions out of the user-visible loop:
//!   the pure router/reducer implement them (and the parity vectors pin
//!   them), while the editor/reopen UX lands with TE31 (FR-Q3-D/E). The mount
//!   option `keysWhenHidden` is still sent so the host channel exists.

use rpi_ext_host::interactive_ui::{
    run_component, Component, ComponentCursor, DisposeReason, HostCall as AbiHostCall,
    InteractiveUiError, MountOptions, OverlayAnchor, OverlayOptions, SizeValue,
};
use serde_json::Value;

use crate::config::{resolve_collapse_key, AskUserQuestionConfig};
use crate::i18n::I18n;
use crate::state::build::{build_items_for_question, QuestionItem};
use crate::state::key_router::{
    route_key, Action, Keybindings, QuestionnaireRuntime, KEYBIND_EDITOR_DOWN, KEYBIND_EDITOR_UP,
    KEYBIND_NEW_LINE,
};
use crate::state::reducer::{apply, result_for, ApplyContext, Effect, QuestionnaireState};
use crate::tool::types::{QuestionData, QuestionParams, QuestionnaireResult};
use crate::view::dialog::{self, DialogModel};
use crate::view::theme::Theme;
use crate::HostCall;

/// Char-indexed multiline text buffer with a cursor.
///
/// The headless equivalent of the two pi-tui `Editor` instances in the
/// upstream session (inline draft + notes). Text editing keys that the router
/// forwards as [`Action::Ignore`] are applied here (the upstream
/// `handleIgnoreInline` fast path); explicit clear/editor actions arrive as
/// reducer effects instead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputBuffer {
    chars: Vec<char>,
    cursor: usize,
}

impl Default for InputBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl InputBuffer {
    /// Empty buffer with the cursor at the start.
    pub fn new() -> Self {
        Self {
            chars: Vec::new(),
            cursor: 0,
        }
    }

    /// Current text.
    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }

    /// Cursor position in characters.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Replace the contents; the cursor moves to the end (upstream
    /// `Editor.setText` semantics for reducer-driven restores).
    pub fn set_text(&mut self, text: &str) {
        self.chars = text.chars().collect();
        self.cursor = self.chars.len();
    }

    /// Empty the buffer.
    pub fn clear(&mut self) {
        self.chars.clear();
        self.cursor = 0;
    }

    fn insert_str(&mut self, text: &str) {
        for character in text.chars() {
            if character == '\r' {
                continue;
            }
            self.chars.insert(self.cursor, character);
            self.cursor += 1;
        }
    }

    /// Insert an explicit newline at the cursor.
    pub fn newline(&mut self) {
        self.insert_str("\n");
    }

    /// Delete the character before the cursor.
    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.chars.remove(self.cursor);
        }
    }

    /// Delete the character at the cursor.
    pub fn delete(&mut self) {
        if self.cursor < self.chars.len() {
            self.chars.remove(self.cursor);
        }
    }

    /// `(line index, char column)` of the cursor.
    pub fn cursor_line_col(&self) -> (usize, usize) {
        let mut line = 0usize;
        let mut column = 0usize;
        for (index, character) in self.chars.iter().enumerate() {
            if index == self.cursor {
                break;
            }
            if *character == '\n' {
                line += 1;
                column = 0;
            } else {
                column += 1;
            }
        }
        (line, column)
    }

    fn line_bounds(&self, line: usize) -> (usize, usize) {
        let mut current = 0usize;
        let mut start = 0usize;
        for (index, character) in self.chars.iter().enumerate() {
            if current == line {
                if *character == '\n' {
                    return (start, index);
                }
                continue;
            }
            if *character == '\n' {
                current += 1;
                start = index + 1;
            }
        }
        (start, self.chars.len())
    }

    fn line_count(&self) -> usize {
        self.chars
            .iter()
            .filter(|character| **character == '\n')
            .count()
            + 1
    }

    /// Move the cursor one column left.
    pub fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    /// Move the cursor one column right.
    pub fn move_right(&mut self) {
        if self.cursor < self.chars.len() {
            self.cursor += 1;
        }
    }

    /// Move the cursor one line up (clamped to the line length).
    pub fn move_up(&mut self) {
        let (line, column) = self.cursor_line_col();
        if line == 0 {
            return;
        }
        let (start, _) = self.line_bounds(line - 1);
        let previous_length = self.line_bounds(line - 1).1 - start;
        self.cursor = start + column.min(previous_length);
    }

    /// Move the cursor one line down (clamped to the line length).
    pub fn move_down(&mut self) {
        let (line, column) = self.cursor_line_col();
        if line + 1 >= self.line_count() {
            return;
        }
        let (start, end) = self.line_bounds(line + 1);
        self.cursor = start + column.min(end - start);
    }

    /// Move the cursor to the start of the line.
    pub fn move_home(&mut self) {
        let (line, _) = self.cursor_line_col();
        self.cursor = self.line_bounds(line).0;
    }

    /// Move the cursor to the end of the line.
    pub fn move_end(&mut self) {
        let (line, _) = self.cursor_line_col();
        self.cursor = self.line_bounds(line).1;
    }

    /// Delete from the line start to the cursor (`Ctrl+U` on POSIX shells;
    /// pi-tui `deleteToLineStart` is whole-buffer clear in the questionnaire —
    /// the router emits [`Action::InputClear`] for it, so this helper exists
    /// for completeness/tests).
    pub fn delete_to_line_start(&mut self) {
        let (line, _) = self.cursor_line_col();
        let start = self.line_bounds(line).0;
        self.chars.drain(start..self.cursor);
        self.cursor = start;
    }

    /// Apply one raw key to the buffer (text editing only; confirm/cancel are
    /// routed by the caller).
    pub fn handle_input(&mut self, data: &str, keybindings: &Keybindings) {
        if keybindings.matches(data, KEYBIND_NEW_LINE) {
            self.newline();
            return;
        }
        if rpi_tui::keys::matches_key(data, "backspace") {
            self.backspace();
            return;
        }
        if rpi_tui::keys::matches_key(data, "delete") {
            self.delete();
            return;
        }
        if rpi_tui::keys::matches_key(data, "left") {
            self.move_left();
            return;
        }
        if rpi_tui::keys::matches_key(data, "right") {
            self.move_right();
            return;
        }
        if keybindings.matches(data, KEYBIND_EDITOR_UP) || rpi_tui::keys::matches_key(data, "up") {
            self.move_up();
            return;
        }
        if keybindings.matches(data, KEYBIND_EDITOR_DOWN)
            || rpi_tui::keys::matches_key(data, "down")
        {
            self.move_down();
            return;
        }
        if rpi_tui::keys::matches_key(data, "home") {
            self.move_home();
            return;
        }
        if rpi_tui::keys::matches_key(data, "end") {
            self.move_end();
            return;
        }
        if let Some(text) = printable_text(data) {
            self.insert_str(&text);
        }
    }
}

/// Printable payload of one input chunk (paste-aware): a lone escape
/// sequence is not text; `\r` is dropped and `\n`/`\t`/printables are kept.
fn printable_text(data: &str) -> Option<String> {
    if data.starts_with('\u{1b}') {
        return None;
    }
    let mut out = String::new();
    for character in data.chars() {
        if character == '\r' {
            continue;
        }
        if character == '\n' || character == '\t' || (character >= ' ' && character != '\u{7f}') {
            out.push(character);
        }
    }
    (!out.is_empty()).then_some(out)
}

/// The questionnaire component driven by the ABI loop.
pub struct QuestionnaireComponent {
    /// Canonical state (reducer-owned).
    pub state: QuestionnaireState,
    questions: Vec<QuestionData>,
    items_by_tab: Vec<Vec<QuestionItem>>,
    keybindings: Keybindings,
    i18n: I18n,
    theme: Theme,
    input: InputBuffer,
    collapse_key: String,
    width: usize,
    height: usize,
    cursor: Option<ComponentCursor>,
    result: Option<QuestionnaireResult>,
    disposed: bool,
}

impl QuestionnaireComponent {
    /// Build a component for `params` (items are derived with `i18n`).
    pub fn new(params: &QuestionParams, i18n: I18n, collapse_key: String) -> Self {
        let items_by_tab = params
            .questions
            .iter()
            .map(|question| build_items_for_question(question, &i18n))
            .collect();
        Self {
            state: QuestionnaireState::initial(),
            questions: params.questions.clone(),
            items_by_tab,
            keybindings: Keybindings::pi_defaults(),
            i18n,
            theme: Theme::dark(),
            input: InputBuffer::new(),
            collapse_key,
            width: 80,
            height: 24,
            cursor: None,
            result: None,
            disposed: false,
        }
    }

    /// Override the palette (tests/golden frames).
    pub fn with_theme(mut self, theme: Theme) -> Self {
        self.theme = theme;
        self
    }

    /// Override key bindings (tests/parity).
    pub fn with_keybindings(mut self, keybindings: Keybindings) -> Self {
        self.keybindings = keybindings;
        self
    }

    /// Render one frame at `width` and remember the cursor for
    /// [`Component::cursor`].
    pub fn render_frame(&mut self, width: usize) -> Vec<String> {
        let frame = dialog::render(&DialogModel {
            state: &self.state,
            questions: &self.questions,
            items_by_tab: &self.items_by_tab,
            i18n: &self.i18n,
            theme: &self.theme,
            input_text: &self.input.text(),
            input_cursor: Some(self.input.cursor()),
            collapse_key: &self.collapse_key,
            width,
        });
        self.cursor = frame.cursor.map(|(row, col)| ComponentCursor { row, col });
        frame.lines
    }

    /// Dispatch one raw key through the router/reducer (public for the golden
    /// fixtures and tests).
    pub fn dispatch(&mut self, data: &str) {
        if self.disposed {
            return;
        }
        let action = {
            let items = self
                .items_by_tab
                .get(self.state.current_tab)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let runtime = QuestionnaireRuntime {
                keybindings: &self.keybindings,
                input_buffer: self.input.text(),
                can_move_input_up: self.input.cursor_line_col().0 > 0,
                can_move_input_down: self.input.cursor_line_col().0 + 1 < self.input.line_count(),
                questions: &self.questions,
                is_multi: self.questions.len() > 1,
                current_item: items.get(self.state.option_index),
                items,
                collapse_key: self.collapse_key.clone(),
            };
            route_key(data, &self.state, &runtime)
        };

        // Q2 staging (TE31 / FR-Q3-D/E): the pure router and reducer implement
        // the notes editor and collapse transitions (parity vectors pin them),
        // but the user-visible notes editor, hidden-state reopen and one-time
        // `ui.notify` land with Q3. Ignoring these actions keeps every Q2 key
        // path from dead-ending: `n`/`Ctrl+]` behave like unbound keys.
        if matches!(
            action,
            Action::NotesEnter
                | Action::NotesExit
                | Action::NotesForward { .. }
                | Action::ToggleCollapsed
        ) {
            return;
        }

        if action.is_ignore() {
            if self.state.input_mode {
                self.input.handle_input(data, &self.keybindings);
            }
            return;
        }

        let result = {
            let ctx = ApplyContext {
                questions: &self.questions,
                items_by_tab: &self.items_by_tab,
            };
            apply(&self.state, &action, &ctx)
        };
        self.state = result.state;
        for effect in result.effects {
            self.run_effect(effect);
        }
    }

    fn run_effect(&mut self, effect: Effect) {
        match effect {
            Effect::SetInputBuffer { value } => self.input.set_text(&value),
            Effect::ClearInputBuffer => self.input.clear(),
            // Q2 staging: notes-editor and overlay-hide effects are unreachable
            // while the actions above are staged out; the reducer still emits
            // them for Q3's runtime.
            Effect::OpenInputEditor { .. }
            | Effect::SetNotesValue { .. }
            | Effect::SetNotesFocused { .. }
            | Effect::ForwardNotesKeystroke { .. }
            | Effect::SetOverlayHidden { .. } => {}
            Effect::Done { result } => self.result = Some(result),
        }
    }
}

impl Component for QuestionnaireComponent {
    fn render(&mut self, width: usize) -> Vec<String> {
        self.width = width;
        self.render_frame(width)
    }

    fn handle_input(&mut self, data: &str) {
        self.dispatch(data);
    }

    fn cursor(&self) -> Option<ComponentCursor> {
        self.cursor
    }

    fn on_resize(&mut self, width: usize, height: usize) {
        self.width = width;
        self.height = height;
    }

    fn on_theme(&mut self, theme: &Value) {
        self.theme = Theme::from_json(theme);
    }

    fn on_dispose(&mut self, _reason: DisposeReason) {
        self.disposed = true;
        if self.result.is_none() {
            self.result = Some(result_for(&self.state, &self.questions, true));
        }
    }

    fn done(&mut self) -> Option<Value> {
        self.result
            .as_ref()
            .and_then(|result| serde_json::to_value(result).ok())
    }
}

/// Adapter from the plugin's JSON host-call surface to the ABI transport.
struct AbiHostAdapter<'a> {
    host: &'a dyn HostCall,
}

impl AbiHostCall for AbiHostAdapter<'_> {
    fn call(&self, method: &str, args: Value) -> Result<Value, InteractiveUiError> {
        self.host
            .call(method, args)
            .map_err(|error| InteractiveUiError::from_host_error(&error.kind, error.message))
    }
}

/// Mount options for the questionnaire overlay (`ui.custom` equivalent).
pub fn mount_options(collapse_key: &str) -> MountOptions {
    MountOptions {
        overlay: true,
        overlay_options: Some(OverlayOptions {
            anchor: Some(OverlayAnchor::BottomCenter),
            width: Some(SizeValue::Percent(100.0)),
            max_height: Some(SizeValue::Percent(100.0)),
            margin: Some(rpi_ext_host::interactive_ui::Margin {
                left: 0,
                right: 0,
                bottom: 0,
                top: 0,
            }),
            ..OverlayOptions::default()
        }),
        // The questionnaire has no animation; it never subscribes to ticks.
        tick_ms: 0,
        keys_when_hidden: if collapse_key == "off" || collapse_key.is_empty() {
            Vec::new()
        } else {
            vec![collapse_key.to_owned()]
        },
        cursor: true,
        label: Some("ask_user_question".to_owned()),
        ..MountOptions::default()
    }
}

/// Drive the questionnaire component through the ABI loop
/// (`ctx.ui.custom(makeSessionFactory(...), options)` equivalent).
///
/// Returns the component result on success. A host that does not implement
/// the interactive UI (C0/old host / RPC / print) answers `unknownMethod` on
/// the first `mountComponent`; the caller falls back per R-Q6.2.
pub fn run(
    host: &dyn HostCall,
    params: &QuestionParams,
    i18n: &I18n,
    config: &AskUserQuestionConfig,
) -> Result<QuestionnaireResult, InteractiveUiError> {
    let collapse_key = resolve_collapse_key(config);
    let component = QuestionnaireComponent::new(params, i18n.clone(), collapse_key.clone());
    let options = mount_options(&collapse_key);
    let adapter = AbiHostAdapter { host };
    let value = run_component(&adapter, component, options)?;
    serde_json::from_value(value).map_err(|error| {
        InteractiveUiError::invalid_request(format!("ask_user_question component result: {error}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::types::OptionData;

    fn params(multi_select: bool, count: usize) -> QuestionParams {
        QuestionParams {
            questions: (0..count)
                .map(|_| QuestionData {
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
                })
                .collect(),
        }
    }

    #[test]
    fn input_buffer_edits_multiline_text_and_reports_line_col() {
        let mut buffer = InputBuffer::new();
        buffer.set_text("abc");
        buffer.move_left();
        buffer.insert_str("X");
        assert_eq!(buffer.text(), "abXc");
        buffer.newline();
        assert_eq!(buffer.text(), "abX\nc");
        assert_eq!(buffer.cursor_line_col(), (1, 0));
        buffer.move_up();
        assert_eq!(buffer.cursor_line_col(), (0, 0));
        buffer.move_end();
        assert_eq!(buffer.cursor_line_col(), (0, 3));
        buffer.move_down();
        assert_eq!(buffer.cursor_line_col(), (1, 1));
        buffer.backspace();
        assert_eq!(buffer.text(), "abX\n");
        buffer.delete();
        assert_eq!(buffer.text(), "abX\n");
        buffer.move_home();
        buffer.delete_to_line_start();
        assert_eq!(buffer.text(), "abX\n");
    }

    #[test]
    fn input_buffer_keyboard_path_matches_pi_defaults() {
        let mut buffer = InputBuffer::new();
        let keybindings = Keybindings::pi_defaults();
        buffer.handle_input("h", &keybindings);
        buffer.handle_input("i", &keybindings);
        buffer.handle_input("\n", &keybindings); // ctrl+j / shift+enter newline
        buffer.handle_input("y", &keybindings);
        assert_eq!(buffer.text(), "hi\ny");
        buffer.handle_input("\x7f", &keybindings); // backspace removes 'y'
        assert_eq!(buffer.text(), "hi\n");
        assert_eq!(buffer.cursor_line_col(), (1, 0));
        buffer.handle_input("\x1b[A", &keybindings); // up to the first line
        assert_eq!(buffer.cursor_line_col(), (0, 0));
        buffer.handle_input("\x1b[C", &keybindings); // right
        assert_eq!(buffer.cursor_line_col(), (0, 1));
    }

    #[test]
    fn dispatch_confirm_answers_and_finishes() {
        let i18n = I18n::for_locale("en");
        let params = params(false, 1);
        let mut component = QuestionnaireComponent::new(&params, i18n, "ctrl+]".to_owned());
        component.dispatch("\r");
        let done = component.done().expect("done");
        assert_eq!(done["answers"][0]["answer"], "A");
        assert_eq!(done["cancelled"], false);
    }

    #[test]
    fn dispatch_navigation_and_inline_input_keeps_drafts_per_option() {
        let i18n = I18n::for_locale("en");
        let params = params(false, 1);
        let mut component = QuestionnaireComponent::new(&params, i18n, "ctrl+]".to_owned());
        // Down to the "Type something." row and type.
        component.dispatch("\x1b[B");
        component.dispatch("\x1b[B");
        assert!(component.state.input_mode);
        component.dispatch("h");
        component.dispatch("i");
        assert_eq!(component.input.text(), "hi");
        // Up back to option rows keeps the draft (state) and leaves input mode.
        component.dispatch("\x1b[A");
        assert!(!component.state.input_mode);
        assert_eq!(
            component
                .state
                .custom_drafts_by_tab
                .get(&0)
                .map(String::as_str),
            Some("hi")
        );
        // Down again restores the draft into the buffer.
        component.dispatch("\x1b[B");
        assert_eq!(component.input.text(), "hi");
    }

    #[test]
    fn q2_stages_notes_and_collapse_out_of_the_loop() {
        let i18n = I18n::for_locale("en");
        let params = params(false, 1);
        let mut component = QuestionnaireComponent::new(&params, i18n, "ctrl+]".to_owned());
        component.dispatch("n");
        assert!(!component.state.notes_visible, "notes editor is Q3");
        component.dispatch("\x1d"); // ctrl+]
        assert!(!component.state.collapsed, "hidden reopen is Q3");
    }

    #[test]
    fn dispose_returns_a_cancelled_result_with_partial_answers() {
        let i18n = I18n::for_locale("en");
        let params = params(false, 1);
        let mut component = QuestionnaireComponent::new(&params, i18n, "ctrl+]".to_owned());
        component.state.answers.insert(
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
        component.on_dispose(DisposeReason::ToolAbort);
        let done = component.done().expect("done");
        assert_eq!(done["cancelled"], true);
        assert_eq!(done["answers"][0]["answer"], "A");
    }

    #[test]
    fn mount_options_carry_overlay_geometry_and_collapse_channel() {
        let options = mount_options("ctrl+]");
        let overlay = options.overlay_options.expect("overlay options");
        assert_eq!(overlay.anchor, Some(OverlayAnchor::BottomCenter));
        assert_eq!(overlay.width, Some(SizeValue::Percent(100.0)));
        assert_eq!(overlay.max_height, Some(SizeValue::Percent(100.0)));
        assert_eq!(options.tick_ms, 0);
        assert_eq!(options.keys_when_hidden, vec!["ctrl+]".to_owned()]);
        assert!(options.cursor);
        assert_eq!(options.label.as_deref(), Some("ask_user_question"));
        assert!(mount_options("off").keys_when_hidden.is_empty());
    }

    #[test]
    fn theme_event_replaces_the_palette() {
        let i18n = I18n::for_locale("en");
        let params = params(false, 1);
        let mut component = QuestionnaireComponent::new(&params, i18n, "ctrl+]".to_owned());
        component.on_theme(&serde_json::json!({"colors": {"accent": "#ff0000"}}));
        assert_eq!(
            component.theme.accent,
            rpi_ext_host::interactive_ui::AnsiColor::Rgb(255, 0, 0)
        );
    }
}
