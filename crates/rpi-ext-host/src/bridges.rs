//! `NullUiBridge` — the no-op UI bridge (T15 W4), mirroring
//! `noOpUIContext` (runner.ts:233-264) method by method.
//!
//! Used for print/json modes and as the unbound default: upstream's runner
//! defaults to `noOpUIContext` rather than throwing, so
//! [`crate::api::ExtensionContext::ui`] falls back to a shared null bridge
//! and `hasUI` is false exactly when the bound bridge is this one
//! (runner.ts:438-440 identity check → [`UiBridge::is_noop`]).
//!
//! [`NamespacedUiBridge`] (below) is the extension-scoped widget-key
//! decorator (TE11 FR-E.1).

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

/// Extension-scoped widget-key decorator (TE11 FR-E.1). The shared UI
/// bridge keys widgets globally (one `HashMap<key, …>` for all extensions),
/// so two extensions using the same `setWidget` key would remove each
/// other's entries. This wrapper prefixes every `setWidget` key with
/// `{namespace}:` — the extension's identity, injected where the context is
/// known to belong to one extension (`ExtensionApi::context()`; host-level
/// callers keep raw keys). Everything else forwards unchanged, so behavior
/// is transparent to the caller and the wrapped bridge.
pub struct NamespacedUiBridge {
    inner: Arc<dyn UiBridge>,
    namespace: String,
}

impl NamespacedUiBridge {
    pub fn new(inner: Arc<dyn UiBridge>, namespace: impl Into<String>) -> Self {
        NamespacedUiBridge {
            inner,
            namespace: namespace.into(),
        }
    }

    fn namespaced(&self, key: &str) -> String {
        format!("{}:{}", self.namespace, key)
    }
}

#[async_trait::async_trait]
impl UiBridge for NamespacedUiBridge {
    async fn select(
        &self,
        title: &str,
        options: &[String],
        opts: Option<UiDialogOptions>,
    ) -> Option<String> {
        self.inner.select(title, options, opts).await
    }

    async fn confirm(&self, title: &str, message: &str, opts: Option<UiDialogOptions>) -> bool {
        self.inner.confirm(title, message, opts).await
    }

    async fn input(
        &self,
        title: &str,
        placeholder: Option<&str>,
        opts: Option<UiDialogOptions>,
    ) -> Option<String> {
        self.inner.input(title, placeholder, opts).await
    }

    fn notify(&self, message: &str, kind: NotifyType) {
        self.inner.notify(message, kind);
    }

    fn on_terminal_input(&self, handler: TerminalInputHandler) -> Unsubscribe {
        self.inner.on_terminal_input(handler)
    }

    fn set_status(&self, key: &str, text: Option<&str>) {
        self.inner.set_status(key, text);
    }

    fn set_working_message(&self, message: Option<&str>) {
        self.inner.set_working_message(message);
    }

    fn set_working_visible(&self, visible: bool) {
        self.inner.set_working_visible(visible);
    }

    fn set_working_indicator(&self, options: Option<WorkingIndicatorOptions>) {
        self.inner.set_working_indicator(options);
    }

    fn set_hidden_thinking_label(&self, label: Option<&str>) {
        self.inner.set_hidden_thinking_label(label);
    }

    fn set_widget(
        &self,
        key: &str,
        content: Option<WidgetContent>,
        options: Option<ExtensionWidgetOptions>,
    ) {
        // The one rewritten method: namespace the key on both push and
        // remove (`None` content), so an extension can only ever address
        // its own widgets.
        self.inner
            .set_widget(&self.namespaced(key), content, options);
    }

    fn set_footer(&self, component: Option<ComponentTree>) {
        self.inner.set_footer(component);
    }

    fn set_header(&self, component: Option<ComponentTree>) {
        self.inner.set_header(component);
    }

    fn set_title(&self, title: &str) {
        self.inner.set_title(title);
    }

    async fn custom(&self, component: ComponentTree, options: Option<Value>) -> Option<Value> {
        self.inner.custom(component, options).await
    }

    fn paste_to_editor(&self, text: &str) {
        self.inner.paste_to_editor(text);
    }

    fn set_editor_text(&self, text: &str) {
        self.inner.set_editor_text(text);
    }

    fn get_editor_text(&self) -> String {
        self.inner.get_editor_text()
    }

    async fn editor(&self, title: &str, prefill: Option<&str>) -> Option<String> {
        self.inner.editor(title, prefill).await
    }

    fn add_autocomplete_provider(&self, provider: Value) {
        self.inner.add_autocomplete_provider(provider);
    }

    fn set_editor_component(&self, component: Option<ComponentTree>) {
        self.inner.set_editor_component(component);
    }

    fn get_editor_component(&self) -> Option<ComponentTree> {
        self.inner.get_editor_component()
    }

    fn theme(&self) -> Value {
        self.inner.theme()
    }

    fn get_all_themes(&self) -> Vec<ThemeInfo> {
        self.inner.get_all_themes()
    }

    fn get_theme(&self, name: &str) -> Option<Value> {
        self.inner.get_theme(name)
    }

    fn set_theme(&self, theme: Value) -> SetThemeResult {
        self.inner.set_theme(theme)
    }

    fn get_tools_expanded(&self) -> bool {
        self.inner.get_tools_expanded()
    }

    fn set_tools_expanded(&self, expanded: bool) {
        self.inner.set_tools_expanded(expanded);
    }

    fn is_noop(&self) -> bool {
        self.inner.is_noop()
    }

    fn as_any(&self) -> Option<&dyn std::any::Any> {
        // The decorator is invisible to L0 downcasts: a built-in needing the
        // concrete bridge gets it from the host level, which is never
        // namespaced.
        self.inner.as_any()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Records `setWidget` calls; everything else no-ops like the null
    /// bridge.
    struct RecordingBridge {
        calls: Mutex<Vec<(String, bool)>>,
    }

    #[async_trait::async_trait]
    impl UiBridge for RecordingBridge {
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
        fn set_widget(
            &self,
            key: &str,
            content: Option<WidgetContent>,
            _o: Option<ExtensionWidgetOptions>,
        ) {
            self.calls
                .lock()
                .unwrap()
                .push((key.to_owned(), content.is_some()));
        }
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
        fn set_tools_expanded(&self, _e: bool) {}
    }

    fn recorded(calls: &Mutex<Vec<(String, bool)>>) -> Vec<(String, bool)> {
        calls.lock().unwrap().clone()
    }

    #[test]
    fn set_widget_keys_are_namespaced_on_push_and_remove() {
        let inner = Arc::new(RecordingBridge {
            calls: Mutex::new(Vec::new()),
        });
        let namespaced =
            NamespacedUiBridge::new(Arc::clone(&inner) as Arc<dyn UiBridge>, "pi-subagents");
        namespaced.set_widget(
            "subagent-fleet-status",
            Some(WidgetContent::Lines(vec!["line".to_owned()])),
            None,
        );
        namespaced.set_widget("subagent-fleet-status", None, None);
        assert_eq!(
            recorded(&inner.calls),
            vec![
                ("pi-subagents:subagent-fleet-status".to_owned(), true),
                ("pi-subagents:subagent-fleet-status".to_owned(), false),
            ]
        );
    }
}
