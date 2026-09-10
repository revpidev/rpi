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
//! decorator (TE11 FR-E.1), and [`UiPromptBridge`] is the `ui_prompt_*`
//! event decorator (V14-11 FR-C, `ccfe79ed2`).

use std::sync::atomic::{AtomicU32, Ordering};
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

    // Interactive custom UI ABI (ADR-0024): the wrapper IS the per-extension
    // identity, so it stamps every call with its own namespace (an extension
    // cannot address another extension's component slot, R-U1.6/R-U8.2).

    fn supports_interactive_ui(&self) -> bool {
        self.inner.supports_interactive_ui()
    }

    async fn mount_component(
        &self,
        _owner: &str,
        options: crate::interactive_ui::MountOptions,
    ) -> Result<crate::interactive_ui::ComponentHandle, crate::interactive_ui::InteractiveUiError>
    {
        self.inner.mount_component(&self.namespace, options).await
    }

    async fn poll_component(
        &self,
        _owner: &str,
        handle: crate::interactive_ui::ComponentHandle,
    ) -> Result<crate::interactive_ui::ComponentEvent, crate::interactive_ui::InteractiveUiError>
    {
        self.inner.poll_component(&self.namespace, handle).await
    }

    fn render_component(
        &self,
        _owner: &str,
        handle: crate::interactive_ui::ComponentHandle,
        frame: crate::interactive_ui::ComponentFrame,
    ) -> Result<(), crate::interactive_ui::InteractiveUiError> {
        self.inner.render_component(&self.namespace, handle, frame)
    }

    fn set_component_hidden(
        &self,
        _owner: &str,
        handle: crate::interactive_ui::ComponentHandle,
        hidden: bool,
    ) -> Result<(), crate::interactive_ui::InteractiveUiError> {
        self.inner
            .set_component_hidden(&self.namespace, handle, hidden)
    }

    fn wake_component(
        &self,
        _owner: &str,
        handle: crate::interactive_ui::ComponentHandle,
    ) -> Result<(), crate::interactive_ui::InteractiveUiError> {
        self.inner.wake_component(&self.namespace, handle)
    }

    fn dispose_component(
        &self,
        _owner: &str,
        handle: crate::interactive_ui::ComponentHandle,
    ) -> Result<(), crate::interactive_ui::InteractiveUiError> {
        self.inner.dispose_component(&self.namespace, handle)
    }

    async fn edit_external(
        &self,
        _owner: &str,
        text: &str,
        language: Option<&str>,
    ) -> Result<Option<String>, crate::interactive_ui::InteractiveUiError> {
        self.inner
            .edit_external(&self.namespace, text, language)
            .await
    }

    fn abort_active_component(
        &self,
        _owner: &str,
        reason: crate::interactive_ui::DisposeReason,
    ) -> Option<crate::interactive_ui::ComponentHandle> {
        self.inner.abort_active_component(&self.namespace, reason)
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

// ============================================================================
// `ui_prompt_*` decorator (V14-11 FR-C, runner.ts:438-486 @ ccfe79ed2)
// ============================================================================

/// `UIPromptKind` (types.ts:741).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiPromptKind {
    Select,
    Confirm,
    Input,
    Editor,
    Custom,
}

impl UiPromptKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            UiPromptKind::Select => "select",
            UiPromptKind::Confirm => "confirm",
            UiPromptKind::Input => "input",
            UiPromptKind::Editor => "editor",
            UiPromptKind::Custom => "custom",
        }
    }
}

/// Fire-and-forget `ui_prompt_*` dispatch (`emitUIPromptEvent`,
/// runner.ts:482-486: `queueMicrotask(() => void this.emit(event))`). The
/// sink must never block the prompt flow; handler errors are contained by
/// the runner's serial `emit` path.
pub type UiPromptEventSink = Arc<dyn Fn(&str, Value) + Send + Sync>;

