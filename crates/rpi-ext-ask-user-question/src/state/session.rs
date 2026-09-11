//! Component session driver: the guest side of the interactive-UI ABI.
//!
//! Port of upstream `state/questionnaire-session.ts` @ `338b264c` for route C:
//! owns the canonical state cell, the headless inline-input and notes buffers,
//! the router/reducer loop and the effect runner, and exposes the
//! [`Component`] surface the ABI drives (`render` / `handle_input` /
//! `on_resize` / `on_dispose`, plus the optional focus/theme hooks).
//!
//! rpi adaptations:
//! - `ctx.ui.custom()` + `OverlayHandle`/`onTerminalInput` are replaced by
//!   `mountComponent` / `pollComponent` / `renderComponent` (R-U1–R-U4); the
//!   session runs synchronously on the tool-execute thread inside
//!   [`run`] and never touches the terminal directly.
//! - The headless pi-tui `Editor` pair becomes two [`InputBuffer`]s (inline
//!   draft + notes). Paste and undo history are out of scope.
//! - Effects that need the host (`setComponentHidden`, one-time collapse
//!   guidance `ui.notify`, the `Ctrl+G` external-editor chain) are queued as
//!   [`HostEffect`]s during dispatch and executed by the run loop, which owns
//!   the transport — the component itself stays host-free so golden fixtures
//!   and unit tests can drive it headlessly.
//! - The external editor chain (`ask-user-question.ts` `editInput` +
//!   `state/external-editor.ts`): `ui.editExternal` (markdown temp file, the
//!   host owns the TUI suspend/resume and trailing-newline normalization);
//!   `unknownMethod` (pre-C3 host) falls back to `ui.editor`; failures notify
//!   `editor.failed` and keep the draft; a cancel keeps the draft silently.

use std::collections::VecDeque;

use rpi_ext_host::interactive_ui::{
    edit_external, mount_component, poll_component, render_component, set_component_hidden,
    Component, ComponentCursor, ComponentEvent, ComponentFrame, DisposeReason, DoneValue,
    HostCall as AbiHostCall, InteractiveUiError, MountOptions, OverlayAnchor, OverlayOptions,
    SizeValue,
};
use serde_json::{json, Value};

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

/// A host-touching side effect queued during dispatch and executed by the
/// session run loop (which owns the transport and the component handle).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostEffect {
    /// `Effect::SetOverlayHidden` — `ui.setComponentHidden(handle, hidden)`.
    SetOverlayHidden {
        /// `true` = hide.
        hidden: bool,
    },
    /// `Effect::OpenInputEditor` — the `Ctrl+G` external-editor chain.
    OpenInputEditor {
        /// Seed text (the live draft).
        value: String,
    },
}

