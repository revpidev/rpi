//! Port of `thinking-selector.ts` @ pi 0.82.1 (2efa728) — T12-S5a.
//!
//! Intentional differences:
//! - The theme is injected (`Arc<Theme>`) instead of read from the global
//!   `theme` getter (theme.ts:799-816); the [`DynamicBorder`] color fn is
//!   explicit (dynamic-border.ts:14).
//! - The current level is marked with a "✓" prefix in its description (per
//!   the T12-S5a task contract); upstream only preselects it and shows the
//!   plain level description.
//! - Callbacks are constructor args (`Box<dyn FnMut ... + Send>`) instead of
//!   `SelectList` property assignments.

use std::sync::Arc;

use rpi_agent::types::ThinkingLevel;
use rpi_tui::components::select_list::{
    SelectItem, SelectList, SelectListLayoutOptions, SelectListTheme,
};
use rpi_tui::tui::Component;

use crate::core::themes::Theme;
use crate::modes::interactive::components::dynamic_border::DynamicBorder;

/// `THINKING_SELECT_LIST_LAYOUT` (thinking-selector.ts:6-9).
const THINKING_SELECT_LIST_LAYOUT: SelectListLayoutOptions = SelectListLayoutOptions {
    min_primary_column_width: Some(12),
    max_primary_column_width: Some(32),
    truncate_primary: None,
};

/// `LEVEL_DESCRIPTIONS` (thinking-selector.ts:11-19): value string and
/// description per level. The upstream `Record` has 7 entries (`off` through
/// `max`); the T12-S5a task brief said "8 档", the pinned upstream file has
/// 7.
pub const THINKING_LEVEL_DESCRIPTIONS: [(&str, &str); 7] = [
    ("off", "No reasoning"),
    ("minimal", "Very brief reasoning (~1k tokens)"),
    ("low", "Light reasoning (~2k tokens)"),
    ("medium", "Moderate reasoning (~8k tokens)"),
    ("high", "Deep reasoning (~16k tokens)"),
    ("xhigh", "Extra-high reasoning (~32k tokens)"),
    ("max", "Maximum reasoning"),
];

/// Description for a level value (upstream `LEVEL_DESCRIPTIONS[level]`).
fn level_description(level: &str) -> Option<&'static str> {
    THINKING_LEVEL_DESCRIPTIONS
        .iter()
        .find(|(value, _)| *value == level)
        .map(|(_, description)| *description)
}

