//! `InteractiveUiBridge` — TUI-mode extension UI bridge (T15 W4), port of
//! `createExtensionUIContext` (interactive-mode.ts:2150-2205) and the
//! `showExtension*` dialog helpers (:2207-2400).
//!
//! Dialogs mount in place of the editor via the shared `show_selector` /
//! `hide_selector` region swap and resolve through a oneshot; `timeout` is
//! enforced bridge-side (auto-resolve with the default) and displayed by
//! the components' countdown.
//!
//! Declarative-component v1 notes (candidate deviations):
//! - `custom()` mounts the component tree in the editor region and resolves
//!   `undefined` immediately — the declarative tree has no interaction
//!   channel in v1 (the `uiEvent` round-trip is W6 ABI), and overlay
//!   positioning (`OverlayOptions`/`OverlayHandle`) is not yet supported by
//!   rpi-tui.
//! - `setEditorComponent` stores the descriptor (readable via
//!   `getEditorComponent`) but does not mount it — a declarative tree
//!   cannot handle input.
//! - `addAutocompleteProvider` is a no-op: upstream composes closure
//!   factories, which have no declarative form.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use async_trait::async_trait;
use rpi_ext_host::api::{
    ExtensionWidgetOptions, NotifyType, SetThemeResult, TerminalInputHandler, TerminalInputResult,
    ThemeInfo, UiBridge, UiDialogOptions, Unsubscribe, WidgetContent, WidgetPlacement,
    WorkingIndicatorOptions,
};
use rpi_ext_host::types::ComponentTree;
use rpi_tui::tui::{Component, Focusable};
use serde_json::Value;
use tokio::sync::oneshot;

use super::{lock, InteractiveUi};
use crate::modes::interactive::component_tree::component_from_tree;
use crate::modes::interactive::components::extension_editor::ExtensionEditorComponent;
use crate::modes::interactive::components::extension_input::ExtensionInputComponent;
use crate::modes::interactive::components::extension_selector::{
    ExtensionSelectorComponent, ExtensionSelectorOptions,
};

/// TUI entry wrapper forwarding component calls and focus (the
/// startup_ui.rs `SelectorRegion` pattern).
struct DialogRegion<C: Component>(Arc<Mutex<C>>);

impl<C: Component + Send> Component for DialogRegion<C> {
    fn render(&self, width: usize) -> Vec<String> {
        lock(&self.0).render(width)
    }
    fn handle_input(&mut self, data: &str) {
        lock(&self.0).handle_input(data);
    }
    fn invalidate(&mut self) {
        lock(&self.0).invalidate();
    }
    fn as_focusable(&self) -> Option<&dyn Focusable> {
        Some(self)
    }
    fn as_focusable_mut(&mut self) -> Option<&mut dyn Focusable> {
        Some(self)
    }
}

impl<C: Component + Send> Focusable for DialogRegion<C> {
    fn focused(&self) -> bool {
        true
    }
    fn set_focused(&mut self, _focused: bool) {}
}

/// The interactive bridge. Holds the UI weakly (the UI owns the session,
/// which owns the runner → host → bridge).
pub struct InteractiveUiBridge {
    ui: Weak<InteractiveUi>,
    /// Widget key → (below_editor, child address) for identity removal.
    widgets: Mutex<HashMap<String, (bool, usize)>>,
    /// Widget key → last pushed (content, options) — the replay source for
    /// theme changes (TE11 FR-E.3): pushed widget components bake the
    /// current theme's ANSI codes into their text, so a theme swap has to
    /// rebuild them from the archived trees.
    widget_archive: Mutex<HashMap<String, (WidgetContent, Option<ExtensionWidgetOptions>)>>,
    editor_component: Mutex<Option<ComponentTree>>,
    working_indicator: Mutex<Option<WorkingIndicatorOptions>>,
    next_input_listener: AtomicU64,
}

