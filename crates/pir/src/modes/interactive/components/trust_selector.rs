//! Port of `trust-selector.ts` @ pi 0.82.1 (2efa728) — T12-S5a.
//!
//! Intentional differences:
//! - The theme is injected (`Arc<Theme>`) instead of read from the global
//!   `theme` getter (theme.ts:799-816); the [`DynamicBorder`] color fn is
//!   explicit (dynamic-border.ts:14).
//! - `getProjectTrustOptions(cwd)` (trust-selector.ts:44) has no local
//!   equivalent in `crate::core::trust_manager`; the integration layer
//!   assembles the options from the local trust manager and passes them in.
//!   [`TrustOption`] mirrors the visible fields of the upstream
//!   `ProjectTrustOption` (`trusted`/`updates` are folded into `value` by
//!   the integration layer).
//! - The header info lines (cwd, saved decision, current session,
//!   trust-selector.ts:53-70) and the saved-option preselect with its "✓"
//!   marker (trust-selector.ts:45-48, 108-110) are dropped: the constructor
//!   contract carries only the options, and `SelectList` has no per-item
//!   checkmark. The integration layer can encode the current state in the
//!   option descriptions.
//! - The list is a [`SelectList`] (the task's component-library choice)
//!   instead of the upstream hand-rolled rows, so the raw `k`/`j`/`\n` keys
//!   (trust-selector.ts:119, 122, 125) are not handled — only the
//!   `tui.select.*` keybindings.

use std::sync::Arc;

use pir_tui::components::select_list::{SelectItem, SelectList, SelectListTheme};
use pir_tui::components::spacer::Spacer;
use pir_tui::components::text::Text;
use pir_tui::tui::Component;

use crate::core::themes::Theme;
use crate::modes::interactive::components::dynamic_border::DynamicBorder;
use crate::modes::interactive::components::keybinding_hints::{key_hint, raw_key_hint};

/// One trust option (upstream `ProjectTrustOption`, trust-manager.ts). The
/// `value` carries the selection the integration layer interprets (upstream
/// `{ trusted, updates }`, trust-selector.ts:11).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustOption {
    pub value: String,
    pub label: String,
    pub description: Option<String>,
}

/// `(text) => theme.fg("border", text)` (dynamic-border.ts:14).
fn border_color(theme: &Arc<Theme>) -> Box<dyn Fn(&str) -> String + Send + Sync> {
    let theme = theme.clone();
    Box::new(move |text| theme.fg("border", text))
}

/// `getSelectListTheme` (theme.ts:1269-1277): accent selection, muted
/// descriptions/scroll/no-match.
fn select_list_theme(theme: &Arc<Theme>) -> Arc<SelectListTheme> {
    let selected_prefix = theme.clone();
    let selected_text = theme.clone();
    let description = theme.clone();
    let scroll_info = theme.clone();
    let no_match = theme.clone();
    Arc::new(SelectListTheme {
        selected_prefix: Box::new(move |text| selected_prefix.fg("accent", text)),
        selected_text: Box::new(move |text| selected_text.fg("accent", text)),
        description: Box::new(move |text| description.fg("muted", text)),
        scroll_info: Box::new(move |text| scroll_info.fg("muted", text)),
        no_match: Box::new(move |text| no_match.fg("muted", text)),
    })
}

/// Component that renders the project-trust selector
/// (trust-selector.ts:32-134).
pub struct TrustSelectorComponent {
    theme: Arc<Theme>,
    select_list: SelectList,
    top_border: DynamicBorder,
    bottom_border: DynamicBorder,
}

impl TrustSelectorComponent {
    /// `constructor` (trust-selector.ts:40-90), minus the trust-manager
    /// lookup: options are assembled by the integration layer.
    pub fn new(
        theme: Arc<Theme>,
        options: Vec<TrustOption>,
        mut on_select: Box<dyn FnMut(&str) + Send>,
        on_cancel: Box<dyn FnMut() + Send>,
    ) -> Self {
        let items: Vec<SelectItem> = options
            .into_iter()
            .map(|option| SelectItem {
                value: option.value,
                label: option.label,
                description: option.description,
            })
            .collect();
        // All options fit on screen (upstream renders every row).
        let max_visible = items.len().max(1);
        let mut select_list = SelectList::new(items, max_visible, select_list_theme(&theme), None);
        select_list.on_select = Some(Box::new(move |item| {
            let value = item.value.clone();
            on_select(&value);
        }));
        select_list.on_cancel = Some(on_cancel);

        let top_border = DynamicBorder::new(border_color(&theme));
        let bottom_border = DynamicBorder::new(border_color(&theme));
        Self {
            theme,
            select_list,
            top_border,
            bottom_border,
        }
    }

