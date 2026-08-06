//! Scoped models selector — port of
//! `packages/coding-agent/src/modes/interactive/components/scoped-models-selector.ts`
//! @ pi 0.82.1 (2efa728).
//!
//! Intentional differences:
//! - The theme is injected explicitly (`Arc<Theme>`) instead of read from
//!   the global `theme` getter (theme.ts:799-816).
//! - `ModelsConfig`/`ModelsCallbacks` (scoped-models-selector.ts:74-85) are
//!   flattened into constructor parameters. `on_persist` (upstream
//!   `ModelsCallbacks.onPersist`) is included so the Ctrl+S
//!   (`app.models.save`) keybinding has a target; the session-only changes
//!   go through `on_change`.
//! - The footer and list are computed in `render` instead of cached `Text`
//!   children that `refresh`/`updateList` mutate; output is identical.
//! - List rows are truncated to the render width with `...` (same idiom as
//!   config-selector.rs); upstream `Text` children wrap instead.
//! - The upstream `move` helper is renamed [`move_item`] (`move` is a Rust
//!   keyword).

use std::collections::HashMap;
use std::sync::Arc;

use pir_ai::types::Model;
use pir_tui::components::input::Input;
use pir_tui::components::text::Text;
use pir_tui::fuzzy::fuzzy_filter;
use pir_tui::keybindings::get_keybindings;
use pir_tui::tui::{Component, Focusable};
use pir_tui::utils::truncate_to_width;

use crate::core::themes::Theme;

use super::dynamic_border::DynamicBorder;
use super::keybinding_hints::key_text;
use super::model_search::{get_model_search_text, ModelSearchItem};

/// `EnabledIds` (scoped-models-selector.ts:18-19): `None` = all enabled (no
/// filter), `Some` = explicit ordered list.
type EnabledIds = Option<Vec<String>>;

/// `isEnabled` (scoped-models-selector.ts:21-23).
fn is_enabled(enabled_ids: &EnabledIds, id: &str) -> bool {
    match enabled_ids {
        None => true,
        Some(ids) => ids.iter().any(|existing| existing == id),
    }
}

/// `toggle` (scoped-models-selector.ts:25-30): the first toggle on "all
/// enabled" starts with only this one.
fn toggle(enabled_ids: &EnabledIds, id: &str) -> EnabledIds {
    match enabled_ids {
        None => Some(vec![id.to_string()]),
        Some(ids) => {
            let mut result = ids.clone();
            match result.iter().position(|existing| existing == id) {
                Some(index) => {
                    result.remove(index);
                }
                None => result.push(id.to_string()),
            }
            Some(result)
        }
    }
}

/// `enableAll` (scoped-models-selector.ts:32-40): `None` when everything is
/// enabled again.
fn enable_all(
    enabled_ids: &EnabledIds,
    all_ids: &[String],
    target_ids: Option<&[String]>,
) -> EnabledIds {
    let Some(ids) = enabled_ids else {
        return None; // Already all enabled.
    };
    let targets = target_ids.unwrap_or(all_ids);
    let mut result = ids.clone();
    for id in targets {
        if !result.iter().any(|existing| existing == id) {
            result.push(id.clone());
        }
    }
    if result.len() == all_ids.len() && result.iter().all(|id| all_ids.contains(id)) {
        None
    } else {
        Some(result)
    }
}

/// `clearAll` (scoped-models-selector.ts:42-48).
fn clear_all(
    enabled_ids: &EnabledIds,
    all_ids: &[String],
    target_ids: Option<&[String]>,
) -> EnabledIds {
    match enabled_ids {
        None => match target_ids {
            Some(targets) => Some(
                all_ids
                    .iter()
                    .filter(|id| !targets.contains(id))
                    .cloned()
                    .collect(),
            ),
            None => Some(Vec::new()),
        },
        Some(ids) => {
            let targets: Vec<&String> = match target_ids {
                Some(targets) => targets.iter().collect(),
                None => ids.iter().collect(),
            };
            Some(
                ids.iter()
                    .filter(|id| !targets.contains(id))
                    .cloned()
                    .collect(),
            )
        }
    }
}