/// `wrapUIPromptContext` / `withUIPrompt` (runner.ts:438-486): the five
/// dialog methods get a depth counter — only the OUTERMOST prompt emits
/// `ui_prompt_start`/`ui_prompt_end`, and a nested span merges into it
/// (the outer kind/title is kept for both events). The prompt's resolve
/// never waits on the event handlers (fire-and-forget via the sink).
pub struct UiPromptBridge {
    inner: Arc<dyn UiBridge>,
    /// `uiPromptDepth` (runner.ts:456).
    depth: AtomicU32,
    /// `activeUIPrompt` (runner.ts:463): the outermost span's kind+title,
    /// reused for `ui_prompt_end`.
    active: std::sync::RwLock<Option<(UiPromptKind, Option<String>)>>,
    sink: UiPromptEventSink,
}

impl UiPromptBridge {
    pub fn new(inner: Arc<dyn UiBridge>, sink: UiPromptEventSink) -> Self {
        UiPromptBridge {
            inner,
            depth: AtomicU32::new(0),
            active: std::sync::RwLock::new(None),
            sink,
        }
    }

    fn enter(&self, kind: UiPromptKind, title: Option<&str>) -> UiPromptGuard<'_> {
        let outer = self.depth.fetch_add(1, Ordering::SeqCst) == 0;
        if outer {
            *self.active.write().unwrap_or_else(|e| e.into_inner()) =
                Some((kind, title.map(str::to_owned)));
            self.emit_event("ui_prompt_start", kind, title);
        }
        UiPromptGuard {
            depth: &self.depth,
            active: &self.active,
            sink: &self.sink,
            kind,
            title: title.map(str::to_owned),
            outer,
        }
    }

    /// `emitUIPromptEvent` payload (types.ts:745-760): `{type, reason,
    /// kind, title?}` — `title` omitted when absent (upstream spreads only
    /// truthy titles).
    fn emit_event(&self, type_tag: &str, kind: UiPromptKind, title: Option<&str>) {
        let mut payload = serde_json::json!({
            "type": type_tag,
            "reason": "ui_prompt",
            "kind": kind.as_str(),
        });
        if let Some(title) = title {
            payload["title"] = serde_json::Value::String(title.to_owned());
        }
        (self.sink)(type_tag, payload);
    }

    async fn with_ui_prompt<T>(
        &self,
        kind: UiPromptKind,
        title: Option<&str>,
        run: impl std::future::Future<Output = T>,
    ) -> T {
        let guard = self.enter(kind, title);
        let output = run.await;
        drop(guard);
        output
    }
}

/// The `finally` half of `withUIPrompt` (runner.ts:468-479): decrement on
/// drop so an aborted/panicking prompt still closes its span. Only the
/// guard of the outermost span emits `ui_prompt_end`, with the span's
/// recorded kind/title (falling back to the closing prompt's own).
struct UiPromptGuard<'a> {
    depth: &'a AtomicU32,
    active: &'a std::sync::RwLock<Option<(UiPromptKind, Option<String>)>>,
    sink: &'a UiPromptEventSink,
    kind: UiPromptKind,
    title: Option<String>,
    outer: bool,
}

impl Drop for UiPromptGuard<'_> {
    fn drop(&mut self) {
        if !self.outer {
            return;
        }
        let depth = self.depth.fetch_sub(1, Ordering::SeqCst);
        if depth > 1 {
            // Clamp like upstream (`this.uiPromptDepth = 0`).
            self.depth.store(0, Ordering::SeqCst);
        }
        let (kind, title) = self
            .active
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .unwrap_or((self.kind, self.title.clone()));
        let mut payload = serde_json::json!({
            "type": "ui_prompt_end",
            "reason": "ui_prompt",
            "kind": kind.as_str(),
        });
        if let Some(title) = title {
            payload["title"] = serde_json::Value::String(title);
        }
        (self.sink)("ui_prompt_end", payload);
    }
}

#[async_trait::async_trait]
impl UiBridge for UiPromptBridge {
    async fn select(
        &self,
        title: &str,
        options: &[String],
        opts: Option<UiDialogOptions>,
    ) -> Option<String> {
        self.with_ui_prompt(
            UiPromptKind::Select,
            Some(title),
            self.inner.select(title, options, opts),
        )
        .await
    }

