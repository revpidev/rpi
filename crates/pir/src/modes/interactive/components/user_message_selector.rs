//! Port of `user_message_selector.ts` @ pi 0.82.1 (2efa728).
//!
//! Intentional differences:
//! - The theme is passed explicitly (`Arc<Theme>`) instead of read from the
//!   global `theme` getter (theme.ts:799-816).
//! - `UserMessageItem.timestamp` (user-message-selector.ts:5-9) is not ported
//!   — upstream rendering never reads it — so items arrive as
//!   `Vec<(String id, String text)>`.
//! - Upstream auto-cancels an empty list via `setTimeout(() => onCancel(), 100)`
//!   (user-message-selector.ts:146-149); the port cancels synchronously in
//!   the constructor and then drops the callbacks, so the dismissal fires
//!   exactly once (no timer thread needed).
//! - Upstream is a plain `Container` with no `Focusable`; the port carries a
//!   `focused` flag (via [`pir_tui::tui::Focusable`]) so the TUI's focus
//!   bookkeeping treats it uniformly with the other selectors. There is no
//!   inner search input to propagate focus to — upstream has none.

use std::sync::Arc;

use pir_tui::components::text::Text;
use pir_tui::tui::{Component, Focusable};
use pir_tui::utils::truncate_to_width;

use crate::core::themes::Theme;

use super::dynamic_border::DynamicBorder;

/// A selectable user message (upstream `UserMessageItem`,
/// user-message-selector.ts:5-9), minus the unused `timestamp`.
struct UserMessageItem {
    id: String,
    text: String,
}

/// `UserMessageSelectorComponent` (user-message-selector.ts:110-155): a
/// bordered "Fork from Message" list of user messages with up/down
/// navigation (wrapping), Enter to select and Escape to cancel. Renders 2
/// lines per message (truncated text + `Message N of M` metadata) plus a
/// blank separator, with a `(n/m)` scroll indicator when the window scrolls.
pub struct UserMessageSelectorComponent {
    theme: Arc<Theme>,
    messages: Vec<UserMessageItem>,
    selected_index: usize,
    max_visible: usize,
    #[allow(clippy::type_complexity)] // mirrors the upstream callback type
    on_select: Option<Box<dyn FnMut(&str) + Send>>,
    on_cancel: Option<Box<dyn FnMut() + Send>>,
    focused: bool,
}

impl UserMessageSelectorComponent {
    /// `constructor` (user-message-selector.ts:21-27, 113-150). The initial
    /// selection is `initial_selected_id` when present, else the most recent
    /// message; an empty list auto-cancels (see header note).
    pub fn new(
        theme: Arc<Theme>,
        items: Vec<(String, String)>,
        on_select: Box<dyn FnMut(&str) + Send>,
        on_cancel: Box<dyn FnMut() + Send>,
        initial_selected_id: Option<String>,
    ) -> Self {
        let messages: Vec<UserMessageItem> = items
            .into_iter()
            .map(|(id, text)| UserMessageItem { id, text })
            .collect();
        let initial_index = initial_selected_id
            .as_deref()
            .and_then(|id| messages.iter().position(|message| message.id == id));
        let selected_index = initial_index.unwrap_or_else(|| messages.len().saturating_sub(1));

        let mut component = Self {
            theme,
            messages,
            selected_index,
            max_visible: 10,
            on_select: Some(on_select),
            on_cancel: Some(on_cancel),
            focused: false,
        };
        if component.messages.is_empty() {
            component.cancel();
            component.on_select = None;
            component.on_cancel = None;
        }
        component
    }

    /// The selected message id, if any (test/integration helper).
    pub fn selected_id(&self) -> Option<&str> {
        self.messages
            .get(self.selected_index)
            .map(|message| message.id.as_str())
    }

    fn cancel(&mut self) {
        if let Some(on_cancel) = self.on_cancel.as_mut() {
            on_cancel();
        }
    }

