//! Scripted interactive-UI host for the V14-22 dual-carrier parity harness.
//!
//! Both fixture guests (native `rpi-test-native-plugin`, wasm
//! `examples/wasm-extension`) run the same scripted component against this
//! deterministic `UiBridge`: the corpus supplies the ordered host events
//! (`ComponentEvent`), `renderComponent` submissions are recorded as
//! `{lines,cursor,done}` frames, and the terminal `done` value is captured.
//! Running the identical corpus through both carriers and diffing the
//! recorded frames byte-for-byte is the R-U7.4 dual-carrier consistency
//! check (G11 item 2).
//!
//! The bridge deliberately keeps the real registry's *observable* protocol
//! semantics (single active slot, `unknownHandle` on a closed slot,
//! visibility echo, `render` wake event) without the TUI; host-side
//! registry behaviour (ticks, limits, focus routing) is covered by the
//! `rpi` crate tests instead.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rpi_ext_host::api::{
    ExtensionWidgetOptions, NotifyType, SetThemeResult, TerminalInputHandler, ThemeInfo, UiBridge,
    UiDialogOptions, Unsubscribe, WidgetContent, WorkingIndicatorOptions,
};
use rpi_ext_host::interactive_ui::{
    ComponentCursor, ComponentEvent, ComponentFrame, ComponentHandle, DisposeReason,
    InteractiveUiError, InteractiveUiErrorKind, MountOptions,
};
use rpi_ext_host::types::ComponentTree;
use serde_json::{json, Value};

/// One recorded `renderComponent` submission.
#[derive(Clone, Debug, PartialEq)]
pub struct FrameRecord {
    /// Frame lines (ANSI verbatim, `CURSOR_MARKER` included — the parity
    /// golden is the guest output, before host compositing).
    pub lines: Vec<String>,
    /// Explicit cursor field, when the guest sent one.
    pub cursor: Option<ComponentCursor>,
    /// `done` payload when the frame terminated the component.
    pub done: Option<Value>,
}

impl FrameRecord {
    /// The `{lines,cursor?,done?}` JSON shape of the golden corpus.
    pub fn to_json(&self) -> Value {
        let mut value = json!({ "lines": self.lines });
        if let Some(cursor) = self.cursor {
            value["cursor"] = json!({ "row": cursor.row, "col": cursor.col });
        }
        if let Some(done) = &self.done {
            value["done"] = done.clone();
        }
        value
    }
}

/// Guest transcript captured by the scripted host.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Transcript {
    /// Every submitted frame, in order.
    pub frames: Vec<FrameRecord>,
    /// The value of the first frame carrying `done`.
    pub terminal: Option<Value>,
    /// Mount options the guest asked for (wire JSON) — parity evidence.
    pub mount_options: Option<Value>,
    /// Whether the script ran out before the guest finished (harness failure).
    pub script_exhausted: bool,
}

impl Transcript {
    /// Frames as JSONL (one `{lines,cursor?,done?}` per line).
    pub fn frames_jsonl(&self) -> String {
        let mut out = String::new();
        for frame in &self.frames {
            out.push_str(&serde_json::to_string(&frame.to_json()).expect("frame json"));
            out.push('\n');
        }
        out
    }
}

struct BridgeState {
    next_handle: u64,
    active: Option<u64>,
    script: VecDeque<ComponentEvent>,
    transcript: Transcript,
}

/// The scripted `UiBridge` (see module docs).
pub struct ScriptedUiBridge {
    state: Mutex<BridgeState>,
}

