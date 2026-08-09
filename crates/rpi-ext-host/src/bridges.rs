//! `NullUiBridge` — the no-op UI bridge (T15 W4), mirroring
//! `noOpUIContext` (runner.ts:233-264) method by method.
//!
//! Used for print/json modes and as the unbound default: upstream's runner
//! defaults to `noOpUIContext` rather than throwing, so
//! [`crate::api::ExtensionContext::ui`] falls back to a shared null bridge
//! and `hasUI` is false exactly when the bound bridge is this one
//! (runner.ts:438-440 identity check → [`UiBridge::is_noop`]).

use std::sync::Arc;

use serde_json::Value;

use crate::api::{ExtensionWidgetOptions, NotifyType};
use crate::api::{
    SetThemeResult, TerminalInputHandler, ThemeInfo, UiBridge, UiDialogOptions, Unsubscribe,
    WidgetContent, WorkingIndicatorOptions,
};
use crate::types::ComponentTree;

/// No-op bridge (runner.ts:233-264). `theme` returns the value given at
/// construction (upstream returns the statically imported default theme;
/// the rpi default theme JSON is injected by the caller — rpi-ext-host has
/// no theme system of its own).
pub struct NullUiBridge {
    default_theme: Value,
}

impl Default for NullUiBridge {
    fn default() -> Self {
        NullUiBridge {
            default_theme: Value::Null,
        }
    }
}

impl NullUiBridge {
    pub fn new(default_theme: Value) -> Self {
        NullUiBridge { default_theme }
    }

    /// Shared plain instance for the unbound fallback.
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::default())
    }
}

#[async_trait::async_trait]
impl UiBridge for NullUiBridge {
    async fn select(
        &self,
        _t: &str,
        _o: &[String],
        _opts: Option<UiDialogOptions>,
    ) -> Option<String> {
        None
    }

    async fn confirm(&self, _t: &str, _m: &str, _opts: Option<UiDialogOptions>) -> bool {
        false
    }

    async fn input(
        &self,
        _t: &str,
        _p: Option<&str>,
        _opts: Option<UiDialogOptions>,
    ) -> Option<String> {
        None
    }

    fn notify(&self, _m: &str, _k: NotifyType) {}

    fn on_terminal_input(&self, _h: TerminalInputHandler) -> Unsubscribe {
        Box::new(|| {})
    }

    fn set_status(&self, _k: &str, _t: Option<&str>) {}
    fn set_working_message(&self, _m: Option<&str>) {}
    fn set_working_visible(&self, _v: bool) {}
    fn set_working_indicator(&self, _o: Option<WorkingIndicatorOptions>) {}
    fn set_hidden_thinking_label(&self, _l: Option<&str>) {}

    fn set_widget(&self, _k: &str, _c: Option<WidgetContent>, _o: Option<ExtensionWidgetOptions>) {}

    fn set_footer(&self, _c: Option<ComponentTree>) {}
    fn set_header(&self, _c: Option<ComponentTree>) {}
    fn set_title(&self, _t: &str) {}

    async fn custom(&self, _c: ComponentTree, _o: Option<Value>) -> Option<Value> {
        None
    }

    fn paste_to_editor(&self, _t: &str) {}
    fn set_editor_text(&self, _t: &str) {}

    fn get_editor_text(&self) -> String {
        String::new()
    }

    async fn editor(&self, _t: &str, _p: Option<&str>) -> Option<String> {
        None
    }

    fn add_autocomplete_provider(&self, _p: Value) {}
    fn set_editor_component(&self, _c: Option<ComponentTree>) {}

    fn get_editor_component(&self) -> Option<ComponentTree> {
        None
    }

    fn theme(&self) -> Value {
        // noOpUIContext returns the default theme (runner.ts:256-258).
        self.default_theme.clone()
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
            error: Some("UI not available".to_owned()),
        }
    }

    fn get_tools_expanded(&self) -> bool {
        false
    }

    fn set_tools_expanded(&self, _e: bool) {}

    fn is_noop(&self) -> bool {
        true
    }
}