    /// The inner select list.
    pub fn get_select_list(&self) -> &SelectList {
        &self.select_list
    }
}

impl Component for TrustSelectorComponent {
    fn render(&self, width: usize) -> Vec<String> {
        let mut lines = Vec::new();
        lines.extend(self.top_border.render(width));
        lines.extend(Spacer::new(1).render(width));
        // Title (trust-selector.ts:54).
        let title = self.theme.fg("accent", &Theme::bold("Project trust"));
        lines.extend(Text::new(title, 1, 0, None).render(width));
        lines.extend(Spacer::new(1).render(width));
        lines.extend(self.select_list.render(width));
        lines.extend(Spacer::new(1).render(width));
        // Key hints (trust-selector.ts:76-85).
        let hints = format!(
            "{}{}{}{}{}",
            raw_key_hint(&self.theme, "↑↓", "navigate"),
            "  ",
            key_hint(&self.theme, "tui.select.confirm", "save"),
            "  ",
            key_hint(&self.theme, "tui.select.cancel", "cancel"),
        );
        lines.extend(Text::new(hints, 1, 0, None).render(width));
        lines.extend(Spacer::new(1).render(width));
        lines.extend(self.bottom_border.render(width));
        lines
    }

    fn handle_input(&mut self, data: &str) {
        self.select_list.handle_input(data);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::themes::load_theme;
    use crate::modes::interactive::interactive_mode::install_global_keybindings;
    use std::sync::Mutex;

    fn theme() -> Arc<Theme> {
        Arc::new(load_theme("dark", None).expect("builtin dark theme"))
    }

    #[allow(clippy::type_complexity)] // mirrors the upstream callback type
    fn trust_options() -> Vec<TrustOption> {
        vec![
            TrustOption {
                value: "trust".into(),
                label: "Trust".into(),
                description: Some("Trust this folder".into()),
            },
            TrustOption {
                value: "untrust".into(),
                label: "Do not trust".into(),
                description: Some("Do not trust this folder".into()),
            },
        ]
    }

    #[allow(clippy::too_many_arguments)] // mirrors the upstream constructor
    #[allow(clippy::type_complexity)] // mirrors the upstream callback type
    fn setup() -> (
        TrustSelectorComponent,
        Arc<Mutex<Vec<String>>>,
        Arc<Mutex<usize>>,
    ) {
        install_global_keybindings();
        let selected: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let cancelled: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
        let selected_cb = Arc::clone(&selected);
        let cancelled_cb = Arc::clone(&cancelled);
        let component = TrustSelectorComponent::new(
            theme(),
            trust_options(),
            Box::new(move |value| selected_cb.lock().unwrap().push(value.to_string())),
            Box::new(move || *cancelled_cb.lock().unwrap() += 1),
        );
        (component, selected, cancelled)
    }

    #[test]
    fn confirm_selects_the_highlighted_option() {
        let (mut component, selected, _cancelled) = setup();
        component.handle_input("\r");
        assert_eq!(*selected.lock().unwrap(), vec!["trust"]);
        component.handle_input("\x1b[B");
        component.handle_input("\r");
        assert_eq!(*selected.lock().unwrap(), vec!["trust", "untrust"]);
    }

    #[test]
    fn selection_wraps_around() {
        let (mut component, selected, _cancelled) = setup();
        // Up at the top wraps to the last option.
        component.handle_input("\x1b[A");
        component.handle_input("\r");
        assert_eq!(*selected.lock().unwrap(), vec!["untrust"]);
    }

    #[test]
    fn escape_cancels() {
        let (mut component, _selected, cancelled) = setup();
        component.handle_input("\x1b");
        assert_eq!(*cancelled.lock().unwrap(), 1);
    }

    #[test]
    fn render_shows_title_options_and_hints() {
        let (component, _selected, _cancelled) = setup();
        let rendered = component.render(60).join("\n");
        assert!(rendered.contains("Project trust"));
        assert!(rendered.contains("Trust"));
        assert!(rendered.contains("Do not trust"));
        assert!(rendered.contains("Trust this folder"));
        assert!(rendered.contains("navigate"));
        assert!(rendered.contains("save"));
        assert!(rendered.contains("cancel"));
    }
}
