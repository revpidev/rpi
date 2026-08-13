//! Port of `extension_editor.ts` @ pi 0.82.1 (2efa728).
//!
//! Intentional differences:
//! - The theme is passed explicitly (`Arc<Theme>`) instead of read from the
//!   global `theme` getter (theme.ts:799-816).
//! - Upstream's `keybindings: KeybindingsManager` parameter and
//!   `externalEditorCommand` (with the `VISUAL`/`EDITOR` env fallback) are
//!   dropped: key matching goes through the installed global keybinding
//!   manager (which carries the `app.*` ids, installed by
//!   `install_global_keybindings`), and the external-editor flow
//!   (`app.editor.external`, extension-editor.ts:107-110) is exposed as the
//!   [`ExtensionEditorComponent::on_external_editor`] hook — called with the
//!   current editor text; the process spawning (`editInExternalEditor`,
//!   extension-editor.ts:116-131, incl. `tui.stop()`/`start()` around it)
//!   lives in `../external_editor.rs`; wiring this component's hook is part
//!   of the extension UI (T15).
//! - The editor theme is `border_color = theme.fg("borderMuted", ...)` with
//!   `SelectListTheme::identity()` for the autocomplete list (upstream uses
//!   `getEditorTheme()`, extension-editor.ts:70).
//! - Callbacks are `Box<dyn FnMut ... + Send>`; the submit callback is shared
//!   with the editor's internal `on_submit` wiring via `Arc<Mutex<...>>`.

use std::sync::{Arc, Mutex};

use rpi_tui::components::editor::{Editor, EditorOptions, EditorTheme};
use rpi_tui::components::select_list::SelectListTheme;
use rpi_tui::components::text::Text;
use rpi_tui::tui::{Component, Focusable};
use rpi_tui::tui_handle::TuiHandle;

use crate::core::themes::Theme;

use super::dynamic_border::DynamicBorder;
use super::keybinding_hints::key_hint;

/// `ExtensionEditorComponent` (extension-editor.ts:22-131): a bordered
/// multi-line editor with a title and a key hint footer. Enter submits
/// (Shift+Enter inserts a newline, like the main editor), Escape/ctrl+c
/// cancels, Ctrl+G (`app.editor.external`) fires the external-editor hook.
pub struct ExtensionEditorComponent {
    editor: Editor,
    theme: Arc<Theme>,
    title: String,
    on_cancel: Option<Box<dyn FnMut() + Send>>,
    /// External editor hook: called with the current text on
    /// `app.editor.external` (extension-editor.ts:107-110); process spawning
    /// is left to the integration layer (see header note).
    #[allow(clippy::type_complexity)] // mirrors the upstream callback type
    pub on_external_editor: Option<Box<dyn FnMut(&str) + Send>>,
    focused: bool,
}

impl ExtensionEditorComponent {
    /// `constructor` (extension-editor.ts:39-96). `options` default to
    /// [`EditorOptions::default`]; `prefill` is set via `editor.setText`
    /// (extension-editor.ts:71-73).
    pub fn new(
        tui: TuiHandle,
        theme: Arc<Theme>,
        title: String,
        prefill: Option<String>,
        on_submit: Box<dyn FnMut(&str) + Send>,
        on_cancel: Box<dyn FnMut() + Send>,
        options: Option<EditorOptions>,
    ) -> Self {
        let on_submit = Arc::new(Mutex::new(Some(on_submit)));

        let mut editor = Editor::new(
            tui,
            EditorTheme {
                border_color: Box::new({
                    let theme = Arc::clone(&theme);
                    move |s: &str| theme.fg("borderMuted", s)
                }),
                select_list: Arc::new(SelectListTheme::identity()),
            },
            options.unwrap_or_default(),
        );
        if let Some(prefill) = prefill {
            editor.set_text(&prefill);
        }
        // Wire up Enter to submit (extension-editor.ts:74-77).
        let editor_on_submit = Arc::clone(&on_submit);
        editor.on_submit = Some(Box::new(move |text: &str| {
            if let Some(on_submit) = editor_on_submit
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .as_mut()
            {
                on_submit(text);
            }
        }));

        Self {
            editor,
            theme,
            title,
            on_cancel: Some(on_cancel),
            on_external_editor: None,
            focused: false,
        }
    }

    /// The current editor text (test/integration helper).
    pub fn text(&self) -> String {
        self.editor.get_text()
    }

