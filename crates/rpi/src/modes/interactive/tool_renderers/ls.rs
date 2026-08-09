//! ls tool renderer — port of the `renderCall`/`renderResult` hooks in
//! `packages/coding-agent/src/core/tools/ls.ts` (formatLsCall :57-65,
//! formatLsResult :67-98, hooks :215-224)
//! @ pi 0.82.1 (2efa728) (T17).
//!
//! Intentional differences:
//! - Upstream reuses a shared `Text` via `context.lastComponent`; here the
//!   component is rebuilt on every update — the rendered bytes are identical.
//! - The renderer is stateless: `formatLsCall`/`formatLsResult` only read
//!   their inputs, so no `RendererStateSlot` state is needed.

use rpi_tui::components::text::Text;
use rpi_tui::tui::Component;
use serde_json::Value;

use super::render_utils::{js_value_text, render_tool_path, str_value};
use crate::core::themes::Theme;
use crate::modes::interactive::components::keybinding_hints::key_hint;
use crate::modes::interactive::components::tool_execution::{
    get_text_output, RenderShell, ResultRenderOptions, ToolDefinition, ToolRenderContext,
    ToolResultState,
};
use crate::tools::truncate::{format_size, DEFAULT_MAX_BYTES};

/// `maxLines` for the collapsed result preview (ls.ts:80).
const LS_PREVIEW_LINES: usize = 20;

/// `formatLsCall` (ls.ts:57-65).
fn format_ls_call(args: &Value, theme: &Theme, cwd: &str) -> String {
    // ls.ts:59: `renderToolPath(str(args?.path), theme, cwd, { emptyFallback: "." })`.
    let path_display = render_tool_path(str_value(args.get("path")), theme, cwd, Some("."));
    let mut text = format!(
        "{} {}",
        theme.fg("toolTitle", &Theme::bold("ls")),
        path_display
    );
    // ls.ts:62-63: `if (limit !== undefined)` — any present value renders.
    if let Some(limit) = args.get("limit") {
        text.push_str(&theme.fg("toolOutput", &format!(" (limit {})", js_value_text(limit))));
    }
    text
}