impl ScriptedUiBridge {
    /// Build a bridge serving `script` events in order.
    pub fn new(script: Vec<ComponentEvent>) -> Self {
        Self {
            state: Mutex::new(BridgeState {
                next_handle: 1,
                active: None,
                script: script.into(),
                transcript: Transcript::default(),
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BridgeState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Transcript snapshot (frames/terminal/mount options).
    pub fn transcript(&self) -> Transcript {
        self.lock().transcript.clone()
    }
}

fn unknown_handle(handle: ComponentHandle) -> InteractiveUiError {
    InteractiveUiError::unknown_handle(handle)
}

#[async_trait]
impl UiBridge for ScriptedUiBridge {
    async fn select(
        &self,
        _title: &str,
        _options: &[String],
        _opts: Option<UiDialogOptions>,
    ) -> Option<String> {
        None
    }

    async fn confirm(&self, _title: &str, _message: &str, _opts: Option<UiDialogOptions>) -> bool {
        false
    }

    async fn input(
        &self,
        _title: &str,
        _placeholder: Option<&str>,
        _opts: Option<UiDialogOptions>,
    ) -> Option<String> {
        None
    }

    fn notify(&self, _message: &str, _kind: NotifyType) {}

    fn on_terminal_input(&self, _handler: TerminalInputHandler) -> Unsubscribe {
        Box::new(|| {})
    }

    fn set_status(&self, _key: &str, _text: Option<&str>) {}

    fn set_working_message(&self, _message: Option<&str>) {}

    fn set_working_visible(&self, _visible: bool) {}

    fn set_working_indicator(&self, _options: Option<WorkingIndicatorOptions>) {}

    fn set_hidden_thinking_label(&self, _label: Option<&str>) {}

    fn set_widget(
        &self,
        _key: &str,
        _content: Option<WidgetContent>,
        _options: Option<ExtensionWidgetOptions>,
    ) {
    }

    fn set_footer(&self, _component: Option<ComponentTree>) {}

    fn set_header(&self, _component: Option<ComponentTree>) {}

    fn set_title(&self, _title: &str) {}

    async fn custom(&self, _component: ComponentTree, _options: Option<Value>) -> Option<Value> {
        None
    }

    fn paste_to_editor(&self, _text: &str) {}

    fn set_editor_text(&self, _text: &str) {}

    fn get_editor_text(&self) -> String {
        String::new()
    }

    async fn editor(&self, _title: &str, _prefill: Option<&str>) -> Option<String> {
        None
    }

    fn add_autocomplete_provider(&self, _provider: Value) {}

    fn set_editor_component(&self, _component: Option<ComponentTree>) {}

    fn get_editor_component(&self) -> Option<ComponentTree> {
        None
    }

    fn theme(&self) -> Value {
        json!({ "name": "dark" })
    }

    fn get_all_themes(&self) -> Vec<ThemeInfo> {
        Vec::new()
    }

    fn get_theme(&self, _name: &str) -> Option<Value> {
        None
    }

    fn set_theme(&self, _theme: Value) -> SetThemeResult {
        SetThemeResult {
            success: false,
            error: None,
        }
    }

    fn get_tools_expanded(&self) -> bool {
        false
    }

    fn set_tools_expanded(&self, _expanded: bool) {}

    fn supports_interactive_ui(&self) -> bool {
        true
    }

    async fn mount_component(
        &self,
        _owner: &str,
        options: MountOptions,
    ) -> Result<ComponentHandle, InteractiveUiError> {
        let mut state = self.lock();
        if state.active.is_some() {
            return Err(InteractiveUiError::component_already_mounted());
        }
        let handle = ComponentHandle(state.next_handle);
        state.next_handle += 1;
        state.active = Some(handle.0);
        state.transcript.mount_options = Some(
            serde_json::to_value(&options)
                .map_err(|error| InteractiveUiError::protocol(format!("mountOptions: {error}")))?,
        );
        Ok(handle)
    }

    async fn poll_component(
        &self,
        _owner: &str,
        handle: ComponentHandle,
    ) -> Result<ComponentEvent, InteractiveUiError> {
        let mut state = self.lock();
        if state.active != Some(handle.0) {
            return Err(unknown_handle(handle));
        }
        match state.script.pop_front() {
            Some(event) => Ok(event),
            None => {
                state.transcript.script_exhausted = true;
                Err(InteractiveUiError::new(
                    InteractiveUiErrorKind::Internal,
                    "script exhausted: corpus must end with a terminating input",
                ))
            }
        }
    }

    fn render_component(
        &self,
        _owner: &str,
        handle: ComponentHandle,
        frame: ComponentFrame,
    ) -> Result<(), InteractiveUiError> {
        let mut state = self.lock();
        if state.active != Some(handle.0) {
            return Err(unknown_handle(handle));
        }
        let done = frame.done.value().cloned();
        let record = FrameRecord {
            lines: frame.lines.clone(),
            cursor: frame.cursor,
            done: done.clone(),
        };
        state.transcript.frames.push(record);
        if let Some(done) = done {
            if state.transcript.terminal.is_none() {
                state.transcript.terminal = Some(done);
            }
            // `done` closes the slot (R-U1.4): later calls are unknownHandle.
            state.active = None;
        }
        Ok(())
    }

    fn set_component_hidden(
        &self,
        _owner: &str,
        handle: ComponentHandle,
        hidden: bool,
    ) -> Result<(), InteractiveUiError> {
        let mut state = self.lock();
        if state.active != Some(handle.0) {
            return Err(unknown_handle(handle));
        }
        // R-U4.4 echo; status-class merge keeps the newest pending value.
        if let Some(ComponentEvent::Visibility { hidden: pending }) = state.script.back_mut() {
            *pending = hidden;
        } else {
            state
                .script
                .push_back(ComponentEvent::Visibility { hidden });
        }
        Ok(())
    }

    fn wake_component(
        &self,
        _owner: &str,
        handle: ComponentHandle,
    ) -> Result<(), InteractiveUiError> {
        let mut state = self.lock();
        if state.active != Some(handle.0) {
            return Err(unknown_handle(handle));
        }
        state.script.push_back(ComponentEvent::Render);
        Ok(())
    }

    fn dispose_component(
        &self,
        _owner: &str,
        handle: ComponentHandle,
    ) -> Result<(), InteractiveUiError> {
        let mut state = self.lock();
        if state.active != Some(handle.0) {
            return Err(unknown_handle(handle));
        }
        state.active = None;
        Ok(())
    }

    async fn edit_external(
        &self,
        _owner: &str,
        _text: &str,
        _language: Option<&str>,
    ) -> Result<Option<String>, InteractiveUiError> {
        Err(InteractiveUiError::new(
            InteractiveUiErrorKind::UnknownMethod,
            "ui.editExternal: unsupported by this scripted host",
        ))
    }

    fn abort_active_component(
        &self,
        _owner: &str,
        _reason: DisposeReason,
    ) -> Option<ComponentHandle> {
        let mut state = self.lock();
        state.active.take().map(ComponentHandle)
    }

    fn begin_forced_dispose(
        &self,
        _owner: Option<&str>,
        _reason: DisposeReason,
    ) -> Option<ComponentHandle> {
        // Host-forced dispose (V14-23 C3): the scripted host has no grace
        // machinery — model it as the force-close the grace would end on.
        let mut state = self.lock();
        state.active.take().map(ComponentHandle)
    }

    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }
}

/// Convenience alias for `Arc`-shared use.
pub type SharedScriptedUiBridge = Arc<ScriptedUiBridge>;
