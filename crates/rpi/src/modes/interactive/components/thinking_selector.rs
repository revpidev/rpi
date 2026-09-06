//! Port of `thinking-selector.ts` @ pi 0.85.0+ (9841914) — V14-12 FR-D.
//!
//! Changelog vs the 0.82.1 port:
//! - Search input + fuzzy filter over level/description
//!   (thinking-selector.ts:86-107, `496185f6e`).
//! - `onSelectAsDefault` (Ctrl+S `app.thinking.save`) + `defaultThinkingLevel`
//!   (`· default` description suffix, `2ff8ba622`/`1d3503fb9`).
//! - `✓ ` label prefix on the current level + two hint lines with live
//!   keybinding display text (`keyDisplayText`).
//!
//! Intentional differences:
//! - The theme is injected (`Arc<Theme>`) instead of read from the global
//!   `theme` getter (theme.ts:799-816).
//! - Callbacks are constructor args (`Box<dyn FnMut ... + Send>`) instead of
//!   `SelectList` property assignments.
//! - Container children are rendered inline (the Rust component model has no
//!   child list; render order mirrors the upstream `addChild` order).

use std::sync::Arc;

use rpi_agent::types::ThinkingLevel;
use rpi_tui::components::input::Input;
use rpi_tui::components::select_list::{
    SelectItem, SelectList, SelectListLayoutOptions, SelectListTheme,
};
use rpi_tui::fuzzy::fuzzy_filter;
use rpi_tui::keybindings::get_keybindings;
use rpi_tui::tui::Component;

use crate::core::themes::Theme;
use crate::modes::interactive::components::dynamic_border::DynamicBorder;
use crate::modes::interactive::components::keybinding_hints::key_text;

/// `THINKING_SELECT_LIST_LAYOUT` (thinking-selector.ts:13-16).
const THINKING_SELECT_LIST_LAYOUT: SelectListLayoutOptions = SelectListLayoutOptions {
    min_primary_column_width: Some(12),
    max_primary_column_width: Some(32),
    truncate_primary: None,
};

/// `LEVEL_DESCRIPTIONS` (thinking-selector.ts:18-26).
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

/// `getSelectListTheme` (theme.ts:1269-1277).
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

/// Shared select callback (every list rebuild re-wires `list.onSelect`,
/// thinking-selector.ts:115-117).
type SharedSelectFn = Arc<std::sync::Mutex<Box<dyn FnMut(ThinkingLevel) + Send>>>;

/// Component that renders a thinking level selector with borders
/// (thinking-selector.ts:29-140).
pub struct ThinkingSelectorComponent {
    search_input: Input,
    select_list: SelectList,
    all_items: Vec<SelectItem>,
    /// Shared so every list rebuild re-wires `list.onSelect`
    /// (buildSelectList, thinking-selector.ts:115-117).
    on_select: SharedSelectFn,
    on_cancel: Box<dyn FnMut() + Send>,
    /// `onSelectAsDefault` (thinking-selector.ts:63): the Ctrl+S
    /// persist-as-default path; `None` disables both the key branch and the
    /// hint line (upstream optional callback).
    on_select_as_default: Option<Box<dyn FnMut(ThinkingLevel) + Send>>,
    top_border: DynamicBorder,
    bottom_border: DynamicBorder,
    theme: Arc<Theme>,
}

impl ThinkingSelectorComponent {
    /// `constructor` (thinking-selector.ts:57-106).
    #[allow(clippy::too_many_arguments)] // mirrors the upstream constructor
    pub fn new(
        theme: Arc<Theme>,
        current_level: ThinkingLevel,
        available_levels: Vec<ThinkingLevel>,
        on_select: Box<dyn FnMut(ThinkingLevel) + Send>,
        on_cancel: Box<dyn FnMut() + Send>,
        on_select_as_default: Option<Box<dyn FnMut(ThinkingLevel) + Send>>,
        default_thinking_level: Option<ThinkingLevel>,
    ) -> Self {
        // `${level === currentLevel ? "✓ " : "  "}${level}` label; the
        // description appends `· default` on the persisted default
        // (thinking-selector.ts:71-76 @ 1d3503fb9 + f2a622789).
        let all_items: Vec<SelectItem> = available_levels
            .iter()
            .map(|level| {
                let value = level.as_str();
                let label = if *level == current_level {
                    format!("✓ {value}")
                } else {
                    format!("  {value}")
                };
                let description = level_description(value).map(|description| {
                    if Some(*level) == default_thinking_level {
                        format!("{description} · default")
                    } else {
                        description.to_string()
                    }
                });
                SelectItem {
                    value: value.to_string(),
                    label,
                    description,
                }
            })
            .collect();

        let mut search_input = Input::new();
        use rpi_tui::tui::Focusable as _;
        search_input.set_focused(false);

        let on_select = Arc::new(std::sync::Mutex::new(on_select));
        let mut select_list = Self::build_select_list(
            &theme,
            &all_items,
            Some(current_level),
            Arc::clone(&on_select),
        );
        select_list.on_cancel = None; // cancel handled at the container level
        Self {
            search_input,
            select_list,
            all_items,
            on_select,
            on_cancel,
            on_select_as_default,
            top_border: DynamicBorder::new(border_color(&theme)),
            bottom_border: DynamicBorder::new(border_color(&theme)),
            theme,
        }
    }

