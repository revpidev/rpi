//! Declarative component-tree (ComponentTree v1) → pir-tui component mapping
//! (T15 W4).
//!
//! Schema: [`pir_ext_host::types::COMPONENT_TREE_SCHEMA_V1`]. Extension
//! renderers (message/entry renderers, tool `renderCall`/`renderResult`,
//! widget/footer/header/custom factories) produce the JSON tree; this module
//! is the single mapping into pir-tui components.
//!
//! Unknown node types render as a text node containing the JSON
//! (fail-visible); malformed nodes degrade likewise rather than panic
//! (extension output is untrusted input, coding-standards §11).

use std::sync::Arc;

use pir_tui::components::r#box::Box as TuiBox;
use pir_tui::components::spacer::Spacer;
use pir_tui::components::text::Text;
use pir_tui::tui::Component;
use serde_json::Value;

use super::components::dynamic_border::DynamicBorder;
use crate::core::themes::Theme;

/// Map a ComponentTree JSON node onto a pir-tui component.
pub fn component_from_tree(tree: &Value, theme: &Arc<Theme>) -> Box<dyn Component> {
    let node_type = tree.get("type").and_then(Value::as_str).unwrap_or("");
    let props = tree.get("props").cloned().unwrap_or(Value::Null);
    match node_type {
        "text" => Box::new(Text::new(
            styled_text(&props, theme),
            usize_prop(&props, "paddingX"),
            usize_prop(&props, "paddingY"),
            None,
        )),
        "spacer" => Box::new(Spacer::new(usize_prop(&props, "lines").max(1))),
        "box" | "column" => {
            let mut container = TuiBox::new(
                usize_prop(&props, "paddingX"),
                usize_prop(&props, "paddingY"),
                None,
            );
            for child in tree
                .get("children")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
            {
                container.add_child(component_from_tree(&child, theme));
            }
            if node_type == "box" {
                // Border as top/bottom border lines (the pir-tui `Box` has
                // no border; DynamicBorder is the pir idiom).
                let color = props
                    .get("borderColor")
                    .and_then(Value::as_str)
                    .unwrap_or("border")
                    .to_owned();
                let border_theme = Arc::clone(theme);
                let border_color = color.clone();
                let mut bordered = TuiBox::new(0, 0, None);
                bordered.add_child(Box::new(DynamicBorder::new(Box::new(move |text| {
                    border_theme.fg(&border_color, text)
                }))));
                bordered.add_child(Box::new(container));
                let border_theme = Arc::clone(theme);
                bordered.add_child(Box::new(DynamicBorder::new(Box::new(move |text| {
                    border_theme.fg(&color, text)
                }))));
                return Box::new(bordered);
            }
            Box::new(container)
        }
        // Fail-visible fallback for unknown/malformed nodes.
        _ => Box::new(Text::new(tree.to_string(), 0, 0, None)),
    }
}

/// `Option<ComponentTree>` renderer convenience: `None`/`null` stays `None`
/// (the caller falls back to its default rendering, custom-message.ts:69-85).
pub fn component_from_optional_tree(
    tree: Option<&Value>,
    theme: &Arc<Theme>,
) -> Option<Box<dyn Component>> {
    let tree = tree.filter(|t| !t.is_null())?;
    Some(component_from_tree(tree, theme))
}

fn usize_prop(props: &Value, key: &str) -> usize {
    props.get(key).and_then(Value::as_u64).unwrap_or(0).min(64) as usize
}

/// Text styling: `fg` via the theme (color names only in v1), then
/// bold/italic/underline/dim markers.
fn styled_text(props: &Value, theme: &Arc<Theme>) -> String {
    let mut text = props
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    match props.get("fg").and_then(Value::as_str) {
        Some(fg) => text = theme.fg(fg, &text),
        // `dim` maps onto the theme's dim color (no dedicated ANSI helper).
        None if props.get("dim").and_then(Value::as_bool).unwrap_or(false) => {
            text = theme.fg("dim", &text);
        }
        None => {}
    }
    if props.get("bold").and_then(Value::as_bool).unwrap_or(false) {
        text = Theme::bold(&text);
    }
    if props
        .get("italic")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        text = Theme::italic(&text);
    }
    if props
        .get("underline")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        text = Theme::underline(&text);
    }
    text
}