impl InteractiveUiBridge {
    pub fn new(ui: &Arc<InteractiveUi>) -> Self {
        InteractiveUiBridge {
            ui: Arc::downgrade(ui),
            widgets: Mutex::new(HashMap::new()),
            widget_archive: Mutex::new(HashMap::new()),
            editor_component: Mutex::new(None),
            working_indicator: Mutex::new(None),
            next_input_listener: AtomicU64::new(0),
        }
    }

    fn ui(&self) -> Option<Arc<InteractiveUi>> {
        self.ui.upgrade()
    }

    /// The live `InteractiveUi` — the L0 escape hatch for built-in native
    /// extensions (llama.cpp manager mounts its native view).
    pub fn interactive_ui(&self) -> Option<Arc<InteractiveUi>> {
        self.ui()
    }

    /// Mount a dialog in the editor region and await its resolution; a
    /// timeout auto-resolves with `None` and closes the dialog
    /// (interactive-mode.ts:2226-2258).
    async fn mount_dialog(
        &self,
        entry: super::SharedComponent,
        opts: Option<UiDialogOptions>,
        rx: oneshot::Receiver<Option<String>>,
    ) -> Option<String> {
        let ui = self.ui()?;
        ui.show_selector(entry);
        let result = match opts.and_then(|o| o.timeout) {
            Some(ms) if ms > 0 => {
                match tokio::time::timeout(std::time::Duration::from_millis(ms), rx).await {
                    Ok(received) => received.ok().flatten(),
                    Err(_) => None,
                }
            }
            _ => rx.await.ok().flatten(),
        };
        if let Some(ui) = self.ui() {
            ui.hide_selector();
        }
        result
    }

    /// Build the component for a widget content against the CURRENT theme
    /// and mount it into its placement container, replacing any existing
    /// entry under the same key.
    ///
    /// V13-05 (FR-A): the swap is atomic per container — the component tree
    /// is built BEFORE touching any container lock, and the remove-old /
    /// add-new pair happens inside one critical section, so no render
    /// deadline can capture a frame with the widget missing. Cross-container
    /// placement changes go **add-then-remove** (the widget appears in the
    /// new container before it leaves the old one): a duplicate for one
    /// frame is imperceptible, a missing frame is not. `remove_child_by_
    /// address` is a retain-filter, a no-op for an already-vacated address.
    /// Lock order: `widgets` (address book) take → container lock(s) →
    /// `widgets` write; the two are never held simultaneously.
    fn mount_widget(
        &self,
        ui: &Arc<InteractiveUi>,
        key: &str,
        content: Option<&WidgetContent>,
        options: Option<&ExtensionWidgetOptions>,
    ) {
        // FR-A R1: build the component (theme baking) before any container
        // lock — the build is millisecond-grade and must not extend a
        // critical section.
        let built = content.map(|content| match content {
            WidgetContent::Lines(lines) => {
                let mut column = rpi_tui::components::r#box::Box::new(0, 0, None);
                for line in lines {
                    column.add_child(Box::new(rpi_tui::components::text::Text::new(
                        line.clone(),
                        0,
                        0,
                        None,
                    )));
                }
                Box::new(column)
            }
            WidgetContent::Component(tree) => {
                component_from_tree(tree, &Arc::clone(&lock(&ui.theme)))
            }
        });
        let below = matches!(
            options.and_then(|o| o.placement),
            Some(WidgetPlacement::BelowEditor)
        );

        // Take the previous address-book entry: container operations below
        // run against this taken entry (R4: address book update happens
        // around the container swap, never while a container lock is held).
        let previous = lock(&self.widgets).remove(key);

        let Some(built) = built else {
            // Removal (`set_widget(key, None)`): drop the old entry from its
            // container in one critical section.
            if let Some((old_below, address)) = previous {
                let container = if old_below {
                    &ui.widgets_below
                } else {
                    &ui.widgets_above
                };
                let mut container = lock(container);
                super::remove_child_by_address(&mut container, address);
            }
            ui.render_handle.request_render();
            return;
        };

        let same_placement = previous
            .as_ref()
            .map(|(old_below, _)| *old_below == below)
            .unwrap_or(false);
        let new_address = match (previous, same_placement) {
            // Same-container path — STRICT atomicity (FR-A R2): remove old +
            // add new inside ONE container lock scope, so no render deadline
            // can capture a frame with the widget missing; request_render
            // fires once after the swap.
            (Some((_old_below, address)), true) => {
                let container = if below {
                    &ui.widgets_below
                } else {
                    &ui.widgets_above
                };
                let mut container = lock(container);
                super::remove_child_by_address(&mut container, address);
                container.add_child(built);
                let address =
                    super::child_address(&**container.children.last().expect("just pushed"));
                (below, address)
            }
            // Cross-container placement change — ADD-THEN-REMOVE (FR-A R3):
            // the widget lands in the new container before it leaves the
            // old one. A duplicate frame is imperceptible; add-then-remove
            // guarantees there is never a MISSING frame (remove-then-add
            // would show one).
            (Some((old_below, address)), false) => {
                debug_assert_ne!(old_below, below);
                let new_container = if below {
                    &ui.widgets_below
                } else {
                    &ui.widgets_above
                };
                {
                    let mut container = lock(new_container);
                    container.add_child(built);
                }
                let old_container = if old_below {
                    &ui.widgets_below
                } else {
                    &ui.widgets_above
                };
                {
                    let mut container = lock(old_container);
                    // retain() is a no-op when the address is already gone.
                    super::remove_child_by_address(&mut container, address);
                }
                let address = {
                    let container = lock(new_container);
                    super::child_address(&**container.children.last().expect("just pushed"))
                };
                (below, address)
            }
            // No previous entry — plain first mount.
            (None, _) => {
                let container = if below {
                    &ui.widgets_below
                } else {
                    &ui.widgets_above
                };
                let mut container = lock(container);
                container.add_child(built);
                let address =
                    super::child_address(&**container.children.last().expect("just pushed"));
                (below, address)
            }
        };
        // R4: address book update after the container swap; the two are
        // never held simultaneously.
        lock(&self.widgets).insert(key.to_owned(), new_address);
        ui.render_handle.request_render();
    }

