//! Port of `packages/tui/src/components/select-list.ts` @ pi 0.82.1
//! (2efa728).
//!
//! Intentional differences:
//! - Theme and truncate callbacks are `Box<dyn Fn ... + Send + Sync>` fields
//!   (upstream plain functions); `SelectList::new` takes the theme as an
//!   `Arc<SelectListTheme>` so components (e.g. the Editor) can reuse one
//!   theme across recreations without cloning the callbacks.
//! - Callbacks are `Box<dyn FnMut ... + Send>` fields instead of plain
//!   function properties.
//! - `set_filter` takes `&str` and clones the matching items (upstream keeps
//!   references to the same item objects).
//! - `handle_input` early-returns on an empty filtered list; upstream's
//!   wrap-around arithmetic would leave an unobservable `-1`/`1` index.
//! - `SelectListTheme::selected_prefix` is carried for interface parity but,
//!   like upstream, is never read by the component.

use std::sync::Arc;

use crate::keybindings::{get_keybindings, Keybinding};
use crate::tui::Component;
use crate::utils::{truncate_to_width, visible_width};

const DEFAULT_PRIMARY_COLUMN_WIDTH: usize = 32;
const PRIMARY_COLUMN_GAP: usize = 2;
const MIN_DESCRIPTION_WIDTH: usize = 10;

/// `normalizeToSingleLine` (select-list.ts:9): collapse CR/LF runs into a
/// single space, then trim with the ECMA-262 `\s` set (JS `String.trim`).
fn normalize_to_single_line(text: &str) -> String {
    let mut collapsed = String::with_capacity(text.len());
    let mut in_newline_run = false;
    for ch in text.chars() {
        if ch == '\r' || ch == '\n' {
            if !in_newline_run {
                collapsed.push(' ');
                in_newline_run = true;
            }
        } else {
            in_newline_run = false;
            collapsed.push(ch);
        }
    }

    // ECMA-262 `\s` (same set as utils::is_whitespace_char): includes
    // U+FEFF, excludes U+0085.
    let is_js_whitespace = |c: char| {
        matches!(
            c,
            '\u{0009}'..='\u{000d}'
                | '\u{0020}'
                | '\u{00a0}'
                | '\u{1680}'
                | '\u{2000}'..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
        )
    };
    let start = collapsed
        .find(|c| !is_js_whitespace(c))
        .unwrap_or(collapsed.len());
    let end = collapsed
        .rfind(|c| !is_js_whitespace(c))
        .map(|index| index + 1)
        .unwrap_or(start);
    collapsed[start..end].to_string()
}

/// JS `Math.max(min, Math.min(value, max))` (select-list.ts:10).
fn clamp(value: usize, min: usize, max: usize) -> usize {
    value.max(min).min(max)
}

/// A selectable item (upstream `SelectItem`, select-list.ts:12-16).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectItem {
    pub value: String,
    pub label: String,
    pub description: Option<String>,
}

/// Text transform callback (upstream `(text: string) => string`).
pub type SelectListTextFn = Box<dyn Fn(&str) -> String + Send + Sync>;
/// Primary-column truncator (upstream `truncatePrimary`).
#[allow(clippy::type_complexity)] // mirrors the upstream callback type exactly
pub type TruncatePrimaryFn =
    Box<dyn Fn(&SelectListTruncatePrimaryContext<'_>) -> String + Send + Sync>;
/// Item callback (upstream `(item: SelectItem) => void`).
pub type SelectItemFn = Box<dyn FnMut(&SelectItem) + Send>;

/// Select list theme callbacks (upstream `SelectListTheme`,
/// select-list.ts:18-24).
pub struct SelectListTheme {
    /// Unused upstream — carried for interface parity.
    pub selected_prefix: SelectListTextFn,
    pub selected_text: SelectListTextFn,
    pub description: SelectListTextFn,
    pub scroll_info: SelectListTextFn,
    pub no_match: SelectListTextFn,
}

impl SelectListTheme {
    /// Identity theme — every callback returns its input unchanged (used by
    /// tests and snapshots).
    pub fn identity() -> Self {
        let identity = |text: &str| text.to_string();
        Self {
            selected_prefix: Box::new(identity),
            selected_text: Box::new(identity),
            description: Box::new(identity),
            scroll_info: Box::new(identity),
            no_match: Box::new(identity),
        }
    }
}

/// Context passed to a custom primary-column truncator (upstream
/// `SelectListTruncatePrimaryContext`, select-list.ts:26-32).
pub struct SelectListTruncatePrimaryContext<'a> {
    pub text: &'a str,
    pub max_width: usize,
    pub column_width: usize,
    pub item: &'a SelectItem,
    pub is_selected: bool,
}

