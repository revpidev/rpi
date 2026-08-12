//! Port of `packages/tui/src/components/settings-list.ts` @ pi 0.82.1
//! (2efa728).
//!
//! Intentional differences:
//! - Theme callbacks are `Box<dyn Fn ... + Send + Sync>` fields (upstream
//!   plain functions); `SettingsList::new` takes the theme as an
//!   `Arc<SettingsListTheme>` so callers can share one theme across instances.
//! - Callbacks (`on_change` / `on_cancel`) are `Box<dyn FnMut ... + Send>`
//!   fields instead of constructor parameters.
//! - The submenu factory is `Box<dyn FnMut(&str, SubmenuDone) -> Box<dyn
//!   Component> + Send>` (upstream `(currentValue, done) => Component`).
//!   Because the `done` callback must write the selected value back into the
//!   list synchronously while the submenu component (which owns the callback)
//!   is being driven by `handle_input` / `render`, the callback cannot
//!   capture a `&mut self`. Instead it pushes the result into a shared queue
//!   (`Arc<Mutex<...>>`) that `SettingsList` drains right after delegating
//!   input to the submenu (and defensively at the start of `render`). The
//!   observable ordering is unchanged: the write-back, `on_change` and
//!   submenu close all happen before the next render.
//! - `fuzzy_filter` takes `&[SettingItem]`-style input and clones the
//!   matching items (upstream keeps references to the same item objects).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crate::components::input::Input;
use crate::fuzzy::fuzzy_filter;
use crate::keybindings::{get_keybindings, Keybinding};
use crate::tui::Component;
use crate::utils::{truncate_to_width, visible_width, wrap_text_with_ansi};

/// `SettingItem` (settings-list.ts:7-19).
pub struct SettingItem {
    /// Unique identifier for this setting.
    pub id: String,
    /// Display label (left side).
    pub label: String,
    /// Optional description shown when selected.
    pub description: Option<String>,
    /// Current value to display (right side).
    pub current_value: String,
    /// If provided, Enter/Space cycles through these values.
    pub values: Option<Vec<String>>,
    /// If provided, Enter opens this submenu. Receives the current value and
    /// a done callback.
    pub submenu: Option<SubmenuFactory>,
}

/// Submenu done callback (upstream `(selectedValue?: string) => void`):
/// `None` closes the submenu without a value, `Some` writes the value back.
pub type SubmenuDone = Box<dyn FnOnce(Option<String>) + Send>;

/// Submenu factory (upstream `(currentValue, done) => Component`).
pub type SubmenuFactory = Box<dyn FnMut(&str, SubmenuDone) -> Box<dyn Component> + Send>;

/// Label/value callback (upstream `(text: string, selected: boolean) =>
/// string`).
pub type SettingsSelectableTextFn = Box<dyn Fn(&str, bool) -> String + Send + Sync>;
/// Text callback (upstream `(text: string) => string`).
pub type SettingsTextFn = Box<dyn Fn(&str) -> String + Send + Sync>;
/// Change callback (upstream `(id: string, newValue: string) => void`).
pub type SettingsChangeFn = Box<dyn FnMut(&str, &str) + Send>;

/// `SettingsListTheme` (settings-list.ts:21-28).
pub struct SettingsListTheme {
    pub label: SettingsSelectableTextFn,
    pub value: SettingsSelectableTextFn,
    pub description: SettingsTextFn,
    pub cursor: String,
    pub hint: SettingsTextFn,
}

impl SettingsListTheme {
    /// Identity theme — every callback returns its input unchanged (used by
    /// tests and snapshots).
    pub fn identity() -> Self {
        let identity = |text: &str| text.to_string();
        Self {
            label: Box::new(|text: &str, _selected: bool| text.to_string()),
            value: Box::new(|text: &str, _selected: bool| text.to_string()),
            description: Box::new(identity),
            cursor: "→ ".to_string(),
            hint: Box::new(identity),
        }
    }
}

/// `SettingsListOptions` (settings-list.ts:30-32).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SettingsListOptions {
    pub enable_search: bool,
}

/// Result queued by a submenu's done callback: `None` closes without a
/// value, `Some((id, value))` writes the value back to the item.
type SubmenuResult = Option<(String, String)>;