    /// Rebuild every mounted widget against the current theme (TE11
    /// FR-E.3). Widget components bake theme ANSI codes into their text at
    /// mount time, so a theme switch leaves them stale; replaying the
    /// archived pushes rebuilds them without the extensions' involvement.
    /// Called from [`crate::modes::interactive::InteractiveUi::apply_theme`].
    pub(crate) fn retheme_widgets(&self) {
        let Some(ui) = self.ui() else {
            return;
        };
        let archived: Vec<(String, WidgetContent, Option<ExtensionWidgetOptions>)> =
            lock(&self.widget_archive)
                .iter()
                .map(|(key, (content, options))| (key.clone(), content.clone(), *options))
                .collect();
        for (key, content, options) in archived {
            self.mount_widget(&ui, &key, Some(&content), options.as_ref());
        }
    }
}

#[async_trait]
impl UiBridge for InteractiveUiBridge {
    /// `showExtensionSelector` (interactive-mode.ts:2207-2260).
    async fn select(
        &self,
        title: &str,
        options: &[String],
        opts: Option<UiDialogOptions>,
    ) -> Option<String> {
        let ui = self.ui()?;
        let (tx, rx) = oneshot::channel();
        let tx = Arc::new(Mutex::new(Some(tx)));
        let selector = Arc::new(Mutex::new(ExtensionSelectorComponent::new(
            Arc::clone(&lock(&ui.theme)),
            Some(title.to_owned()),
            options.to_vec(),
            {
                let tx = tx.clone();
                Box::new(move |selected: Option<String>| {
                    if let Some(tx) = lock(&tx).take() {
                        let _ = tx.send(selected);
                    }
                })
            },
            {
                let tx = tx.clone();
                Box::new(move || {
                    if let Some(tx) = lock(&tx).take() {
                        let _ = tx.send(None);
                    }
                })
            },
            Some(ExtensionSelectorOptions {
                render_handle: Some(ui.render_handle.clone()),
                timeout_ms: opts.and_then(|o| o.timeout),
                on_toggle_tools_expanded: None,
            }),
        )));
        let entry = super::shared_component_from_boxed(Box::new(DialogRegion(selector)));
        self.mount_dialog(entry, opts, rx).await
    }

