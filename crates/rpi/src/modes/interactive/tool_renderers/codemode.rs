//! codemode tool renderer — port of `renderCall`/`renderResult` in
//! `packages/coding-agent/src/extensions/codemode/renderer.ts` @ a13d35a74
//! (v1.0.0).
//!
//! The call shows the script folded to the first [`CODE_PREVIEW_LINES`]
//! visual lines; the result lists the nested tool calls (the last
//! [`CALL_PREVIEW_COUNT`] when collapsed, with the `models.*` cost total)
//! and the script output with the "Script completed/failed" header dropped,
//! folded to [`OUTPUT_PREVIEW_LINES`] visual lines.
//!
//! The extension registers no render hooks (`crates/rpi/src/extensions/
//! codemode/tool.rs`), so this built-in definition is what the tool
//! component resolves by name. Intentional differences:
//! - Upstream reuses the component kept in `context.lastComponent` and
//!   mutates it in place; the port rebuilds the [`Container`] on every
//!   update from the args/result — the rendered bytes are identical (the
//!   same rebuild-per-update model as the other T17 renderers).

use rpi_tui::components::spacer::Spacer;
use rpi_tui::components::text::Text;
use rpi_tui::tui::{Component, Container};
use serde_json::Value;

use super::render_utils::{invalid_arg_text, normalize_display_text, replace_tabs, str_value};
use crate::core::highlight::highlight_code;
use crate::core::themes::Theme;
use crate::modes::interactive::components::keybinding_hints::key_hint;
use crate::modes::interactive::components::tool_execution::{
    RenderShell, ResultRenderOptions, ToolDefinition, ToolRenderContext, ToolResultState,
    get_text_output,
};
use crate::modes::interactive::components::visual_truncate::{VisualKeep, VisualLinePreview};

/// `CODE_PREVIEW_LINES` (renderer.ts:15).
const CODE_PREVIEW_LINES: usize = 10;
/// `CALL_PREVIEW_COUNT` (renderer.ts:16).
const CALL_PREVIEW_COUNT: usize = 8;
/// `OUTPUT_PREVIEW_LINES` (renderer.ts:17).
const OUTPUT_PREVIEW_LINES: usize = 5;
/// `COLLAPSED_ARGS_CHARS` (renderer.ts:18).
const COLLAPSED_ARGS_CHARS: usize = 80;

/// `expandHint` (renderer.ts:21-23).
fn expand_hint(theme: &Theme, hidden: usize, noun: &str) -> String {
    format!(
        "{} {}{}",
        theme.fg("muted", &format!("... ({hidden} more {noun},")),
        key_hint(theme, "app.tools.expand", "to expand"),
        theme.fg("muted", ")"),
    )
}

/// `formatDuration` (renderer.ts:25-28): `Math.round` under a second,
/// `(ms / 1000).toFixed(1)` above.
fn format_duration(ms: Option<f64>) -> String {
    let Some(ms) = ms else {
        return String::new();
    };
    if ms < 1000.0 {
        // `Math.round(ms)` — `{}` prints `5` for `5.0`, like JS.
        return format!("{}ms", ms.round());
    }
    format!("{}s", format_js_fixed(ms / 1000.0, 1))
}