/// `formatLsResult` (ls.ts:67-98).
fn format_ls_result(
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
        // ls.ts:80-82: head preview — all lines when expanded, else 20.
        let max_lines = if options.expanded {
            lines.len()
        } else {
            LS_PREVIEW_LINES
        };
        let display_lines = &lines[..max_lines.min(lines.len())];
        let remaining = lines.len().saturating_sub(max_lines);
        // ls.ts:83: leading `\n`, then per-line `toolOutput` coloring.
        text.push_str(&format!(
            "\n{}",
            display_lines
                .iter()
                .map(|line| theme.fg("toolOutput", line))
                .collect::<Vec<_>>()
                .join("\n")
        ));
        if remaining > 0 {
            // ls.ts:85: `... (N more lines, <key> to expand)`.
            text.push_str(&format!(
                "{} {}{}",
                theme.fg("muted", &format!("\n... ({remaining} more lines,")),
                key_hint(theme, "app.tools.expand", "to expand"),
                theme.fg("muted", ")")
            ));
        }
    }

    // ls.ts:89-96: `[Truncated: …]` warning line for the entry limit and
    // byte truncation (both JS-truthiness checks).
    let details = result.details.as_ref();
    let entry_limit = details
        .and_then(|d| d.get("entryLimitReached"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let truncation = details.and_then(|d| d.get("truncation"));
    let truncation_truncated = truncation
        .and_then(|t| t.get("truncated"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if entry_limit > 0 || truncation_truncated {
        let mut warnings: Vec<String> = Vec::new();
        if entry_limit > 0 {
            warnings.push(format!("{entry_limit} entries limit"));
        }
        if truncation_truncated {
            // `truncation.maxBytes ?? DEFAULT_MAX_BYTES` (ls.ts:94).
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

/// The ls tool's render definition (ls.ts:215-224).
pub struct LsToolRenderer;

impl ToolDefinition for LsToolRenderer {
    fn render_call(
        &self,
        args: &Value,
        theme: &Theme,
        context: &ToolRenderContext,
    ) -> Option<Box<dyn Component>> {
        Some(Box::new(Text::new(
            format_ls_call(args, theme, &context.cwd),
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
        // ls.ts:220-224: always a `Text` — an empty result still returns a
        // component (rendering no lines); `None` would fall through to the
        // generic fallback, which prints the raw output.
        Some(Box::new(Text::new(
            format_ls_result(result, options, theme, context.show_images),
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
    use rpi_tui::tui::RenderHandle;
    use serde_json::json;

    /// Serializes tests that mutate the process-global terminal capabilities
    /// (same pattern as tool_execution.rs).
    static CAPS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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

    /// Strip CSI SGR (`ESC[..m`) and OSC 8 hyperlink (`ESC]8;;..ESC\`)
    /// sequences so assertions are independent of terminal capabilities.
    fn strip_escape_sequences(input: &str) -> String {
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
    fn call_renders_path_and_limit() {
        let theme = theme();
        let text = format_ls_call(&json!({"path": "src", "limit": 30}), &theme, "/cwd");
        assert_eq!(strip_escape_sequences(&text), "ls src (limit 30)");
        // Missing path → empty-fallback "."; missing limit → no suffix.
        let minimal = format_ls_call(&json!({}), &theme, "/cwd");
        assert_eq!(strip_escape_sequences(&minimal), "ls .");
        // Limit 0 renders (`!== undefined`, unlike bash's truthy timeout).
        let zero_limit = format_ls_call(&json!({"limit": 0}), &theme, "/cwd");
        assert!(strip_escape_sequences(&zero_limit).ends_with(" (limit 0)"));
    }

    #[test]
    fn call_falls_back_on_invalid_args() {
        let theme = theme();
        // Non-string path → invalid-arg text.
        let invalid = format_ls_call(&json!({"path": 42}), &theme, "/cwd");
        assert_eq!(strip_escape_sequences(&invalid), "ls [invalid arg]");
    }

    #[test]
    fn call_links_path_when_hyperlinks_available() {
        // The terminal-image capability cache is a process global; serialize
        // with the other capability-mutating test.
        let _guard = CAPS_LOCK.lock().unwrap();
        rpi_tui::terminal_image::set_capabilities(rpi_tui::terminal_image::TerminalCapabilities {
            images: None,
            true_color: true,
            hyperlinks: true,
        });
        let theme = theme();
        let text = format_ls_call(&json!({"path": "."}), &theme, "/cwd");
        // OSC 8 wrap around the accent path (render-utils.rs `linkPath`).
        assert!(text.contains("\u{1b}]8;;file:///cwd\u{1b}\\"));
        assert_eq!(strip_escape_sequences(&text), "ls .");
    }

    #[test]
    fn result_preview_collapses_to_20_lines_with_hint() {
        let theme = theme();
        let state = RendererStateSlot::default();
        let renderer = LsToolRenderer;
        let context = context(&state);
        let result = result_with(
            &(1..=25)
                .map(|i| format!("entry{i}"))
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
        let stripped = strip_escape_sequences(&component.render(80).join("\n"));
        // First 20 lines, then the verbatim expand hint (keybinding defaults:
        // app.tools.expand is ctrl+o).
        assert!(stripped.contains("entry1"));
        assert!(stripped.contains("entry20"));
        assert!(!stripped.contains("entry21"));
        assert!(stripped.contains("\n... (5 more lines, ctrl+o to expand)"));
    }

    #[test]
    fn result_expanded_shows_all_lines_without_hint() {
        let theme = theme();
        let state = RendererStateSlot::default();
        let renderer = LsToolRenderer;
        let context = context(&state);
        let result = result_with(
            &(1..=25)
                .map(|i| format!("entry{i}"))
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
        let stripped = strip_escape_sequences(&component.render(80).join("\n"));
        assert!(stripped.contains("entry25"));
        assert!(!stripped.contains("more lines"));
    }

    #[test]
    fn result_empty_output_still_returns_component() {
        let theme = theme();
        let state = RendererStateSlot::default();
        let renderer = LsToolRenderer;
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
        let renderer = LsToolRenderer;
        let context = context(&state);
        let result = result_with(
            "src/",
            Some(json!({
                "entryLimitReached": 500,
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
        let stripped = strip_escape_sequences(&component.render(80).join("\n"));
        assert!(stripped.contains("\n[Truncated: 500 entries limit, 50.0KB limit]"));
        // Falsy details (0 / untruncated) → no warning line.
        let falsy = renderer
            .render_result(
                &result_with(
                    "src/",
                    Some(json!({
                        "entryLimitReached": 0,
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
        assert!(!strip_escape_sequences(&falsy.render(80).join("\n")).contains("Truncated"));
    }

    #[test]
    fn render_call_returns_component() {
        let theme = theme();
        let state = RendererStateSlot::default();
        let renderer = LsToolRenderer;
        let component = renderer
            .render_call(&json!({"path": "src"}), &theme, &context(&state))
            .expect("call component");
        // Text pads rendered lines to the full width (text.rs).
        assert_eq!(
            strip_escape_sequences(&component.render(80).join("\n")).trim_end(),
            "ls src"
        );
    }
}