/// Settings list component (upstream `SettingsList`, settings-list.ts:34).
pub struct SettingsList {
    items: Vec<SettingItem>,
    /// Indices into `items` after filtering (upstream `filteredItems` holds
    /// references to the same item objects; `SettingItem` is not `Clone`
    /// because of the `FnMut` submenu factory).
    filtered_indices: Vec<usize>,
    theme: Arc<SettingsListTheme>,
    selected_index: usize,
    max_visible: usize,
    search_enabled: bool,
    search_input: Option<Input>,

    /// Called when a setting value changes (upstream `onChange`).
    pub on_change: Option<SettingsChangeFn>,
    /// Called on escape/cancel (upstream `onCancel`).
    pub on_cancel: Option<Box<dyn FnMut() + Send>>,

    // Submenu state
    submenu_component: Option<Box<dyn Component>>,
    submenu_item_index: Option<usize>,
    submenu_done_queue: Arc<Mutex<VecDeque<SubmenuResult>>>,
}

impl SettingsList {
    pub fn new(
        items: Vec<SettingItem>,
        max_visible: usize,
        theme: Arc<SettingsListTheme>,
        options: Option<SettingsListOptions>,
    ) -> Self {
        let options = options.unwrap_or_default();
        let search_input = if options.enable_search {
            Some(Input::new())
        } else {
            None
        };
        Self {
            filtered_indices: (0..items.len()).collect(),
            items,
            theme,
            selected_index: 0,
            max_visible,
            search_enabled: options.enable_search,
            search_input,
            on_change: None,
            on_cancel: None,
            submenu_component: None,
            submenu_item_index: None,
            submenu_done_queue: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    /// Update an item's current value (upstream `updateValue`,
    /// settings-list.ts:69-75).
    pub fn update_value(&mut self, id: &str, new_value: &str) {
        if let Some(item) = self.items.iter_mut().find(|item| item.id == id) {
            item.current_value = new_value.to_string();
        }
    }

    /// The currently selected item of the visible list (upstream index
    /// access `displayItems[this.selectedIndex]`).
    pub fn selected_item(&self) -> Option<&SettingItem> {
        let index = if self.search_enabled {
            self.filtered_indices.get(self.selected_index).copied()
        } else {
            Some(self.selected_index)
        };
        index.and_then(|index| self.items.get(index))
    }

    /// The display index into `items` at `selected_index` (search-aware).
    fn display_item_index(&self, selected_index: usize) -> Option<usize> {
        if self.search_enabled {
            self.filtered_indices.get(selected_index).copied()
        } else {
            Some(selected_index)
        }
    }

    /// `activateItem` (settings-list.ts:199-221).
    fn activate_item(&mut self) {
        let Some(item_index) = self.display_item_index(self.selected_index) else {
            return;
        };
        let Some(item) = self.items.get_mut(item_index) else {
            return;
        };

        if let Some(submenu) = item.submenu.as_mut() {
            // Open the submenu, passing the current value so it can
            // pre-select correctly. The done callback cannot capture `self`
            // (the submenu component owns it and is driven by this
            // component's own `handle_input`), so the result is queued and
            // drained right after the submenu call completes.
            let id = item.id.clone();
            let current_value = item.current_value.clone();
            let queue = Arc::clone(&self.submenu_done_queue);
            let done: SubmenuDone = Box::new(move |selected_value: Option<String>| {
                let result = selected_value.map(|value| (id, value));
                let mut queue = queue
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                queue.push_back(result);
            });
            self.submenu_item_index = Some(self.selected_index);
            self.submenu_component = Some(submenu(&current_value, done));
        } else if let Some(values) = item.values.as_ref().filter(|values| !values.is_empty()) {
            // Cycle through values.
            let current_index = values
                .iter()
                .position(|value| *value == item.current_value)
                .unwrap_or(0);
            let next_index = (current_index + 1) % values.len();
            let new_value = values[next_index].clone();
            item.current_value = new_value.clone();
            if let Some(on_change) = self.on_change.as_mut() {
                on_change(&item.id, &new_value);
            }
        }
    }

    /// `closeSubmenu` (settings-list.ts:223-230): restore the selection to
    /// the item that opened the submenu.
    fn close_submenu(&mut self) {
        self.submenu_component = None;
        if let Some(index) = self.submenu_item_index.take() {
            self.selected_index = index;
        }
    }

    /// Drain submenu done results (see the module header note on deferred
    /// write-back).
    fn drain_submenu_results(&mut self) {
        let results: Vec<SubmenuResult> = {
            let mut queue = self
                .submenu_done_queue
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            queue.drain(..).collect()
        };
        for result in results {
            if let Some((id, value)) = result {
                if let Some(item) = self.items.iter_mut().find(|item| item.id == id) {
                    item.current_value = value.clone();
                }
                if let Some(on_change) = self.on_change.as_mut() {
                    on_change(&id, &value);
                }
            }
            self.close_submenu();
        }
    }

    /// `applyFilter` (settings-list.ts:232-235).
    fn apply_filter(&mut self, query: &str) {
        let all: Vec<usize> = (0..self.items.len()).collect();
        self.filtered_indices = fuzzy_filter(all, query, |&index| self.items[index].label.clone());
        self.selected_index = 0;
    }

    /// `addHintLine` (settings-list.ts:237-249).
    fn add_hint_line(&self, lines: &mut Vec<String>, width: usize) {
        lines.push(String::new());
        let hint = if self.search_enabled {
            "  Type to search · Enter/Space to change · Esc to cancel"
        } else {
            "  Enter/Space to change · Esc to cancel"
        };
        lines.push(truncate_to_width(
            &(self.theme.hint)(hint),
            width,
            "...",
            false,
        ));
    }

    /// `renderMainList` (settings-list.ts:90-166).
    fn render_main_list(&self, width: usize) -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();

        if self.search_enabled {
            if let Some(search_input) = &self.search_input {
                lines.extend(search_input.render(width));
            }
            lines.push(String::new());
        }

        if self.items.is_empty() {
            lines.push((self.theme.hint)("  No settings available"));
            if self.search_enabled {
                self.add_hint_line(&mut lines, width);
            }
            return lines;
        }

        let display_len = if self.search_enabled {
            self.filtered_indices.len()
        } else {
            self.items.len()
        };
        if display_len == 0 {
            lines.push(truncate_to_width(
                &(self.theme.hint)("  No matching settings"),
                width,
                "...",
                false,
            ));
            self.add_hint_line(&mut lines, width);
            return lines;
        }

        // Calculate visible range with scrolling.
        let start_index = self
            .selected_index
            .saturating_sub(self.max_visible / 2)
            .min(display_len.saturating_sub(self.max_visible));
        let end_index = (start_index + self.max_visible).min(display_len);

        // Calculate max label width for alignment (over ALL items, matching
        // upstream `this.items.map(...)`).
        let max_label_width = self
            .items
            .iter()
            .map(|item| visible_width(&item.label))
            .max()
            .unwrap_or(0)
            .min(30);

        // Render visible items.
        for index in start_index..end_index {
            let item_index = if self.search_enabled {
                self.filtered_indices[index]
            } else {
                index
            };
            let item = &self.items[item_index];
            let is_selected = index == self.selected_index;
            let prefix = if is_selected {
                self.theme.cursor.clone()
            } else {
                "  ".to_string()
            };
            let prefix_width = visible_width(&prefix);

            // Pad label to align values.
            let label_padded = format!(
                "{}{}",
                item.label,
                " ".repeat(max_label_width.saturating_sub(visible_width(&item.label)))
            );
            let label_text = (self.theme.label)(&label_padded, is_selected);

            // Calculate space for value.
            let separator = "  ";
            let used_width = prefix_width + max_label_width + visible_width(separator);
            let value_max_width = width.saturating_sub(used_width + 2);

            let value_text = (self.theme.value)(
                &truncate_to_width(&item.current_value, value_max_width, "", false),
                is_selected,
            );

            lines.push(truncate_to_width(
                &format!("{prefix}{label_text}{separator}{value_text}"),
                width,
                "...",
                false,
            ));
        }

        // Add scroll indicator if needed.
        if start_index > 0 || end_index < display_len {
            let scroll_text = format!("  ({}/{})", self.selected_index + 1, display_len);
            lines.push((self.theme.hint)(&truncate_to_width(
                &scroll_text,
                width.saturating_sub(2),
                "",
                false,
            )));
        }

        // Add description for selected item.
        if let Some(selected_item) = self.selected_item() {
            if let Some(description) = &selected_item.description {
                lines.push(String::new());
                let wrapped_description =
                    wrap_text_with_ansi(description, width.saturating_sub(4).max(1));
                for line in wrapped_description {
                    lines.push((self.theme.description)(&format!("  {line}")));
                }
            }
        }

        // Add hint.
        self.add_hint_line(&mut lines, width);

        lines
    }
}

impl Component for SettingsList {
    fn render(&self, width: usize) -> Vec<String> {
        // If a submenu is active, render it instead.
        if let Some(submenu_component) = &self.submenu_component {
            return submenu_component.render(width);
        }

        self.render_main_list(width)
    }