    async fn confirm(&self, title: &str, message: &str, opts: Option<UiDialogOptions>) -> bool {
        self.with_ui_prompt(
            UiPromptKind::Confirm,
            Some(title),
            self.inner.confirm(title, message, opts),
        )
        .await
    }

    async fn input(
        &self,
        title: &str,
        placeholder: Option<&str>,
        opts: Option<UiDialogOptions>,
    ) -> Option<String> {
        self.with_ui_prompt(
            UiPromptKind::Input,
            Some(title),
            self.inner.input(title, placeholder, opts),
        )
        .await
    }

    async fn custom(&self, component: ComponentTree, options: Option<Value>) -> Option<Value> {
        // `custom` has no title (runner.ts:443 passes undefined).
        self.with_ui_prompt(
            UiPromptKind::Custom,
            None,
            self.inner.custom(component, options),
        )
        .await
    }

    async fn editor(&self, title: &str, prefill: Option<&str>) -> Option<String> {
        self.with_ui_prompt(
            UiPromptKind::Editor,
            Some(title),
            self.inner.editor(title, prefill),
        )
        .await
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
        self.inner.set_widget(key, content, options);
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

    fn paste_to_editor(&self, text: &str) {
        self.inner.paste_to_editor(text);
    }

    fn set_editor_text(&self, text: &str) {
        self.inner.set_editor_text(text);
    }

    fn get_editor_text(&self) -> String {
        self.inner.get_editor_text()
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

    // Interactive custom UI ABI (ADR-0024) forwards unchanged: these are not
    // prompts, so they must not open a `ui_prompt_*` span.

    fn supports_interactive_ui(&self) -> bool {
        self.inner.supports_interactive_ui()
    }

    async fn mount_component(
        &self,
        owner: &str,
        options: crate::interactive_ui::MountOptions,
    ) -> Result<crate::interactive_ui::ComponentHandle, crate::interactive_ui::InteractiveUiError>
    {
        self.inner.mount_component(owner, options).await
    }

    async fn poll_component(
        &self,
        owner: &str,
        handle: crate::interactive_ui::ComponentHandle,
    ) -> Result<crate::interactive_ui::ComponentEvent, crate::interactive_ui::InteractiveUiError>
    {
        self.inner.poll_component(owner, handle).await
    }

    fn render_component(
        &self,
        owner: &str,
        handle: crate::interactive_ui::ComponentHandle,
        frame: crate::interactive_ui::ComponentFrame,
    ) -> Result<(), crate::interactive_ui::InteractiveUiError> {
        self.inner.render_component(owner, handle, frame)
    }

    fn set_component_hidden(
        &self,
        owner: &str,
        handle: crate::interactive_ui::ComponentHandle,
        hidden: bool,
    ) -> Result<(), crate::interactive_ui::InteractiveUiError> {
        self.inner.set_component_hidden(owner, handle, hidden)
    }

    fn wake_component(
        &self,
        owner: &str,
        handle: crate::interactive_ui::ComponentHandle,
    ) -> Result<(), crate::interactive_ui::InteractiveUiError> {
        self.inner.wake_component(owner, handle)
    }

    fn dispose_component(
        &self,
        owner: &str,
        handle: crate::interactive_ui::ComponentHandle,
    ) -> Result<(), crate::interactive_ui::InteractiveUiError> {
        self.inner.dispose_component(owner, handle)
    }

    async fn edit_external(
        &self,
        owner: &str,
        text: &str,
        language: Option<&str>,
    ) -> Result<Option<String>, crate::interactive_ui::InteractiveUiError> {
        self.inner.edit_external(owner, text, language).await
    }

    fn abort_active_component(
        &self,
        owner: &str,
        reason: crate::interactive_ui::DisposeReason,
    ) -> Option<crate::interactive_ui::ComponentHandle> {
        self.inner.abort_active_component(owner, reason)
    }

    fn is_noop(&self) -> bool {
        self.inner.is_noop()
    }

    fn as_any(&self) -> Option<&dyn std::any::Any> {
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

    // ------------------------------------------------------------------
    // V14-11 FR-C: `UiPromptBridge` (runner.ts:438-486 @ ccfe79ed2)
    // ------------------------------------------------------------------

    /// Records sink events; dialog methods return immediately.
    struct DialogBridge {
        events: Mutex<Vec<(String, Value)>>,
    }

    #[async_trait::async_trait]
    impl UiBridge for DialogBridge {
        async fn select(
            &self,
            _t: &str,
            _o: &[String],
            _opts: Option<UiDialogOptions>,
        ) -> Option<String> {
            self.events
                .lock()
                .unwrap()
                .push(("dialog".to_owned(), Value::Null));
            None
        }
        async fn confirm(&self, _t: &str, _m: &str, _opts: Option<UiDialogOptions>) -> bool {
            self.events
                .lock()
                .unwrap()
                .push(("dialog".to_owned(), Value::Null));
            true
        }
        async fn input(
            &self,
            _t: &str,
            _p: Option<&str>,
            _opts: Option<UiDialogOptions>,
        ) -> Option<String> {
            self.events
                .lock()
                .unwrap()
                .push(("dialog".to_owned(), Value::Null));
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
            _k: &str,
            _c: Option<WidgetContent>,
            _o: Option<ExtensionWidgetOptions>,
        ) {
        }
        fn set_footer(&self, _c: Option<ComponentTree>) {}
        fn set_header(&self, _c: Option<ComponentTree>) {}
        fn set_title(&self, _t: &str) {}
        async fn custom(&self, _c: ComponentTree, _o: Option<Value>) -> Option<Value> {
            self.events
                .lock()
                .unwrap()
                .push(("dialog".to_owned(), Value::Null));
            None
        }
        fn paste_to_editor(&self, _t: &str) {}
        fn set_editor_text(&self, _t: &str) {}
        fn get_editor_text(&self) -> String {
            String::new()
        }
        async fn editor(&self, _t: &str, _p: Option<&str>) -> Option<String> {
            self.events
                .lock()
                .unwrap()
                .push(("dialog".to_owned(), Value::Null));
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

    fn prompt_bridge(
        sink_events: Arc<Mutex<Vec<(String, Value)>>>,
    ) -> (Arc<UiPromptBridge>, Arc<DialogBridge>) {
        let inner = Arc::new(DialogBridge {
            events: Mutex::new(Vec::new()),
        });
        let sink: UiPromptEventSink = Arc::new(move |event, payload| {
            sink_events
                .lock()
                .unwrap()
                .push((event.to_owned(), payload));
        });
        (
            Arc::new(UiPromptBridge::new(
                Arc::clone(&inner) as Arc<dyn UiBridge>,
                sink,
            )),
            inner,
        )
    }

    #[tokio::test]
    async fn ui_prompt_dialog_emits_start_and_end_pair() {
        let events: Arc<Mutex<Vec<(String, Value)>>> = Arc::new(Mutex::new(Vec::new()));
        let (bridge, inner) = prompt_bridge(events.clone());
        let confirmed = bridge.confirm("Proceed?", "continue?", None).await;
        assert!(confirmed, "dialog result forwards");
        // Exactly one pair; kind + title from the dialog.
        let events = events.lock().unwrap().clone();
        assert_eq!(events.len(), 2, "{events:?}");
        assert_eq!(events[0].0, "ui_prompt_start");
        assert_eq!(events[0].1["kind"], "confirm");
        assert_eq!(events[0].1["title"], "Proceed?");
        assert_eq!(events[0].1["reason"], "ui_prompt");
        assert_eq!(events[1].0, "ui_prompt_end");
        assert_eq!(events[1].1["kind"], "confirm");
        // The dialog itself ran exactly once.
        assert_eq!(recorded_dialogs(&inner), 1);
    }

    #[tokio::test]
    async fn ui_prompt_nested_spans_merge_into_outer_pair() {
        let events: Arc<Mutex<Vec<(String, Value)>>> = Arc::new(Mutex::new(Vec::new()));
        let events_sink = events.clone();
        let sink: UiPromptEventSink = Arc::new(move |event, payload| {
            events_sink
                .lock()
                .unwrap()
                .push((event.to_owned(), payload));
        });
        // An inner bridge whose select() opens a NESTED confirm through the
        // same decorator (an extension handler prompting again).
        struct NestingBridge {
            decorator: Mutex<Option<std::sync::Weak<UiPromptBridge>>>,
            dialogs: Mutex<Vec<&'static str>>,
        }
        #[async_trait::async_trait]
        impl UiBridge for NestingBridge {
            async fn select(
                &self,
                _t: &str,
                _o: &[String],
                _opts: Option<UiDialogOptions>,
            ) -> Option<String> {
                self.dialogs.lock().unwrap().push("select");
                let decorator = self
                    .decorator
                    .lock()
                    .unwrap()
                    .clone()
                    .and_then(|weak| weak.upgrade());
                if let Some(decorator) = decorator {
                    let _ = decorator.confirm("Nested?", "really", None).await;
                }
                Some("a".to_owned())
            }
            async fn confirm(&self, _t: &str, _m: &str, _opts: Option<UiDialogOptions>) -> bool {
                self.dialogs.lock().unwrap().push("confirm");
                true
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
                _k: &str,
                _c: Option<WidgetContent>,
                _o: Option<ExtensionWidgetOptions>,
            ) {
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
        let inner = Arc::new(NestingBridge {
            decorator: Mutex::new(None),
            dialogs: Mutex::new(Vec::new()),
        });
        let bridge = Arc::new(UiPromptBridge::new(
            Arc::clone(&inner) as Arc<dyn UiBridge>,
            sink,
        ));
        *inner.decorator.lock().unwrap() = Some(Arc::downgrade(&bridge));

        let picked = bridge
            .select("Pick one", &["a".to_owned(), "b".to_owned()], None)
            .await;
        assert_eq!(picked, Some("a".to_owned()));
        // Both dialogs ran (outer select + nested confirm).
        assert_eq!(*inner.dialogs.lock().unwrap(), vec!["select", "confirm"]);
        // Exactly ONE start/end pair — the nested span merged; kind and
        // title come from the OUTERMOST prompt (runner.ts:462-475).
        let events = events.lock().unwrap().clone();
        assert_eq!(events.len(), 2, "{events:?}");
        assert_eq!(events[0].0, "ui_prompt_start");
        assert_eq!(events[0].1["kind"], "select");
        assert_eq!(events[0].1["title"], "Pick one");
        assert_eq!(events[1].0, "ui_prompt_end");
        assert_eq!(events[1].1["kind"], "select");
        assert_eq!(events[1].1["title"], "Pick one");
    }

    #[tokio::test]
    async fn ui_prompt_panicking_dialog_still_closes_span() {
        let events: Arc<Mutex<Vec<(String, Value)>>> = Arc::new(Mutex::new(Vec::new()));
        let events_sink = events.clone();
        let sink: UiPromptEventSink = Arc::new(move |event, payload| {
            events_sink
                .lock()
                .unwrap()
                .push((event.to_owned(), payload));
        });
        struct PanicBridge;
        #[async_trait::async_trait]
        impl UiBridge for PanicBridge {
            async fn select(
                &self,
                _t: &str,
                _o: &[String],
                _opts: Option<UiDialogOptions>,
            ) -> Option<String> {
                panic!("dialog exploded");
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
                _k: &str,
                _c: Option<WidgetContent>,
                _o: Option<ExtensionWidgetOptions>,
            ) {
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
        let bridge = UiPromptBridge::new(Arc::new(PanicBridge), sink);
        let result = tokio::task::spawn(async move { bridge.select("t", &[], None).await }).await;
        assert!(result.is_err(), "panic propagates");
        // try/finally semantics: the span still closed.
        let events = events.lock().unwrap().clone();
        assert_eq!(events.len(), 2, "{events:?}");
        assert_eq!(events[1].0, "ui_prompt_end");
        assert_eq!(events[1].1["kind"], "select");
    }

    fn recorded_dialogs(inner: &DialogBridge) -> usize {
        inner
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|(name, _)| name == "dialog")
            .count()
    }
}
