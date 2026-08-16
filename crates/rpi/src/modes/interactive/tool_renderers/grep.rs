//! grep tool renderer — port of the `renderCall`/`renderResult` hooks in
//! `packages/coding-agent/src/core/tools/grep.ts` (formatGrepCall :73-91,
//! formatGrepResult :93-126, hooks :375-384)
//! @ pi 0.82.1 (2efa728) (T17).
//!
//! Intentional differences:
//! - Upstream reuses a shared `Text` via `context.lastComponent`; here the
//!   component is rebuilt on every update — the rendered bytes are identical.
//! - The renderer is stateless: `formatGrepCall`/`formatGrepResult` only read
//!   their inputs, so no `RendererStateSlot` state is needed.

use rpi_tui::components::text::Text;
use rpi_tui::tui::Component;
use serde_json::Value;

use super::render_utils::{invalid_arg_text, js_value_text, shorten_path, str_value};
use crate::core::themes::Theme;
use crate::modes::interactive::components::keybinding_hints::key_hint;
use crate::modes::interactive::components::tool_execution::{
    get_text_output, RenderShell, ResultRenderOptions, ToolDefinition, ToolRenderContext,
    ToolResultState,
};
use crate::tools::truncate::{format_size, DEFAULT_MAX_BYTES};

/// `maxLines` for the collapsed result preview (grep.ts:106).
const GREP_PREVIEW_LINES: usize = 15;