/// Layout options for the primary column (upstream
/// `SelectListLayoutOptions`, select-list.ts:34-38).
#[derive(Default)]
pub struct SelectListLayoutOptions {
    pub min_primary_column_width: Option<usize>,
    pub max_primary_column_width: Option<usize>,
    pub truncate_primary: Option<TruncatePrimaryFn>,
}

/// Select list with prefix filtering and a scrolling window (upstream
/// `SelectList`, select-list.ts:40).
pub struct SelectList {
    items: Vec<SelectItem>,
    filtered_items: Vec<SelectItem>,
    selected_index: usize,
    max_visible: usize,
    theme: Arc<SelectListTheme>,
    layout: SelectListLayoutOptions,

    /// Called when the selection is confirmed (upstream `onSelect`).
    pub on_select: Option<SelectItemFn>,
    /// Called on escape/cancel (upstream `onCancel`).
    pub on_cancel: Option<Box<dyn FnMut() + Send>>,
    /// Called when the selection moves (upstream `onSelectionChange`).
    pub on_selection_change: Option<SelectItemFn>,
}

impl SelectList {
    pub fn new(
        items: Vec<SelectItem>,
        max_visible: usize,
        theme: Arc<SelectListTheme>,
        layout: Option<SelectListLayoutOptions>,
    ) -> Self {
        let filtered_items = items.clone();
        Self {
            items,
            filtered_items,
            selected_index: 0,
            max_visible,
            theme,
            layout: layout.unwrap_or_default(),
            on_select: None,
            on_cancel: None,
            on_selection_change: None,
        }
    }

    /// Filter items by a value prefix (case-insensitive, select-list.ts:60-64).
    pub fn set_filter(&mut self, filter: &str) {
        let filter = filter.to_lowercase();
        self.filtered_items = self
            .items
            .iter()
            .filter(|item| item.value.to_lowercase().starts_with(&filter))
            .cloned()
            .collect();
        // Reset selection when filter changes
        self.selected_index = 0;
    }

    /// Clamp the selected index into the filtered list
    /// (select-list.ts:66-68).
    pub fn set_selected_index(&mut self, index: usize) {
        self.selected_index = index.min(self.filtered_items.len().saturating_sub(1));
    }

    /// The currently selected item, or `None` when the list is empty
    /// (upstream `getSelectedItem`, select-list.ts:225-228).
    pub fn get_selected_item(&self) -> Option<&SelectItem> {
        self.filtered_items.get(self.selected_index)
    }

    fn render_item(
        &self,
        item: &SelectItem,
        is_selected: bool,
        width: usize,
        description_single_line: Option<&str>,
        primary_column_width: usize,
    ) -> String {
        let prefix = if is_selected { "→ " } else { "  " };
        let prefix_width = visible_width(prefix);

        if let Some(description_single_line) = description_single_line {
            if width > 40 {
                let effective_primary_column_width = primary_column_width
                    .min(width.saturating_sub(prefix_width + 4))
                    .max(1);
                let max_primary_width = effective_primary_column_width
                    .saturating_sub(PRIMARY_COLUMN_GAP)
                    .max(1);
                let truncated_value = self.truncate_primary(
                    item,
                    is_selected,
                    max_primary_width,
                    effective_primary_column_width,
                );
                let truncated_value_width = visible_width(&truncated_value);
                let spacing = " ".repeat(
                    effective_primary_column_width
                        .saturating_sub(truncated_value_width)
                        .max(1),
                );
                let description_start = prefix_width + truncated_value_width + spacing.len();
                let remaining_width = width.saturating_sub(description_start + 2); // -2 for safety

                if remaining_width > MIN_DESCRIPTION_WIDTH {
                    let truncated_desc =
                        truncate_to_width(description_single_line, remaining_width, "", false);
                    if is_selected {
                        return (self.theme.selected_text)(&format!(
                            "{prefix}{truncated_value}{spacing}{truncated_desc}"
                        ));
                    }

                    let desc_text = (self.theme.description)(&format!("{spacing}{truncated_desc}"));
                    return format!("{prefix}{truncated_value}{desc_text}");
                }
            }
        }

        let max_width = width.saturating_sub(prefix_width + 2);
        let truncated_value = self.truncate_primary(item, is_selected, max_width, max_width);
        if is_selected {
            return (self.theme.selected_text)(&format!("{prefix}{truncated_value}"));
        }

        format!("{prefix}{truncated_value}")
    }

