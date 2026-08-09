//! Port of `show-images-selector.ts` @ pi 0.82.1 (2efa728) — T12-S5a.
//!
//! Intentional differences:
//! - The theme is injected (`Arc<Theme>`) instead of read from the global
//!   `theme` getter (theme.ts:799-816); the [`DynamicBorder`] color fn is
//!   explicit (dynamic-border.ts:14).
//! - Callbacks are constructor args (`Box<dyn FnMut ... + Send>`) instead of
//!   `SelectList` property assignments.

use std::sync::Arc;

use rpi_tui::components::select_list::{
    SelectItem, SelectList, SelectListLayoutOptions, SelectListTheme,
};
use rpi_tui::tui::Component;

use crate::core::themes::Theme;
use crate::modes::interactive::components::dynamic_border::DynamicBorder;

/// `SHOW_IMAGES_SELECT_LIST_LAYOUT` (show-images-selector.ts:5-8).
const SHOW_IMAGES_SELECT_LIST_LAYOUT: SelectListLayoutOptions = SelectListLayoutOptions {
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

/// Component that renders a show-images selector with borders
/// (show-images-selector.ts:13-50).
pub struct ShowImagesSelectorComponent {
    select_list: SelectList,
    top_border: DynamicBorder,
    bottom_border: DynamicBorder,
}

impl ShowImagesSelectorComponent {
    /// `constructor` (show-images-selector.ts:16-45).
    pub fn new(
        theme: Arc<Theme>,
        current_value: bool,
        mut on_select: Box<dyn FnMut(bool) + Send>,
        on_cancel: Box<dyn FnMut() + Send>,
    ) -> Self {
        // show-images-selector.ts:19-22.
        let items: Vec<SelectItem> = vec![
            SelectItem {
                value: "yes".to_string(),
                label: "Yes".to_string(),
                description: Some("Show images inline in terminal".to_string()),
            },
            SelectItem {
                value: "no".to_string(),
                label: "No".to_string(),
                description: Some("Show text placeholder instead".to_string()),
            },
        ];

        let mut select_list = SelectList::new(
            items,
            5,
            select_list_theme(&theme),
            Some(SHOW_IMAGES_SELECT_LIST_LAYOUT),
        );

        // Preselect current value (show-images-selector.ts:31).
        select_list.set_selected_index(if current_value { 0 } else { 1 });

        select_list.on_select = Some(Box::new(move |item| {
            on_select(item.value == "yes");
        }));
        select_list.on_cancel = Some(on_cancel);

        Self {
            select_list,
            top_border: DynamicBorder::new(border_color(&theme)),
            bottom_border: DynamicBorder::new(border_color(&theme)),
        }
    }

    /// `getSelectList` (show-images-selector.ts:47-49).
    pub fn get_select_list(&self) -> &SelectList {
        &self.select_list
    }
}

impl Component for ShowImagesSelectorComponent {
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
        current: bool,
    ) -> (
        ShowImagesSelectorComponent,
        Arc<Mutex<Vec<bool>>>,
        Arc<Mutex<usize>>,
    ) {
        install_global_keybindings();
        let selected: Arc<Mutex<Vec<bool>>> = Arc::new(Mutex::new(Vec::new()));
        let cancelled: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
        let selected_cb = Arc::clone(&selected);
        let cancelled_cb = Arc::clone(&cancelled);
        let component = ShowImagesSelectorComponent::new(
            theme(),
            current,
            Box::new(move |show| selected_cb.lock().unwrap().push(show)),
            Box::new(move || *cancelled_cb.lock().unwrap() += 1),
        );
        (component, selected, cancelled)
    }

    #[test]
    fn confirm_selects_currently_highlighted_value() {
        let (mut component, selected, _cancelled) = setup(true);
        component.handle_input("\r");
        // Current value (Yes) is preselected.
        assert_eq!(*selected.lock().unwrap(), vec![true]);
        component.handle_input("\x1b[B");
        component.handle_input("\r");
        assert_eq!(*selected.lock().unwrap(), vec![true, false]);
    }

    #[test]
    fn no_is_preselected_when_images_are_off() {
        let (mut component, selected, _cancelled) = setup(false);
        component.handle_input("\r");
        assert_eq!(*selected.lock().unwrap(), vec![false]);
    }

    #[test]
    fn escape_cancels() {
        let (mut component, _selected, cancelled) = setup(true);
        component.handle_input("\x1b");
        assert_eq!(*cancelled.lock().unwrap(), 1);
    }

    #[test]
    fn render_shows_both_options_and_descriptions() {
        let (component, _selected, _cancelled) = setup(true);
        let rendered = component.render(60).join("\n");
        assert!(rendered.contains("Yes"));
        assert!(rendered.contains("No"));
        assert!(rendered.contains("Show images inline in terminal"));
        assert!(rendered.contains("Show text placeholder instead"));
    }
}