    fn border_line(&self, width: usize) -> String {
        let theme = Arc::clone(&self.theme);
        DynamicBorder::new(Box::new(move |s: &str| theme.fg("border", s)))
            .render(width)
            .pop()
            .unwrap_or_default()
    }
}

impl Component for ExtensionEditorComponent {
    /// Container layout (extension-editor.ts:62-95): border, title, editor
    /// (with its own borders), hint, border.
    fn render(&self, width: usize) -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();
        lines.push(self.border_line(width)); // DynamicBorder
        lines.push(String::new()); // Spacer(1)
        lines.extend(Text::new(self.theme.fg("accent", &self.title), 1, 0, None).render(width));
        lines.push(String::new()); // Spacer(1)
        lines.extend(self.editor.render(width));
        lines.push(String::new()); // Spacer(1)
                                   // Key hints (extension-editor.ts:83-90).
        let hint = format!(
            "{}  {}  {}  {}",
            key_hint(&self.theme, "tui.select.confirm", "submit"),
            key_hint(&self.theme, "tui.input.newLine", "newline"),
            key_hint(&self.theme, "tui.select.cancel", "cancel"),
            key_hint(&self.theme, "app.editor.external", "external editor"),
        );
        lines.extend(Text::new(hint, 1, 0, None).render(width));
        lines.push(String::new()); // Spacer(1)
        lines.push(self.border_line(width)); // DynamicBorder
        lines
    }

    /// `handleInput` (extension-editor.ts:98-114): cancel, external editor,
    /// else forward to the editor.
    fn handle_input(&mut self, data: &str) {
        let kb = rpi_tui::keybindings::get_keybindings();
        let read = kb.read().unwrap_or_else(|poisoned| poisoned.into_inner());
        // Escape or Ctrl+C to cancel (extension-editor.ts:100-104).
        if read.matches_id(data, "tui.select.cancel") {
            if let Some(on_cancel) = self.on_cancel.as_mut() {
                on_cancel();
            }
            return;
        }
        // External editor (app keybinding, extension-editor.ts:106-110).
        if read.matches_id(data, "app.editor.external") {
            let text = self.editor.get_text();
            if let Some(on_external_editor) = self.on_external_editor.as_mut() {
                on_external_editor(&text);
            }
            return;
        }
        // Forward to editor (extension-editor.ts:112-113).
        drop(read);
        self.editor.handle_input(data);
    }

    fn invalidate(&mut self) {
        self.editor.invalidate();
    }

    fn as_focusable(&self) -> Option<&dyn Focusable> {
        Some(self)
    }

    fn as_focusable_mut(&mut self) -> Option<&mut dyn Focusable> {
        Some(self)
    }
}

impl Focusable for ExtensionEditorComponent {
    fn focused(&self) -> bool {
        self.focused
    }

    /// Propagate focus to the editor (extension-editor.ts:30-37).
    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
        self.editor.set_focused(focused);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modes::interactive::test_support::TestTerminal;
    use rpi_tui::tui_handle::TuiHandle;
    use rpi_tui::tui_main_screen::TuiMainScreen;
    use std::sync::Mutex;

    fn theme() -> Arc<Theme> {
        Arc::new(crate::core::themes::load_theme("dark", None).expect("builtin dark theme"))
    }

    fn tui() -> TuiHandle {
        TuiHandle::from_main(TuiMainScreen::new(Box::new(TestTerminal::new())))
    }

