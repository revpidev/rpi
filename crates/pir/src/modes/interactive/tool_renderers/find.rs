//! find tool renderer — port of the `renderCall`/`renderResult` hooks in
//! `packages/coding-agent/src/core/tools/find.ts` (formatFindCall :73-88,
//! formatFindResult :90-121, hooks :365-374)
//! @ pi 0.82.1 (2efa728) (T17).
//!
//! Intentional differences:
//! - Upstream reuses a shared `Text` via `context.lastComponent`; here the
//!   component is rebuilt on every update — the rendered bytes are identical.
//! - The renderer is stateless: `formatFindCall`/`formatFindResult` only read
//!   their inputs, so no `RendererStateSlot` state is needed.

use pir_tui::components::text::Text;
use pir_tui::tui::Component;
use serde_json::Value;

use super::render_utils::{invalid_arg_text, js_value_text, shorten_path, str_value};
use crate::core::themes::Theme;
use crate::modes::interactive::components::keybinding_hints::key_hint;
use crate::modes::interactive::components::tool_execution::{
    get_text_output, RenderShell, ResultRenderOptions, ToolDefinition, ToolRenderContext,
    ToolResultState,
};
use crate::tools::truncate::{format_size, DEFAULT_MAX_BYTES};

/// `maxLines` for the collapsed result preview (find.ts:103).
const FIND_PREVIEW_LINES: usize = 20;

/// `formatFindCall` (find.ts:73-88).
fn format_find_call(args: &Value, theme: &Theme) -> String {
    let pattern = str_value(args.get("pattern"));
    // find.ts:76: `rawPath !== null ? shortenPath(rawPath || ".") : null` —
    // a missing/empty path falls back to "."; a non-string path is invalid.
    let path = str_value(args.get("path"))
        .map(|raw_path| shorten_path(if raw_path.is_empty() { "." } else { &raw_path }));
    let invalid_arg = invalid_arg_text(theme);
    // find.ts:79-83: the bare pattern in accent; `in <path>` in toolOutput
    // (the invalid-arg text nests inside the toolOutput color, same bytes as
    // chalk's nested styles).
    let mut text = format!(
        "{}{}{}{}",
        theme.fg("toolTitle", &Theme::bold("find")),
        " ",
        match pattern {
            None => invalid_arg.clone(),
            Some(pattern) => theme.fg("accent", &pattern),
        },
        theme.fg(
            "toolOutput",
            &format!(
                " in {}",
                match path {
                    None => invalid_arg,
                    Some(path) => path,
                }
            )
        )
    );
    // find.ts:84-86: `if (limit !== undefined)` — any present value renders.
    if let Some(limit) = args.get("limit") {
        text.push_str(&theme.fg("toolOutput", &format!(" (limit {})", js_value_text(limit))));
    }
    text
}