    fn get_primary_column_width(&self) -> usize {
        let (min, max) = self.get_primary_column_bounds();
        let widest_primary = self.filtered_items.iter().fold(0, |widest, item| {
            widest.max(visible_width(Self::get_display_value(item)) + PRIMARY_COLUMN_GAP)
        });

        clamp(widest_primary, min, max)
    }

    /// `getPrimaryColumnBounds` (select-list.ts:187-197): each bound falls
    /// back to the other option and then to the default.
    fn get_primary_column_bounds(&self) -> (usize, usize) {
        let raw_min = self
            .layout
            .min_primary_column_width
            .or(self.layout.max_primary_column_width)
            .unwrap_or(DEFAULT_PRIMARY_COLUMN_WIDTH);
        let raw_max = self
            .layout
            .max_primary_column_width
            .or(self.layout.min_primary_column_width)
            .unwrap_or(DEFAULT_PRIMARY_COLUMN_WIDTH);

        (raw_min.min(raw_max).max(1), raw_min.max(raw_max).max(1))
    }

    /// `truncatePrimary` (select-list.ts:199-212): custom truncator, then
    /// always re-truncated to `max_width`.
    fn truncate_primary(
        &self,
        item: &SelectItem,
        is_selected: bool,
        max_width: usize,
        column_width: usize,
    ) -> String {
        let display_value = Self::get_display_value(item);
        let truncated_value = match &self.layout.truncate_primary {
            Some(truncate_primary) => truncate_primary(&SelectListTruncatePrimaryContext {
                text: display_value,
                max_width,
                column_width,
                item,
                is_selected,
            }),
            None => truncate_to_width(display_value, max_width, "", false),
        };

        truncate_to_width(&truncated_value, max_width, "", false)
    }

    /// `getDisplayValue` (select-list.ts:214-216): label takes precedence
    /// over value.
    fn get_display_value(item: &SelectItem) -> &str {
        if item.label.is_empty() {
            &item.value
        } else {
            &item.label
        }
    }

    fn notify_selection_change(&mut self) {
        if let Some(selected_item) = self.filtered_items.get(self.selected_index) {
            if let Some(on_selection_change) = self.on_selection_change.as_mut() {
                on_selection_change(selected_item);
            }
        }
    }
}

impl Component for SelectList {
    fn invalidate(&mut self) {
        // No cached state to invalidate currently
    }

    fn render(&self, width: usize) -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();

        // If no items match filter, show message
        if self.filtered_items.is_empty() {
            lines.push((self.theme.no_match)("  No matching commands"));
            return lines;
        }

        let primary_column_width = self.get_primary_column_width();

        // Calculate visible range with scrolling
        let start_index = self
            .selected_index
            .saturating_sub(self.max_visible / 2)
            .min(self.filtered_items.len().saturating_sub(self.max_visible));
        let end_index = (start_index + self.max_visible).min(self.filtered_items.len());

        // Render visible items
        for i in start_index..end_index {
            let item = &self.filtered_items[i];

            let is_selected = i == self.selected_index;
            let description_single_line = item.description.as_deref().map(normalize_to_single_line);
            lines.push(self.render_item(
                item,
                is_selected,
                width,
                description_single_line.as_deref(),
                primary_column_width,
            ));
        }

        // Add scroll indicators if needed
        if start_index > 0 || end_index < self.filtered_items.len() {
            let scroll_text = format!(
                "  ({}/{})",
                self.selected_index + 1,
                self.filtered_items.len()
            );
            // Truncate if too long for terminal
            lines.push((self.theme.scroll_info)(&truncate_to_width(
                &scroll_text,
                width.saturating_sub(2),
                "",
                false,
            )));
        }

