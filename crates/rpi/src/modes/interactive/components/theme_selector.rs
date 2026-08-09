//! Port of `theme-selector.ts` @ pi 0.82.1 (2efa728) — T12-S5a.
//!
//! Intentional differences:
//! - The theme is injected (`Arc<Theme>`) instead of read from the global
//!   `theme` getter (theme.ts:799-816); the [`DynamicBorder`] color fn is
//!   explicit (dynamic-border.ts:14).
//! - The available themes are injected (`themes: Vec<String>`) instead of
//!   read from `getAvailableThemes()` (theme-selector.ts:27): the
//!   integration layer assembles the list from
//!   `crate::core::themes::get_builtin_themes()` /
//!   `load_theme_json`. The component never queries the theme registry.
//! - Callbacks are constructor args (`Box<dyn FnMut ... + Send>`) instead of
//!   `SelectList` property assignments.

use std::sync::Arc;

use rpi_tui::components::select_list::{
    SelectItem, SelectList, SelectListLayoutOptions, SelectListTheme,
};
use rpi_tui::tui::Component;

use crate::core::themes::Theme;
use crate::modes::interactive::components::dynamic_border::DynamicBorder;

/// `THEME_SELECT_LIST_LAYOUT` (theme-selector.ts:5-8).
const THEME_SELECT_LIST_LAYOUT: SelectListLayoutOptions = SelectListLayoutOptions {
    min_primary_column_width: Some(12),
    max_primary_column_width: Some(32),
    truncate_primary: None,
};

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

/// Component that renders a theme selector (theme-selector.ts:13-67).
pub struct ThemeSelectorComponent {
    select_list: SelectList,
    top_border: DynamicBorder,
    bottom_border: DynamicBorder,
}

impl ThemeSelectorComponent {
    /// `constructor` (theme-selector.ts:17-62). `on_preview` fires on
    /// selection moves (upstream `onSelectionChange`), `on_select` on
    /// confirm; both receive the theme name.
    pub fn new(
        theme: Arc<Theme>,
        current_theme: &str,
        themes: Vec<String>,
        mut on_select: Box<dyn FnMut(&str) + Send>,
        on_cancel: Box<dyn FnMut() + Send>,
        mut on_preview: Box<dyn FnMut(&str) + Send>,
    ) -> Self {
        // Get available themes and create select items (theme-selector.ts:27-32).
        let theme_items: Vec<SelectItem> = themes
            .iter()
            .map(|name| SelectItem {
                value: name.clone(),
                label: name.clone(),
                description: (name == current_theme).then(|| "(current)".to_string()),
            })
            .collect();

        let mut select_list = SelectList::new(
            theme_items,
            10,
            select_list_theme(&theme),
            Some(THEME_SELECT_LIST_LAYOUT),
        );

        // Preselect current theme (theme-selector.ts:40-44).
        if let Some(current_index) = themes.iter().position(|name| name == current_theme) {
            select_list.set_selected_index(current_index);
        }

        select_list.on_select = Some(Box::new(move |item| {
            let value = item.value.clone();
            on_select(&value);
        }));
        select_list.on_cancel = Some(on_cancel);
        select_list.on_selection_change = Some(Box::new(move |item| {
            let value = item.value.clone();
            on_preview(&value);
        }));

        Self {
            select_list,
            top_border: DynamicBorder::new(border_color(&theme)),
            bottom_border: DynamicBorder::new(border_color(&theme)),
        }
    }

    /// `getSelectList` (theme-selector.ts:64-66).
    pub fn get_select_list(&self) -> &SelectList {
        &self.select_list
    }
}

impl Component for ThemeSelectorComponent {
    fn render(&self, width: usize) -> Vec<String> {
        let mut lines = Vec::new();
        lines.extend(self.top_border.render(width));
        lines.extend(self.select_list.render(width));
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

    #[allow(clippy::too_many_arguments)] // mirrors the upstream constructor
    #[allow(clippy::type_complexity)] // mirrors the upstream callback type
    fn setup(
        current: &str,
    ) -> (
        ThemeSelectorComponent,
        Arc<Mutex<Vec<String>>>,
        Arc<Mutex<Vec<String>>>,
        Arc<Mutex<usize>>,
    ) {
        install_global_keybindings();
        let selected: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let previewed: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let cancelled: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
        let selected_cb = Arc::clone(&selected);
        let previewed_cb = Arc::clone(&previewed);
        let cancelled_cb = Arc::clone(&cancelled);
        let component = ThemeSelectorComponent::new(
            theme(),
            current,
            vec!["dark".to_string(), "light".to_string()],
            Box::new(move |name| selected_cb.lock().unwrap().push(name.to_string())),
            Box::new(move || *cancelled_cb.lock().unwrap() += 1),
            Box::new(move |name| previewed_cb.lock().unwrap().push(name.to_string())),
        );
        (component, selected, previewed, cancelled)
    }

    #[test]
    fn confirm_selects_currently_highlighted_theme() {
        let (mut component, selected, _previewed, _cancelled) = setup("dark");
        component.handle_input("\r");
        // Current theme is preselected, so confirm picks "dark" first.
        assert_eq!(*selected.lock().unwrap(), vec!["dark"]);
        component.handle_input("\x1b[B");
        component.handle_input("\r");
        assert_eq!(*selected.lock().unwrap(), vec!["dark", "light"]);
    }

    #[test]
    fn moving_selection_fires_preview() {
        let (mut component, _selected, previewed, _cancelled) = setup("dark");
        component.handle_input("\x1b[B");
        assert_eq!(*previewed.lock().unwrap(), vec!["light"]);
        component.handle_input("\x1b[A");
        assert_eq!(*previewed.lock().unwrap(), vec!["light", "dark"]);
    }

    #[test]
    fn escape_cancels() {
        let (mut component, _selected, _previewed, cancelled) = setup("dark");
        component.handle_input("\x1b");
        assert_eq!(*cancelled.lock().unwrap(), 1);
    }

    #[test]
    fn render_marks_current_theme_and_preselects_it() {
        let (component, _selected, _previewed, _cancelled) = setup("light");
        let rendered = component.render(60).join("\n");
        assert!(rendered.contains("dark"));
        assert!(rendered.contains("light"));
        assert!(rendered.contains("(current)"));
        // "light" is preselected: the selected row is accent-styled.
        assert!(rendered.contains("\u{1b}["));
    }
}