/// `formatGrepCall` (grep.ts:73-91).
fn format_grep_call(args: &Value, theme: &Theme) -> String {
    let pattern = str_value(args.get("pattern"));
    // grep.ts:79: `rawPath !== null ? shortenPath(rawPath || ".") : null` —
    // a missing/empty path falls back to "."; a non-string path is invalid.
    let path = str_value(args.get("path"))
        .map(|raw_path| shorten_path(if raw_path.is_empty() { "." } else { &raw_path }));
    let glob = str_value(args.get("glob"));
    let invalid_arg = invalid_arg_text(theme);
    // grep.ts:84-87: `/pattern/` in accent; `in <path>` in toolOutput (the
    // invalid-arg text nests inside the toolOutput color, same bytes as
    // chalk's nested styles).
    let mut text = format!(
        "{}{}{}{}",
        theme.fg("toolTitle", &Theme::bold("grep")),
        " ",
        match pattern {
            None => invalid_arg.clone(),
            Some(pattern) => theme.fg("accent", &format!("/{pattern}/")),
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
    // grep.ts:88: `if (glob)` — JS truthiness: only non-empty strings render.
    if let Some(glob) = glob {
        if !glob.is_empty() {
            text.push_str(&theme.fg("toolOutput", &format!(" ({glob})")));
        }
    }
    // grep.ts:89: `if (limit !== undefined)` — any present value renders.
    if let Some(limit) = args.get("limit") {
        text.push_str(&theme.fg("toolOutput", &format!(" limit {}", js_value_text(limit))));
    }
    text
}

/// `formatGrepResult` (grep.ts:93-126).
fn format_grep_result(
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
        // grep.ts:106-108: head preview — all lines when expanded, else 15.
        let max_lines = if options.expanded {
            lines.len()
        } else {
            GREP_PREVIEW_LINES
        };
        let display_lines = &lines[..max_lines.min(lines.len())];
        let remaining = lines.len().saturating_sub(max_lines);
        // grep.ts:109: leading `\n`, then per-line `toolOutput` coloring.
        text.push_str(&format!(
            "\n{}",
            display_lines
                .iter()
                .map(|line| theme.fg("toolOutput", line))
                .collect::<Vec<_>>()
                .join("\n")
        ));
        if remaining > 0 {
            // grep.ts:111: `... (N more lines, <key> to expand)`.
            text.push_str(&format!(
                "{} {}{}",
                theme.fg("muted", &format!("\n... ({remaining} more lines,")),
                key_hint(theme, "app.tools.expand", "to expand"),
                theme.fg("muted", ")")
            ));
        }
    }

    // grep.ts:115-124: `[Truncated: …]` warning line for match-limit,
    // byte-truncation, and line-truncation notices (all JS-truthiness checks).
    let details = result.details.as_ref();
    let match_limit = details
        .and_then(|d| d.get("matchLimitReached"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let truncation = details.and_then(|d| d.get("truncation"));
    let truncation_truncated = truncation
        .and_then(|t| t.get("truncated"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let lines_truncated = details
        .and_then(|d| d.get("linesTruncated"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if match_limit > 0 || truncation_truncated || lines_truncated {
        let mut warnings: Vec<String> = Vec::new();
        if match_limit > 0 {
            warnings.push(format!("{match_limit} matches limit"));
        }
        if truncation_truncated {
            // `truncation.maxBytes ?? DEFAULT_MAX_BYTES` (grep.ts:121).
            let max_bytes = truncation
                .and_then(|t| t.get("maxBytes"))
                .and_then(Value::as_u64)
                .map(|v| v as usize);
            warnings.push(format!(
                "{} limit",
                format_size(max_bytes.unwrap_or(DEFAULT_MAX_BYTES))
            ));
        }
        if lines_truncated {
            warnings.push("some lines truncated".to_string());
        }
        text.push_str(&format!(
            "\n{}",
            theme.fg("warning", &format!("[Truncated: {}]", warnings.join(", ")))
        ));
    }
    text
}

/// The grep tool's render definition (grep.ts:375-384).
pub struct GrepToolRenderer;

impl ToolDefinition for GrepToolRenderer {
    fn render_call(
        &self,
        args: &Value,
        theme: &Theme,
        _context: &ToolRenderContext,
    ) -> Option<Box<dyn Component>> {
        Some(Box::new(Text::new(
            format_grep_call(args, theme),
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
        // grep.ts:380-384: always a `Text` — an empty result still returns a
        // component (rendering no lines); `None` would fall through to the
        // generic fallback, which prints the raw output.
        Some(Box::new(Text::new(
            format_grep_result(result, options, theme, context.show_images),
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
            terminal_width: 0,
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
    fn call_renders_pattern_path_glob_and_limit() {
        let theme = theme();
        let text = format_grep_call(
            &json!({"pattern": "fn main", "path": "src", "glob": "*.rs", "limit": 50}),
            &theme,
        );
        assert_eq!(strip_ansi(&text), "grep /fn main/ in src (*.rs) limit 50");
        // Missing path → "."; missing glob → no suffix; missing limit → none.
        let minimal = format_grep_call(&json!({"pattern": "foo"}), &theme);
        assert_eq!(strip_ansi(&minimal), "grep /foo/ in .");
        // Empty pattern renders as `//`; empty glob is falsy in JS.
        let empty_pattern = format_grep_call(&json!({"pattern": "", "glob": ""}), &theme);
        assert_eq!(strip_ansi(&empty_pattern), "grep // in .");
        // Limit 0 renders (`!== undefined`, unlike bash's truthy timeout).
        let zero_limit = format_grep_call(&json!({"pattern": "foo", "limit": 0}), &theme);
        assert!(strip_ansi(&zero_limit).ends_with(" limit 0"));
    }

    #[test]
    fn call_falls_back_on_invalid_args() {
        let theme = theme();
        // Non-string pattern/path/glob → invalid-arg text.
        let invalid = format_grep_call(&json!({"pattern": 42, "path": 42, "glob": 42}), &theme);
        let stripped = strip_ansi(&invalid);
        assert_eq!(stripped, "grep [invalid arg] in [invalid arg]");
        // A non-string glob never renders a suffix.
        assert!(!stripped.contains('('));
    }

    #[test]
    fn call_shortens_home_path() {
        let theme = theme();
        // Paths under `$HOME` render as `~/…` (render-utils.ts:10-17).
        let home = std::env::var("HOME").unwrap_or_default();
        if home.is_empty() {
            return;
        }
        let text = format_grep_call(
            &json!({"pattern": "foo", "path": format!("{home}/proj")}),
            &theme,
        );
        assert_eq!(strip_ansi(&text), "grep /foo/ in ~/proj");
    }

    #[test]
    fn result_preview_collapses_to_15_lines_with_hint() {
        let theme = theme();
        let state = RendererStateSlot::default();
        let renderer = GrepToolRenderer;
        let context = context(&state);
        let result = result_with(
            &(1..=20)
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
        // First 15 lines, then the verbatim expand hint (keybinding defaults:
        // app.tools.expand is ctrl+o).
        assert!(stripped.contains("line1"));
        assert!(stripped.contains("line15"));
        assert!(!stripped.contains("line16"));
        assert!(stripped.contains("\n... (5 more lines, ctrl+o to expand)"));
        assert_eq!(stripped.matches("line15").count(), 1);
    }

    #[test]
    fn result_expanded_shows_all_lines_without_hint() {
        let theme = theme();
        let state = RendererStateSlot::default();
        let renderer = GrepToolRenderer;
        let context = context(&state);
        let result = result_with(
            &(1..=20)
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
        assert!(stripped.contains("line20"));
        assert!(!stripped.contains("more lines"));
    }

    #[test]
    fn result_exactly_15_lines_has_no_hint() {
        let theme = theme();
        let state = RendererStateSlot::default();
        let renderer = GrepToolRenderer;
        let context = context(&state);
        let result = result_with(
            &(1..=15)
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
        assert!(stripped.contains("line15"));
        assert!(!stripped.contains("more lines"));
    }

    #[test]
    fn result_empty_output_still_returns_component() {
        let theme = theme();
        let state = RendererStateSlot::default();
        let renderer = GrepToolRenderer;
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
        let renderer = GrepToolRenderer;
        let context = context(&state);
        let result = result_with(
            "src/main.rs:1: fn main() {}",
            Some(json!({
                "matchLimitReached": 100,
                "linesTruncated": true,
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
        assert!(stripped
            .contains("\n[Truncated: 100 matches limit, 50.0KB limit, some lines truncated]"));
        // No details → no warning line.
        let plain = renderer
            .render_result(
                &result_with("src/main.rs:1: fn main() {}", None),
                ResultRenderOptions {
                    expanded: false,
                    is_partial: false,
                },
                &theme,
                &context,
            )
            .expect("result component");
        let stripped = strip_ansi(&plain.render(80).join("\n"));
        assert!(!stripped.contains("Truncated"));
        // Falsy details (0 / false / untruncated) → no warning line.
        let falsy = renderer
            .render_result(
                &result_with(
                    "src/main.rs:1: fn main() {}",
                    Some(json!({
                        "matchLimitReached": 0,
                        "linesTruncated": false,
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
        let renderer = GrepToolRenderer;
        let component = renderer
            .render_call(&json!({"pattern": "foo"}), &theme, &context(&state))
            .expect("call component");
        // Text pads rendered lines to the full width (text.rs).
        assert_eq!(
            strip_ansi(&component.render(80).join("\n")).trim_end(),
            "grep /foo/ in ."
        );
    }
}