/// `Number.prototype.toFixed(decimals)`: fixed-point with the value rounded
/// half away from zero on the **exact** decimal expansion of the double
/// (JS picks the larger `n` on ties). Scaling in binary first would round
/// `1.15` up to `1.2` (`1.15 * 10` rounds to `11.5`), while JS `toFixed`
/// rounds the exact `1.149999...` down to `1.1`; formatting at 20
/// fractional digits first (wider than the ~1e-16 gap between a double and
/// a decimal tie) then rounding the decimal string avoids that.
fn format_js_fixed(value: f64, decimals: usize) -> String {
    if !value.is_finite() {
        return format!("{value}");
    }
    let sign = if value < 0.0 { "-" } else { "" };
    let text = format!("{:.20}", value.abs());
    let (integer, fraction) = text.split_once('.').unwrap_or((text.as_str(), ""));
    let integer_len = integer.len();
    let mut digits: Vec<u8> = integer
        .bytes()
        .chain(fraction.bytes().take(decimals.min(fraction.len())))
        .map(|digit| digit - b'0')
        .collect();
    // `decimals` never exceeds the 20 formatted digits (1 or 2), but the
    // arithmetic stays general.
    digits.resize(integer_len + decimals, 0);
    if fraction
        .as_bytes()
        .get(decimals)
        .is_some_and(|d| *d >= b'5')
    {
        let mut index = digits.len();
        loop {
            if index == 0 {
                digits.insert(0, 1);
                break;
            }
            index -= 1;
            if digits[index] == 9 {
                digits[index] = 0;
            } else {
                digits[index] += 1;
                break;
            }
        }
    }
    let split = digits.len() - decimals;
    let mut out = String::from(sign);
    out.extend(digits[..split].iter().map(|d| (d + b'0') as char));
    if decimals > 0 {
        out.push('.');
        out.extend(digits[split..].iter().map(|d| (d + b'0') as char));
    }
    out
}

/// `Number.prototype.toPrecision(precision)` for the sub-cent branch
/// (renderer.ts:31-33 `cost.toPrecision(2)`): `precision` significant
/// digits, fixed notation while the decimal exponent is in
/// `[-6, precision)`, exponent notation otherwise, rounding `n` in
/// `n / 10^(e - precision + 1)` away from zero like the spec.
fn format_js_to_precision(value: f64, precision: usize) -> String {
    if !value.is_finite() {
        return format!("{value}");
    }
    if value == 0.0 {
        return if precision > 1 {
            format!("0.{}", "0".repeat(precision - 1))
        } else {
            "0".to_owned()
        };
    }
    let sign = if value < 0.0 { "-" } else { "" };
    let value = value.abs();
    let mut exponent = value.log10().floor() as i32;
    let factor = 10f64.powi(precision as i32 - 1 - exponent);
    let mut digits = (value * factor).round() as u64;
    // A carry (`99` → `100`) is one more significant digit: drop the
    // trailing zero and bump the exponent.
    if digits >= 10u64.pow(precision as u32) {
        digits /= 10;
        exponent += 1;
    }
    if exponent < -6 || exponent >= precision as i32 {
        let digits = format!("{digits:0>width$}", width = precision);
        let (head, tail) = digits.split_at(1);
        format!(
            "{sign}{head}.{tail}e{}{}",
            if exponent < 0 { "-" } else { "+" },
            exponent.abs()
        )
    } else {
        let decimals = (precision as i32 - 1 - exponent) as usize;
        let scaled = digits as f64 * 10f64.powi(exponent - (precision as i32 - 1));
        format!("{sign}{scaled:.decimals$}")
    }
}

/// `formatCost` (renderer.ts:31-33).
fn format_cost(cost: f64) -> String {
    if cost >= 0.01 {
        format!("${}", format_js_fixed(cost, 2))
    } else {
        format!("${}", format_js_to_precision(cost, 2))
    }
}

/// JS truthiness of `call.cost` (`if (call.cost)`, renderer.ts:48, 87):
/// `undefined`/`null` (absent), `0` and `NaN` are falsy.
fn truthy_cost(cost: Option<f64>) -> Option<f64> {
    cost.filter(|cost| *cost != 0.0 && !cost.is_nan())
}

/// `statusIcon` (renderer.ts:35-46).
fn status_icon(status: &str, theme: &Theme) -> String {
    match status {
        "running" => theme.fg("warning", "…"),
        "ok" => theme.fg("success", "✓"),
        "error" => theme.fg("error", "✗"),
        "cancelled" => theme.fg("muted", "⊘"),
        _ => String::new(),
    }
}