/// `formatFindResult` (find.ts:90-121).
fn format_find_result(
    result: &ToolResultState,
    options: ResultRenderOptions,
    theme: &Theme,
    show_images: bool,
) -> String {
    let output = get_text_output(Some(result), show_images)
        .trim()
        .to_string();
    let mut text = String::new();
    if !output.is_empty() {
        let lines: Vec<&str> = output.split('\n').collect();
        // find.ts:103-105: head preview — all lines when expanded, else 20.
        let max_lines = if options.expanded {
            lines.len()
        } else {
            FIND_PREVIEW_LINES
        };
        let display_lines = &lines[..max_lines.min(lines.len())];
        let remaining = lines.len().saturating_sub(max_lines);
        // find.ts:106: leading `\n`, then per-line `toolOutput` coloring.
        text.push_str(&format!(
            "\n{}",
            display_lines
                .iter()
                .map(|line| theme.fg("toolOutput", line))
                .collect::<Vec<_>>()
                .join("\n")
        ));
        if remaining > 0 {
            // find.ts:108: `... (N more lines, <key> to expand)`.
            text.push_str(&format!(
                "{} {}{}",
                theme.fg("muted", &format!("\n... ({remaining} more lines,")),
                key_hint(theme, "app.tools.expand", "to expand"),
                theme.fg("muted", ")")
            ));
        }
    }

    // find.ts:112-119: `[Truncated: …]` warning line for the result limit and
    // byte truncation (both JS-truthiness checks).
    let details = result.details.as_ref();
    let result_limit = details
        .and_then(|d| d.get("resultLimitReached"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let truncation = details.and_then(|d| d.get("truncation"));
    let truncation_truncated = truncation
        .and_then(|t| t.get("truncated"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if result_limit > 0 || truncation_truncated {
        let mut warnings: Vec<String> = Vec::new();
        if result_limit > 0 {
            warnings.push(format!("{result_limit} results limit"));
        }
        if truncation_truncated {
            // `truncation.maxBytes ?? DEFAULT_MAX_BYTES` (find.ts:117).
            let max_bytes = truncation
                .and_then(|t| t.get("maxBytes"))
                .and_then(Value::as_u64)
                .map(|v| v as usize);
            warnings.push(format!(
                "{} limit",
                format_size(max_bytes.unwrap_or(DEFAULT_MAX_BYTES))
            ));
        }
        text.push_str(&format!(
            "\n{}",
            theme.fg("warning", &format!("[Truncated: {}]", warnings.join(", ")))
        ));
    }
    text
}

/// The find tool's render definition (find.ts:365-374).
pub struct FindToolRenderer;

impl ToolDefinition for FindToolRenderer {
    fn render_call(
        &self,
        args: &Value,
        theme: &Theme,
        _context: &ToolRenderContext,
    ) -> Option<Box<dyn Component>> {
        Some(Box::new(Text::new(
            format_find_call(args, theme),
            0,
            0,
            None,
        )))
    }

    fn render_result(
        &self,
        result: &ToolResultState,
        options: ResultRenderOptions,
        theme: &Theme,
        context: &ToolRenderContext,
    ) -> Option<Box<dyn Component>> {
        // find.ts:370-374: always a `Text` — an empty result still returns a
        // component (rendering no lines); `None` would fall through to the
        // generic fallback, which prints the raw output.
        Some(Box::new(Text::new(
            format_find_result(result, options, theme, context.show_images),
            0,
            0,
            None,
        )))
    }

    fn render_shell(&self) -> Option<RenderShell> {
        // No `renderShell` in the upstream definition → `undefined`.
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::themes::load_theme;
    use crate::modes::interactive::components::tool_execution::{
        RendererStateSlot, ToolResultContentLoose,
    };
    use pir_tui::tui::RenderHandle;
    use serde_json::json;

    fn theme() -> Theme {
        load_theme("dark", None).expect("builtin dark theme")
    }

    fn context(state: &RendererStateSlot) -> ToolRenderContext {
        ToolRenderContext {
            args: json!({}),
            tool_call_id: "call_1".to_owned(),
            render_handle: RenderHandle::new(|| {}),
            state: state.clone(),
            cwd: "/cwd".to_owned(),
            execution_started: true,
            args_complete: true,
            is_partial: true,
            expanded: false,
            show_images: false,
            is_error: false,
        }
    }

    fn strip_ansi(input: &str) -> String {
        let mut out = String::with_capacity(input.len());
        let mut chars = input.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\u{1b}' if chars.peek() == Some(&'[') => {
                    chars.next();
                    for c in chars.by_ref() {
                        if c == 'm' {
                            break;
                        }
                    }
                }
                // OSC 8 hyperlink (`ESC]8;;..ESC\`): strip it too, so exact
                // assertions are independent of the process-global terminal
                // capability cache that other tests mutate.
                '\u{1b}' if chars.peek() == Some(&']') => {
                    chars.next();
                    for c in chars.by_ref() {
                        if c == '\u{1b}' {
                            chars.next(); // `\` of the `ESC\` terminator
                            break;
                        }
                    }
                }
                _ => out.push(c),
            }
        }
        out
    }

    fn result_with(text: &str, details: Option<Value>) -> ToolResultState {
        ToolResultState {
            content: vec![ToolResultContentLoose::text(text.to_string())],
            is_error: false,
            details,
        }
    }

    #[test]
    fn call_renders_pattern_path_and_limit() {
        let theme = theme();
        let text = format_find_call(
            &json!({"pattern": "*.ts", "path": "src", "limit": 50}),
            &theme,
        );
        assert_eq!(strip_ansi(&text), "find *.ts in src (limit 50)");
        // Missing path → "."; missing limit → no suffix.
        let minimal = format_find_call(&json!({"pattern": "*.json"}), &theme);
        assert_eq!(strip_ansi(&minimal), "find *.json in .");
        // Empty pattern renders as an empty accent segment.
        let empty_pattern = format_find_call(&json!({"pattern": ""}), &theme);
        assert_eq!(strip_ansi(&empty_pattern), "find  in .");
        // Limit 0 renders (`!== undefined`, unlike bash's truthy timeout).
        let zero_limit = format_find_call(&json!({"pattern": "a", "limit": 0}), &theme);
        assert!(strip_ansi(&zero_limit).ends_with(" (limit 0)"));
    }

    #[test]
    fn call_falls_back_on_invalid_args() {
        let theme = theme();
        // Non-string pattern/path → invalid-arg text.
        let invalid = format_find_call(&json!({"pattern": 42, "path": 42}), &theme);
        assert_eq!(strip_ansi(&invalid), "find [invalid arg] in [invalid arg]");
    }

    #[test]
    fn result_preview_collapses_to_20_lines_with_hint() {
        let theme = theme();
        let state = RendererStateSlot::default();
        let renderer = FindToolRenderer;
        let context = context(&state);
        let result = result_with(
            &(1..=25)
                .map(|i| format!("line{i}"))
                .collect::<Vec<_>>()
                .join("\n"),
            None,
        );
        let component = renderer
            .render_result(
                &result,
                ResultRenderOptions {
                    expanded: false,
                    is_partial: false,
                },
                &theme,
                &context,
            )
            .expect("result component");
        let stripped = strip_ansi(&component.render(80).join("\n"));
        // First 20 lines, then the verbatim expand hint (keybinding defaults:
        // app.tools.expand is ctrl+o).
        assert!(stripped.contains("line1"));
        assert!(stripped.contains("line20"));
        assert!(!stripped.contains("line21"));
        assert!(stripped.contains("\n... (5 more lines, ctrl+o to expand)"));
    }

    #[test]
    fn result_expanded_shows_all_lines_without_hint() {
        let theme = theme();
        let state = RendererStateSlot::default();
        let renderer = FindToolRenderer;
        let context = context(&state);
        let result = result_with(
            &(1..=25)
                .map(|i| format!("line{i}"))
                .collect::<Vec<_>>()
                .join("\n"),
            None,
        );
        let component = renderer
            .render_result(
                &result,
                ResultRenderOptions {
                    expanded: true,
                    is_partial: false,
                },
                &theme,
                &context,
            )
            .expect("result component");
        let stripped = strip_ansi(&component.render(80).join("\n"));
        assert!(stripped.contains("line25"));
        assert!(!stripped.contains("more lines"));
    }

    #[test]
    fn result_empty_output_still_returns_component() {
        let theme = theme();
        let state = RendererStateSlot::default();
        let renderer = FindToolRenderer;
        let context = context(&state);
        let result = result_with("", None);
        let component = renderer
            .render_result(
                &result,
                ResultRenderOptions {
                    expanded: false,
                    is_partial: false,
                },
                &theme,
                &context,
            )
            .expect("empty result still yields a component (never None)");
        assert!(component.render(80).is_empty());
    }

    #[test]
    fn result_warns_on_limits_verbatim() {
        let theme = theme();
        let state = RendererStateSlot::default();
        let renderer = FindToolRenderer;
        let context = context(&state);
        let result = result_with(
            "src/main.rs",
            Some(json!({
                "resultLimitReached": 1000,
                "truncation": {"truncated": true, "maxBytes": 51200}
            })),
        );
        let component = renderer
            .render_result(
                &result,
                ResultRenderOptions {
                    expanded: false,
                    is_partial: false,
                },
                &theme,
                &context,
            )
            .expect("result component");
        let stripped = strip_ansi(&component.render(80).join("\n"));
        assert!(stripped.contains("\n[Truncated: 1000 results limit, 50.0KB limit]"));
        // Falsy details (0 / untruncated) → no warning line.
        let falsy = renderer
            .render_result(
                &result_with(
                    "src/main.rs",
                    Some(json!({
                        "resultLimitReached": 0,
                        "truncation": {"truncated": false}
                    })),
                ),
                ResultRenderOptions {
                    expanded: false,
                    is_partial: false,
                },
                &theme,
                &context,
            )
            .expect("result component");
        assert!(!strip_ansi(&falsy.render(80).join("\n")).contains("Truncated"));
    }

    #[test]
    fn render_call_returns_component() {
        let theme = theme();
        let state = RendererStateSlot::default();
        let renderer = FindToolRenderer;
        let component = renderer
            .render_call(&json!({"pattern": "*.ts"}), &theme, &context(&state))
            .expect("call component");
        // Text pads rendered lines to the full width (text.rs).
        assert_eq!(
            strip_ansi(&component.render(80).join("\n")).trim_end(),
            "find *.ts in ."
        );
    }
}