    /// `showExtensionConfirm` (interactive-mode.ts:2262-2269): a Yes/No
    /// selector over `"{title}\n{message}"`.
    async fn confirm(&self, title: &str, message: &str, opts: Option<UiDialogOptions>) -> bool {
        let combined = format!("{title}\n{message}");
        self.select(&combined, &["Yes".to_owned(), "No".to_owned()], opts)
            .await
            .as_deref()
            == Some("Yes")
    }

    /// `showExtensionInput` (interactive-mode.ts:2282-2330).
    async fn input(
        &self,
        title: &str,
        placeholder: Option<&str>,
        opts: Option<UiDialogOptions>,
    ) -> Option<String> {
        let ui = self.ui()?;
        let (tx, rx) = oneshot::channel();
        let tx = Arc::new(Mutex::new(Some(tx)));
        let input = Arc::new(Mutex::new(ExtensionInputComponent::new(
            Arc::clone(&lock(&ui.theme)),
            title.to_owned(),
            placeholder.map(str::to_owned),
            {
                let tx = tx.clone();
                Box::new(move |value: &str| {
                    if let Some(tx) = lock(&tx).take() {
                        let _ = tx.send(Some(value.to_owned()));
                    }
                })
            },
            {
                let tx = tx.clone();
                Box::new(move || {
                    if let Some(tx) = lock(&tx).take() {
                        let _ = tx.send(None);
                    }
                })
            },
            None,
        )));
        let entry = super::shared_component_from_boxed(Box::new(DialogRegion(input)));
        self.mount_dialog(entry, opts, rx).await
    }

    /// `showExtensionEditor` (interactive-mode.ts:2332-2400).
    async fn editor(&self, title: &str, prefill: Option<&str>) -> Option<String> {
        let ui = self.ui()?;
        let (tx, rx) = oneshot::channel();
        let tx = Arc::new(Mutex::new(Some(tx)));
        let editor = Arc::new(Mutex::new(ExtensionEditorComponent::new(
            ui.ui.clone(),
            Arc::clone(&lock(&ui.theme)),
            title.to_owned(),
            prefill.map(str::to_owned),
            {
                let tx = tx.clone();
                Box::new(move |value: &str| {
                    if let Some(tx) = lock(&tx).take() {
                        let _ = tx.send(Some(value.to_owned()));
                    }
                })
            },
            {
                let tx = tx.clone();
                Box::new(move || {
                    if let Some(tx) = lock(&tx).take() {
                        let _ = tx.send(None);
                    }
                })
            },
            None,
        )));
        let entry = super::shared_component_from_boxed(Box::new(DialogRegion(editor)));
        self.mount_dialog(entry, None, rx).await
    }

    /// `notify` (interactive-mode.ts:2177-2185 area): non-blocking status
    /// lines — info → status, warning/error → their styled variants.
    fn notify(&self, message: &str, kind: NotifyType) {
        if let Some(ui) = self.ui() {
            match kind {
                NotifyType::Info => ui.show_status(message),
                NotifyType::Warning => ui.show_warning(message),
                NotifyType::Error => ui.show_error(message),
            }
        }
    }

    /// `onTerminalInput` (interactive-mode.ts:2187-2205 area) via the Tui
    /// input-listener registry (tui.ts:651-658). The handler is also stored
    /// in `extension_input_listeners` for re-registration during
    /// `switch_tui_mode` (interactive-mode.ts:2303-2318).
    fn on_terminal_input(&self, handler: TerminalInputHandler) -> Unsubscribe {
        let Some(ui) = self.ui() else {
            return Box::new(|| {});
        };
        let _ = self.next_input_listener.fetch_add(1, Ordering::Relaxed);
        let handler_arc: Arc<dyn Fn(String) -> Option<TerminalInputResult> + Send + Sync> =
            Arc::clone(&handler);
        let id = ui.ui.add_input_listener(Box::new(move |data: &str| {
            handler(data.to_owned()).map(|result: TerminalInputResult| {
                rpi_tui::tui::TuiInputListenerResult {
                    consume: result.consume.unwrap_or(false),
                    data: result.data,
                }
            })
        }));
        // Track for rebind on switch_tui_mode (interactive-mode.ts:2306-2307).
        lock(&ui.extension_input_listeners).push((id, handler_arc));
        let ui_weak = Arc::downgrade(&ui);
        let tui = ui.ui.clone();
        Box::new(move || {
            tui.remove_input_listener(id);
            if let Some(ui) = ui_weak.upgrade() {
                lock(&ui.extension_input_listeners).retain(|(lid, _)| *lid != id);
            }
        })
    }