/// `move` (scoped-models-selector.ts:50-60): swap `id` with its neighbour at
/// `delta` (`-1` up / `+1` down); out-of-bounds moves are a no-op.
fn move_item(enabled_ids: &EnabledIds, id: &str, delta: isize) -> EnabledIds {
    let Some(ids) = enabled_ids else {
        return None;
    };
    let Some(index) = ids.iter().position(|existing| existing == id) else {
        // Upstream `if (index < 0) return list` (scoped-models-selector.ts:
        // 53-54): unknown ids leave the list unchanged.
        return Some(ids.clone());
    };
    let new_index = index as isize + delta;
    if new_index < 0 || new_index >= ids.len() as isize {
        // Upstream `if (newIndex < 0 || ...) return list`
        // (scoped-models-selector.ts:56-57): out-of-bounds moves are a no-op.
        return Some(ids.clone());
    }
    let mut result = ids.clone();
    result.swap(index, new_index as usize);
    Some(result)
}

/// `getSortedIds` (scoped-models-selector.ts:62-66): enabled ids in their
/// stored order, then the remaining ids in catalog order.
fn get_sorted_ids(enabled_ids: &EnabledIds, all_ids: &[String]) -> Vec<String> {
    match enabled_ids {
        None => all_ids.to_vec(),
        Some(ids) => {
            let mut result = ids.clone();
            for id in all_ids {
                if !ids.iter().any(|existing| existing == id) {
                    result.push(id.clone());
                }
            }
            result
        }
    }
}

/// `ModelItem` (scoped-models-selector.ts:68-72). `model` is `None` for ids
/// that are enabled but no longer in the catalog ("unavailable").
#[derive(Clone)]
struct ModelItem {
    full_id: String,
    model: Option<Model>,
    enabled: bool,
}

/// Component for enabling/disabling models for Ctrl+P cycling
/// (scoped-models-selector.ts:91-375). Changes are session-only until
/// explicitly persisted with Ctrl+S (`app.models.save`).
pub struct ScopedModelsSelectorComponent {
    theme: Arc<Theme>,
    models_by_id: HashMap<String, Model>,
    all_ids: Vec<String>,
    enabled_ids: EnabledIds,
    filtered_items: Vec<ModelItem>,
    selected_index: usize,
    search_input: Input,
    focused: bool,
    on_change: Box<dyn FnMut(Option<Vec<String>>) + Send>,
    on_persist: Box<dyn FnMut(Option<Vec<String>>) + Send>,
    on_cancel: Box<dyn FnMut() + Send>,
    is_dirty: bool,
    top_border: DynamicBorder,
    bottom_border: DynamicBorder,
}

impl ScopedModelsSelectorComponent {
    /// `constructor` (scoped-models-selector.ts:114-152).
    pub fn new(
        all_models: Vec<Model>,
        enabled_model_ids: Option<Vec<String>>,
        theme: Arc<Theme>,
        on_change: Box<dyn FnMut(Option<Vec<String>>) + Send>,
        on_persist: Box<dyn FnMut(Option<Vec<String>>) + Send>,
        on_cancel: Box<dyn FnMut() + Send>,
    ) -> Self {
        let border_color = {
            let theme = Arc::clone(&theme);
            Box::new(move |text: &str| theme.fg("border", text))
        };
        let mut models_by_id = HashMap::new();
        let mut all_ids = Vec::new();
        for model in all_models {
            let full_id = format!("{}/{}", model.provider, model.id);
            all_ids.push(full_id.clone());
            models_by_id.insert(full_id, model);
        }
        let mut component = Self {
            theme,
            models_by_id,
            all_ids,
            enabled_ids: enabled_model_ids,
            filtered_items: Vec::new(),
            selected_index: 0,
            search_input: Input::new(),
            focused: false,
            on_change,
            on_persist,
            on_cancel,
            is_dirty: false,
            top_border: DynamicBorder::new(border_color.clone()),
            bottom_border: DynamicBorder::new(border_color),
        };
        component.filtered_items = component.build_items();
        component
    }