/// One row of `CodemodeToolDetails.calls` (`CodemodeNestedCall`,
/// tool.ts:105-117). Deserialization is lenient: session replays can carry
/// fields dropped by later versions.
struct CallView {
    name: String,
    args: String,
    status: String,
    duration_ms: Option<f64>,
    error: Option<String>,
    cost: Option<f64>,
}

impl CallView {
    fn from_value(value: &Value) -> Option<Self> {
        let object = value.as_object()?;
        Some(Self {
            name: object.get("name")?.as_str()?.to_owned(),
            args: object
                .get("args")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            status: object
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            duration_ms: object.get("durationMs").and_then(Value::as_f64),
            error: object
                .get("error")
                .and_then(Value::as_str)
                .map(str::to_owned),
            cost: object.get("cost").and_then(Value::as_f64),
        })
    }
}

/// `formatCall` (renderer.ts:39-52).
fn format_call(call: &CallView, theme: &Theme, expanded: bool) -> String {
    let args = if !expanded && call.args.chars().count() > COLLAPSED_ARGS_CHARS {
        let truncated: String = call.args.chars().take(COLLAPSED_ARGS_CHARS - 3).collect();
        format!("{truncated}...")
    } else {
        call.args.clone()
    };
    let duration = format_duration(call.duration_ms);
    let mut line = format!(
        "{} {}",
        status_icon(&call.status, theme),
        theme.fg("toolTitle", &call.name)
    );
    if !args.is_empty() {
        line.push_str(&format!(" {}", theme.fg("muted", &args)));
    }
    if !duration.is_empty() {
        line.push_str(&format!(" {}", theme.fg("dim", &duration)));
    }
    if let Some(cost) = truthy_cost(call.cost) {
        line.push_str(&format!(" {}", theme.fg("dim", &format_cost(cost))));
    }
    if expanded && let Some(error) = &call.error {
        line.push_str(&format!(
            "\n    {}",
            theme.fg(
                "error",
                &error.split('\n').collect::<Vec<_>>().join("\n    ")
            )
        ));
    }
    line
}

/// `SCRIPT_HEADER` (renderer.ts:19): the script result's first text block,
/// dropped before the output is shown. Upstream anchors both ends
/// (`^Script (completed|failed)\nWall time [\d.]+ seconds\nOutput:\n$`), so
/// the whole block must match.
fn is_script_header(text: &str) -> bool {
    let Some(rest) = text
        .strip_prefix("Script completed\n")
        .or_else(|| text.strip_prefix("Script failed\n"))
    else {
        return false;
    };
    let Some((wall_time, output)) = rest.split_once(" seconds\nOutput:\n") else {
        return false;
    };
    let Some(wall_time) = wall_time.strip_prefix("Wall time ") else {
        return false;
    };
    !wall_time.is_empty()
        && wall_time.chars().all(|c| c.is_ascii_digit() || c == '.')
        && output.is_empty()
}

/// The codemode tool's render definition (renderer.ts:54-140).
pub struct CodemodeToolRenderer;

impl ToolDefinition for CodemodeToolRenderer {
    fn render_call(
        &self,
        args: &Value,
        theme: &Theme,
        context: &ToolRenderContext,
    ) -> Option<Box<dyn Component>> {
        // The code includes the `// @options:` line, so options show as
        // part of the script.
        let code = str_value(args.get("code"));
        let title = theme.fg("toolTitle", &Theme::bold("codemode"));
        let mut component = Container::new();
        let Some(code) = code else {
            component.add_child(Box::new(Text::new(
                format!("{title} {}", invalid_arg_text(theme)),
                0,
                0,
                None,
            )));
            return Some(Box::new(component));
        };
        component.add_child(Box::new(Text::new(title, 0, 0, None)));
        if !code.is_empty() {
            let code = replace_tabs(normalize_display_text(&code).trim_end());
            let highlighted = highlight_code(&code, Some("javascript"), theme).join("\n");
            if context.expanded {
                component.add_child(Box::new(Text::new(highlighted, 0, 0, None)));
            } else {
                let hint_theme = theme.clone();
                component.add_child(Box::new(
                    VisualLinePreview::new(highlighted, CODE_PREVIEW_LINES, VisualKeep::Start)
                        .with_hint(Box::new(move |hidden| {
                            expand_hint(&hint_theme, hidden, "lines")
                        })),
                ));
            }
        }
        Some(Box::new(component))
    }