/// `editor.failed` canonical English (`notify` message prefix).
pub const EDITOR_FAILED: &str = "External editor failed";

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
    notes: InputBuffer,
    collapse_key: String,
    width: usize,
    height: Option<usize>,
    terminal_width: usize,
    cursor: Option<ComponentCursor>,
    result: Option<QuestionnaireResult>,
    disposed: bool,
    pending_effects: VecDeque<HostEffect>,
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
            notes: InputBuffer::new(),
            collapse_key,
            width: 80,
            height: None,
            terminal_width: 0,
            cursor: None,
            result: None,
            disposed: false,
            pending_effects: VecDeque::new(),
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

    /// Override the terminal width (preview breakpoint gate; `0` = follow the
    /// pane width, which equals the terminal for the 100% overlay).
    pub fn with_terminal_width(mut self, terminal_width: usize) -> Self {
        self.terminal_width = terminal_width;
        self
    }

    /// Override the content height (scroll window; `None` disables it).
    pub fn with_height(mut self, height: Option<usize>) -> Self {
        self.height = height;
        self
    }

    /// Drain the queued host effects (run loop / tests).
    pub fn take_host_effects(&mut self) -> Vec<HostEffect> {
        self.pending_effects.drain(..).collect()
    }

    /// Live notes buffer (tests).
    pub fn notes_text(&self) -> String {
        self.notes.text()
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
            notes_text: &self.notes.text(),
            notes_cursor: Some(self.notes.cursor()),
            collapse_key: &self.collapse_key,
            width,
            terminal_width: if self.terminal_width == 0 {
                width
            } else {
                self.terminal_width
            },
            height: self.height,
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
        self.dispatch_action(action, Some(data));
    }

    /// Apply one already-routed action (the external-editor write-back uses
    /// this entry directly). `source` is the raw key when routed (the
    /// `Ignore` fast path needs it); `None` for synthesized actions.
    pub fn dispatch_action(&mut self, action: Action, source: Option<&str>) {
        if self.disposed {
            return;
        }
        if action.is_ignore() {
            if self.state.input_mode {
                if let Some(data) = source {
                    self.input.handle_input(data, &self.keybindings);
                }
            }
            return;
        }

        // `mirrorNotesDraft`: the reducer's `notes_draft` is the canonical
        // mirror of the live notes buffer — sync it before every reduction so
        // `NotesExit` commits what the user actually typed.
        self.state.notes_draft = self.notes.text();

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
            Effect::OpenInputEditor { value } => {
                self.pending_effects
                    .push_back(HostEffect::OpenInputEditor { value });
            }
            Effect::SetNotesValue { value } => self.notes.set_text(&value),
            Effect::SetNotesFocused { .. } => {
                // The rpi dialog renders the notes editor inline; focus is
                // implied by `state.notes_visible` (no separate widget).
            }
            Effect::ForwardNotesKeystroke { data } => {
                self.notes.handle_input(&data, &self.keybindings)
            }
            Effect::SetOverlayHidden { hidden } => {
                self.pending_effects
                    .push_back(HostEffect::SetOverlayHidden { hidden });
            }
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
        self.height = Some(height);
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

/// `COLLAPSE_NOTICE` — the one-time guidance emitted on the first hide
/// (upstream `registerCollapseKeyListener`; literal, not a locale key).
fn collapse_notice(collapse_key: &str) -> String {
    format!(
        "ask_user_question hidden — press {} to reopen",
        crate::config::format_key_spec_for_display(collapse_key)
    )
}

/// Notify `editor.failed: <detail>` (upstream `editInput` catch).
fn notify_editor_failed(host: &dyn HostCall, i18n: &I18n, detail: &str) {
    let message = format!("{}: {detail}", i18n.t("editor.failed", EDITOR_FAILED));
    if let Err(error) = host.call(
        "ui.notify",
        json!({"message": message, "notifyType": "error"}),
    ) {
        tracing::warn!(
            kind = %error.kind,
            message = %error.message,
            "rpiv-ask-user-question: editor-failure notify failed"
        );
    }
}

/// The `Ctrl+G` external-editor chain (`editInput`): `ui.editExternal`
/// (markdown draft, host owns suspend/resume + trailing-newline + cancel
/// semantics) with the `ui.editor` fallback for pre-C3 hosts
/// (R-Q5.9/R-U11.3); failures notify and keep the draft.
fn run_input_editor_chain(
    host: &dyn HostCall,
    component: &mut QuestionnaireComponent,
    value: &str,
    title: &str,
) {
    let adapter = AbiHostAdapter { host };
    let i18n = component.i18n.clone();
    match edit_external(&adapter, value, Some("markdown")) {
        Ok(Some(edited)) => {
            component.dispatch_action(Action::InputReplace { value: edited }, None);
        }
        Ok(None) => {
            // User cancelled the editor (non-zero exit) — keep the draft.
        }
        Err(error) if error.is_unknown_method() => {
            // Pre-C3 host: fall back to the host-internal editor.
            match host.call("ui.editor", json!({"title": title, "prefill": value})) {
                Ok(Value::String(edited)) => {
                    component.dispatch_action(Action::InputReplace { value: edited }, None);
                }
                Ok(_) => {
                    // Cancelled (null) or malformed success — keep the draft.
                }
                Err(fallback_error) => notify_editor_failed(host, &i18n, &fallback_error.message),
            }
        }
        Err(error) => notify_editor_failed(host, &i18n, &error.message),
    }
}

/// Drive the questionnaire component through the ABI loop
/// (`ctx.ui.custom(makeSessionFactory(...), options)` equivalent).
///
/// Unlike the stock `run_component` helper this loop also executes the
/// component's queued [`HostEffect`]s after every event (the collapse
/// `setComponentHidden`, the one-time hide guidance and the external-editor
/// chain all need the transport + handle). Returns the component result on
/// success. A host that does not implement the interactive UI (C0/old host /
/// RPC / print) answers `unknownMethod` on the first `mountComponent`; the
/// caller falls back per R-Q6.2.
pub fn run(
    host: &dyn HostCall,
    params: &QuestionParams,
    i18n: &I18n,
    config: &AskUserQuestionConfig,
) -> Result<QuestionnaireResult, InteractiveUiError> {
    let collapse_key = resolve_collapse_key(config);
    let mut component = QuestionnaireComponent::new(params, i18n.clone(), collapse_key.clone());
    let options = mount_options(&collapse_key);
    let adapter = AbiHostAdapter { host };
    let handle = mount_component(&adapter, &options)?;

    let mut width = 0usize;
    let mut announced_hide = false;
    loop {
        let event = poll_component(&adapter, handle)?;
        match &event {
            ComponentEvent::Resize {
                width: event_width,
                height,
            } => {
                width = *event_width;
                component.on_resize(*event_width, *height);
            }
            ComponentEvent::Input { data } => component.handle_input(data),
            ComponentEvent::Focus => component.on_focus(),
            ComponentEvent::Blur => component.on_blur(),
            ComponentEvent::Tick => component.on_tick(),
            ComponentEvent::Theme { theme } => component.on_theme(theme),
            ComponentEvent::Visibility { hidden } => component.on_visibility(*hidden),
            ComponentEvent::Render => component.on_render(),
            ComponentEvent::Dispose { reason } => {
                component.on_dispose(*reason);
                let frame = current_frame(&mut component, width);
                // Final frame after dispose is best-effort (bounded grace).
                let _ = render_component(&adapter, handle, &frame);
                return finish(frame);
            }
        }

        // Execute host effects queued by the dispatched event (the editor
        // chain may itself dispatch further actions).
        for effect in component.take_host_effects() {
            match effect {
                HostEffect::SetOverlayHidden { hidden } => {
                    if hidden && !announced_hide {
                        announced_hide = true;
                        if let Err(error) = host.call(
                            "ui.notify",
                            json!({
                                "message": collapse_notice(&collapse_key),
                                "notifyType": "info"
                            }),
                        ) {
                            tracing::warn!(
                                kind = %error.kind,
                                message = %error.message,
                                "rpiv-ask-user-question: collapse guidance notify failed"
                            );
                        }
                    }
                    if let Err(error) = set_component_hidden(&adapter, handle, hidden) {
                        // Hide failures degrade to the visible collapsed row
                        // (upstream `canReopenWhileHidden == false` fallback).
                        tracing::warn!(
                            kind = %error.kind,
                            message = %error.message,
                            "rpiv-ask-user-question: setComponentHidden failed"
                        );
                    }
                }
                HostEffect::OpenInputEditor { value } => {
                    let title = component
                        .state
                        .current_tab
                        .min(params.questions.len().saturating_sub(1));
                    let title = params
                        .questions
                        .get(title)
                        .map(|question| question.question.clone())
                        .unwrap_or_default();
                    run_input_editor_chain(host, &mut component, &value, &title);
                }
            }
        }

        let frame = current_frame(&mut component, width);
        let finished = frame.is_done();
        render_component(&adapter, handle, &frame)?;
        if finished {
            return finish(frame);
        }
    }
}

fn current_frame(component: &mut QuestionnaireComponent, width: usize) -> ComponentFrame {
    ComponentFrame {
        lines: component.render(width),
        cursor: component.cursor(),
        done: component
            .done()
            .map_or(DoneValue::Absent, DoneValue::Present),
    }
}

fn finish(frame: ComponentFrame) -> Result<QuestionnaireResult, InteractiveUiError> {
    serde_json::from_value(frame.done.into_value().unwrap_or(Value::Null)).map_err(|error| {
        InteractiveUiError::invalid_request(format!("ask_user_question component result: {error}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::types::OptionData;
    use serde_json::json;

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

    /// FR-Q3-D: `n` opens the notes editor; keystrokes edit the buffer
    /// (`n` types `n`); `Shift+Enter` newlines; `Enter`/`Esc` commit and
    /// close; notes never mark the question answered.
    #[test]
    fn note_editor_draft_and_close() {
        let i18n = I18n::for_locale("en");
        let params = params(false, 1);
        let mut component = QuestionnaireComponent::new(&params, i18n, "ctrl+]".to_owned());
        component.dispatch("n");
        assert!(component.state.notes_visible);
        component.dispatch("n"); // 'n' types an 'n' inside the editor
        component.dispatch("o");
        component.dispatch("t");
        component.dispatch("e");
        assert_eq!(component.notes_text(), "note");
        component.dispatch("\n"); // shift+enter newline
        component.dispatch("two");
        assert_eq!(component.notes_text(), "note\ntwo");
        assert!(
            component.state.answers.is_empty(),
            "notes never mark the question answered"
        );
        component.dispatch("\r"); // Enter commits and closes
        assert!(!component.state.notes_visible);
        assert_eq!(
            component.state.notes_by_tab.get(&0).map(String::as_str),
            Some("note\ntwo")
        );
        assert!(component.state.answers.is_empty());
        // The note merges into the answer on confirm.
        component.dispatch("\r");
        let done = component.done().expect("done");
        assert_eq!(done["answers"][0]["notes"], "note\ntwo");
        assert_eq!(done["answers"][0]["answer"], "A");
    }

    /// FR-Q3-D: Submit-page `n` opens the global note; it survives tab
    /// switches and lifts into `globalNote` on submit.
    #[test]
    fn global_note_opens_from_submit_and_lifts_on_done() {
        let i18n = I18n::for_locale("en");
        let params = params(false, 2);
        let mut component = QuestionnaireComponent::new(&params, i18n, "ctrl+]".to_owned());
        component.state.current_tab = 2; // Submit tab
        component.dispatch("n");
        assert!(component.state.notes_visible);
        component.dispatch("g");
        component.dispatch("\r");
        assert_eq!(
            component.state.notes_by_tab.get(&2).map(String::as_str),
            Some("g")
        );
        component.dispatch("\r"); // Submit
        let done = component.done().expect("done");
        assert_eq!(done["globalNote"], "g");
    }

    /// FR-Q3-E: collapse toggles the state and queues the hide/reopen host
    /// effects; `collapseKey:"off"` never routes a toggle.
    #[test]
    fn collapse_cycle_and_reopen() {
        let i18n = I18n::for_locale("en");
        let params = params(false, 1);
        let mut component = QuestionnaireComponent::new(&params, i18n, "ctrl+]".to_owned());
        component.dispatch("\x1d"); // ctrl+]
        assert!(component.state.collapsed);
        assert_eq!(
            component.take_host_effects(),
            vec![HostEffect::SetOverlayHidden { hidden: true }]
        );
        // While collapsed the same key reopens (the keysWhenHidden channel).
        component.dispatch("\x1d");
        assert!(!component.state.collapsed);
        assert_eq!(
            component.take_host_effects(),
            vec![HostEffect::SetOverlayHidden { hidden: false }]
        );

        let mut off =
            QuestionnaireComponent::new(&params, I18n::for_locale("en"), "off".to_owned());
        off.dispatch("\x1d");
        assert!(!off.state.collapsed, "off never toggles");
        assert!(off.take_host_effects().is_empty());
    }

    /// TE-D38: a hidden overlay receives only the collapse key
    /// (`keysWhenHidden`), so Esc can never cancel from the hidden state; the
    /// pure router's collapsed-Esc `Cancel` exists only for the visible
    /// collapsed-row fallback (upstream `routeCollapsed` parity).
    #[test]
    fn hidden_esc_does_not_cancel() {
        let i18n = I18n::for_locale("en");
        let params = params(false, 1);
        let mut component = QuestionnaireComponent::new(&params, i18n, "ctrl+]".to_owned());
        component.dispatch("\x1d");
        assert!(component.state.collapsed);
        let _ = component.take_host_effects();
        // The only key the host routes while hidden is the collapse key.
        let options = mount_options("ctrl+]");
        assert_eq!(options.keys_when_hidden, vec!["ctrl+]"]);
        assert!(!options.keys_when_hidden.iter().any(|key| key == "escape"));
        // Esc on the visible collapsed fallback row cancels (upstream parity).
        component.dispatch("\x1b");
        let done = component.done().expect("done");
        assert_eq!(done["cancelled"], true);
    }

    /// FR-Q3-F: `Ctrl+G` queues the external-editor effect with the live
    /// draft; the write-back (`InputReplace`) lands in the buffer + draft map.
    #[test]
    fn external_editor_effect_and_writeback() {
        let i18n = I18n::for_locale("en");
        let params = params(false, 1);
        let mut component = QuestionnaireComponent::new(&params, i18n, "ctrl+]".to_owned());
        component.dispatch("\x1b[B");
        component.dispatch("\x1b[B"); // input mode
        component.dispatch("draft");
        component.dispatch("\x07"); // ctrl+g
        assert_eq!(
            component.take_host_effects(),
            vec![HostEffect::OpenInputEditor {
                value: "draft".to_owned()
            }]
        );
        component.dispatch_action(
            Action::InputReplace {
                value: "edited\ntext".to_owned(),
            },
            None,
        );
        assert_eq!(component.input.text(), "edited\ntext");
        assert_eq!(
            component
                .state
                .custom_drafts_by_tab
                .get(&0)
                .map(String::as_str),
            Some("edited\ntext")
        );
    }

    /// The full run loop against a scripted host: collapse hides via
    /// `ui.setComponentHidden` with the one-time notify, the collapse key
    /// reopens, and the questionnaire still answers afterwards.
    #[test]
    fn run_loop_executes_collapse_effects_with_one_time_notify() {
        let host = tests_support::ScriptHost::new(vec![
            ("ui.mountComponent", Ok(json!({"handle": 3}))),
            (
                "ui.pollComponent",
                Ok(json!({"event": {"type": "resize", "width": 80, "height": 24}})),
            ),
            (
                "ui.pollComponent",
                Ok(json!({"event": {"type": "input", "data": "\u{1d}"}})),
            ),
            ("ui.notify", Ok(Value::Null)),
            ("ui.setComponentHidden", Ok(json!({"ok": true}))),
            ("ui.renderComponent", Ok(json!({"ok": true}))),
            (
                "ui.pollComponent",
                Ok(json!({"event": {"type": "input", "data": "\u{1d}"}})),
            ),
            ("ui.setComponentHidden", Ok(json!({"ok": true}))),
            ("ui.renderComponent", Ok(json!({"ok": true}))),
            (
                "ui.pollComponent",
                Ok(json!({"event": {"type": "input", "data": "\r"}})),
            ),
            ("ui.renderComponent", Ok(json!({"ok": true}))),
        ]);
        let params = params(false, 1);
        let config = AskUserQuestionConfig::default();
        let result =
            run(&host, &params, &I18n::for_locale("en"), &config).expect("component session");
        assert!(!result.cancelled);
        assert_eq!(result.answers[0].answer.as_deref(), Some("A"));
        let calls = host.calls();
        let notify_calls: Vec<&(String, Value)> = calls
            .iter()
            .filter(|(method, _)| method == "ui.notify")
            .collect();
        assert_eq!(notify_calls.len(), 1, "one-time guidance only");
        assert_eq!(
            notify_calls[0].1["message"],
            "ask_user_question hidden — press Ctrl+] to reopen"
        );
        assert_eq!(notify_calls[0].1["notifyType"], "info");
        let hides: Vec<bool> = calls
            .iter()
            .filter(|(method, _)| method == "ui.setComponentHidden")
            .map(|(_, args)| args["hidden"].as_bool().unwrap_or(false))
            .collect();
        assert_eq!(hides, vec![true, false]);
    }

    /// FR-Q3-F: `ui.editExternal` success writes back; `unknownMethod`
    /// (pre-C3 host) falls back to `ui.editor`; failure notifies
    /// `editor.failed` and keeps the draft; a cancel keeps the draft.
    #[test]
    fn run_loop_external_editor_success_fallback_and_failure() {
        // Success via ui.editExternal.
        let host = tests_support::ScriptHost::new(vec![
            ("ui.mountComponent", Ok(json!({"handle": 1}))),
            (
                "ui.pollComponent",
                Ok(json!({"event": {"type": "input", "data": "\u{1b}[B"}})),
            ),
            (
                "ui.pollComponent",
                Ok(json!({"event": {"type": "input", "data": "\u{1b}[B"}})),
            ),
            ("ui.renderComponent", Ok(json!({"ok": true}))),
            (
                "ui.pollComponent",
                Ok(json!({"event": {"type": "input", "data": "ab"}})),
            ),
            ("ui.renderComponent", Ok(json!({"ok": true}))),
            (
                "ui.pollComponent",
                Ok(json!({"event": {"type": "input", "data": "\u{7}"}})),
            ),
            (
                "ui.editExternal",
                Ok(json!({"text": "from external editor"})),
            ),
            ("ui.renderComponent", Ok(json!({"ok": true}))),
            (
                "ui.pollComponent",
                Ok(json!({"event": {"type": "input", "data": "\r"}})),
            ),
            ("ui.renderComponent", Ok(json!({"ok": true}))),
        ]);
        let params = params(false, 1);
        let result = run(
            &host,
            &params,
            &I18n::for_locale("en"),
            &AskUserQuestionConfig::default(),
        )
        .expect("session");
        assert_eq!(
            result.answers[0].answer.as_deref(),
            Some("from external editor")
        );
        let calls = host.calls();
        let edit_calls: Vec<&Value> = calls
            .iter()
            .filter(|(method, _)| method == "ui.editExternal")
            .map(|(_, args)| args)
            .collect();
        assert_eq!(edit_calls.len(), 1);
        assert_eq!(edit_calls[0]["text"], "ab");
        assert_eq!(edit_calls[0]["language"], "markdown");

        // unknownMethod → ui.editor fallback.
        let host = tests_support::ScriptHost::new(vec![
            ("ui.mountComponent", Ok(json!({"handle": 1}))),
            (
                "ui.pollComponent",
                Ok(json!({"event": {"type": "input", "data": "\u{1b}[B"}})),
            ),
            (
                "ui.pollComponent",
                Ok(json!({"event": {"type": "input", "data": "\u{1b}[B"}})),
            ),
            ("ui.renderComponent", Ok(json!({"ok": true}))),
            (
                "ui.pollComponent",
                Ok(json!({"event": {"type": "input", "data": "\u{7}"}})),
            ),
            (
                "ui.editExternal",
                Err(("unknownMethod", "no host call: ui.editExternal")),
            ),
            ("ui.editor", Ok(json!("fallback editor text"))),
            ("ui.renderComponent", Ok(json!({"ok": true}))),
            (
                "ui.pollComponent",
                Ok(json!({"event": {"type": "input", "data": "\r"}})),
            ),
            ("ui.renderComponent", Ok(json!({"ok": true}))),
        ]);
        let result = run(
            &host,
            &params,
            &I18n::for_locale("en"),
            &AskUserQuestionConfig::default(),
        )
        .expect("session");
        assert_eq!(
            result.answers[0].answer.as_deref(),
            Some("fallback editor text")
        );

        // Failure → notify editor.failed, draft kept.
        let host = tests_support::ScriptHost::new(vec![
            ("ui.mountComponent", Ok(json!({"handle": 1}))),
            (
                "ui.pollComponent",
                Ok(json!({"event": {"type": "input", "data": "\u{1b}[B"}})),
            ),
            (
                "ui.pollComponent",
                Ok(json!({"event": {"type": "input", "data": "\u{1b}[B"}})),
            ),
            ("ui.renderComponent", Ok(json!({"ok": true}))),
            (
                "ui.pollComponent",
                Ok(json!({"event": {"type": "input", "data": "keep"}})),
            ),
            ("ui.renderComponent", Ok(json!({"ok": true}))),
            (
                "ui.pollComponent",
                Ok(json!({"event": {"type": "input", "data": "\u{7}"}})),
            ),
            (
                "ui.editExternal",
                Err(("call", "editExternal: no external editor configured")),
            ),
            ("ui.notify", Ok(Value::Null)),
            ("ui.renderComponent", Ok(json!({"ok": true}))),
            (
                "ui.pollComponent",
                Ok(json!({"event": {"type": "input", "data": "\r"}})),
            ),
            ("ui.renderComponent", Ok(json!({"ok": true}))),
        ]);
        let result = run(
            &host,
            &params,
            &I18n::for_locale("en"),
            &AskUserQuestionConfig::default(),
        )
        .expect("session");
        assert_eq!(result.answers[0].answer.as_deref(), Some("keep"));
        let notify = host
            .calls()
            .iter()
            .find(|(method, _)| method == "ui.notify")
            .map(|(_, args)| args.clone())
            .expect("failure notify");
        assert_eq!(
            notify["message"],
            "External editor failed: editExternal: no external editor configured"
        );
        assert_eq!(notify["notifyType"], "error");
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

    /// A queued test host shared by the session-loop tests.
    mod tests_support {
        use crate::{HostCall, HostError};
        use serde_json::Value;
        use std::sync::Mutex;

        type ScriptReply = Result<Value, (&'static str, &'static str)>;

        pub struct ScriptHost {
            calls: Mutex<Vec<(String, Value)>>,
            replies: Mutex<Vec<(String, ScriptReply)>>,
        }

        impl ScriptHost {
            pub fn new(replies: Vec<(&'static str, ScriptReply)>) -> Self {
                Self {
                    calls: Mutex::new(Vec::new()),
                    replies: Mutex::new(
                        replies
                            .into_iter()
                            .map(|(m, r)| (m.to_owned(), r))
                            .collect(),
                    ),
                }
            }

            pub fn calls(&self) -> Vec<(String, Value)> {
                self.calls.lock().unwrap().clone()
            }
        }

        impl HostCall for ScriptHost {
            fn call(&self, method: &str, args: Value) -> Result<Value, HostError> {
                self.calls
                    .lock()
                    .unwrap()
                    .push((method.to_owned(), args.clone()));
                let mut queue = self.replies.lock().unwrap();
                match queue.iter().position(|(name, _)| name == method) {
                    Some(index) => queue.remove(index).1.map_err(|(kind, message)| HostError {
                        kind: kind.to_owned(),
                        message: message.to_owned(),
                    }),
                    None => Ok(Value::Null),
                }
            }
        }
    }
}