    /// `buildItems` (scoped-models-selector.ts:154-160).
    fn build_items(&self) -> Vec<ModelItem> {
        get_sorted_ids(&self.enabled_ids, &self.all_ids)
            .into_iter()
            .map(|full_id| ModelItem {
                full_id: full_id.clone(),
                model: self.models_by_id.get(&full_id).cloned(),
                enabled: is_enabled(&self.enabled_ids, &full_id),
            })
            .collect()
    }

    /// `getFooterText` (scoped-models-selector.ts:162-181).
    fn get_footer_text(&self) -> String {
        let enabled_count = match &self.enabled_ids {
            Some(ids) => ids
                .iter()
                .filter(|id| self.models_by_id.contains_key(*id))
                .count(),
            None => self.all_ids.len(),
        };
        let unavailable_count = match &self.enabled_ids {
            Some(ids) => ids
                .iter()
                .filter(|id| !self.models_by_id.contains_key(*id))
                .count(),
            None => 0,
        };
        let all_enabled = self.enabled_ids.is_none();
        let count_text = if all_enabled {
            "all enabled".to_string()
        } else {
            let mut text = format!("{enabled_count}/{} enabled", self.all_ids.len());
            if unavailable_count > 0 {
                text.push_str(&format!(" · {unavailable_count} unavailable"));
            }
            text
        };
        let parts = [
            key_text("tui.select.confirm"),
            key_text("app.models.enableAll"),
            key_text("app.models.clearAll"),
            key_text("app.models.toggleProvider"),
            key_text("app.models.reorderUp"),
            key_text("app.models.reorderDown"),
            key_text("app.models.save"),
            count_text,
        ];
        let joined = format!("  {} ", parts.join(" · "));
        if self.is_dirty {
            format!(
                "{}{}",
                self.theme.fg("dim", &joined),
                self.theme.fg("warning", "(unsaved)")
            )
        } else {
            self.theme.fg("dim", &joined)
        }
    }

    /// `refresh` (scoped-models-selector.ts:183-196): rebuild the items,
    /// re-apply the search filter and clamp the selection.
    fn refresh(&mut self) {
        let query = self.search_input.get_value().to_string();
        let items = self.build_items();
        self.filtered_items = if query.is_empty() {
            items
        } else {
            fuzzy_filter(items, &query, |item: &ModelItem| match &item.model {
                Some(model) => get_model_search_text(&ModelSearchItem {
                    id: model.id.clone(),
                    provider: model.provider.clone(),
                    name: Some(model.name.clone()),
                }),
                None => item.full_id.clone(),
            })
        };
        self.selected_index = self
            .selected_index
            .min(self.filtered_items.len().saturating_sub(1));
    }

    /// `notifyChange` (scoped-models-selector.ts:198-200).
    fn notify_change(&mut self) {
        (self.on_change)(self.enabled_ids.clone());
    }

    /// `getSearchInput` (scoped-models-selector.ts:372-374).
    pub fn get_search_input(&self) -> &Input {
        &self.search_input
    }
}

