//! Custom editor with app-level keybinding dispatch — port of
//! `packages/coding-agent/src/modes/interactive/components/custom-editor.ts`
//! @ pi 0.82.1 (2efa728).
//!
//! Intentional differences:
//! - Upstream extends the `Editor` class and is placed in the tree directly;
//!   here the mode keeps a concrete `Arc<Mutex<CustomEditor>>` (for
//!   out-of-dispatch mutations like submit history / border color) and the
//!   tree holds a [`CustomEditorRegion`] wrapper that renders/dispatches
//!   through that handle (see tui.rs lock-contract note; the region is the
//!   TUI-visible focusable and delegates to the inner editor).
//! - The keybindings manager is read from the pir-tui global registry
//!   (`get_keybindings`) instead of being injected — upstream's
//!   `setKeybindings` global singleton, with the manager installed once at
//!   startup (interactive-mode.ts:468-469). `app.*` ids match via
//!   [`KeybindingsManager::matches_id`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use pir_tui::components::editor::{Editor, EditorOptions, EditorTheme};
use pir_tui::keybindings::get_keybindings;
use pir_tui::tui::{Component, Focusable, Tui};

/// App-action handler (upstream `() => void`).
pub type ActionHandler = Box<dyn FnMut() + Send>;
/// Dynamic escape handler slot (upstream `onEscape?`).
pub type EscapeHandler = Box<dyn FnMut() + Send>;
/// Extension-registered shortcut handler (upstream
/// `onExtensionShortcut?: (data: string) => boolean`).
pub type ExtensionShortcutHandler = Box<dyn FnMut(&str) -> bool + Send>;

/// `CustomEditor` (custom-editor.ts:7-79).
pub struct CustomEditor {
    editor: Editor,
    /// App action handlers, keyed by `app.*` keybinding id
    /// (`actionHandlers`, custom-editor.ts:9).
    pub action_handlers: HashMap<&'static str, ActionHandler>,
    /// Dynamic escape handler (custom-editor.ts:12).
    pub on_escape: Option<EscapeHandler>,
    /// Dynamic Ctrl+D handler (custom-editor.ts:13).
    pub on_ctrl_d: Option<EscapeHandler>,
    /// Clipboard paste handler (custom-editor.ts:14).
    pub on_paste_image: Option<EscapeHandler>,
    /// Extension shortcut hook (custom-editor.ts:16).
    pub on_extension_shortcut: Option<ExtensionShortcutHandler>,
}

impl CustomEditor {
    pub fn new(tui: Tui, theme: EditorTheme, options: EditorOptions) -> Self {
        Self {
            editor: Editor::new(tui, theme, options),
            action_handlers: HashMap::new(),
            on_escape: None,
            on_ctrl_d: None,
            on_paste_image: None,
            on_extension_shortcut: None,
        }
    }

    /// `onAction` (custom-editor.ts:26-28).
    pub fn on_action(&mut self, action: &'static str, handler: ActionHandler) {
        self.action_handlers.insert(action, handler);
    }

    // --- Editor delegation (the mode mutates through the shared handle) ----

    pub fn get_text(&self) -> String {
        self.editor.get_text()
    }

    pub fn set_text(&mut self, text: &str) {
        self.editor.set_text(text);
    }

    /// `editor.setPaddingX` (applyRuntimeSettings,
    /// interactive-mode.ts:1724-1726).
    pub fn set_padding_x(&mut self, padding: usize) {
        self.editor.set_padding_x(padding);
    }

    /// Current horizontal padding (test assertions for the /settings
    /// `EditorPaddingX` change).
    pub fn get_padding_x(&self) -> usize {
        self.editor.get_padding_x()
    }

    /// `editor.setAutocompleteMaxVisible` (applyRuntimeSettings,
    /// interactive-mode.ts:1724-1726).
    pub fn set_autocomplete_max_visible(&mut self, max_visible: usize) {
        self.editor.set_autocomplete_max_visible(max_visible);
    }

    /// Current autocomplete popup cap (test assertions for the /settings
    /// `AutocompleteMaxVisible` change).
    pub fn get_autocomplete_max_visible(&self) -> usize {
        self.editor.get_autocomplete_max_visible()
    }

    pub fn get_expanded_text(&self) -> String {
        self.editor.get_expanded_text()
    }

    pub fn add_to_history(&mut self, text: &str) {
        self.editor.add_to_history(text);
    }

    pub fn insert_text_at_cursor(&mut self, text: &str) {
        self.editor.insert_text_at_cursor(text);
    }

    pub fn set_autocomplete_provider(
        &mut self,
        provider: Arc<dyn pir_tui::autocomplete::AutocompleteProvider>,
    ) {
        self.editor.set_autocomplete_provider(provider);
    }