    /// `buildSelectList` (thinking-selector.ts:108-117).
    fn build_select_list(
        theme: &Arc<Theme>,
        items: &[SelectItem],
        preselect: Option<ThinkingLevel>,
        on_select: SharedSelectFn,
    ) -> SelectList {
        let mut list = SelectList::new(
            items.to_vec(),
            items.len().max(1),
            select_list_theme(theme),
            Some(THINKING_SELECT_LIST_LAYOUT),
        );
        if let Some(level) = preselect {
            if let Some(index) = items.iter().position(|item| item.value == level.as_str()) {
                list.set_selected_index(index);
            }
        }
        list.on_select = Some(Box::new(move |item| {
            if let Some(level) = thinking_level_from_str(&item.value) {
                if let Ok(mut callback) = on_select.lock() {
                    callback(level);
                }
            }
        }));
        list
    }

    /// `applyFilter` (thinking-selector.ts:119-126): rebuild the list from
    /// the fuzzy-filtered items, preserving the selected value.
    fn apply_filter(&mut self, query: &str) {
        let filtered: Vec<SelectItem> = if query.is_empty() {
            self.all_items.clone()
        } else {
            fuzzy_filter(self.all_items.clone(), query, |item: &SelectItem| {
                format!(
                    "{} {}",
                    item.value,
                    item.description.clone().unwrap_or_default()
                )
            })
        };
        let selected_value = self
            .select_list
            .get_selected_item()
            .map(|item| item.value.clone());
        let preselect = selected_value.as_deref().and_then(thinking_level_from_str);
        let mut list = Self::build_select_list(
            &self.theme,
            &filtered,
            preselect,
            Arc::clone(&self.on_select),
        );
        list.on_cancel = None;
        self.select_list = list;
    }

    /// `getSelectList` (thinking-selector.ts:138-140).
    pub fn get_select_list(&self) -> &SelectList {
        &self.select_list
    }
}

impl Component for ThinkingSelectorComponent {
    fn render(&self, width: usize) -> Vec<String> {
        let mut lines = Vec::new();
        lines.extend(self.top_border.render(width));
        lines.push(String::new());
        lines.push("Thinking Level".to_string());
        lines.push(String::new());
        // `{keyDisplayText("app.thinking.cycle")} cycles thinking levels
        // in-session` (thinking-selector.ts:81).
        lines.push(format!(
            "{} cycles thinking levels in-session",
            key_text("app.thinking.cycle")
        ));
        lines.push(String::new());
        lines.extend(self.search_input.render(width));
        lines.push(String::new());
        lines.extend(self.select_list.render(width));
        lines.push(String::new());
        if self.on_select_as_default.is_some() {
            // `  {confirm} to select · {app.thinking.save} to set as default
            // · {cancel} to cancel` (thinking-selector.ts:97-105).
            lines.push(self.theme.fg(
                "dim",
                &format!(
                    "  {} to select · {} to set as default · {} to cancel",
                    key_text("tui.select.confirm"),
                    key_text("app.thinking.save"),
                    key_text("tui.select.cancel"),
                ),
            ));
        }
        lines.extend(self.bottom_border.render(width));
        lines
    }

    /// `handleInput` (thinking-selector.ts:112-136).
    fn handle_input(&mut self, data: &str) {
        // Ctrl+S — select as default (thinking-selector.ts:113-119).
        if self.on_select_as_default.is_some() {
            let is_save = {
                let keybindings = get_keybindings();
                let read = keybindings.read().unwrap_or_else(|e| e.into_inner());
                read.matches_id(data, "app.thinking.save")
            };
            if is_save {
                if let Some(item) = self.select_list.get_selected_item() {
                    if let Some(level) = thinking_level_from_str(&item.value) {
                        if let Some(callback) = self.on_select_as_default.as_mut() {
                            callback(level);
                        }
                    }
                }
                return;
            }
        }

        let is_nav = {
            let keybindings = get_keybindings();
            let read = keybindings.read().unwrap_or_else(|e| e.into_inner());
            read.matches_id(data, "tui.select.up")
                || read.matches_id(data, "tui.select.down")
                || read.matches_id(data, "tui.select.confirm")
                || read.matches_id(data, "tui.select.cancel")
        };
        if is_nav {
            // Cancel is handled here so the search input never sees Esc.
            let is_cancel = {
                let keybindings = get_keybindings();
                let read = keybindings.read().unwrap_or_else(|e| e.into_inner());
                read.matches_id(data, "tui.select.cancel")
            };
            if is_cancel {
                (self.on_cancel)();
                return;
            }
            // Confirm dispatches through the list's on_select.
            self.select_list.handle_input(data);
            return;
        }

        self.search_input.handle_input(data);
        let query = self.search_input.get_value().to_string();
        self.apply_filter(&query);
    }
}

