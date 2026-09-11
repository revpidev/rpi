//! Shared **test-only** `UiBridge` double (V14-22 C2).
//!
//! `wasm.rs` carrier-failure tests need a bridge that (a) claims interactive
//! UI support so the dispatch path reaches the component methods and (b)
//! records [`UiBridge::abort_active_component`] calls. The production
//! registry lives in the `rpi` crate (which depends on this crate), so the
//! host tests observe the cleanup through this recording seam.
//!
//! Compiled only under `cfg(test)`; never part of the shipped surface.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::Value;

use crate::api::{
    ExtensionWidgetOptions, NotifyType, SetThemeResult, TerminalInputHandler, ThemeInfo, UiBridge,
    UiDialogOptions, Unsubscribe, WidgetContent, WorkingIndicatorOptions,
};
use crate::interactive_ui::{
    ComponentEvent, ComponentFrame, ComponentHandle, DisposeReason, InteractiveUiError,
    InteractiveUiErrorKind, MountOptions,
};
use crate::types::ComponentTree;

/// No-op bridge with an `abort_active_component` log.
pub(crate) struct TestUiBridge {
    aborts: Mutex<Vec<(String, DisposeReason)>>,
    supports: bool,
    interactive_calls: Mutex<Vec<String>>,
}

impl TestUiBridge {
    pub(crate) fn new() -> Self {
        Self {
            aborts: Mutex::new(Vec::new()),
            supports: true,
            interactive_calls: Mutex::new(Vec::new()),
        }
    }

    /// A bridge that reports no interactive UI support (mode fallback tests).
    #[allow(dead_code)]
    pub(crate) fn unsupported() -> Self {
        Self {
            supports: false,
            ..Self::new()
        }
    }

    pub(crate) fn aborts(&self) -> Vec<(String, DisposeReason)> {
        self.aborts.lock().unwrap().clone()
    }

    /// Interactive method names observed in order (mount/poll/render/...).
    #[allow(dead_code)]
    pub(crate) fn interactive_calls(&self) -> Vec<String> {
        self.interactive_calls.lock().unwrap().clone()
    }

    fn record(&self, method: &str) {
        self.interactive_calls
            .lock()
            .unwrap()
            .push(method.to_owned());
    }
}

impl Default for TestUiBridge {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl UiBridge for TestUiBridge {
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
        Value::Null
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
        self.supports
    }

    async fn mount_component(
        &self,
        _owner: &str,
        _options: MountOptions,
    ) -> Result<ComponentHandle, InteractiveUiError> {
        self.record("mount");
        Ok(ComponentHandle(1))
    }

    async fn poll_component(
        &self,
        _owner: &str,
        _handle: ComponentHandle,
    ) -> Result<ComponentEvent, InteractiveUiError> {
        self.record("poll");
        Err(InteractiveUiError::new(
            InteractiveUiErrorKind::Internal,
            "test bridge: no events",
        ))
    }

    fn render_component(
        &self,
        _owner: &str,
        _handle: ComponentHandle,
        _frame: ComponentFrame,
    ) -> Result<(), InteractiveUiError> {
        self.record("render");
        Ok(())
    }

    fn set_component_hidden(
        &self,
        _owner: &str,
        _handle: ComponentHandle,
        _hidden: bool,
    ) -> Result<(), InteractiveUiError> {
        self.record("hidden");
        Ok(())
    }

    fn wake_component(
        &self,
        _owner: &str,
        _handle: ComponentHandle,
    ) -> Result<(), InteractiveUiError> {
        self.record("wake");
        Ok(())
    }

    fn dispose_component(
        &self,
        _owner: &str,
        _handle: ComponentHandle,
    ) -> Result<(), InteractiveUiError> {
        self.record("dispose");
        Ok(())
    }

    async fn edit_external(
        &self,
        _owner: &str,
        _text: &str,
        _language: Option<&str>,
    ) -> Result<Option<String>, InteractiveUiError> {
        self.record("editExternal");
        Err(InteractiveUiError::new(
            InteractiveUiErrorKind::UnknownMethod,
            "test bridge: editExternal",
        ))
    }

    fn abort_active_component(
        &self,
        owner: &str,
        reason: DisposeReason,
    ) -> Option<ComponentHandle> {
        self.record("abort");
        self.aborts.lock().unwrap().push((owner.to_owned(), reason));
        None
    }

    fn begin_forced_dispose(
        &self,
        owner: Option<&str>,
        reason: DisposeReason,
    ) -> Option<ComponentHandle> {
        self.record("forcedDispose");
        self.aborts
            .lock()
            .unwrap()
            .push((owner.unwrap_or("<any>").to_owned(), reason));
        None
    }

    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }
}

/// Convenience alias for shared ownership in tests.
#[allow(dead_code)]
pub(crate) type SharedTestUiBridge = Arc<TestUiBridge>;