/// Parse a level value string back into a [`ThinkingLevel`].
fn thinking_level_from_str(value: &str) -> Option<ThinkingLevel> {
    match value {
        "off" => Some(ThinkingLevel::Off),
        "minimal" => Some(ThinkingLevel::Minimal),
        "low" => Some(ThinkingLevel::Low),
        "medium" => Some(ThinkingLevel::Medium),
        "high" => Some(ThinkingLevel::High),
        "xhigh" => Some(ThinkingLevel::Xhigh),
        "max" => Some(ThinkingLevel::Max),
        _ => None,
    }
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

/// Component that renders a thinking level selector with borders
/// (thinking-selector.ts:24-75).
pub struct ThinkingSelectorComponent {
    select_list: SelectList,
    top_border: DynamicBorder,
    bottom_border: DynamicBorder,
}

impl ThinkingSelectorComponent {
    /// `constructor` (thinking-selector.ts:27-70). `available_levels` is
    /// injected (the integration layer derives it from the model's
    /// capabilities and includes the default level, e.g. `Off`).
    pub fn new(
        theme: Arc<Theme>,
        current_level: ThinkingLevel,
        available_levels: Vec<ThinkingLevel>,
        mut on_select: Box<dyn FnMut(ThinkingLevel) + Send>,
        on_cancel: Box<dyn FnMut() + Send>,
    ) -> Self {
        // thinking-selector.ts:35-39.
        let thinking_levels: Vec<SelectItem> = available_levels
            .iter()
            .map(|level| {
                let value = level.as_str();
                SelectItem {
                    value: value.to_string(),
                    label: value.to_string(),
                    description: level_description(value).map(|description| {
                        if *level == current_level {
                            format!("✓ {description}")
                        } else {
                            description.to_string()
                        }
                    }),
                }
            })
            .collect();

        // All levels fit on screen (upstream passes `thinkingLevels.length`,
        // thinking-selector.ts:46-50).
        let max_visible = thinking_levels.len().max(1);
        let mut select_list = SelectList::new(
            thinking_levels,
            max_visible,
            select_list_theme(&theme),
            Some(THINKING_SELECT_LIST_LAYOUT),
        );

        // Preselect current level (thinking-selector.ts:52-56).
        if let Some(current_index) = available_levels
            .iter()
            .position(|level| *level == current_level)
        {
            select_list.set_selected_index(current_index);
        }

        select_list.on_select = Some(Box::new(move |item| {
            if let Some(level) = thinking_level_from_str(&item.value) {
                on_select(level);
            }
        }));
        select_list.on_cancel = Some(on_cancel);

        Self {
            select_list,
            top_border: DynamicBorder::new(border_color(&theme)),
            bottom_border: DynamicBorder::new(border_color(&theme)),
        }
    }

    /// `getSelectList` (thinking-selector.ts:72-74).
    pub fn get_select_list(&self) -> &SelectList {
        &self.select_list
    }
}

impl Component for ThinkingSelectorComponent {
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
        current: ThinkingLevel,
    ) -> (
        ThinkingSelectorComponent,
        Arc<Mutex<Vec<ThinkingLevel>>>,
        Arc<Mutex<usize>>,
    ) {
        install_global_keybindings();
        let selected: Arc<Mutex<Vec<ThinkingLevel>>> = Arc::new(Mutex::new(Vec::new()));
        let cancelled: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
        let selected_cb = Arc::clone(&selected);
        let cancelled_cb = Arc::clone(&cancelled);
        // All 7 upstream levels (thinking-selector.ts:11-19).
        let all_levels = vec![
            ThinkingLevel::Off,
            ThinkingLevel::Minimal,
            ThinkingLevel::Low,
            ThinkingLevel::Medium,
            ThinkingLevel::High,
            ThinkingLevel::Xhigh,
            ThinkingLevel::Max,
        ];
        let component = ThinkingSelectorComponent::new(
            theme(),
            current,
            all_levels,
            Box::new(move |level| selected_cb.lock().unwrap().push(level)),
            Box::new(move || *cancelled_cb.lock().unwrap() += 1),
        );
        (component, selected, cancelled)
    }

    #[test]
    fn confirm_selects_currently_highlighted_level() {
        let (mut component, selected, _cancelled) = setup(ThinkingLevel::Low);
        component.handle_input("\r");
        // Current level is preselected, so confirm picks "low" first.
        assert_eq!(*selected.lock().unwrap(), vec![ThinkingLevel::Low]);
        component.handle_input("\x1b[A");
        component.handle_input("\r");
        assert_eq!(
            *selected.lock().unwrap(),
            vec![ThinkingLevel::Low, ThinkingLevel::Minimal]
        );
    }

    #[test]
    fn escape_cancels() {
        let (mut component, _selected, cancelled) = setup(ThinkingLevel::Off);
        component.handle_input("\x1b");
        assert_eq!(*cancelled.lock().unwrap(), 1);
    }

    #[test]
    fn render_lists_descriptions_and_marks_current_level() {
        let (component, _selected, _cancelled) = setup(ThinkingLevel::Medium);
        let rendered = component.render(60).join("\n");
        // Only the injected levels render; inject all 7 to cover the table.
        for (value, description) in THINKING_LEVEL_DESCRIPTIONS {
            assert!(rendered.contains(value), "level {value} missing");
            assert!(
                rendered.contains(description),
                "description {description} missing"
            );
        }
        // The current level is marked with a ✓.
        let current_desc = THINKING_LEVEL_DESCRIPTIONS
            .iter()
            .find(|(value, _)| *value == "medium")
            .map(|(_, description)| *description)
            .unwrap();
        assert!(rendered.contains(&format!("✓ {current_desc}")));
    }
}