    fn render_result(
        &self,
        result: &ToolResultState,
        options: ResultRenderOptions,
        theme: &Theme,
        context: &ToolRenderContext,
    ) -> Option<Box<dyn Component>> {
        let mut component = Container::new();

        let calls: Vec<CallView> = result
            .details
            .as_ref()
            .and_then(|details| details.get("calls"))
            .and_then(Value::as_array)
            .map(|calls| calls.iter().filter_map(CallView::from_value).collect())
            .unwrap_or_default();
        if !calls.is_empty() {
            // Collapsed rows hide earlier calls; the shown set is the tail.
            let shown: Vec<&CallView> = if options.expanded {
                calls.iter().collect()
            } else {
                calls[calls.len().saturating_sub(CALL_PREVIEW_COUNT)..]
                    .iter()
                    .collect()
            };
            let mut lines: Vec<String> = shown
                .iter()
                .map(|call| format_call(call, theme, options.expanded))
                .collect();
            if shown.len() < calls.len() {
                lines.insert(
                    0,
                    format!(
                        "{} {}{}",
                        theme.fg(
                            "muted",
                            &format!("... ({} earlier calls,", calls.len() - shown.len())
                        ),
                        key_hint(theme, "app.tools.expand", "to expand"),
                        theme.fg("muted", ")"),
                    ),
                );
            }
            // The total covers every call, including the collapsed rows.
            let priced: Vec<f64> = calls
                .iter()
                .filter_map(|call| truthy_cost(call.cost))
                .collect();
            if priced.len() > 1 {
                lines.push(theme.fg(
                    "muted",
                    &format!("Model calls: {}", format_cost(priced.iter().sum())),
                ));
            }
            component.add_child(Box::new(Spacer::new(1)));
            component.add_child(Box::new(Text::new(lines.join("\n"), 0, 0, None)));
        }

        // Drop the "Script completed\nWall time ...\nOutput:\n" header.
        // Rejected input (invalid options) has no header.
        let mut content_state = result.clone();
        let has_header = result.content.first().is_some_and(|block| {
            block.kind == "text" && block.text.as_deref().is_some_and(is_script_header)
        });
        if has_header {
            content_state.content.remove(0);
        }
        let output = if options.is_partial {
            String::new()
        } else {
            get_text_output(Some(&content_state), context.show_images)
                .trim()
                .to_string()
        };
        if !output.is_empty() {
            let color = if context.is_error {
                "error"
            } else {
                "toolOutput"
            };
            let styled = replace_tabs(&output)
                .split('\n')
                .map(|line| theme.fg(color, line))
                .collect::<Vec<_>>()
                .join("\n");
            component.add_child(Box::new(Spacer::new(1)));
            if options.expanded {
                component.add_child(Box::new(Text::new(styled, 0, 0, None)));
            } else {
                // Limit wrapped lines, not logical ones: script output is
                // often one long JSON line.
                let hint_theme = theme.clone();
                component.add_child(Box::new(
                    VisualLinePreview::new(styled, OUTPUT_PREVIEW_LINES, VisualKeep::Start)
                        .with_hint(Box::new(move |hidden| {
                            expand_hint(&hint_theme, hidden, "lines")
                        })),
                ));
                // The collapsed preview hides the truncation notice at the
                // end, so name the file here.
                if let Some(path) = result
                    .details
                    .as_ref()
                    .and_then(|details| details.get("fullOutputPath"))
                    .and_then(Value::as_str)
                {
                    component.add_child(Box::new(Text::new(
                        theme.fg("muted", &format!("Full output: {path}")),
                        0,
                        0,
                        None,
                    )));
                }
            }
        }
        Some(Box::new(component))
    }