impl Component for ScopedModelsSelectorComponent {
    fn handle_input(&mut self, data: &str) {
        // Keybinding matching runs inside a block so the read guard is
        // dropped before the fall-through forwards to the search input:
        // `Input::handle_input` takes a second read lock on the same global,
        // which can deadlock against a queued writer
        // (`install_global_keybindings`) — std::sync::RwLock is not
        // reentrant.
        {
            let kb = get_keybindings();
            let read = kb.read().unwrap_or_else(|poisoned| poisoned.into_inner());

            // Navigation (scoped-models-selector.ts:258-269).
            if read.matches_id(data, "tui.select.up") {
                if self.filtered_items.is_empty() {
                    return;
                }
                self.selected_index = if self.selected_index == 0 {
                    self.filtered_items.len() - 1
                } else {
                    self.selected_index - 1
                };
                return;
            }
            if read.matches_id(data, "tui.select.down") {
                if self.filtered_items.is_empty() {
                    return;
                }
                self.selected_index = if self.selected_index == self.filtered_items.len() - 1 {
                    0
                } else {
                    self.selected_index + 1
                };
                return;
            }

            // Reorder enabled models (scoped-models-selector.ts:271-291).
            let reorder_up = read.matches_id(data, "app.models.reorderUp");
            let reorder_down = read.matches_id(data, "app.models.reorderDown");
            if reorder_up || reorder_down {
                if self.enabled_ids.is_none() {
                    return;
                }
                let Some(item) = self.filtered_items.get(self.selected_index) else {
                    return;
                };
                if is_enabled(&self.enabled_ids, &item.full_id) {
                    let delta = if reorder_up { -1 } else { 1 };
                    let current_index =
                        self.enabled_ids
                            .as_ref()
                            .expect("checked above")
                            .iter()
                            .position(|id| *id == item.full_id)
                            .expect("enabled item is in the list") as isize;
                    let new_index = current_index + delta;
                    let len = self.enabled_ids.as_ref().expect("checked above").len() as isize;
                    // Only move if within bounds (scoped-models-selector.ts:282).
                    if new_index >= 0 && new_index < len {
                        self.enabled_ids = move_item(&self.enabled_ids, &item.full_id, delta);
                        self.is_dirty = true;
                        self.selected_index = (self.selected_index as isize + delta) as usize;
                        self.refresh();
                        self.notify_change();
                    }
                }
                return;
            }

            // Toggle on Enter (scoped-models-selector.ts:293-303).
            if read.matches_id(data, "tui.select.confirm") {
                let Some(item) = self.filtered_items.get(self.selected_index).cloned() else {
                    return;
                };
                self.enabled_ids = toggle(&self.enabled_ids, &item.full_id);
                self.is_dirty = true;
                self.refresh();
                self.notify_change();
                return;
            }

            // Enable all — filtered when a search is active, otherwise all
            // (scoped-models-selector.ts:305-313).
            if read.matches_id(data, "app.models.enableAll") {
                let target_ids = self.filtered_target_ids();
                self.enabled_ids =
                    enable_all(&self.enabled_ids, &self.all_ids, target_ids.as_deref());
                self.is_dirty = true;
                self.refresh();
                self.notify_change();
                return;
            }

            // Clear all (scoped-models-selector.ts:315-323).
            if read.matches_id(data, "app.models.clearAll") {
                let target_ids = self.filtered_target_ids();
                self.enabled_ids =
                    clear_all(&self.enabled_ids, &self.all_ids, target_ids.as_deref());
                self.is_dirty = true;
                self.refresh();
                self.notify_change();
                return;
            }

            // Toggle the provider of the current item
            // (scoped-models-selector.ts:325-340).
            if read.matches_id(data, "app.models.toggleProvider") {
                let Some(item) = self.filtered_items.get(self.selected_index) else {
                    return;
                };
                if let Some(model) = &item.model {
                    let provider = &model.provider;
                    let provider_ids: Vec<String> = self
                        .all_ids
                        .iter()
                        .filter(|id| {
                            self.models_by_id
                                .get(*id)
                                .is_some_and(|m| &m.provider == provider)
                        })
                        .cloned()
                        .collect();
                    let all_enabled = provider_ids
                        .iter()
                        .all(|id| is_enabled(&self.enabled_ids, id));
                    self.enabled_ids = if all_enabled {
                        clear_all(&self.enabled_ids, &self.all_ids, Some(&provider_ids))
                    } else {
                        enable_all(&self.enabled_ids, &self.all_ids, Some(&provider_ids))
                    };
                    self.is_dirty = true;
                    self.refresh();
                    self.notify_change();
                }
                return;
            }

            // Save/persist to settings (scoped-models-selector.ts:342-348).
            if read.matches_id(data, "app.models.save") {
                (self.on_persist)(self.enabled_ids.clone());
                self.is_dirty = false;
                return;
            }

            // Ctrl+C — clear the search, or cancel when it is empty
            // (scoped-models-selector.ts:350-359). Upstream matches the raw
            // `ctrl+c` key here (not the `tui.select.cancel` binding) so it
            // can be distinguished from Escape.
            if data == "\u{3}" {
                if self.search_input.get_value().is_empty() {
                    (self.on_cancel)();
                } else {
                    self.search_input.set_value("");
                    self.refresh();
                }
                return;
            }

            // Escape — cancel (scoped-models-selector.ts:361-365).
            if data == "\u{1b}" {
                (self.on_cancel)();
                return;
            }
        }

        // Everything else goes to the search input (scoped-models-selector.ts:
        // 367-369). The keybinding read guard was already dropped at the end
        // of the matching block above (Input::handle_input takes its own
        // read lock on the same global).
        self.search_input.handle_input(data);
        self.refresh();
    }