    /// `setStatus` — footer extension status; `None` clears.
    fn set_status(&self, key: &str, text: Option<&str>) {
        if let Some(ui) = self.ui() {
            match text {
                Some(text) => ui.footer_data.set_extension_status(key, text),
                None => ui.footer_data.remove_extension_status(key),
            }
            lock(&ui.footer).invalidate();
            ui.render_handle.request_render();
        }
    }

    /// `setWorkingMessage`; `None` restores the default
    /// (interactive-mode.ts:347, 1181-1186).
    fn set_working_message(&self, message: Option<&str>) {
        if let Some(ui) = self.ui() {
            *lock(&ui.working_message) = message.map(str::to_owned);
            ui.refresh_working_indicator(lock(&self.working_indicator).clone());
        }
    }

    fn set_working_visible(&self, visible: bool) {
        if let Some(ui) = self.ui() {
            ui.working_visible
                .store(visible, std::sync::atomic::Ordering::Relaxed);
            ui.refresh_working_indicator(lock(&self.working_indicator).clone());
        }
    }

    /// `setWorkingIndicator`; `None` restores the default spinner
    /// (types.ts:155-163).
    fn set_working_indicator(&self, options: Option<WorkingIndicatorOptions>) {
        if let Some(ui) = self.ui() {
            *lock(&self.working_indicator) = options.clone();
            ui.refresh_working_indicator(options);
        }
    }

    fn set_hidden_thinking_label(&self, label: Option<&str>) {
        if let Some(ui) = self.ui() {
            *lock(&ui.hidden_thinking_label) = label
                .map(str::to_owned)
                .unwrap_or_else(|| "Thinking...".to_owned());
        }
    }

    /// `setWidget` (interactive-mode.ts:2190s): keyed entries above/below
    /// the editor; `None` removes.
    fn set_widget(
        &self,
        key: &str,
        content: Option<WidgetContent>,
        options: Option<ExtensionWidgetOptions>,
    ) {
        let Some(ui) = self.ui() else {
            return;
        };
        self.mount_widget(&ui, key, content.as_ref(), options.as_ref());
        // Archive the push (or forget the removal) for theme replay
        // (TE11 FR-E.3).
        let mut archive = lock(&self.widget_archive);
        match content {
            Some(content) => {
                archive.insert(key.to_owned(), (content, options));
            }
            None => {
                archive.remove(key);
            }
        }
    }

    /// `setFooter` — declarative tree replaces the built-in footer region;
    /// `None` restores it.
    fn set_footer(&self, component: Option<ComponentTree>) {
        if let Some(ui) = self.ui() {
            let region = lock(&ui.footer_region).clone();
            if let Some(region) = region {
                ui.swap_region_component(&region, component, &ui.custom_footer);
            }
        }
    }

    /// `setHeader` — same swap against the header region.
    fn set_header(&self, component: Option<ComponentTree>) {
        if let Some(ui) = self.ui() {
            let region = lock(&ui.header_region).clone();
            if let Some(region) = region {
                ui.swap_region_component(&region, component, &ui.custom_header);
            }
        }
    }

    /// `setTitle` — terminal window/tab title.
    fn set_title(&self, title: &str) {
        if let Some(ui) = self.ui() {
            ui.ui.with_terminal(|terminal| terminal.set_title(title));
        }
    }