    fn handle_input(&mut self, data: &str) {
        // If a submenu is active, delegate all input to it. The submenu's
        // cancel (triggered by escape) will call done() which closes it.
        if self.submenu_component.is_some() {
            if let Some(submenu_component) = self.submenu_component.as_mut() {
                submenu_component.handle_input(data);
            }
            self.drain_submenu_results();
            return;
        }

        // Main list input handling.
        let kb = get_keybindings();
        let kb = kb.read().unwrap_or_else(|poisoned| poisoned.into_inner());
        let display_items_len = if self.search_enabled {
            self.filtered_indices.len()
        } else {
            self.items.len()
        };

        if kb.matches(data, Keybinding::SelectUp) {
            if display_items_len == 0 {
                return;
            }
            self.selected_index = if self.selected_index == 0 {
                display_items_len - 1
            } else {
                self.selected_index - 1
            };
        } else if kb.matches(data, Keybinding::SelectDown) {
            if display_items_len == 0 {
                return;
            }
            self.selected_index = if self.selected_index == display_items_len - 1 {
                0
            } else {
                self.selected_index + 1
            };
        } else if kb.matches(data, Keybinding::SelectConfirm)
            || (data == " "
                && (!self.search_enabled
                    || self
                        .search_input
                        .as_ref()
                        .is_some_and(|search_input| search_input.get_value().is_empty())))
        {
            // Space only activates when no search query is entered; with an
            // active query it is part of the search text (settings-list.ts:
            // 185-189 @ 4181f66, bf4a90d81).
            self.activate_item();
        } else if kb.matches(data, Keybinding::SelectCancel) {
            if let Some(on_cancel) = self.on_cancel.as_mut() {
                on_cancel();
            }
        } else if self.search_enabled {
            if let Some(search_input) = self.search_input.as_mut() {
                // The query is forwarded verbatim — spaces included
                // (settings-list.ts:192-195 @ 4181f66, bf4a90d81).
                search_input.handle_input(data);
                let query = search_input.get_value().to_string();
                self.apply_filter(&query);
            }
        }
    }