    fn render_shell(&self) -> Option<RenderShell> {
        // No `renderShell` in the upstream renderer → `undefined`.
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
            if c == '\u{1b}' && chars.peek() == Some(&'[') {
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
    }

    /// `render` from `codemode-renderer.test.ts:10-37`: strip ANSI,
    /// `trimEnd` every line, then `trim` the block.
    fn render_result(
        result: ToolResultState,
        is_error: bool,
        expanded: bool,
        width: usize,
    ) -> String {
        let theme = theme();
        let state = RendererStateSlot::default();
        let mut context = context(&state);
        context.is_error = is_error;
        context.expanded = expanded;
        let component = CodemodeToolRenderer
            .render_result(
                &result,
                ResultRenderOptions {
                    expanded,
                    is_partial: false,
                },
                &theme,
                &context,
            )
            .expect("result component");
        strip_ansi(&component.render(width).join("\n"))
            .split('\n')
            .map(str::trim_end)
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_owned()
    }

    fn script_header() -> ToolResultContentLoose {
        ToolResultContentLoose::text("Script completed\nWall time 0.1 seconds\nOutput:\n")
    }

    /// `codemode` must resolve the built-in renderer (the extension itself
    /// registers no hooks); without this entry the tool falls back to
    /// `formatToolExecution`, which prints the whole script unfolded.
    #[test]
    fn registry_resolves_the_builtin_codemode_renderer() {
        assert!(
            crate::modes::interactive::tool_renderers::builtin_tool_definition("codemode")
                .is_some()
        );
    }

    #[test]
    fn hides_the_script_header_and_shows_the_output() {
        let text = render_result(
            ToolResultState {
                content: vec![script_header(), ToolResultContentLoose::text("hello")],
                is_error: false,
                details: Some(json!({
                    "calls": [
                        {"id": "call/1", "name": "read", "args": "{\"path\":\"a\"}", "status": "ok", "durationMs": 5}
                    ]
                })),
            },
            false,
            true,
            200,
        );
        assert_eq!(text, "✓ read {\"path\":\"a\"} 5ms\n\nhello");
    }

    #[test]
    fn shows_the_cost_of_model_calls_and_their_total() {
        let call = |id: &str, cost: Option<f64>| {
            let mut value = json!({
                "id": id, "name": "models.classify", "args": "scorer/judge",
                "status": "ok", "durationMs": 5
            });
            if let Some(cost) = cost {
                value["cost"] = json!(cost);
            }
            value
        };
        let text = render_result(
            ToolResultState {
                content: vec![script_header()],
                is_error: false,
                details: Some(json!({
                    "calls": [
                        call("call/models.classify/1", Some(0.000012936)),
                        call("call/models.classify/2", Some(0.02)),
                        call("call/models.classify/3", None),
                    ]
                })),
            },
            false,
            true,
            200,
        );
        assert_eq!(
            text,
            [
                "✓ models.classify scorer/judge 5ms $0.000013",
                "✓ models.classify scorer/judge 5ms $0.02",
                "✓ models.classify scorer/judge 5ms",
                "Model calls: $0.02",
            ]
            .join("\n")
        );
    }

    #[test]
    fn shows_results_without_a_header() {
        let text = render_result(
            ToolResultState {
                content: vec![ToolResultContentLoose::text(
                    "The @options line must be followed by JavaScript source",
                )],
                is_error: true,
                details: None,
            },
            true,
            true,
            200,
        );
        assert_eq!(
            text,
            "The @options line must be followed by JavaScript source"
        );
    }

    #[test]
    fn limits_collapsed_output_to_wrapped_lines_and_names_full_output() {
        let text = render_result(
            ToolResultState {
                content: vec![
                    script_header(),
                    ToolResultContentLoose::text("x".repeat(1000)),
                ],
                is_error: false,
                details: Some(json!({"calls": [], "fullOutputPath": "/tmp/out.txt"})),
            },
            false,
            false,
            50,
        );
        let lines: Vec<&str> = text.split('\n').collect();
        assert_eq!(lines.len(), 7);
        let expected_line = "x".repeat(50);
        for line in &lines[..5] {
            assert_eq!(*line, expected_line);
        }
        assert!(
            lines[5].starts_with("... (15 more lines,"),
            "{:?}",
            lines[5]
        );
        assert_eq!(lines[6], "Full output: /tmp/out.txt");
    }

    #[test]
    fn collapsed_result_shows_the_last_eight_calls_with_a_hint() {
        let calls: Vec<Value> = (1..=10)
            .map(|index| {
                json!({
                    "id": format!("call/{index}"), "name": format!("call-{index}"),
                    "args": "{}", "status": "ok", "durationMs": 1
                })
            })
            .collect();
        let text = render_result(
            ToolResultState {
                content: vec![script_header()],
                is_error: false,
                details: Some(json!({"calls": calls})),
            },
            false,
            false,
            200,
        );
        assert!(text.starts_with("... (2 earlier calls,"), "{text}");
        assert!(text.contains("call-3"), "{text}");
        assert!(!text.contains("call-2"), "{text}");
        assert!(text.contains("call-10"), "{text}");
    }

    #[test]
    fn expanded_result_shows_all_calls_and_the_error_body() {
        let text = render_result(
            ToolResultState {
                content: vec![script_header()],
                is_error: false,
                details: Some(json!({
                    "calls": [
                        {"id": "call/1", "name": "read", "args": "{}", "status": "error",
                         "durationMs": 5, "error": "line one\nline two"}
                    ]
                })),
            },
            false,
            true,
            200,
        );
        assert_eq!(text, "✗ read {} 5ms\n    line one\n    line two");
    }

    #[test]
    fn collapsed_calls_fold_long_argument_previews() {
        let long_args = format!("{{\"path\":\"{}\"}}", "x".repeat(200));
        let text = render_result(
            ToolResultState {
                content: vec![script_header()],
                is_error: false,
                details: Some(json!({
                    "calls": [
                        {"id": "call/1", "name": "read", "args": long_args,
                         "status": "ok", "durationMs": 1}
                    ]
                })),
            },
            false,
            false,
            200,
        );
        // `formatCall` cuts at 80 chars while collapsed.
        let args_line = text.lines().next().expect("call line");
        let args = args_line
            .strip_prefix("✓ read ")
            .and_then(|rest| rest.strip_suffix(" 1ms"))
            .expect("call line shape");
        assert!(args.ends_with("..."), "{args}");
        assert_eq!(args.chars().count(), 80);
    }

    fn render_call(code: Option<Value>, expanded: bool) -> String {
        let theme = theme();
        let state = RendererStateSlot::default();
        let mut context = context(&state);
        context.expanded = expanded;
        let args = match code {
            Some(code) => json!({"code": code}),
            None => json!({}),
        };
        let component = CodemodeToolRenderer
            .render_call(&args, &theme, &context)
            .expect("call component");
        strip_ansi(&component.render(80).join("\n"))
            .split('\n')
            .map(str::trim_end)
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_owned()
    }

    #[test]
    fn call_collapses_the_script_to_ten_visual_lines() {
        let code: String = (1..=40)
            .map(|index| format!("const v{index} = {index};\n"))
            .collect();
        let text = render_call(Some(json!(code)), false);
        let lines: Vec<&str> = text.split('\n').collect();
        assert_eq!(lines[0], "codemode");
        assert_eq!(lines.len(), 12, "title + 10 kept lines + hint: {text}");
        assert_eq!(lines[1], "const v1 = 1;");
        assert_eq!(lines[10], "const v10 = 10;");
        assert!(
            lines[11].starts_with("... (30 more lines,"),
            "{}",
            lines[11]
        );
        assert!(!text.contains("const v11"), "{text}");
    }

    #[test]
    fn expanded_call_shows_the_whole_script() {
        let code: String = (1..=40)
            .map(|index| format!("const v{index} = {index};\n"))
            .collect();
        let text = render_call(Some(json!(code)), true);
        assert_eq!(text.lines().count(), 41, "title + 40 script lines");
        assert!(text.contains("const v40 = 40;"), "{text}");
        assert!(!text.contains("more lines"), "{text}");
    }

    #[test]
    fn call_handles_invalid_empty_and_missing_code() {
        assert_eq!(
            render_call(Some(json!(42)), false),
            "codemode [invalid arg]"
        );
        assert_eq!(render_call(Some(json!("")), false), "codemode");
        assert_eq!(render_call(None, false), "codemode");
    }

    #[test]
    fn format_cost_matches_js_to_fixed_and_to_precision() {
        assert_eq!(format_cost(0.02), "$0.02");
        assert_eq!(format_cost(0.000012936), "$0.000013");
        // Two significant digits exactly at the exponent boundary stay fixed.
        assert_eq!(format_cost(0.000001), "$0.0000010");
        // Below 1e-6 JS switches to exponent notation.
        assert_eq!(format_cost(0.0000005), "$5.0e-7");
        assert_eq!(format_cost(0.00999), "$0.010");
        // `toFixed(2)` on the exact decimal expansion, not a binary scale:
        // 0.125 is an exact tie (rounds up), 0.145 is below the tie.
        assert_eq!(format_cost(0.125), "$0.13");
        assert_eq!(format_cost(0.145), "$0.14");
        assert_eq!(format_cost(0.0145), "$0.01");
        assert_eq!(format_cost(0.0), "$0.0");
    }

    #[test]
    fn format_duration_matches_js_round_and_to_fixed() {
        assert_eq!(format_duration(None), "");
        assert_eq!(format_duration(Some(5.4)), "5ms");
        assert_eq!(format_duration(Some(999.0)), "999ms");
        assert_eq!(format_duration(Some(1000.0)), "1.0s");
        assert_eq!(format_duration(Some(1001.0)), "1.0s");
        assert_eq!(format_duration(Some(1234.0)), "1.2s");
        // `(ms / 1000).toFixed(1)`: exact binary values round half away from
        // zero (1.25 → 1.3) while values just below the tie stay down
        // (the double 1.15 is 1.14999... → 1.1).
        assert_eq!(format_duration(Some(1150.0)), "1.1s");
        assert_eq!(format_duration(Some(1250.0)), "1.3s");
        assert_eq!(format_duration(Some(1750.0)), "1.8s");
        assert_eq!(format_duration(Some(2250.0)), "2.3s");
        assert_eq!(format_duration(Some(59_999.0)), "60.0s");
    }

    #[test]
    fn script_header_matches_the_upstream_regex() {
        assert!(is_script_header(
            "Script completed\nWall time 0.1 seconds\nOutput:\n"
        ));
        assert!(is_script_header(
            "Script failed\nWall time 12.34 seconds\nOutput:\n"
        ));
        // The whole block must match.
        assert!(!is_script_header(
            "Script completed\nWall time 0.1 seconds\nOutput:\nextra"
        ));
        assert!(!is_script_header(
            "Script completed\nWall time 0.1 seconds\n"
        ));
        assert!(!is_script_header(
            "The @options line must be followed by JavaScript source"
        ));
        assert!(!is_script_header(
            "Script completed\nWall time x seconds\nOutput:\n"
        ));
    }
}