    fn strip_ansi(input: &str) -> String {
        let mut out = String::with_capacity(input.len());
        let mut chars = input.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' && chars.peek() == Some(&'[') {
                chars.next();
                for c in chars.by_ref() {
                    if c == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    struct Captures {
        submitted: Arc<Mutex<Option<String>>>,
        cancelled: Arc<Mutex<u32>>,
        external: Arc<Mutex<Option<String>>>,
    }

    fn component_with(prefill: Option<&str>) -> (ExtensionEditorComponent, Captures) {
        let submitted = Arc::new(Mutex::new(None));
        let cancelled = Arc::new(Mutex::new(0u32));
        let external = Arc::new(Mutex::new(None));
        let submitted_thread = Arc::clone(&submitted);
        let cancelled_thread = Arc::clone(&cancelled);
        let external_thread = Arc::clone(&external);
        let mut component = ExtensionEditorComponent::new(
            tui(),
            theme(),
            "Extension Editor".to_string(),
            prefill.map(str::to_string),
            Box::new(move |text: &str| {
                *submitted_thread.lock().unwrap() = Some(text.to_string());
            }),
            Box::new(move || {
                *cancelled_thread.lock().unwrap() += 1;
            }),
            None,
        );
        component.on_external_editor = Some(Box::new(move |text: &str| {
            *external_thread.lock().unwrap() = Some(text.to_string());
        }));
        (
            component,
            Captures {
                submitted,
                cancelled,
                external,
            },
        )
    }

    #[test]
    fn typing_and_enter_submit_editor_text() {
        crate::modes::interactive::interactive_mode::install_global_keybindings();
        let (mut component, captures) = component_with(None);

        for ch in "hello".chars() {
            component.handle_input(&ch.to_string());
        }
        assert_eq!(component.text(), "hello");

        component.handle_input("\r"); // forwarded to the editor → InputSubmit
        assert_eq!(captures.submitted.lock().unwrap().as_deref(), Some("hello"));
        assert_eq!(*captures.cancelled.lock().unwrap(), 0);
    }

    #[test]
    fn prefill_is_loaded_and_rendered() {
        crate::modes::interactive::interactive_mode::install_global_keybindings();
        let (component, _) = component_with(Some("line1\nline2"));
        assert_eq!(component.text(), "line1\nline2");

        let lines: Vec<String> = component.render(80).iter().map(|l| strip_ansi(l)).collect();
        assert!(lines.iter().any(|l| l.contains("Extension Editor")));
        assert!(
            lines.iter().any(|l| l.contains("line1")),
            "editor content rendered"
        );
        assert!(lines.iter().any(|l| l.contains("line2")));
        assert!(lines.iter().any(|l| l.contains("submit")));
        assert!(lines.iter().any(|l| l.contains("newline")));
        assert!(lines.iter().any(|l| l.contains("cancel")));
        assert!(lines.iter().any(|l| l.contains("external editor")));
        assert!(lines.iter().any(|l| l.contains('─')), "borders rendered");
    }

    #[test]
    fn escape_cancels_without_submitting() {
        crate::modes::interactive::interactive_mode::install_global_keybindings();
        let (mut component, captures) = component_with(None);

        component.handle_input("abc");
        component.handle_input("\x1b");
        assert_eq!(*captures.cancelled.lock().unwrap(), 1);
        assert_eq!(captures.submitted.lock().unwrap().as_deref(), None);

        component.handle_input("\x03"); // ctrl+c also cancels
        assert_eq!(*captures.cancelled.lock().unwrap(), 2);
    }

    #[test]
    fn external_editor_key_fires_hook_with_current_text() {
        crate::modes::interactive::interactive_mode::install_global_keybindings();
        let (mut component, captures) = component_with(Some("draft"));

        component.handle_input("\x07"); // ctrl+g = app.editor.external
        assert_eq!(captures.external.lock().unwrap().as_deref(), Some("draft"));
        assert_eq!(*captures.cancelled.lock().unwrap(), 0);

        component.handle_input("\x18"); // ctrl+x must not fire the hook
        assert_eq!(captures.external.lock().unwrap().as_deref(), Some("draft"));
    }

    #[test]
    fn shift_enter_inserts_newline_instead_of_submitting() {
        crate::modes::interactive::interactive_mode::install_global_keybindings();
        let (mut component, captures) = component_with(None);

        component.handle_input("ab");
        // Shift+Enter is tui.input.newLine — forwarded to the editor, which
        // inserts a newline instead of submitting.
        component.handle_input("\x1b[106;5u"); // kitty CSI-u shift+enter
        component.handle_input("cd");
        assert_eq!(component.text(), "ab\ncd");
        assert_eq!(captures.submitted.lock().unwrap().as_deref(), None);
    }

    #[test]
    fn focus_propagates_to_inner_editor() {
        crate::modes::interactive::interactive_mode::install_global_keybindings();
        let (mut component, _) = component_with(None);
        assert!(!component.focused());
        assert!(!component.editor.focused());
        component.set_focused(true);
        assert!(component.focused());
        assert!(
            component.editor.focused(),
            "inner editor must receive focus"
        );
        component.set_focused(false);
        assert!(!component.editor.focused());
    }
}
