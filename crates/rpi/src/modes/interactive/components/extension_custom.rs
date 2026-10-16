//! `ctx.ui.custom()` declarative dialog (V17-11 FR-B R1), the interaction
//! round-trip for `ComponentTree` factories.
//!
//! Upstream `showExtensionCustom` (interactive-mode.ts:2966 @ abe508e1b) mounts the
//! factory's component with keyboard focus and resolves the returned promise
//! through the `done(result)` callback. Factories cannot cross the JSON ABI
//! (ADR-0007 gap 3), so the additive approximation here interprets a
//! declarative tree: top-level children whose `props.done` is present are
//! selectable, `<Up>`/`<Down>` move the selection, `<Enter>` resolves with
//! the selected child's `done` value, and `<Escape>` resolves `undefined`.
//! Arbitrary interactive components stay the domain of the interactive UI ABI
//! (`ui.mountComponent`, ADR-0024); a tree without `done` children blocks
//! until Escape, matching upstream's "resolve only via `done`".
//!
//! The selection renders as a `>` marker prefix on the selectable rows; the
//! remaining lines pass through untouched.

use std::sync::{Arc, Mutex};

use rpi_tui::tui::{Component, Focusable};
use serde_json::Value;
use tokio::sync::oneshot;

use crate::core::themes::Theme;
use crate::modes::interactive::component_tree::component_from_tree;

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// One top-level tree child.
struct CustomRow {
    component: Box<dyn Component>,
    /// `props.done` value; `None` marks the row as non-selectable.
    done: Option<Value>,
}

pub struct ExtensionCustomComponent {
    theme: Arc<Theme>,
    rows: Vec<CustomRow>,
    /// Index into `rows` of the selected selectable row.
    selected: Option<usize>,
    sender: Mutex<Option<oneshot::Sender<Option<Value>>>>,
    focused: bool,
}

impl ExtensionCustomComponent {
    /// Build the dialog from a declarative tree. `box`/`column` roots
    /// contribute their children as rows; any other node is a single row.
    pub fn new(theme: Arc<Theme>, tree: &Value, sender: oneshot::Sender<Option<Value>>) -> Self {
        let children: Vec<Value> = match tree.get("type").and_then(Value::as_str) {
            Some("box" | "column") => tree
                .get("children")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
            _ => vec![tree.clone()],
        };
        let rows: Vec<CustomRow> = children
            .iter()
            .map(|node| CustomRow {
                component: component_from_tree(node, &theme),
                done: node
                    .get("props")
                    .and_then(|props| props.get("done"))
                    .cloned(),
            })
            .collect();
        let selected = rows.iter().position(|row| row.done.is_some());
        Self {
            theme,
            rows,
            selected,
            sender: Mutex::new(Some(sender)),
            focused: true,
        }
    }

    fn selectable(&self) -> Vec<usize> {
        self.rows
            .iter()
            .enumerate()
            .filter_map(|(index, row)| row.done.is_some().then_some(index))
            .collect()
    }

    fn move_selection(&mut self, delta: isize) {
        let selectable = self.selectable();
        if selectable.is_empty() {
            return;
        }
        let current = self
            .selected
            .and_then(|selected| selectable.iter().position(|index| *index == selected))
            .unwrap_or(0) as isize;
        let next = (current + delta).rem_euclid(selectable.len() as isize) as usize;
        self.selected = Some(selectable[next]);
    }

    fn resolve(&self, value: Option<Value>) {
        if let Some(sender) = lock(&self.sender).take() {
            let _ = sender.send(value);
        }
    }
}

impl Component for ExtensionCustomComponent {
    fn render(&self, width: usize) -> Vec<String> {
        let mut lines = Vec::new();
        for (index, row) in self.rows.iter().enumerate() {
            let selected = self.selected == Some(index);
            let marker = if row.done.is_some() {
                if selected { "> " } else { "  " }
            } else {
                ""
            };
            for line in row.component.render(width.saturating_sub(2)) {
                lines.push(format!("{marker}{line}"));
            }
        }
        let _ = &self.theme;
        lines
    }

    fn handle_input(&mut self, data: &str) {
        match data {
            "\x1b[A" | "k" => self.move_selection(-1),
            "\x1b[B" | "j" => self.move_selection(1),
            "\r" | "\n" => {
                let value = self
                    .selected
                    .and_then(|index| self.rows.get(index))
                    .and_then(|row| row.done.clone());
                self.resolve(value);
            }
            "\x1b" | "\x03" => self.resolve(None),
            _ => {}
        }
    }

    fn has_handle_input(&self) -> bool {
        true
    }

    fn as_focusable(&self) -> Option<&dyn Focusable> {
        Some(self)
    }

    fn as_focusable_mut(&mut self) -> Option<&mut dyn Focusable> {
        Some(self)
    }

    fn invalidate(&mut self) {
        for row in &mut self.rows {
            row.component.invalidate();
        }
    }
}

impl Focusable for ExtensionCustomComponent {
    fn focused(&self) -> bool {
        self.focused
    }

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn dark() -> Arc<Theme> {
        Arc::new(crate::core::themes::load_theme("dark", None).expect("dark theme"))
    }

    fn dialog() -> (ExtensionCustomComponent, oneshot::Receiver<Option<Value>>) {
        let (tx, rx) = oneshot::channel();
        let tree = json!({
            "type": "box",
            "children": [
                {"type": "text", "props": {"text": "Header"}},
                {"type": "text", "props": {"text": "Option A", "done": {"value": "a"}}},
                {"type": "text", "props": {"text": "Option B", "done": "b"}},
            ]
        });
        (ExtensionCustomComponent::new(dark(), &tree, tx), rx)
    }

    #[tokio::test]
    async fn enter_resolves_with_the_selected_done_value() {
        let (mut dialog, rx) = dialog();
        let rendered = dialog.render(40).join("\n");
        assert!(rendered.contains("> Option A"), "{rendered}");
        dialog.handle_input("\x1b[B");
        dialog.handle_input("\r");
        assert_eq!(rx.await.expect("resolved"), Some(json!("b")));
    }

    #[tokio::test]
    async fn escape_resolves_undefined() {
        let (mut dialog, rx) = dialog();
        dialog.handle_input("\x1b");
        assert_eq!(rx.await.expect("resolved"), None);
    }

    #[tokio::test]
    async fn repeated_input_resolves_once() {
        let (mut dialog, rx) = dialog();
        dialog.handle_input("\r");
        dialog.handle_input("\x1b");
        assert_eq!(rx.await.expect("resolved"), Some(json!({"value": "a"})));
    }

    #[tokio::test]
    async fn tree_without_done_rows_stays_until_escape() {
        let (tx, rx) = oneshot::channel();
        let tree = json!({"type": "text", "props": {"text": "display only"}});
        let mut dialog = ExtensionCustomComponent::new(dark(), &tree, tx);
        assert!(dialog.selectable().is_empty());
        dialog.handle_input("\r");
        dialog.handle_input("\x1b");
        assert_eq!(rx.await.expect("resolved"), None);
    }
}