    fn invalidate(&mut self) {
        if let Some(submenu_component) = self.submenu_component.as_mut() {
            submenu_component.invalidate();
        }
    }
}

#[cfg(test)]
mod tests {
    //! SettingsList has no upstream test file; these cover the behavior the
    //! component contract requires (main list, search, submenu flow) and the
    //! snapshot goldens in `tests/snapshots.rs`.

    use super::*;
    use std::sync::Mutex as StdMutex;

    fn theme() -> Arc<SettingsListTheme> {
        Arc::new(SettingsListTheme::identity())
    }

    fn item(id: &str, label: &str, current_value: &str) -> SettingItem {
        SettingItem {
            id: id.to_string(),
            label: label.to_string(),
            description: None,
            current_value: current_value.to_string(),
            values: None,
            submenu: None,
        }
    }

    fn list(items: Vec<SettingItem>, options: Option<SettingsListOptions>) -> SettingsList {
        SettingsList::new(items, 5, theme(), options)
    }

    fn plain(lines: Vec<String>) -> Vec<String> {
        lines
            .into_iter()
            .map(|line| {
                let mut out = String::with_capacity(line.len());
                let mut chars = line.chars().peekable();
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
            })
            .collect()
    }

    #[test]
    fn renders_items_with_aligned_labels() {
        let mut settings = list(
            vec![
                item("a", "alpha", "one"),
                item("b", "beta", "two"),
                item("c", "gamma", "three"),
            ],
            None,
        );
        let lines = plain(settings.render(40));
        // Label column width = widest label (5) + 2-space separator.
        assert!(lines[0].starts_with("→ alpha  one"));
        assert!(lines[1].starts_with("  beta   two"));
        assert!(lines[2].starts_with("  gamma  three"));
        assert!(lines[3].is_empty()); // description gap
        assert!(lines[4].contains("Enter/Space to change"));

        // Moving down changes the cursor row.
        settings.handle_input("\x1b[B");
        let lines = plain(settings.render(40));
        assert!(lines[1].starts_with("→ beta"));
    }