    /// `UserMessageList.render` (user-message-selector.ts:33-79): the message
    /// list only — the container layout (borders, header) is rendered by
    /// [`Component::render`].
    fn render_list(&self, width: usize) -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();

        if self.messages.is_empty() {
            lines.push(self.theme.fg("muted", "  No user messages found"));
            return lines;
        }

        // Visible range with scrolling (user-message-selector.ts:41-46).
        let len = self.messages.len();
        let start_index = self
            .selected_index
            .saturating_sub(self.max_visible / 2)
            .min(len.saturating_sub(self.max_visible));
        let end_index = (start_index + self.max_visible).min(len);

        // Render visible messages (2 lines per message + blank line,
        // user-message-selector.ts:48-70).
        for (i, message) in self.messages[start_index..end_index].iter().enumerate() {
            let i = start_index + i;
            let is_selected = i == self.selected_index;

            // Normalize message to single line.
            let normalized_message = message.text.replace('\n', " ").trim().to_string();

            // First line: cursor + message.
            let cursor = if is_selected {
                self.theme.fg("accent", "› ")
            } else {
                "  ".to_string()
            };
            let max_msg_width = width.saturating_sub(2); // Account for cursor (2 chars)
            let truncated_msg = truncate_to_width(&normalized_message, max_msg_width, "", false);
            let message_line = if is_selected {
                format!("{cursor}{}", Theme::bold(&truncated_msg))
            } else {
                format!("{cursor}{truncated_msg}")
            };
            lines.push(message_line);

            // Second line: metadata (position in history).
            let metadata = format!("  Message {} of {len}", i + 1);
            lines.push(self.theme.fg("muted", &metadata));
            lines.push(String::new()); // Blank line between messages
        }

        // Scroll indicator (user-message-selector.ts:72-76).
        if start_index > 0 || end_index < len {
            lines.push(
                self.theme
                    .fg("muted", &format!("  ({}/{len})", self.selected_index + 1)),
            );
        }

        lines
    }

    fn border_line(&self, width: usize) -> String {
        let theme = Arc::clone(&self.theme);
        DynamicBorder::new(Box::new(move |s: &str| theme.fg("border", s)))
            .render(width)
            .pop()
            .unwrap_or_default()
    }
}

impl Component for UserMessageSelectorComponent {
    /// `render` of the container layout (user-message-selector.ts:121-144):
    /// header, border, list, bottom border. The message list itself is
    /// [`Self::render_list`]; the layout is exactly what the upstream
    /// `Container` children would render.
    fn render(&self, width: usize) -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();
        lines.push(String::new()); // Spacer(1)
        lines.extend(Text::new(Theme::bold("Fork from Message"), 1, 0, None).render(width));
        lines.extend(
            Text::new(
                self.theme.fg(
                    "muted",
                    "Select a user message to copy the active path up to that point into a new session",
                ),
                1,
                0,
                None,
            )
            .render(width),
        );
        lines.push(String::new()); // Spacer(1)
        lines.push(self.border_line(width)); // DynamicBorder
        lines.push(String::new()); // Spacer(1)
        lines.extend(self.render_list(width));
        lines.push(String::new()); // Spacer(1)
        lines.push(self.border_line(width)); // DynamicBorder
        lines
    }

    /// `UserMessageList.handleInput` (user-message-selector.ts:81-104):
    /// up/down wrap around, Enter selects (via the message id), Escape/ctrl+c
    /// cancels.
    fn handle_input(&mut self, data: &str) {
        if self.messages.is_empty() {
            // Already auto-cancelled at construction (user-message-selector.ts:146-149).
            return;
        }
        let kb = pir_tui::keybindings::get_keybindings();
        let read = kb.read().unwrap_or_else(|poisoned| poisoned.into_inner());
        if read.matches_id(data, "tui.select.up") {
            self.selected_index = if self.selected_index == 0 {
                self.messages.len() - 1
            } else {
                self.selected_index - 1
            };
        } else if read.matches_id(data, "tui.select.down") {
            self.selected_index = if self.selected_index == self.messages.len() - 1 {
                0
            } else {
                self.selected_index + 1
            };
        } else if read.matches_id(data, "tui.select.confirm") {
            if let Some(message) = self.messages.get(self.selected_index) {
                if let Some(on_select) = self.on_select.as_mut() {
                    on_select(&message.id);
                }
            }
        } else if read.matches_id(data, "tui.select.cancel") {
            self.cancel();
        }
    }

    fn invalidate(&mut self) {
        // No cached state to invalidate (user-message-selector.ts:29-31).
    }

    fn as_focusable(&self) -> Option<&dyn Focusable> {
        Some(self)
    }

    fn as_focusable_mut(&mut self) -> Option<&mut dyn Focusable> {
        Some(self)
    }
}