    pub fn is_showing_autocomplete(&self) -> bool {
        self.editor.is_showing_autocomplete()
    }

    /// Submit callback (delegates to the base editor's `onSubmit` field).
    pub fn set_on_submit(&mut self, callback: Option<pir_tui::components::editor::SubmitFn>) {
        self.editor.on_submit = callback;
    }

    /// Change callback (delegates to the base editor's `onChange` field).
    pub fn set_on_change(&mut self, callback: Option<pir_tui::components::editor::ChangeFn>) {
        self.editor.on_change = callback;
    }

    /// The base editor's border color (upstream `borderColor`, replaced
    /// dynamically by `updateEditorBorderColor`).
    pub fn border_color(&self) -> &(dyn Fn(&str) -> String + Send + Sync) {
        &*self.editor.border_color
    }

    pub fn set_border_color(&mut self, border_color: Box<dyn Fn(&str) -> String + Send + Sync>) {
        self.editor.border_color = border_color;
    }

    /// `handleInput` (custom-editor.ts:30-79).
    pub fn handle_input(&mut self, data: &str) {
        let keybindings = get_keybindings();
        let read = keybindings
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        // Extension-registered shortcuts first (custom-editor.ts:32-34).
        if let Some(handler) = self.on_extension_shortcut.as_mut() {
            if handler(data) {
                return;
            }
        }

        // Clipboard paste keybinding (custom-editor.ts:37-41).
        if read.matches_id(data, "app.clipboard.pasteImage") {
            if let Some(handler) = self.on_paste_image.as_mut() {
                handler();
            }
            return;
        }

        // Escape/interrupt — only if autocomplete is NOT active
        // (custom-editor.ts:45-57): use the dynamic onEscape if set, otherwise
        // the registered handler; let the parent handle autocomplete
        // cancellation.
        if read.matches_id(data, "app.interrupt") {
            if !self.editor.is_showing_autocomplete() {
                let handler = match self.on_escape.as_mut() {
                    Some(handler) => Some(handler.as_mut() as &mut dyn FnMut()),
                    None => self
                        .action_handlers
                        .get_mut("app.interrupt")
                        .map(|handler| handler.as_mut() as &mut dyn FnMut()),
                };
                if let Some(handler) = handler {
                    handler();
                    return;
                }
            }
            self.editor.handle_input(data);
            return;
        }

        // Exit (Ctrl+D) — only when editor is empty (custom-editor.ts:60-67);
        // otherwise fall through to delete-char-forward.
        if read.matches_id(data, "app.exit") && self.editor.get_text().is_empty() {
            let handler = match self.on_ctrl_d.as_mut() {
                Some(handler) => Some(handler.as_mut() as &mut dyn FnMut()),
                None => self
                    .action_handlers
                    .get_mut("app.exit")
                    .map(|handler| handler.as_mut() as &mut dyn FnMut()),
            };
            if let Some(handler) = handler {
                handler();
                return;
            }
        }

        // All other app actions (custom-editor.ts:70-75).
        for (action, handler) in self.action_handlers.iter_mut() {
            if *action != "app.interrupt" && *action != "app.exit" && read.matches_id(data, action)
            {
                handler();
                return;
            }
        }

        // Pass to the parent editor (custom-editor.ts:78).
        self.editor.handle_input(data);
    }
}

/// Tree entry for a shared [`CustomEditor`]: the TUI owns this wrapper while
/// the interactive mode keeps the concrete `Arc<Mutex<CustomEditor>>` for
/// mutations from the event-drain (submit history, border color). The region
/// itself is the focusable the TUI tracks; `focused` state lives in the inner
/// editor (its render emits the cursor marker).
pub struct CustomEditorRegion {
    inner: Arc<Mutex<CustomEditor>>,
}

impl CustomEditorRegion {
    pub fn new(inner: Arc<Mutex<CustomEditor>>) -> Self {
        Self { inner }
    }

    /// The shared editor handle (mode-side mutation entry point).
    pub fn inner(&self) -> &Arc<Mutex<CustomEditor>> {
        &self.inner
    }
}

fn lock_editor(editor: &Arc<Mutex<CustomEditor>>) -> std::sync::MutexGuard<'_, CustomEditor> {
    editor
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Component for CustomEditorRegion {
    fn render(&self, width: usize) -> Vec<String> {
        lock_editor(&self.inner).editor.render(width)
    }

    fn handle_input(&mut self, data: &str) {
        lock_editor(&self.inner).handle_input(data);
    }

    fn invalidate(&mut self) {
        lock_editor(&self.inner).editor.invalidate();
    }

    fn as_focusable(&self) -> Option<&dyn Focusable> {
        Some(self)
    }

    fn as_focusable_mut(&mut self) -> Option<&mut dyn Focusable> {
        Some(self)
    }
}