    /// `custom` — declarative v1: mount the tree in the editor region and
    /// resolve `undefined` (see module header for the deviation).
    async fn custom(&self, component: ComponentTree, _options: Option<Value>) -> Option<Value> {
        let ui = self.ui()?;
        let tree = component_from_tree(&component, &Arc::clone(&lock(&ui.theme)));
        let entry = super::shared_component_from_boxed(tree);
        ui.show_selector(entry);
        // No interaction channel in declarative v1: the component displays
        // until the next dialog/restore; resolve immediately.
        if let Some(ui) = self.ui() {
            ui.hide_selector();
        }
        None
    }

    /// `pasteToEditor` — full paste handling (large-content collapse).
    fn paste_to_editor(&self, text: &str) {
        if let Some(ui) = self.ui() {
            lock(&ui.editor).paste_text(text);
            ui.render_handle.request_render();
        }
    }

    fn set_editor_text(&self, text: &str) {
        if let Some(ui) = self.ui() {
            lock(&ui.editor).set_text(text);
            ui.render_handle.request_render();
        }
    }

    fn get_editor_text(&self) -> String {
        self.ui()
            .map(|ui| lock(&ui.editor).get_text())
            .unwrap_or_default()
    }

    /// No-op (see module header).
    fn add_autocomplete_provider(&self, _provider: Value) {}

    /// Stores the descriptor only (see module header).
    fn set_editor_component(&self, component: Option<ComponentTree>) {
        *lock(&self.editor_component) = component;
    }

    fn get_editor_component(&self) -> Option<ComponentTree> {
        lock(&self.editor_component).clone()
    }

    /// `theme` getter — the current theme JSON.
    fn theme(&self) -> Value {
        self.ui()
            .map(|ui| {
                let theme = lock(&ui.theme);
                crate::core::themes::theme_json_value(theme.name.as_deref().unwrap_or("dark"))
                    .unwrap_or_else(crate::core::themes::default_theme_json)
            })
            .unwrap_or_else(crate::core::themes::default_theme_json)
    }

    /// `getAllThemes` (themes.ts:880s): built-ins + discovered customs.
    fn get_all_themes(&self) -> Vec<ThemeInfo> {
        crate::core::themes::get_available_themes()
            .into_iter()
            .map(|info| ThemeInfo {
                name: info.name,
                path: info.path.map(|p| p.to_string_lossy().into_owned()),
            })
            .collect()
    }

    fn get_theme(&self, name: &str) -> Option<Value> {
        crate::core::themes::get_theme_by_name(name).map(|theme| {
            crate::core::themes::theme_json_value(theme.name.as_deref().unwrap_or("dark"))
                .unwrap_or_else(crate::core::themes::default_theme_json)
        })
    }

    /// `setTheme` by name or theme JSON object (types.ts:274).
    fn set_theme(&self, theme: Value) -> SetThemeResult {
        let Some(ui) = self.ui() else {
            return SetThemeResult {
                success: false,
                error: Some("UI not available".to_owned()),
            };
        };
        let name = match &theme {
            Value::String(name) => name.clone(),
            Value::Object(_) => theme
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            _ => String::new(),
        };
        let resolved = if name.is_empty() {
            None
        } else {
            crate::core::themes::get_theme_by_name(&name)
        };
        match resolved {
            Some(resolved) => {
                ui.apply_theme(Arc::new(resolved));
                // Persist like `/theme` (theme-selector flow).
                if let Some(session) = Some(ui.session()) {
                    session
                        .resource_loader()
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .settings_manager_mut()
                        .set_theme(&name);
                }
                SetThemeResult {
                    success: true,
                    error: None,
                }
            }
            None => SetThemeResult {
                success: false,
                error: Some(format!("Theme not found: {name}")),
            },
        }
    }

    fn get_tools_expanded(&self) -> bool {
        self.ui()
            .map(|ui| *lock(&ui.tool_output_expanded))
            .unwrap_or(false)
    }

    fn set_tools_expanded(&self, expanded: bool) {
        if let Some(ui) = self.ui() {
            ui.set_tools_expanded(expanded);
        }
    }

    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }
}