impl Focusable for UserMessageSelectorComponent {
    fn focused(&self) -> bool {
        self.focused
    }

    /// No inner search input to propagate to (upstream has none); the flag is
    /// carried for TUI focus bookkeeping only (see header note).
    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn theme() -> Arc<Theme> {
        Arc::new(crate::core::themes::load_theme("dark", None).expect("builtin dark theme"))
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

    /// Build a component over 3 items, returning the captured callbacks.
    #[allow(clippy::too_many_arguments)] // mirrors the upstream constructor
    #[allow(clippy::type_complexity)] // mirrors the upstream callback type
    fn selector(
        items: Vec<(&str, &str)>,
        initial: Option<&str>,
    ) -> (
        UserMessageSelectorComponent,
        Arc<Mutex<Option<String>>>,
        Arc<Mutex<u32>>,
    ) {
        let selected = Arc::new(Mutex::new(None));
        let cancelled = Arc::new(Mutex::new(0u32));
        let selected_thread = Arc::clone(&selected);
        let cancelled_thread = Arc::clone(&cancelled);
        let component = UserMessageSelectorComponent::new(
            theme(),
            items
                .into_iter()
                .map(|(id, text)| (id.to_string(), text.to_string()))
                .collect(),
            Box::new(move |id: &str| {
                *selected_thread.lock().unwrap() = Some(id.to_string());
            }),
            Box::new(move || {
                *cancelled_thread.lock().unwrap() += 1;
            }),
            initial.map(str::to_string),
        );
        (component, selected, cancelled)
    }

    #[test]
    fn defaults_to_most_recent_message_and_renders_layout() {
        crate::modes::interactive::interactive_mode::install_global_keybindings();
        let (component, _, _) =
            selector(vec![("1", "first"), ("2", "second"), ("3", "third")], None);
        assert_eq!(component.selected_id(), Some("3"));

        let lines: Vec<String> = component.render(80).iter().map(|l| strip_ansi(l)).collect();
        assert!(lines.iter().any(|l| l.contains("Fork from Message")));
        assert!(lines
            .iter()
            .any(|l| l.contains("Select a user message to copy the active path")));
        assert!(lines.iter().any(|l| l.contains('─')), "borders rendered");
        // Selected entry: accent cursor + bold text + metadata.
        let selected_line = lines
            .iter()
            .find(|l| l.starts_with("› "))
            .expect("cursor line");
        assert!(selected_line.contains("third"));
        assert!(lines.iter().any(|l| l.contains("Message 3 of 3")));
        assert!(lines.iter().any(|l| l.contains("Message 1 of 3")));
        // 8 items fit the window: no scroll indicator.
        assert!(
            !lines.iter().any(|l| l.contains("(3/3)")),
            "no scroll indicator when window fits"
        );
    }

    #[test]
    fn initial_selected_id_preselects_and_enter_selects() {
        crate::modes::interactive::interactive_mode::install_global_keybindings();
        let (mut component, selected, _) = selector(
            vec![("1", "first"), ("2", "second"), ("3", "third")],
            Some("2"),
        );
        assert_eq!(component.selected_id(), Some("2"));

        component.handle_input("\r"); // tui.select.confirm
        assert_eq!(selected.lock().unwrap().as_deref(), Some("2"));
    }

    #[test]
    fn up_and_down_wrap_around() {
        crate::modes::interactive::interactive_mode::install_global_keybindings();
        let (mut component, selected, _) =
            selector(vec![("1", "first"), ("2", "second"), ("3", "third")], None);
        // Default selection is the last message; down wraps to the first.
        component.handle_input("\x1b[B");
        assert_eq!(component.selected_id(), Some("1"));
        // Up from the first wraps to the last.
        component.handle_input("\x1b[A");
        assert_eq!(component.selected_id(), Some("3"));
        component.handle_input("\r");
        assert_eq!(selected.lock().unwrap().as_deref(), Some("3"));
    }

    #[test]
    fn escape_and_ctrl_c_cancel() {
        crate::modes::interactive::interactive_mode::install_global_keybindings();
        let (mut component, _, cancelled) = selector(vec![("1", "first"), ("2", "second")], None);
        component.handle_input("\x1b");
        assert_eq!(*cancelled.lock().unwrap(), 1);
        component.handle_input("\x03");
        assert_eq!(*cancelled.lock().unwrap(), 2);
    }

    #[test]
    fn empty_list_auto_cancels_and_renders_empty_message() {
        crate::modes::interactive::interactive_mode::install_global_keybindings();
        let (mut component, _, cancelled) = selector(vec![], None);
        assert_eq!(*cancelled.lock().unwrap(), 1, "constructor auto-cancels");

        let lines: Vec<String> = component.render(80).iter().map(|l| strip_ansi(l)).collect();
        assert!(lines.iter().any(|l| l.contains("No user messages found")));
        // Callbacks dropped after the auto-cancel: later keys are inert.
        component.handle_input("\x1b");
        component.handle_input("\r");
        assert_eq!(*cancelled.lock().unwrap(), 1);
    }

    #[test]
    fn multiline_messages_are_normalized_and_long_text_truncated() {
        crate::modes::interactive::interactive_mode::install_global_keybindings();
        let long = "x".repeat(200);
        let (component, _, _) = selector(vec![("1", "line1\nline2"), ("2", &long)], None);
        let lines = component.render(40);

        let normalized = lines.iter().find(|l| strip_ansi(l).contains("line1 line2"));
        assert!(normalized.is_some(), "newlines collapse to a single space");
        for line in &lines {
            assert!(
                pir_tui::utils::visible_width(line) <= 40,
                "rendered line overflowed: {:?}",
                strip_ansi(line)
            );
        }
    }

    #[test]
    fn scroll_indicator_appears_when_messages_overflow_window() {
        crate::modes::interactive::interactive_mode::install_global_keybindings();
        let items: Vec<(String, String)> = (1..=15)
            .map(|i| (format!("{i}"), "msg".to_string()))
            .collect();
        let (mut component, _, _) = selector(
            items
                .iter()
                .map(|(id, text)| (id.as_str(), text.as_str()))
                .collect(),
            None,
        );
        assert_eq!(component.selected_id(), Some("15"));

        let lines: Vec<String> = component.render(80).iter().map(|l| strip_ansi(l)).collect();
        assert!(
            lines.iter().any(|l| l.contains("  (15/15)")),
            "scrolled to the bottom"
        );
        // Move up out of the tail window: indicator still present, position updates.
        for _ in 0..9 {
            component.handle_input("\x1b[A");
        }
        let lines: Vec<String> = component.render(80).iter().map(|l| strip_ansi(l)).collect();
        assert!(
            lines.iter().any(|l| l.contains("  (6/15)")),
            "scroll position tracked"
        );
    }

    #[test]
    fn focus_flag_is_carried() {
        let (mut component, _, _) = selector(vec![("1", "first")], None);
        assert!(!component.focused());
        component.set_focused(true);
        assert!(component.focused());
        component.set_focused(false);
        assert!(!component.focused());
    }
}