    #[test]
    fn renders_no_settings_available_for_empty_list() {
        let settings = list(vec![], None);
        let lines = plain(settings.render(40));
        assert_eq!(lines[0], "  No settings available");
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn scrolls_window_and_shows_indicator() {
        let mut settings = list(
            (0..10)
                .map(|i| item(&i.to_string(), &format!("setting {i}"), "v"))
                .collect(),
            None,
        );
        // Move to the last item: the window scrolls.
        for _ in 0..9 {
            settings.handle_input("\x1b[B");
        }
        let lines = plain(settings.render(40));
        assert!(lines.iter().any(|line| line.contains("(10/10)")));
        // Selected item visible.
        assert!(lines.iter().any(|line| line.starts_with("→ setting 9")));
    }

    #[test]
    fn cycles_through_values_on_confirm() {
        let mut settings = list(
            vec![SettingItem {
                id: "theme".into(),
                label: "Theme".into(),
                description: None,
                current_value: "dark".into(),
                values: Some(vec!["dark".into(), "light".into()]),
                submenu: None,
            }],
            None,
        );
        let changes = StdMutex::new(Vec::<String>::new());
        let captured = std::sync::Arc::new(changes);
        let captured2 = std::sync::Arc::clone(&captured);
        settings.on_change = Some(Box::new(move |_id, value| {
            captured.lock().unwrap().push(value.to_string());
        }));
        let _ = &captured2;

        settings.handle_input("\r");
        assert_eq!(settings.items[0].current_value, "light");
        settings.handle_input(" ");
        assert_eq!(settings.items[0].current_value, "dark");
        assert_eq!(
            *captured2.lock().unwrap(),
            vec!["light".to_string(), "dark".to_string()]
        );
    }

    #[test]
    fn search_filters_items_and_resets_selection() {
        let mut settings = list(
            vec![
                item("a", "alpha", "1"),
                item("b", "beta", "2"),
                item("c", "gamma", "3"),
            ],
            Some(SettingsListOptions {
                enable_search: true,
            }),
        );
        // Search input line + blank + items + hint gap + hint.
        let lines = plain(settings.render(40));
        assert_eq!(lines.len(), 7);

        // Type "al" — only alpha matches.
        settings.handle_input("a");
        settings.handle_input("l");
        let lines = plain(settings.render(40));
        assert!(lines.iter().any(|line| line.contains("alpha")));
        assert!(!lines.iter().any(|line| line.contains("beta")));

        // No matches (after the search input line and its blank line).
        settings.handle_input("z");
        let lines = plain(settings.render(40));
        assert!(lines[2].contains("No matching settings"));
    }

    /// `SettingsList` space handling (settings-list.test.ts @ 4181f66,
    /// bf4a90d81): the searchable item used by both tests.
    fn searchable_list(changes: Arc<StdMutex<Vec<(String, String)>>>) -> SettingsList {
        let mut settings = list(
            vec![SettingItem {
                id: "ui-mode".into(),
                label: "UI mode".into(),
                description: None,
                current_value: "regular".into(),
                values: Some(vec!["regular".into(), "fullscreen".into()]),
                submenu: None,
            }],
            Some(SettingsListOptions {
                enable_search: true,
            }),
        );
        settings.on_change = Some(Box::new(move |id, value| {
            changes
                .lock()
                .unwrap()
                .push((id.to_string(), value.to_string()));
        }));
        settings
    }

    #[test]
    fn includes_spaces_in_an_active_search_instead_of_changing_the_selected_setting() {
        let changes = Arc::new(StdMutex::new(Vec::new()));
        let mut settings = searchable_list(Arc::clone(&changes));

        for character in "UI mode".chars() {
            settings.handle_input(&character.to_string());
        }

        assert_eq!(*changes.lock().unwrap(), Vec::new());
        assert!(plain(settings.render(80))[0].contains("UI mode"));

        settings.handle_input("\r");
        assert_eq!(
            *changes.lock().unwrap(),
            vec![("ui-mode".to_string(), "fullscreen".to_string())]
        );
    }

    #[test]
    fn keeps_space_as_a_change_shortcut_before_a_search_query_is_entered() {
        let changes = Arc::new(StdMutex::new(Vec::new()));
        let mut settings = searchable_list(Arc::clone(&changes));

        settings.handle_input(" ");

        assert_eq!(
            *changes.lock().unwrap(),
            vec![("ui-mode".to_string(), "fullscreen".to_string())]
        );
    }

    #[test]
    fn submenu_opens_closes_and_writes_back_value() {
        // A submenu component that immediately calls done with a value.
        let mut settings = list(
            vec![SettingItem {
                id: "model".into(),
                label: "Model".into(),
                description: None,
                current_value: "claude".into(),
                values: None,
                submenu: Some(Box::new(|current_value: &str, done: SubmenuDone| {
                    assert_eq!(current_value, "claude");
                    let component = SubmenuStub {
                        lines: vec!["submenu".to_string()],
                        on_action: Some(Box::new(move || {
                            done(Some("gpt".to_string()));
                        })),
                    };
                    Box::new(component)
                })),
            }],
            None,
        );
        let changes = StdMutex::new(Vec::<(String, String)>::new());
        let captured = std::sync::Arc::new(changes);
        let captured2 = std::sync::Arc::clone(&captured);
        settings.on_change = Some(Box::new(move |id, value| {
            captured
                .lock()
                .unwrap()
                .push((id.to_string(), value.to_string()));
        }));
        let _ = &captured2;

        // Open the submenu with Enter.
        settings.handle_input("\r");
        let lines = plain(settings.render(40));
        assert_eq!(
            lines[0], "submenu",
            "submenu should render instead of the list"
        );

        // Trigger the done callback through the submenu's action.
        if let Some(submenu) = settings.submenu_component.as_mut() {
            submenu.handle_input("x");
        }
        settings.drain_submenu_results();

        // Value written back, onChange fired, submenu closed, selection kept.
        assert_eq!(settings.items[0].current_value, "gpt");
        assert_eq!(
            *captured2.lock().unwrap(),
            [("model".to_string(), "gpt".to_string())]
        );
        assert!(settings.submenu_component.is_none());
        assert_eq!(settings.selected_index, 0);
    }

    #[test]
    fn submenu_done_without_value_just_closes() {
        let mut settings = list(
            vec![SettingItem {
                id: "model".into(),
                label: "Model".into(),
                description: None,
                current_value: "claude".into(),
                values: None,
                submenu: Some(Box::new(|_current_value: &str, done: SubmenuDone| {
                    Box::new(SubmenuStub {
                        lines: vec!["submenu".to_string()],
                        on_action: Some(Box::new(move || {
                            done(None);
                        })),
                    })
                })),
            }],
            None,
        );
        settings.handle_input("\r");
        assert!(settings.submenu_component.is_some());
        if let Some(submenu) = settings.submenu_component.as_mut() {
            submenu.handle_input("x");
        }
        settings.drain_submenu_results();
        assert!(settings.submenu_component.is_none());
        assert_eq!(settings.items[0].current_value, "claude");
        assert_eq!(settings.selected_index, 0);
    }

    /// Test double for a submenu component that runs an action when it
    /// receives input.
    struct SubmenuStub {
        lines: Vec<String>,
        on_action: Option<Box<dyn FnOnce() + Send>>,
    }

    impl Component for SubmenuStub {
        fn render(&self, _width: usize) -> Vec<String> {
            self.lines.clone()
        }

        fn handle_input(&mut self, _data: &str) {
            if let Some(on_action) = self.on_action.take() {
                on_action();
            }
        }
    }

    #[test]
    fn delegates_input_to_submenu_and_recovers_selection() {
        let mut settings = list(
            vec![
                item("a", "alpha", "1"),
                SettingItem {
                    id: "b".into(),
                    label: "beta".into(),
                    description: None,
                    current_value: "2".into(),
                    values: None,
                    submenu: Some(Box::new(|_current_value: &str, _done: SubmenuDone| {
                        Box::new(SubmenuStub {
                            lines: vec!["submenu".to_string()],
                            on_action: None,
                        })
                    })),
                },
                item("c", "gamma", "3"),
            ],
            None,
        );
        // Move to "beta", open its submenu, then cancel via the submenu's
        // done callback (None).
        settings.handle_input("\x1b[B");
        settings.handle_input("\r");
        assert!(settings.submenu_component.is_some());
        assert_eq!(settings.submenu_item_index, Some(1));

        // Submenu input must not move the main list selection.
        settings.handle_input("\x1b[B");
        assert_eq!(settings.selected_index, 1);
    }

    #[test]
    fn update_value_finds_item_by_id() {
        let mut settings = list(vec![item("a", "alpha", "1"), item("b", "beta", "2")], None);
        settings.update_value("b", "updated");
        assert_eq!(settings.items[1].current_value, "updated");
    }
}
