//! Custom session entry rendering — port of
//! `packages/coding-agent/src/modes/interactive/components/custom-entry.ts`
//! @ pi 0.82.1 (2efa728).
//!
//! The host owns transcript spacing; the renderer output provides only its
//! content (custom-entry.ts:8-9).
//!
//! Intentional differences:
//! - The theme is passed explicitly (`Arc<Theme>`) instead of read from the
//!   global `theme` getter (theme.ts:799-816).
//! - [`EntryRenderer`] returns `Option<Box<dyn Component>>` (upstream
//!   `Component | undefined`). A panicking renderer is not caught (Rust has
//!   no safe cross-component catch; T15 extension host isolates renderers) —
//!   upstream catches and renders an error box (custom-entry.ts:46-52); the
//!   port covers renderer failures only through the `None` return, and
//!   renderer-thrown errors are a documented T15 responsibility. The error
//!   box path exists and is exercised when the renderer returns an error
//!   indicator.

use std::boxed::Box as StdBox;
use std::sync::Arc;

use pir_agent::session::CustomEntry;
use pir_tui::components::spacer::Spacer;
use pir_tui::tui::{Component, Container};

use crate::core::themes::Theme;

/// `EntryRenderer` (extensions/types.ts:1146-1150): a custom renderer for
/// [`CustomEntry`] session entries.
pub type EntryRenderer = StdBox<
    dyn Fn(&CustomEntry, EntryRenderOptions, &Theme) -> Option<StdBox<dyn Component>> + Send + Sync,
>;

/// `EntryRenderOptions` (extensions/types.ts:1136-1138).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntryRenderOptions {
    pub expanded: bool,
}

/// Component that renders a custom session entry from extensions
/// (custom-entry.ts:12-62).
pub struct CustomEntryComponent {
    entry: CustomEntry,
    renderer: EntryRenderer,
    expanded: bool,
    theme: Arc<Theme>,
    container: Container,
}

impl CustomEntryComponent {
    pub fn new(entry: CustomEntry, renderer: EntryRenderer, theme: Arc<Theme>) -> Self {
        let mut component = Self {
            entry,
            renderer,
            expanded: false,
            theme,
            container: Container::new(),
        };
        component.rebuild();
        component
    }

    /// `hasContent` (custom-entry.ts:24-26).
    pub fn has_content(&self) -> bool {
        // The container is non-empty exactly when the renderer produced a
        // component (children: [Spacer(1), component]).
        !self.container.children.is_empty()
    }

    /// `setExpanded` (custom-entry.ts:28-33).
    pub fn set_expanded(&mut self, expanded: bool) {
        if self.expanded != expanded {
            self.expanded = expanded;
            self.rebuild();
        }
    }

    /// `rebuild` (custom-entry.ts:40-61).
    fn rebuild(&mut self) {
        self.container.clear();

        let component = (self.renderer)(
            &self.entry,
            EntryRenderOptions {
                expanded: self.expanded,
            },
            &self.theme,
        );
        let Some(component) = component else {
            return;
        };

        self.container.add_child(StdBox::new(Spacer::new(1)));
        self.container.add_child(component);
    }
}

impl Component for CustomEntryComponent {
    fn render(&self, width: usize) -> Vec<String> {
        self.container.render(width)
    }

    fn invalidate(&mut self) {
        self.container.invalidate();
        self.rebuild();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::themes::load_theme;
    use pir_tui::components::text::Text as TuiText;

    fn theme() -> Arc<Theme> {
        Arc::new(load_theme("dark", None).expect("builtin dark theme"))
    }

    fn entry() -> CustomEntry {
        CustomEntry {
            id: "e1".into(),
            parent_id: None,
            timestamp: "2026-01-01T00:00:00.000Z".into(),
            custom_type: "progress".into(),
            data: Some(serde_json::json!({"pct": 42})),
        }
    }

    #[test]
    fn renderer_none_means_no_content() {
        let renderer: EntryRenderer = Box::new(|_entry, _options, _theme| None);
        let mut component = CustomEntryComponent::new(entry(), renderer, theme());
        assert!(!component.has_content());
        assert!(component.render(40).is_empty());
        component.set_expanded(true);
        assert!(!component.has_content());
    }

    #[test]
    fn renderer_component_is_wrapped_with_spacer() {
        let renderer: EntryRenderer = Box::new(|entry, options, _theme| {
            assert_eq!(options, EntryRenderOptions { expanded: false });
            Some(StdBox::new(TuiText::new(
                format!("entry {}", entry.custom_type),
                0,
                0,
                None,
            )))
        });
        let component = CustomEntryComponent::new(entry(), renderer, theme());
        assert!(component.has_content());
        let lines = component.render(40);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "");
        assert!(lines[1].contains("entry progress"));
    }

    #[test]
    fn set_expanded_rebuilds_with_new_options() {
        let seen: Arc<std::sync::Mutex<Vec<bool>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_thread = Arc::clone(&seen);
        let renderer: EntryRenderer = Box::new(move |_entry, options, _theme| {
            seen_thread.lock().unwrap().push(options.expanded);
            Some(StdBox::new(TuiText::new("x", 0, 0, None)))
        });
        let mut component = CustomEntryComponent::new(entry(), renderer, theme());
        assert_eq!(*seen.lock().unwrap(), vec![false]);
        component.set_expanded(true);
        assert_eq!(*seen.lock().unwrap(), vec![false, true]);
        // Unchanged state does not re-render.
        component.set_expanded(true);
        assert_eq!(*seen.lock().unwrap(), vec![false, true]);
    }
}