impl Focusable for CustomEditorRegion {
    fn focused(&self) -> bool {
        lock_editor(&self.inner).editor.focused()
    }

    fn set_focused(&mut self, focused: bool) {
        lock_editor(&self.inner).editor.set_focused(focused);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Install the full 73-entry keybinding table (the ids the dispatch
    /// checks are all default bindings, so the full table is equivalent to
    /// the previously-used custom manager — and identical across parallel
    /// test threads, avoiding global-manager races).
    fn install_keybindings() {
        crate::modes::interactive::interactive_mode::install_global_keybindings();
    }

    struct Harness {
        editor: Arc<Mutex<CustomEditor>>,
        _tui: Tui,
    }

    impl Harness {
        fn new() -> Self {
            install_keybindings();
            let tui = Tui::new(Box::new(super::super::test_support::TestTerminal::new()));
            let editor = Arc::new(Mutex::new(CustomEditor::new(
                tui.clone(),
                EditorTheme {
                    border_color: Box::new(|text: &str| text.to_string()),
                    select_list: Arc::new(
                        pir_tui::components::select_list::SelectListTheme::identity(),
                    ),
                },
                EditorOptions::default(),
            )));
            Self { editor, _tui: tui }
        }

        fn dispatch(&self, data: &str) {
            lock_editor(&self.editor).handle_input(data);
        }

        fn set_text(&self, text: &str) {
            lock_editor(&self.editor).editor.set_text(text);
        }
    }

    #[test]
    fn interrupt_routes_to_on_escape_when_set() {
        let h = Harness::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        lock_editor(&h.editor).on_escape = Some(Box::new(move || {
            c.fetch_add(1, Ordering::SeqCst);
        }));
        h.dispatch("\u{1b}");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // Editor text unchanged (escape handler consumed the key).
        assert_eq!(h.editor.lock().unwrap().get_text(), "");
    }

    #[test]
    fn interrupt_falls_back_to_action_handler() {
        let h = Harness::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        lock_editor(&h.editor).on_action(
            "app.interrupt",
            Box::new(move || {
                c.fetch_add(1, Ordering::SeqCst);
            }),
        );
        h.dispatch("\u{1b}");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn clear_action_routes_to_handler() {
        let h = Harness::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        lock_editor(&h.editor).on_action(
            "app.clear",
            Box::new(move || {
                c.fetch_add(1, Ordering::SeqCst);
            }),
        );
        h.dispatch("\u{3}");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn exit_only_fires_on_empty_editor() {
        let h = Harness::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        lock_editor(&h.editor).on_action(
            "app.exit",
            Box::new(move || {
                c.fetch_add(1, Ordering::SeqCst);
            }),
        );
        // Empty editor: Ctrl+D dispatches to the exit handler.
        h.dispatch("\u{4}");
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // Non-empty editor: falls through to delete-char-forward (upstream
        // custom-editor.ts:60-67). At the line end there is nothing to
        // delete; move the cursor left first.
        h.set_text("abc");
        h.dispatch("\u{1b}[D"); // left
        h.dispatch("\u{4}");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(h.editor.lock().unwrap().get_text(), "ab");
    }

    #[test]
    fn paste_image_matches_before_generic_actions() {
        let h = Harness::new();
        let pastes = Arc::new(AtomicUsize::new(0));
        let p = pastes.clone();
        lock_editor(&h.editor).on_paste_image = Some(Box::new(move || {
            p.fetch_add(1, Ordering::SeqCst);
        }));
        h.dispatch("\u{16}");
        assert_eq!(pastes.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn unknown_input_passes_through_to_editor() {
        let h = Harness::new();
        h.dispatch("a");
        assert_eq!(h.editor.lock().unwrap().get_text(), "a");
        h.dispatch("b");
        assert_eq!(h.editor.lock().unwrap().get_text(), "ab");
    }

    #[test]
    fn submit_clears_editor_and_invokes_callback() {
        let h = Harness::new();
        let submitted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let s = submitted.clone();
        lock_editor(&h.editor).set_on_submit(Some(Box::new(move |text: &str| {
            s.lock().unwrap().push(text.to_string());
        })));
        h.set_text("hello");
        h.dispatch("\r");
        assert_eq!(*submitted.lock().unwrap(), vec!["hello".to_string()]);
        assert_eq!(h.editor.lock().unwrap().get_text(), "");
    }
}