impl rpi_tui::tui::Focusable for ThinkingSelectorComponent {
    fn focused(&self) -> bool {
        true
    }

    fn set_focused(&mut self, _focused: bool) {}
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

    fn all_levels() -> Vec<ThinkingLevel> {
        vec![
            ThinkingLevel::Off,
            ThinkingLevel::Minimal,
            ThinkingLevel::Low,
            ThinkingLevel::Medium,
            ThinkingLevel::High,
            ThinkingLevel::Xhigh,
            ThinkingLevel::Max,
        ]
    }

    #[allow(clippy::type_complexity)] // mirrors the upstream callback type
    fn setup(
        current: ThinkingLevel,
        default: Option<ThinkingLevel>,
    ) -> (
        ThinkingSelectorComponent,
        Arc<Mutex<Vec<(ThinkingLevel, bool)>>>,
        Arc<Mutex<usize>>,
    ) {
        install_global_keybindings();
        let calls: Arc<Mutex<Vec<(ThinkingLevel, bool)>>> = Arc::new(Mutex::new(Vec::new()));
        let cancelled: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
        let calls_cb = Arc::clone(&calls);
        let calls_default = Arc::clone(&calls);
        let cancelled_cb = Arc::clone(&cancelled);
        let component = ThinkingSelectorComponent::new(
            theme(),
            current,
            all_levels(),
            Box::new(move |level| calls_cb.lock().unwrap().push((level, false))),
            Box::new(move || *cancelled_cb.lock().unwrap() += 1),
            {
                let calls_default = Arc::clone(&calls_default);
                Some(Box::new(move |level| {
                    calls_default.lock().unwrap().push((level, true))
                }))
            },
            default,
        );
        (component, calls, cancelled)
    }

    #[test]
    fn confirm_selects_currently_highlighted_level_session_only() {
        let (mut component, calls, _cancelled) = setup(ThinkingLevel::Low, None);
        component.handle_input("\r");
        assert_eq!(*calls.lock().unwrap(), vec![(ThinkingLevel::Low, false)]);
        component.handle_input("\x1b[A");
        component.handle_input("\r");
        assert_eq!(
            *calls.lock().unwrap(),
            vec![(ThinkingLevel::Low, false), (ThinkingLevel::Minimal, false)]
        );
    }

    #[test]
    fn ctrl_s_selects_as_default() {
        // \x13 = Ctrl+S, the default app.thinking.save binding.
        let (mut component, calls, _cancelled) = setup(ThinkingLevel::Off, None);
        component.handle_input("\x1b[B"); // down to minimal
        component.handle_input("\x13");
        assert_eq!(*calls.lock().unwrap(), vec![(ThinkingLevel::Minimal, true)]);
    }

    #[test]
    fn escape_cancels() {
        let (mut component, _calls, cancelled) = setup(ThinkingLevel::Off, None);
        component.handle_input("\x1b");
        assert_eq!(*cancelled.lock().unwrap(), 1);
    }

    #[test]
    fn search_filters_and_default_marker_renders() {
        let (mut component, _calls, _cancelled) =
            setup(ThinkingLevel::Medium, Some(ThinkingLevel::High));
        let rendered = component.render(60).join("\n");
        // Default marker on the persisted default (1d3503fb9).
        let high_description = THINKING_LEVEL_DESCRIPTIONS
            .iter()
            .find(|(value, _)| *value == "high")
            .map(|(_, description)| *description)
            .unwrap();
        assert!(rendered.contains(&format!("{high_description} · default")));
        assert!(rendered.contains("✓ medium"));
        assert!(rendered.contains("  high"));
        // Hint lines render live key names (not hardcoded "ctrl+s").
        assert!(rendered.contains("to select ·"));
        assert!(rendered.contains("to set as default"));

        // Search narrows the list ("deep" matches the high description).
        for ch in "deep".chars() {
            component.handle_input(&ch.to_string());
        }
        let filtered = component.render(60).join("\n");
        assert!(filtered.contains("high"));
        assert!(!filtered.contains("medium"));
        // The filtered selection confirms the filtered item.
        component.handle_input("\r");
    }
}
