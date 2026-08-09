//! Port of `packages/tui/src/editor-component.ts` @ pi 0.82.1 (2efa728).
//!
//! Interface for custom editor components (e.g. vim mode, emacs mode, custom
//! keybindings) while maintaining compatibility with the core application.
//!
//! Intentional differences:
//! - Upstream is a structural TypeScript interface; the Rust port is a trait
//!   with default implementations, so a custom editor only implements the
//!   members it needs. The optional members become no-op/`None` defaults.
//! - Callbacks (`onSubmit`/`onChange`) are plain fields on the concrete
//!   editors (matching `components/input.rs`); the trait exposes them through
//!   accessors instead of mutable properties.
//! - `borderColor` (a public function property upstream) is exposed as a
//!   getter returning an optional reference.

use std::sync::Arc;

use crate::autocomplete::AutocompleteProvider;
use crate::tui::Component;

/// Submit callback (upstream `onSubmit?: (text: string) => void`).
pub type EditorSubmitFn = Box<dyn FnMut(&str) + Send>;
/// Change callback (upstream `onChange?: (text: string) => void`).
pub type EditorChangeFn = Box<dyn FnMut(&str) + Send>;

/// Interface for custom editor components (upstream `EditorComponent`,
/// editor-component.ts:11-74).
pub trait EditorComponent: Component {
    // =========================================================================
    // Core text access (required)
    // =========================================================================

    /// Get the current text content.
    fn get_text(&self) -> String;

    /// Set the text content.
    fn set_text(&mut self, text: &str);

    /// Handle raw terminal input (key presses, paste sequences, etc.).
    fn handle_input(&mut self, data: &str) {
        Component::handle_input(self, data);
    }

    // =========================================================================
    // Callbacks (required)
    // =========================================================================

    /// Called when the user submits (e.g. Enter key) (upstream `onSubmit?`).
    fn on_submit(&mut self) -> Option<&mut EditorSubmitFn> {
        None
    }

    /// Called when the text changes (upstream `onChange?`).
    fn on_change(&mut self) -> Option<&mut EditorChangeFn> {
        None
    }

    // =========================================================================
    // History support (optional)
    // =========================================================================

    /// Add text to history for up/down navigation (upstream `addToHistory?`).
    fn add_to_history(&mut self, _text: &str) {}

    // =========================================================================
    // Advanced text manipulation (optional)
    // =========================================================================

    /// Insert text at the current cursor position (upstream
    /// `insertTextAtCursor?`).
    fn insert_text_at_cursor(&mut self, _text: &str) {}

    /// Get text with any markers expanded (e.g. paste markers). Falls back
    /// to [`Self::get_text`] when not implemented (upstream
    /// `getExpandedText?`).
    fn get_expanded_text(&self) -> String {
        self.get_text()
    }

    // =========================================================================
    // Autocomplete support (optional)
    // =========================================================================

    /// Set the autocomplete provider (upstream `setAutocompleteProvider?`).
    fn set_autocomplete_provider(&mut self, _provider: Arc<dyn AutocompleteProvider>) {}

    // =========================================================================
    // Appearance (optional)
    // =========================================================================

    /// Border color function (upstream `borderColor?`).
    fn border_color(&self) -> Option<&dyn Fn(&str) -> String> {
        None
    }

    /// Set horizontal padding (upstream `setPaddingX?`).
    fn set_padding_x(&mut self, _padding: usize) {}

    /// Set max visible items in the autocomplete dropdown (upstream
    /// `setAutocompleteMaxVisible?`).
    fn set_autocomplete_max_visible(&mut self, _max_visible: usize) {}
}