    fn render(&self, width: usize) -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();

        // Container children (scoped-models-selector.ts:128-150):
        // DynamicBorder, Spacer(1), title, subtitle, Spacer(1), search
        // input, Spacer(1), list, Spacer(1), footer, DynamicBorder.
        lines.extend(self.top_border.render(width));
        lines.push(String::new());
        lines.push(truncate_to_width(
            &self.theme.fg("accent", &Theme::bold("Model Configuration")),
            width,
            "",
            false,
        ));
        lines.push(truncate_to_width(
            &self.theme.fg(
                "muted",
                &format!(
                    "Session-only. {} to save to settings.",
                    key_text("app.models.save")
                ),
            ),
            width,
            "",
            false,
        ));
        lines.push(String::new());

        lines.extend(self.search_input.render(width));
        lines.push(String::new());

        // List container (`updateList`, scoped-models-selector.ts:202-252).
        if self.filtered_items.is_empty() {
            lines.push(self.theme.fg("muted", "  No matching models"));
        } else {
            const MAX_VISIBLE: usize = 8;
            let len = self.filtered_items.len();
            let start_index = len
                .saturating_sub(MAX_VISIBLE)
                .min(self.selected_index.saturating_sub(MAX_VISIBLE / 2));
            let end_index = (start_index + MAX_VISIBLE).min(len);
            let all_enabled = self.enabled_ids.is_none();

            for i in start_index..end_index {
                let item = &self.filtered_items[i];
                let is_selected = i == self.selected_index;
                let prefix = if is_selected {
                    self.theme.fg("accent", "→ ")
                } else {
                    "  ".to_string()
                };
                let id = match &item.model {
                    Some(model) => model.id.clone(),
                    None => item.full_id.clone(),
                };
                let model_text = if is_selected {
                    self.theme.fg("accent", &id)
                } else {
                    id
                };
                let provider_badge = self.theme.fg(
                    "muted",
                    &match &item.model {
                        Some(model) => format!(" [{}]", model.provider),
                        None => " [unavailable]".to_string(),
                    },
                );
                let status = match &item.model {
                    Some(_) if all_enabled => String::new(),
                    Some(_) if item.enabled => self.theme.fg("success", " ✓"),
                    Some(_) => self.theme.fg("dim", " ✗"),
                    None => self.theme.fg("dim", " ✗"),
                };
                lines.push(truncate_to_width(
                    &format!("{prefix}{model_text}{provider_badge}{status}"),
                    width,
                    "...",
                    false,
                ));
            }

            // Scroll indicator (scoped-models-selector.ts:235-239).
            if start_index > 0 || end_index < len {
                let scroll_info = self
                    .theme
                    .fg("muted", &format!("  ({}/{})", self.selected_index + 1, len));
                lines.push(truncate_to_width(&scroll_info, width, "", false));
            }

            // Selected model name (scoped-models-selector.ts:241-251).
            let selected = &self.filtered_items[self.selected_index];
            lines.push(String::new());
            let name_line = match &selected.model {
                Some(model) => format!("  Model Name: {}", model.name),
                None => "  Model unavailable".to_string(),
            };
            lines.push(truncate_to_width(
                &self.theme.fg("muted", &name_line),
                width,
                "...",
                false,
            ));
        }