        lines
    }

    fn handle_input(&mut self, key_data: &str) {
        // Empty lists have nothing to select; upstream's wrap-around
        // arithmetic would produce an unobservable -1/1 index instead.
        if self.filtered_items.is_empty() {
            return;
        }

        let kb = get_keybindings();
        let kb = kb.read().unwrap_or_else(|poisoned| poisoned.into_inner());
        // Up arrow - wrap to bottom when at top
        if kb.matches(key_data, Keybinding::SelectUp) {
            self.selected_index = if self.selected_index == 0 {
                self.filtered_items.len() - 1
            } else {
                self.selected_index - 1
            };
            self.notify_selection_change();
        }
        // Down arrow - wrap to top when at bottom
        else if kb.matches(key_data, Keybinding::SelectDown) {
            self.selected_index = if self.selected_index == self.filtered_items.len() - 1 {
                0
            } else {
                self.selected_index + 1
            };
            self.notify_selection_change();
        }
        // Enter
        else if kb.matches(key_data, Keybinding::SelectConfirm) {
            if let Some(selected_item) = self.filtered_items.get(self.selected_index) {
                if let Some(on_select) = self.on_select.as_mut() {
                    on_select(selected_item);
                }
            }
        }
        // Escape or Ctrl+C
        else if kb.matches(key_data, Keybinding::SelectCancel) {
            if let Some(on_cancel) = self.on_cancel.as_mut() {
                on_cancel();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! Ports of `test/select-list.test.ts` @ pi 0.82.1 (2efa728), all 5
    //! cases.

    use super::*;

    /// `visibleIndexOf` (select-list.test.ts:14-18): column of `text` within
    /// `line`.
    fn visible_index_of(line: &str, text: &str) -> usize {
        let index = line.find(text).expect("text must be present in line");
        visible_width(&line[..index])
    }

    /// `line.indexOf(text)` in characters (upstream UTF-16 code units; the
    /// difference only matters for astral prefix characters like "→").
    fn char_index_of(line: &str, text: &str) -> usize {
        let index = line.find(text).expect("text must be present in line");
        line[..index].chars().count()
    }

    fn item(value: &str, description: Option<&str>) -> SelectItem {
        SelectItem {
            value: value.to_string(),
            label: value.to_string(),
            description: description.map(str::to_string),
        }
    }

    fn list(items: Vec<SelectItem>, layout: Option<SelectListLayoutOptions>) -> SelectList {
        SelectList::new(items, 5, Arc::new(SelectListTheme::identity()), layout)
    }

    #[test]
    fn normalizes_multiline_descriptions_to_single_line() {
        let items = vec![item("test", Some("Line one\nLine two\nLine three"))];

        let list = list(items, None);
        let rendered = list.render(100);

        assert!(!rendered.is_empty());
        assert!(!rendered[0].contains('\n'));
        assert!(rendered[0].contains("Line one Line two Line three"));
    }

    #[test]
    fn keeps_descriptions_aligned_when_the_primary_text_is_truncated() {
        let items = vec![
            item("short", Some("short description")),
            item(
                "very-long-command-name-that-needs-truncation",
                Some("long description"),
            ),
        ];

        let list = list(items, None);
        let rendered = list.render(80);

        assert_eq!(
            visible_index_of(&rendered[0], "short description"),
            visible_index_of(&rendered[1], "long description")
        );
    }

    #[test]
    fn uses_the_configured_minimum_primary_column_width() {
        let items = vec![item("a", Some("first")), item("bb", Some("second"))];

        let list = list(
            items,
            Some(SelectListLayoutOptions {
                min_primary_column_width: Some(12),
                max_primary_column_width: Some(20),
                truncate_primary: None,
            }),
        );
        let rendered = list.render(80);

        assert_eq!(char_index_of(&rendered[0], "first"), 14);
        assert_eq!(char_index_of(&rendered[1], "second"), 14);
    }

    #[test]
    fn uses_the_configured_maximum_primary_column_width() {
        let items = vec![
            item(
                "very-long-command-name-that-needs-truncation",
                Some("first"),
            ),
            item("short", Some("second")),
        ];

        let list = list(
            items,
            Some(SelectListLayoutOptions {
                min_primary_column_width: Some(12),
                max_primary_column_width: Some(20),
                truncate_primary: None,
            }),
        );
        let rendered = list.render(80);

        assert_eq!(visible_index_of(&rendered[0], "first"), 22);
        assert_eq!(visible_index_of(&rendered[1], "second"), 22);
    }

    #[test]
    fn allows_overriding_primary_truncation_while_preserving_description_alignment() {
        let items = vec![
            item(
                "very-long-command-name-that-needs-truncation",
                Some("first"),
            ),
            item("short", Some("second")),
        ];

        let list = list(
            items,
            Some(SelectListLayoutOptions {
                min_primary_column_width: Some(12),
                max_primary_column_width: Some(12),
                truncate_primary: Some(Box::new(
                    |context: &SelectListTruncatePrimaryContext<'_>| {
                        if context.text.chars().count() <= context.max_width {
                            return context.text.to_string();
                        }
                        format!(
                            "{}…",
                            context
                                .text
                                .chars()
                                .take(context.max_width.saturating_sub(1))
                                .collect::<String>()
                        )
                    },
                )),
            }),
        );
        let rendered = list.render(80);

        assert!(rendered[0].contains('…'));
        assert_eq!(
            visible_index_of(&rendered[0], "first"),
            visible_index_of(&rendered[1], "second")
        );
    }
}
