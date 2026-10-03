//! Declarative component-tree (ComponentTree v1) → rpi-tui component mapping
//! (T15 W4).
//!
//! Schema: [`rpi_ext_host::types::COMPONENT_TREE_SCHEMA_V1`]. Extension
//! renderers (message/entry renderers, tool `renderCall`/`renderResult`,
//! widget/footer/header/custom factories) produce the JSON tree; this module
//! is the single mapping into rpi-tui components.
//!
//! Unknown node types render as a text node containing the JSON
//! (fail-visible); malformed nodes degrade likewise rather than panic
//! (extension output is untrusted input, coding-standards §11).

use std::sync::Arc;

use rpi_tui::components::r#box::Box as TuiBox;
use rpi_tui::components::spacer::Spacer;
use rpi_tui::components::text::Text;
use rpi_tui::components::truncated_text::TruncatedText;
use rpi_tui::tui::Component;
use serde_json::Value;

use super::components::dynamic_border::DynamicBorder;
use super::components::visual_truncate::{VisualKeep, VisualLinePreview};
use crate::core::themes::Theme;

/// Map a ComponentTree JSON node onto a rpi-tui component.
pub fn component_from_tree(tree: &Value, theme: &Arc<Theme>) -> Box<dyn Component> {
    let node_type = tree.get("type").and_then(Value::as_str).unwrap_or("");
    let props = tree.get("props").cloned().unwrap_or(Value::Null);
    match node_type {
        // `truncate` (TE11 FR-E.2): clip oversized lines to the render width
        // (ANSI-preserving, `...` ellipsis) instead of word-wrapping — the
        // fleet rows and single-line title rows must stay one visual line at
        // any terminal width, and the extension has no width knowledge of
        // its own to pre-clip with. Multi-line text truncates to its first
        // line (single-line component semantics).
        "text" if bool_prop(&props, "truncate") => Box::new(TruncatedText::new(
            styled_text(&props, theme),
            usize_prop(&props, "paddingX"),
            usize_prop(&props, "paddingY"),
        )),
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
                // Border as top/bottom border lines (the rpi-tui `Box` has
                // no border; DynamicBorder is the rpi idiom).
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
        // Additive v1 node (V16-09, 0582d9c11): collapsed output limited
        // to visual lines. A single long line such as minified JSON must
        // not fill the screen; the native fallback and bash renderer use
        // the same component directly.
        "visualPreview" => {
            let raw = props.get("text").and_then(Value::as_str).unwrap_or("");
            let text = match props.get("fg").and_then(Value::as_str) {
                // Style each line separately so kept lines carry their
                // color after truncation slices the block.
                Some(fg) => raw
                    .split('\n')
                    .map(|line| theme.fg(fg, line))
                    .collect::<Vec<_>>()
                    .join("\n"),
                None => raw.to_owned(),
            };
            let keep = match props.get("keep").and_then(Value::as_str) {
                Some("end") => VisualKeep::End,
                _ => VisualKeep::Start,
            };
            let max_visual_lines = props
                .get("maxVisualLines")
                .and_then(Value::as_u64)
                .unwrap_or(5)
                .clamp(1, 64) as usize;
            let mut preview = VisualLinePreview::new(text, max_visual_lines, keep);
            if let Some(template) = props.get("hint").and_then(Value::as_str) {
                let template = template.to_owned();
                let hint_theme = Arc::clone(theme);
                preview = preview.with_hint(Box::new(move |hidden| {
                    hint_theme.fg(
                        "muted",
                        &template.replace("{hidden}", &hidden.to_string()),
                    )
                }));
            }
            Box::new(preview)
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

fn bool_prop(props: &Value, key: &str) -> bool {
    props.get(key).and_then(Value::as_bool).unwrap_or(false)
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

#[cfg(test)]
mod tests {
    use super::*;
    use rpi_tui::utils::visible_width;

    #[test]
    fn visual_preview_node_limits_wrapped_lines() {
        let theme = Arc::new(crate::core::themes::load_theme("dark", None).unwrap());
        let long = "x".repeat(1000);
        let tree = serde_json::json!({
            "type": "visualPreview",
            "props": {
                "text": long,
                "fg": "toolOutput",
                "maxVisualLines": 5,
                "keep": "start",
                "hint": "... ({hidden} more lines)",
            },
        });
        let lines = component_from_tree(&tree, &theme).render(50);
        assert_eq!(lines.len(), 6, "5 kept + hint");
        assert_eq!(visible_width(&lines[5]), "... (15 more lines)".len());
        assert!(
            lines[0].contains("xxx") && visible_width(&lines[0]) == 50,
            "kept visual line: {:?}",
            lines[0]
        );
    }

    #[test]
    fn visual_preview_node_keeps_end_lines_with_the_hint_first() {
        let theme = Arc::new(crate::core::themes::load_theme("dark", None).unwrap());
        let text = (1..=20)
            .map(|i| format!("line-{i:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        let tree = serde_json::json!({
            "type": "visualPreview",
            "props": {"text": text, "maxVisualLines": 2, "keep": "end"},
        });
        let lines = component_from_tree(&tree, &theme).render(40);
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("... (18 more lines)"));
        assert!(lines[1].contains("line-19"));
        assert!(lines[2].contains("line-20"));
    }

    #[test]
    fn truncate_prop_clips_instead_of_wrapping() {
        let theme = Arc::new(crate::core::themes::load_theme("dark", None).unwrap());
        let long = "a".repeat(120);
        let truncating = serde_json::json!({
            "type": "text",
            "props": {"text": long.clone(), "truncate": true},
        });
        let wrapping = serde_json::json!({
            "type": "text",
            "props": {"text": long},
        });
        let width = 40;
        let truncated = component_from_tree(&truncating, &theme).render(width);
        assert_eq!(
            truncated.len(),
            1,
            "truncate renders exactly one line at any text length"
        );
        assert!(
            truncated[0].contains("..."),
            "truncation is visible (ellipsis)"
        );
        let wrapped = component_from_tree(&wrapping, &theme).render(width);
        assert!(
            wrapped.len() > 1,
            "without the prop the default stays word-wrap"
        );
        assert_eq!(visible_width(&truncated[0]), width);
    }
}