        lines.push(String::new());
        // The footer can exceed one line; Text wraps it like the upstream
        // `Text` child (scoped-models-selector.ts:147-148).
        lines.extend(Text::new(self.get_footer_text(), 0, 0, None).render(width));
        lines.extend(self.bottom_border.render(width));
        lines
    }

    fn invalidate(&mut self) {
        self.search_input.invalidate();
    }

    fn as_focusable(&self) -> Option<&dyn Focusable> {
        Some(self)
    }

    fn as_focusable_mut(&mut self) -> Option<&mut dyn Focusable> {
        Some(self)
    }
}

impl Focusable for ScopedModelsSelectorComponent {
    fn focused(&self) -> bool {
        self.focused
    }

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
        // Propagate to the search input for IME cursor positioning
        // (scoped-models-selector.ts:99-107).
        self.search_input.set_focused(focused);
    }
}

impl ScopedModelsSelectorComponent {
    /// The ids to enable/clear: the filtered items when a search is active,
    /// otherwise all (scoped-models-selector.ts:307, 317).
    fn filtered_target_ids(&self) -> Option<Vec<String>> {
        if self.search_input.get_value().is_empty() {
            None
        } else {
            Some(
                self.filtered_items
                    .iter()
                    .map(|item| item.full_id.clone())
                    .collect(),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::themes::load_theme;
    use pir_ai::types::ApiKind;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;

    fn theme() -> Arc<Theme> {
        Arc::new(load_theme("dark", None).expect("builtin dark theme"))
    }

    /// Install the global 73-entry keybindings table (tui.select.*,
    /// app.models.*, ...).
    fn install_keybindings() {
        crate::modes::interactive::interactive_mode::install_global_keybindings();
    }

    fn model(id: &str, provider: &str) -> Model {
        Model {
            id: id.to_string(),
            name: format!("{id} Name"),
            api: ApiKind(ApiKind::OPENAI_COMPLETIONS.to_string()),
            provider: provider.to_string(),
            base_url: "https://api.example.com/v1".to_string(),
            reasoning: false,
            thinking_level_map: None,
            input: Vec::new(),
            cost: Default::default(),
            context_window: 128000,
            max_tokens: 16384,
            headers: None,
            compat: None,
        }
    }

    /// Strip ANSI escape sequences (shared with config_selector tests).
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

    /// Capture calls through a shared mutex for callback assertions.
    type CallLog = Arc<Mutex<Vec<Option<Vec<String>>>>>;

    struct Harness {
        component: ScopedModelsSelectorComponent,
        changes: CallLog,
        persists: CallLog,
        cancels: Arc<AtomicU64>,
    }

    fn harness(all_models: Vec<Model>, enabled: Option<Vec<String>>) -> Harness {
        let changes: CallLog = Arc::new(Mutex::new(Vec::new()));
        let persists: CallLog = Arc::new(Mutex::new(Vec::new()));
        let cancels = Arc::new(AtomicU64::new(0));
        let changes_clone = changes.clone();
        let persists_clone = persists.clone();
        let cancels_clone = cancels.clone();
        let component = ScopedModelsSelectorComponent::new(
            all_models,
            enabled,
            theme(),
            Box::new(move |value| {
                changes_clone.lock().unwrap().push(value);
            }),
            Box::new(move |value| {
                persists_clone.lock().unwrap().push(value);
            }),
            Box::new(move || {
                cancels_clone.fetch_add(1, Ordering::SeqCst);
            }),
        );
        Harness {
            component,
            changes,
            persists,
            cancels,
        }
    }

    fn sample_models() -> Vec<Model> {
        vec![
            model("a1", "alpha"),
            model("a2", "alpha"),
            model("b1", "beta"),
        ]
    }

    /// The model rows of a render: lines with a cursor/indent prefix and a
    /// `[provider]` badge (scroll indicator and model-name line excluded).
    fn list_rows(rendered: &[String]) -> Vec<String> {
        plain(rendered.to_vec())
            .into_iter()
            .filter(|line| line.starts_with("→ ") || line.starts_with("  "))
            .filter(|line| line.contains('['))
            .collect()
    }

    #[test]
    fn toggle_on_enter_disables_and_notifies() {
        install_keybindings();
        let mut h = harness(
            sample_models(),
            Some(vec![
                "alpha/a1".to_string(),
                "alpha/a2".to_string(),
                "beta/b1".to_string(),
            ]),
        );
        // Explicit list: all three enabled, no marks.
        let rows = list_rows(&h.component.render(80));
        assert_eq!(rows[0], "→ a1 [alpha] ✓");
        // Enter toggles the selected (first) model off.
        h.component.handle_input("\r");
        let rows = list_rows(&h.component.render(80));
        // The disabled id falls to the end of the sorted list.
        assert_eq!(rows[0], "→ a2 [alpha] ✓");
        assert_eq!(rows[1], "  b1 [beta] ✓");
        assert_eq!(rows[2], "  a1 [alpha] ✗");
        let changes = h.changes.lock().unwrap();
        assert_eq!(
            *changes,
            vec![Some(vec!["alpha/a2".to_string(), "beta/b1".to_string()])]
        );
    }

    #[test]
    fn first_toggle_starts_with_only_that_model() {
        install_keybindings();
        let mut h = harness(sample_models(), None);
        // Move down to b1 (last), then toggle: enabled = [b1] only
        // (scoped-models-selector.ts:26: first toggle starts with only this
        // one).
        h.component.handle_input("\x1b[B");
        h.component.handle_input("\x1b[B");
        h.component.handle_input("\r");
        let changes = h.changes.lock().unwrap();
        assert_eq!(*changes, vec![Some(vec!["beta/b1".to_string()])]);
    }

    #[test]
    fn clear_all_and_enable_all() {
        install_keybindings();
        let mut h = harness(
            sample_models(),
            Some(vec!["alpha/a1".to_string(), "beta/b1".to_string()]),
        );
        // Ctrl+X clears everything.
        h.component.handle_input("\x18");
        let changes = h.changes.lock().unwrap();
        assert_eq!(*changes, vec![Some(vec![])]);
        // Drop the guard before the next key: `notify_change` locks the same
        // mutex from inside handle_input (std Mutex is not reentrant).
        drop(changes);
        // Footer reports 0/3 enabled (unsaved).
        let joined = plain(h.component.render(80)).join("\n");
        assert!(joined.contains("0/3 enabled"));
        assert!(joined.contains("(unsaved)"));
        // Ctrl+A re-enables everything → None (all enabled).
        h.component.handle_input("\x01");
        let changes = h.changes.lock().unwrap();
        assert_eq!(changes[1], None);
        drop(changes);
        let joined = plain(h.component.render(80)).join("\n");
        assert!(joined.contains("all enabled"));
    }

    #[test]
    fn reorder_moves_selected_enabled_model() {
        install_keybindings();
        let mut h = harness(
            sample_models(),
            Some(vec![
                "alpha/a1".to_string(),
                "alpha/a2".to_string(),
                "beta/b1".to_string(),
            ]),
        );
        // Select a2 (index 1), move it down with Alt+Down ("\x1bn",
        // keys.rs:464).
        h.component.handle_input("\x1b[B");
        h.component.handle_input("\x1bn");
        let changes = h.changes.lock().unwrap();
        assert_eq!(
            *changes,
            vec![Some(vec![
                "alpha/a1".to_string(),
                "beta/b1".to_string(),
                "alpha/a2".to_string(),
            ])]
        );
        // Alt+Up moves it back up (selection follows the item).
        drop(changes);
        h.component.handle_input("\x1bp");
        let changes = h.changes.lock().unwrap();
        assert_eq!(
            changes[1],
            Some(vec![
                "alpha/a1".to_string(),
                "alpha/a2".to_string(),
                "beta/b1".to_string(),
            ])
        );
    }

    #[test]
    fn save_persists_and_clears_dirty_flag() {
        install_keybindings();
        let mut h = harness(sample_models(), None);
        h.component.handle_input("\r"); // toggle → dirty
        h.component.handle_input("\x13"); // Ctrl+S
        let persists = h.persists.lock().unwrap();
        // The first toggle from "all enabled" enables only the toggled id
        // (scoped-models-selector.ts:26).
        assert_eq!(persists[0], Some(vec!["alpha/a1".to_string()]));
        let joined = plain(h.component.render(80)).join("\n");
        assert!(!joined.contains("(unsaved)"), "saved footer is not dirty");
    }

    #[test]
    fn toggle_provider_flips_all_models_of_provider() {
        install_keybindings();
        // Single-provider catalog: the provider toggle covers the whole
        // list, exercising the explicit-list → None round trip.
        let mut h = harness(
            vec![
                model("a1", "alpha"),
                model("a2", "alpha"),
                model("a3", "alpha"),
            ],
            Some(vec![
                "alpha/a1".to_string(),
                "alpha/a2".to_string(),
                "alpha/a3".to_string(),
            ]),
        );
        // Selected a1: Ctrl+P disables every alpha model.
        h.component.handle_input("\x10");
        let changes = h.changes.lock().unwrap();
        assert_eq!(*changes, vec![Some(vec![])]);
        // Ctrl+P again re-enables them — the whole provider is the whole
        // catalog, so the explicit list collapses back to None (all
        // enabled).
        drop(changes);
        h.component.handle_input("\x10");
        let changes = h.changes.lock().unwrap();
        assert_eq!(changes[1], None);
    }

    #[test]
    fn ctrl_c_clears_search_before_cancelling() {
        install_keybindings();
        let mut h = harness(sample_models(), None);
        // Type a search query.
        h.component.handle_input("a");
        h.component.handle_input("l");
        assert_eq!(h.component.get_search_input().get_value(), "al");
        let rows = list_rows(&h.component.render(80));
        assert_eq!(rows.len(), 2, "filtered to alpha models");
        // Ctrl+C clears the search instead of cancelling.
        h.component.handle_input("\x03");
        assert_eq!(h.component.get_search_input().get_value(), "");
        assert_eq!(h.cancels.load(Ordering::SeqCst), 0);
        // Ctrl+C again (search empty) cancels.
        h.component.handle_input("\x03");
        assert_eq!(h.cancels.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn escape_cancels() {
        install_keybindings();
        let mut h = harness(sample_models(), None);
        h.component.handle_input("\x1b");
        assert_eq!(h.cancels.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn unavailable_enabled_ids_render_as_unavailable() {
        install_keybindings();
        let h = harness(
            sample_models(),
            Some(vec!["alpha/a1".to_string(), "gone/x1".to_string()]),
        );
        let joined = plain(h.component.render(80)).join("\n");
        // The stale id is listed with the unavailable badge (the full id),
        // and the footer counts it.
        assert!(joined.contains("gone/x1 [unavailable]"));
        assert!(joined.contains("1 unavailable"));
    }
}
