//! Tool-call and tool-result renderers (renderCall + renderResult): the
//! call lines summarize the invocation (`mcp call …`), the result tree lets
//! the host collapse long output instead of the generic fallback full
//! expansion.
//!
//! Port of `tool-result-renderer.ts` @ 3d953f90: `renderMcpToolResult`
//! (:269-297) + `collectCollapsedResultLines` (:188-238) +
//! `formatMcpToolResultIdentity` (:240-252) (result face), and
//! `formatMcpProxyToolCallLines` (:124-152) / `formatMcpDirectToolCallLines`
//! (:154-161) / `formatJsonish` (:104-118) / `renderToolCallLines`
//! (:163-169) / `renderMcpProxyToolCall` + `createMcpDirectToolCallRenderer`
//! (:171-179) (call face, TE09 FR-E).
//!
//! Upstream returns rpi-tui components (Text/CollapsibleText with theme
//! colors); the native ABI renders declaratively, so this module returns a
//! static ComponentTree text node (`rpi.component-tree.v1`). For the result
//! face the theme colors and the width-dependent CollapsibleText
//! re-clamping are dropped: the static text carries the final
//! line/char-clamped output, with the "(Ctrl+O to expand)" hint folded into
//! the text when truncated. The call face keeps the toolTitle/muted split
//! via a `column` node (ComponentTree v1 has no inline spans, so the title
//! line and the muted remainder become two styled text children).

use serde_json::{json, Value};

/// `DEFAULT_MAX_COLLAPSED_LINES` (tool-result-renderer.ts:36).
pub const DEFAULT_MAX_COLLAPSED_LINES: usize = 3;
/// `DEFAULT_MAX_COLLAPSED_CHARS` (tool-result-renderer.ts:37).
pub const DEFAULT_MAX_COLLAPSED_CHARS: usize = 8000;
/// `DEFAULT_MAX_CALL_INPUT_CHARS` (tool-result-renderer.ts:35) — the call
/// args summary budget for both call-line formatters.
pub const DEFAULT_MAX_CALL_INPUT_CHARS: usize = 1500;

/// `McpToolResultDisplay` (tool-result-renderer.ts:30-33).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct McpToolResultDisplay {
    pub lines: Vec<String>,
    pub truncated: bool,
}

/// `blockToLines` (tool-result-renderer.ts:181-186): text blocks split on
/// newlines; any non-text block is the `[image: ...]` placeholder.
fn block_to_lines(block: &Value) -> Vec<String> {
    if block.get("type").and_then(Value::as_str) == Some("text") {
        let text = block.get("text").and_then(Value::as_str).unwrap_or("");
        return text.split('\n').map(str::to_string).collect();
    }
    vec![format!(
        "[image: {}]",
        block.get("mimeType").and_then(Value::as_str).unwrap_or("")
    )]
}

/// `collectCollapsedResultLines` (tool-result-renderer.ts:188-238).
pub fn collect_collapsed_result_lines(
    content: &[Value],
    max_lines: usize,
    max_chars: usize,
) -> McpToolResultDisplay {
    if content.is_empty() {
        return McpToolResultDisplay {
            lines: vec!["(empty result)".to_string()],
            truncated: false,
        };
    }

    let mut lines: Vec<String> = Vec::new();
    let mut remaining_chars = max_chars as i64;
    let mut truncated = false;

    #[derive(Clone, Copy, PartialEq)]
    enum AppendOutcome {
        Appended,
        Truncated,
    }

    let append_line =
        |lines: &mut Vec<String>, remaining_chars: &mut i64, line: &str| -> AppendOutcome {
            if lines.len() >= max_lines || *remaining_chars <= 0 {
                return AppendOutcome::Truncated;
            }
            if line.len() as i64 > *remaining_chars {
                // Upstream slices by JS UTF-16 code units; the Rust port slices
                // on the char boundary at or before the budget.
                let mut end = (*remaining_chars).max(0) as usize;
                while end > 0 && !line.is_char_boundary(end) {
                    end -= 1;
                }
                lines.push(line[..end].to_string());
                *remaining_chars = 0;
                return AppendOutcome::Truncated;
            }
            lines.push(line.to_string());
            *remaining_chars -= line.len() as i64 + 1;
            AppendOutcome::Appended
        };

    for block in content {
        if block.get("type").and_then(Value::as_str) != Some("text") {
            let placeholder = format!(
                "[image: {}]",
                block.get("mimeType").and_then(Value::as_str).unwrap_or("")
            );
            if append_line(&mut lines, &mut remaining_chars, &placeholder)
                == AppendOutcome::Truncated
            {
                truncated = true;
                break;
            }
            continue;
        }
        let text = block.get("text").and_then(Value::as_str).unwrap_or("");
        let mut start = 0;
        loop {
            if start > text.len() {
                break;
            }
            let line_end = text[start..]
                .find('\n')
                .map(|pos| start + pos)
                .unwrap_or(text.len());
            if append_line(&mut lines, &mut remaining_chars, &text[start..line_end])
                == AppendOutcome::Truncated
            {
                truncated = true;
                break;
            }
            if line_end == text.len() {
                break;
            }
            start = line_end + 1;
        }
        if truncated {
            break;
        }
    }

    if lines.is_empty() {
        lines.push(String::new());
    }
    if truncated && lines.len() >= max_lines {
        lines.push("…".to_string());
    }
    McpToolResultDisplay { lines, truncated }
}

/// `formatMcpToolResultIdentity` (tool-result-renderer.ts:240-252).
pub fn format_mcp_tool_result_identity(details: Option<&Value>) -> Option<String> {
    let details = details?;
    if details.get("mode").and_then(Value::as_str) != Some("call") {
        return None;
    }
    let server = details
        .get("server")
        .and_then(Value::as_str)
        .or_else(|| details.get("hintServer").and_then(Value::as_str))?;
    if let Some(tool) = details.get("tool").and_then(Value::as_str) {
        return Some(format!("MCP {server}/{tool}"));
    }
    if let Some(uri) = details.get("resourceUri").and_then(Value::as_str) {
        return Some(format!("MCP {server} resource {uri}"));
    }
    if let Some(tool) = details.get("requestedTool").and_then(Value::as_str) {
        return Some(format!("MCP {server}/{tool}"));
    }
    None
}

/// `formatMcpToolResultLines` (tool-result-renderer.ts:254-267).
pub fn format_mcp_tool_result_lines(
    result: &Value,
    expanded: bool,
    max_collapsed_lines: usize,
    max_collapsed_chars: usize,
) -> McpToolResultDisplay {
    let content = result
        .get("content")
        .and_then(Value::as_array)
        .map(|a| a.as_slice())
        .unwrap_or(&[]);
    if expanded {
        let all_lines: Vec<String> = content.iter().flat_map(block_to_lines).collect();
        let lines = if all_lines.is_empty() {
            vec!["(empty result)".to_string()]
        } else {
            all_lines
        };
        return McpToolResultDisplay {
            lines,
            truncated: false,
        };
    }
    collect_collapsed_result_lines(content, max_collapsed_lines, max_collapsed_chars)
}

/// `CompactMcpToolResult` (tool-result-renderer.ts:70-112 @ 10a45367)
/// mapped onto a static ComponentTree: first body line carries the
/// `title preview → ` prefix; a hidden remainder (truncation marker or
/// overflow) appends the muted ` … (Ctrl+O to expand)` hint. Width-derived
/// clipping stays the TUI's job — the static text keeps the full lines and
/// the marker.
fn compact_row_text(title: &str, input_preview: &str, display: &McpToolResultDisplay) -> String {
    let mut lines: Vec<String> = display
        .lines
        .iter()
        .filter(|line| {
            // The line-cap marker is folded into the hidden hint instead.
            !(display.truncated && line.as_str() == "…")
        })
        .cloned()
        .collect();
    if lines.is_empty() {
        lines.push(String::new());
    }
    let mut prefix = String::new();
    if !title.is_empty() {
        prefix = format!("{title} ");
        if !input_preview.is_empty() {
            prefix = format!("{prefix}{input_preview} ");
        }
        prefix = format!("{prefix}→ ");
    }
    let mut rendered = lines
        .iter()
        .enumerate()
        .map(|(index, line)| {
            if index == 0 {
                format!("{prefix}{line}")
            } else {
                line.clone()
            }
        })
        .collect::<Vec<_>>();
    if display.truncated {
        rendered.push(" … (Ctrl+O to expand)".to_string());
    }
    rendered.join("\n")
}

/// `renderMcpToolResult` (tool-result-renderer.ts:435-460 @ 10a45367),
/// mapped onto a ComponentTree text node. Pure and synchronous: only the
/// result/options/context JSON plus the caller-resolved render options and
/// the per-call compact state (seeded by `renderToolCall`) are consulted.
pub fn render_mcp_tool_result(
    result: &Value,
    options: &Value,
    context: &Value,
    render_options: &McpToolRenderOptions,
    compact_state: Option<&McpToolRenderState>,
) -> Value {
    let is_partial = options.get("isPartial").and_then(Value::as_bool) == Some(true);
    if is_partial {
        return json!({ "type": "text", "props": { "text": "Running MCP tool..." } });
    }

    let details = result.get("details");
    let has_error_details = details
        .and_then(|d| d.get("error"))
        .is_some_and(|e| !e.is_null());
    let expanded = options.get("expanded").and_then(Value::as_bool) == Some(true)
        || context.get("isError").and_then(Value::as_bool) == Some(true)
        || has_error_details;

    // #349: the compact default renders a single self-contained row built
    // from the call-time state (title + bounded input preview) and the
    // collapsed result lines (1-3).
    if !expanded && render_options.result_rendering == McpToolResultRendering::Compact {
        let display = format_mcp_tool_result_lines(
            result,
            false,
            render_options.collapsed_result_lines,
            DEFAULT_MAX_COLLAPSED_CHARS,
        );
        let details_identity = format_mcp_tool_result_identity(details);
        let title = compact_state
            .and_then(|state| state.compact_title.clone())
            .filter(|title| !title.is_empty())
            .or(details_identity)
            .unwrap_or_default();
        let input_preview = compact_state
            .and_then(|state| state.compact_input_preview.clone())
            .unwrap_or_default();
        let text = compact_row_text(&title, &input_preview, &display);
        return json!({
            "type": "column",
            "props": {},
            "children": [json!({
                "type": "text",
                "props": { "text": text, "fg": "toolOutput" },
            })],
        });
    }

    let identity = format_mcp_tool_result_identity(details);
    let display = format_mcp_tool_result_lines(
        result,
        expanded,
        render_options.collapsed_result_lines,
        DEFAULT_MAX_COLLAPSED_CHARS,
    );

    let mut output_lines: Vec<String> = Vec::new();
    if let Some(identity) = &identity {
        output_lines.push(identity.clone());
    }
    output_lines.extend(display.lines.iter().cloned());
    // Upstream wraps in CollapsibleText: the collapsed form appends the
    // "…\n(Ctrl+O to expand)" footer below the clamped text (tool-result-
    // renderer.ts:83-89). When the line cap already emitted the "…" marker
    // (collectCollapsedResultLines :236) that marker stands in for the
    // footer's ellipsis; otherwise the char-budget truncation still needs
    // one.
    if !expanded && display.truncated {
        if output_lines.last().map(String::as_str) != Some("…") {
            output_lines.push("…".to_string());
        }
        output_lines.push("(Ctrl+O to expand)".to_string());
    }
    json!({ "type": "text", "props": { "text": output_lines.join("\n") } })
}

// ===== Tool-call renderer (renderCall, TE09 FR-E) =====

/// `McpToolResultRendering` (tool-result-renderer.ts:38 @ 10a45367).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum McpToolResultRendering {
    #[default]
    Compact,
    Boxed,
}

/// `McpToolRenderOptions` (tool-result-renderer.ts:40-43 @ 10a45367):
/// settings-resolved rendering knobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct McpToolRenderOptions {
    pub result_rendering: McpToolResultRendering,
    pub collapsed_result_lines: usize,
}

/// `DEFAULT_MAX_COMPACT_INPUT_CHARS` (tool-result-renderer.ts:34 @
/// 10a45367).
pub const DEFAULT_MAX_COMPACT_INPUT_CHARS: usize = 240;
/// `DEFAULT_BOXED_COLLAPSED_LINES` (:35) / `DEFAULT_COMPACT_COLLAPSED_LINES`
/// (:36): the boxed row keeps the historical 3-line default; the compact
/// row defaults to a single line (#349 — the only user-visible default
/// change of the rebase, `changes/` documents the `toolResultRendering:
/// "boxed"` escape hatch).
pub const DEFAULT_BOXED_COLLAPSED_LINES: usize = 3;
pub const DEFAULT_COMPACT_COLLAPSED_LINES: usize = 1;

/// `McpToolRenderState` (tool-result-renderer.ts:26-29 @ 10a45367): the
/// per-tool-call render state the call render seeds and the result row
/// consumes (compact title / bounded input preview). The upstream state
/// rides the TUI render context; the plugin ABI context carries
/// `toolCallId`, so the plugin keeps the map keyed by it (see
/// `lib.rs`'s render dispatch).
#[derive(Debug, Clone, Default)]
pub struct McpToolRenderState {
    pub compact_title: Option<String>,
    pub compact_input_preview: Option<String>,
}

/// `resolveMcpToolRenderOptions` (tool-result-renderer.ts:270-279 @
/// 10a45367): `toolResultRendering === "boxed"` selects the legacy row
/// (anything else — including unset — is the compact default);
/// `collapsedResultLines` accepts exactly 1|2|3.
pub fn resolve_mcp_tool_render_options(
    settings: Option<&serde_json::Map<String, Value>>,
) -> McpToolRenderOptions {
    let result_rendering = match settings {
        Some(settings)
            if settings.get("toolResultRendering").and_then(Value::as_str) == Some("boxed") =>
        {
            McpToolResultRendering::Boxed
        }
        _ => McpToolResultRendering::Compact,
    };
    // JS `collapsedLines === 1 || === 2 || === 3` accepts the float form
    // (`2.0 === 2`); serde_json keeps `2.0` as f64, so match on the float
    // and require an integral value in range.
    let collapsed = settings
        .and_then(|s| s.get("collapsedResultLines"))
        .and_then(Value::as_f64);
    let default_lines = match result_rendering {
        McpToolResultRendering::Boxed => DEFAULT_BOXED_COLLAPSED_LINES,
        McpToolResultRendering::Compact => DEFAULT_COMPACT_COLLAPSED_LINES,
    };
    McpToolRenderOptions {
        result_rendering,
        collapsed_result_lines: match collapsed {
            Some(lines) if lines.fract() == 0.0 && (1.0..=3.0).contains(&lines) => lines as usize,
            _ => default_lines,
        },
    }
}

/// `formatCompactInputPreview` (tool-result-renderer.ts:268-270 @
/// 10a45367): the call lines minus the title, space-joined, whitespace
/// collapsed, bounded to 240 UTF-16 units.
pub fn format_compact_input_preview(lines: &[String], max_chars: usize) -> String {
    let joined = lines
        .iter()
        .skip(1)
        .cloned()
        .collect::<Vec<_>>()
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    truncate_text(&joined, max_chars)
}

/// `truncateText` (tool-result-renderer.ts:99-102): keep the first
/// `max_chars - 1` UTF-16 code units and append `…`. BOTH the budget check
/// and the cut are UTF-16 (`value.length` / `slice` upstream) — a
/// `chars().count()` check would under-count astral input (emoji) and skip
/// truncation entirely. The cut lands on a whole char at or before the
/// budget (a JS cut mid-surrogate-pair only yields a replacement char;
/// same precedent as subagents display.rs `truncate_display_text`).
fn truncate_text(value: &str, max_chars: usize) -> String {
    if value.encode_utf16().count() <= max_chars {
        return value.to_string();
    }
    let mut units = 0usize;
    let mut out = String::new();
    for ch in value.chars() {
        let width = ch.len_utf16();
        if units + width > max_chars.saturating_sub(1) {
            break;
        }
        units += width;
        out.push(ch);
    }
    out + "\u{2026}"
}

/// `formatJsonish` (tool-result-renderer.ts:104-118): a string input is
/// parsed as JSON first (falling back to the raw string), then pretty
/// printed with 2-space indent and truncated.
pub fn format_jsonish(value: &Value, max_chars: usize) -> String {
    let rendered = match value {
        Value::String(raw) => match serde_json::from_str::<Value>(raw) {
            Ok(parsed) => serde_json::to_string_pretty(&parsed),
            Err(_) => Ok(raw.clone()),
        },
        other => serde_json::to_string_pretty(other),
    };
    // serde_json cannot fail on an already-valid Value; the Err arm mirrors
    // upstream's `String(value)` fallback for symmetry.
    match rendered {
        Ok(text) => truncate_text(&text, max_chars),
        Err(_) => truncate_text(&value.to_string(), max_chars),
    }
}

/// `hasUsefulObjectContent` (tool-result-renderer.ts:120-122).
fn has_useful_object_content(value: &Value) -> bool {
    value.as_object().is_some_and(|map| !map.is_empty())
}

/// `formatMcpProxyToolCallLines` (tool-result-renderer.ts:124-152):
/// branch priority `action === "ui-messages"` → `tool` → `connect` →
/// `describe` → `search` → `server` → any `action` → `mcp status`. Every
/// branch key is tested with JS truthiness — a present-but-EMPTY string
/// (`if (args.tool)` etc.) is falsy and falls through to the next branch,
/// ending at `mcp status`; the same applies to the `server` annotations and
/// to the nested `args` summary line (empty string/0/false/null skip it);
/// `regex` must be exactly true and `includeSchemas` exactly false to
/// annotate.
pub fn format_mcp_proxy_tool_call_lines(args: &Value, max_input_chars: usize) -> Vec<String> {
    let arg_str = |key: &str| {
        args.get(key)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
    };
    let is_true = |key: &str| args.get(key) == Some(&Value::Bool(true));
    let is_false = |key: &str| args.get(key) == Some(&Value::Bool(false));

    if arg_str("action") == Some("ui-messages") {
        return vec![format!("mcp {}", arg_str("action").unwrap_or_default())];
    }

    if let Some(tool) = arg_str("tool") {
        let target = match arg_str("server") {
            Some(server) => format!("{tool} @ {server}"),
            None => tool.to_string(),
        };
        let mut lines = vec![format!("mcp call {target}")];
        // Truthy args (JS truthiness): `""`/`0`/`false`/null/absent skip
        // the summary line; any object (even `{}`) or array is truthy and
        // formats through `formatJsonish` (`"{}"` for the empty object).
        let args_present = match args.get("args") {
            None | Some(Value::Null) => false,
            Some(Value::String(raw)) => !raw.is_empty(),
            Some(Value::Number(number)) => number.as_f64() != Some(0.0),
            Some(Value::Bool(flag)) => *flag,
            Some(Value::Object(_)) | Some(Value::Array(_)) => true,
        };
        if args_present {
            lines.push(format_jsonish(&args["args"], max_input_chars));
        }
        return lines;
    }

    if let Some(connect) = arg_str("connect") {
        return vec![format!("mcp connect {connect}")];
    }
    if let Some(describe) = arg_str("describe") {
        return vec![format!("mcp describe {describe}")];
    }

    if let Some(search) = arg_str("search") {
        let mut line = format!("mcp search {search}");
        if let Some(server) = arg_str("server") {
            line.push_str(&format!(" @ {server}"));
        }
        if is_true("regex") {
            line.push_str(" (regex)");
        }
        if is_false("includeSchemas") {
            line.push_str(" (schemas hidden)");
        }
        return vec![line];
    }

    if let Some(server) = arg_str("server") {
        return vec![format!("mcp list {server}")];
    }
    if let Some(action) = arg_str("action") {
        return vec![format!("mcp {action}")];
    }

    vec!["mcp status".to_string()]
}

/// `formatMcpDirectToolCallLines` (tool-result-renderer.ts:154-161): the
/// displayName alone when the args carry no useful object content.
pub fn format_mcp_direct_tool_call_lines(
    display_name: &str,
    args: &Value,
    max_input_chars: usize,
) -> Vec<String> {
    if !has_useful_object_content(args) {
        return vec![display_name.to_string()];
    }
    vec![
        display_name.to_string(),
        format_jsonish(args, max_input_chars),
    ]
}

/// `renderToolCallLines` (tool-result-renderer.ts:163-169): first line in
/// the `toolTitle` color (bold when the theme bolds titles — upstream wraps
/// `theme.bold` when present, rpi themes always do), remainder muted.
/// Upstream joins everything into one `Text`; ComponentTree v1 has no
/// inline spans, so the title line and the muted remainder ride a `column`
/// of two styled text children (visually identical line output).
pub fn render_tool_call_lines(lines: &[String]) -> Value {
    let title = lines.first().cloned().unwrap_or_else(|| "mcp".to_string());
    let rest = if lines.len() > 1 {
        lines[1..].join("\n")
    } else {
        String::new()
    };
    let mut children = vec![json!({
        "type": "text",
        "props": { "text": title, "fg": "toolTitle", "bold": true },
    })];
    if !rest.is_empty() {
        children.push(json!({
            "type": "text",
            "props": { "text": rest, "fg": "muted" },
        }));
    }
    json!({ "type": "column", "props": {}, "children": children })
}

/// `renderMcpProxyToolCall` (tool-result-renderer.ts:171-174): format the
/// proxy args into call lines and render them. The default input budget is
/// the upstream constant (call sites never override it).
/// `renderToolCall` (tool-result-renderer.ts:282-296 @ 10a45367): seed the
/// per-call compact state, and — on the compact default with a final,
/// non-expanded, non-error context — render NOTHING (the compact result
/// row carries the whole tool call; upstream `EmptyComponent`). The static
/// ABI equivalent of an empty component is a childless `column`.
pub fn render_tool_call_with_state(
    lines: &[String],
    options: &McpToolRenderOptions,
    context: Option<&Value>,
    state: Option<&mut McpToolRenderState>,
) -> Value {
    if let Some(state) = state {
        state.compact_title = Some(lines.first().cloned().unwrap_or_else(|| "mcp".to_string()));
        state.compact_input_preview = Some(format_compact_input_preview(
            lines,
            DEFAULT_MAX_COMPACT_INPUT_CHARS,
        ));
    }
    if options.result_rendering == McpToolResultRendering::Compact
        && context.is_some_and(|context| {
            context.get("isPartial").and_then(Value::as_bool) != Some(true)
                && context.get("expanded").and_then(Value::as_bool) != Some(true)
                && context.get("isError").and_then(Value::as_bool) != Some(true)
        })
    {
        return json!({ "type": "column", "props": {}, "children": [] });
    }
    render_tool_call_lines(lines)
}

/// `renderMcpProxyToolCall` (tool-result-renderer.ts:171-174): format the
/// proxy args into call lines and render them. The default input budget is
/// the upstream constant (call sites never override it).
pub fn render_mcp_proxy_tool_call(args: &Value) -> Value {
    render_tool_call_lines(&format_mcp_proxy_tool_call_lines(
        args,
        DEFAULT_MAX_CALL_INPUT_CHARS,
    ))
}

/// `createMcpDirectToolCallRenderer(displayName)` (tool-result-renderer.ts:
/// 176-179), uncurried: the display name is the dispatch toolName.
pub fn render_mcp_direct_tool_call(display_name: &str, args: &Value) -> Value {
    render_tool_call_lines(&format_mcp_direct_tool_call_lines(
        display_name,
        args,
        DEFAULT_MAX_CALL_INPUT_CHARS,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_block(text: &str) -> Value {
        json!({ "type": "text", "text": text })
    }

    fn boxed_options() -> McpToolRenderOptions {
        McpToolRenderOptions {
            result_rendering: McpToolResultRendering::Boxed,
            collapsed_result_lines: DEFAULT_BOXED_COLLAPSED_LINES,
        }
    }

    fn default_options() -> McpToolRenderOptions {
        resolve_mcp_tool_render_options(None)
    }

    fn call_boxed(text: &str, expanded: bool) -> Value {
        render_mcp_tool_result(
            &json!({ "content": [text_block(text)] }),
            &json!({ "expanded": expanded, "isPartial": false }),
            &json!({ "isError": false }),
            &boxed_options(),
            None,
        )
    }

    #[test]
    fn compact_default_single_line_row_with_state_title() {
        // #349: the DEFAULT rendering is the compact row - one collapsed
        // result line carrying the call-time title + input preview.
        let options = default_options();
        assert_eq!(options.result_rendering, McpToolResultRendering::Compact);
        assert_eq!(options.collapsed_result_lines, 1);
        let state = McpToolRenderState {
            compact_title: Some("mcp call echo @ demo".to_string()),
            compact_input_preview: Some("{\"q\": \"x\"}".to_string()),
        };
        let tree = render_mcp_tool_result(
            &json!({ "content": [text_block("one\ntwo\nthree\nfour")] }),
            &json!({ "expanded": false, "isPartial": false }),
            &json!({ "isError": false }),
            &options,
            Some(&state),
        );
        let text = tree["children"][0]["props"]["text"].as_str().expect("row");
        assert_eq!(
            text,
            "mcp call echo @ demo {\"q\": \"x\"} \u{2192} one\n \u{2026} (Ctrl+O to expand)"
        );
        // Without state the title falls back to the details identity.
        let tree = render_mcp_tool_result(
            &json!({
                "content": [text_block("one")],
                "details": { "mode": "call", "server": "demo", "tool": "echo" }
            }),
            &json!({ "expanded": false, "isPartial": false }),
            &json!({ "isError": false }),
            &options,
            None,
        );
        let text = tree["children"][0]["props"]["text"].as_str().expect("row");
        assert_eq!(text, "MCP demo/echo \u{2192} one");
        // Errors never take the compact row (upstream forces expansion).
        let tree = render_mcp_tool_result(
            &json!({
                "content": [text_block("boom")],
                "details": { "error": "call_failed" }
            }),
            &json!({ "expanded": false, "isPartial": false }),
            &json!({ "isError": false }),
            &options,
            Some(&state),
        );
        assert_eq!(tree["type"], json!("text"), "error results stay boxed");
    }

    #[test]
    fn compact_input_preview_joins_and_collapses_whitespace() {
        // formatCompactInputPreview (:268-270): title dropped, rest joined,
        // whitespace collapsed, 240-unit bound.
        let lines = vec![
            "mcp call demo_search".to_string(),
            "{\n  \"q\": \"  spaced  \"\n}".to_string(),
        ];
        assert_eq!(
            format_compact_input_preview(&lines, DEFAULT_MAX_COMPACT_INPUT_CHARS),
            "{ \"q\": \" spaced \" }"
        );
        let long: Vec<String> = vec!["mcp".into(), "y".repeat(600)];
        let preview = format_compact_input_preview(&long, DEFAULT_MAX_COMPACT_INPUT_CHARS);
        assert!(preview.encode_utf16().count() <= DEFAULT_MAX_COMPACT_INPUT_CHARS);
        assert!(preview.ends_with('\u{2026}'));
    }

    #[test]
    fn render_options_resolution_accepts_only_valid_collapsed_lines() {
        // resolveMcpToolRenderOptions (:270-279).
        assert_eq!(
            resolve_mcp_tool_render_options(None),
            McpToolRenderOptions {
                result_rendering: McpToolResultRendering::Compact,
                collapsed_result_lines: 1,
            }
        );
        let boxed = json!({ "toolResultRendering": "boxed" })
            .as_object()
            .cloned();
        assert_eq!(
            resolve_mcp_tool_render_options(boxed.as_ref()).result_rendering,
            McpToolResultRendering::Boxed
        );
        assert_eq!(
            resolve_mcp_tool_render_options(boxed.as_ref()).collapsed_result_lines,
            3,
            "boxed default stays 3"
        );
        let invalid_lines = json!({ "collapsedResultLines": 4 }).as_object().cloned();
        assert_eq!(
            resolve_mcp_tool_render_options(invalid_lines.as_ref()).collapsed_result_lines,
            1,
            "4 is not accepted; compact default applies"
        );
        let valid = json!({ "collapsedResultLines": 2, "toolResultRendering": "boxed" })
            .as_object()
            .cloned();
        assert_eq!(
            resolve_mcp_tool_render_options(valid.as_ref()).collapsed_result_lines,
            2
        );
        // JS accepts the float form (`2.0 === 2`).
        let float_lines = json!({ "collapsedResultLines": 2.0 }).as_object().cloned();
        assert_eq!(
            resolve_mcp_tool_render_options(float_lines.as_ref()).collapsed_result_lines,
            2,
            "integer-valued float accepted"
        );
        let fractional = json!({ "collapsedResultLines": 2.5 }).as_object().cloned();
        assert_eq!(
            resolve_mcp_tool_render_options(fractional.as_ref()).collapsed_result_lines,
            1,
            "fractional value falls to the compact default"
        );
        let junk = json!({ "toolResultRendering": "weird", "collapsedResultLines": "x" })
            .as_object()
            .cloned();
        assert_eq!(
            resolve_mcp_tool_render_options(junk.as_ref()).result_rendering,
            McpToolResultRendering::Compact,
            "unknown rendering value falls to the compact default"
        );
    }

    #[test]
    fn compact_final_renders_empty_component_on_call() {
        // shouldUseCompactFinalRender: compact + final + !expanded + !error
        // -> the call part renders nothing (EmptyComponent equivalent).
        let options = default_options();
        let context = json!({ "isPartial": false, "expanded": false, "isError": false });
        let mut state = McpToolRenderState::default();
        let tree = render_tool_call_with_state(
            &["mcp call demo".to_string(), "{\"q\":1}".to_string()],
            &options,
            Some(&context),
            Some(&mut state),
        );
        assert_eq!(tree["type"], json!("column"));
        assert_eq!(tree["children"].as_array().map(Vec::len), Some(0));
        // The state was seeded before the empty short-circuit.
        assert_eq!(state.compact_title.as_deref(), Some("mcp call demo"));
        assert!(state.compact_input_preview.is_some());
        // A partial call render keeps the classic lines.
        let partial_context = json!({ "isPartial": true, "expanded": false, "isError": false });
        let mut state = McpToolRenderState::default();
        let tree = render_tool_call_with_state(
            &["mcp call demo".to_string()],
            &options,
            Some(&partial_context),
            Some(&mut state),
        );
        assert_eq!(tree["type"], json!("column"));
        assert!(tree["children"].as_array().is_some_and(|c| !c.is_empty()));
    }

    #[test]
    fn collapsed_caps_at_three_lines_with_expand_hint() {
        let tree = call_boxed("one\ntwo\nthree\nfour", false);
        let text = tree["props"]["text"].as_str().unwrap();
        assert_eq!(text, "one\ntwo\nthree\n…\n(Ctrl+O to expand)");
    }

    #[test]
    fn short_output_collapses_without_hint() {
        let tree = call_boxed("one\ntwo", false);
        let text = tree["props"]["text"].as_str().unwrap();
        assert_eq!(text, "one\ntwo");
    }

    #[test]
    fn expanded_shows_all_lines_without_hint() {
        let tree = call_boxed("one\ntwo\nthree\nfour", true);
        let text = tree["props"]["text"].as_str().unwrap();
        assert_eq!(text, "one\ntwo\nthree\nfour");
    }

    #[test]
    fn empty_content_renders_empty_result() {
        let tree = render_mcp_tool_result(
            &json!({ "content": [] }),
            &json!({ "expanded": false, "isPartial": false }),
            &json!({ "isError": false }),
            &boxed_options(),
            None,
        );
        assert_eq!(tree["props"]["text"], json!("(empty result)"));
    }

    #[test]
    fn char_budget_truncates_last_line() {
        let long = "x".repeat(9000);
        let tree = call_boxed(&long, false);
        let text = tree["props"]["text"].as_str().unwrap();
        // 8000-char budget consumed by the single line, then the
        // CollapsibleText footer "…\n(Ctrl+O to expand)" (line cap not
        // reached, so the footer supplies the ellipsis).
        assert!(text.starts_with(&"x".repeat(8000)), "len: {}", text.len());
        assert!(text.ends_with("…\n(Ctrl+O to expand)"));
        assert_eq!(text.matches('\n').count(), 2);
    }

    #[test]
    fn identity_line_prepended_for_call_mode() {
        let tree = render_mcp_tool_result(
            &json!({
                "content": [text_block("ok")],
                "details": { "mode": "call", "server": "demo", "tool": "echo" }
            }),
            &json!({ "expanded": false, "isPartial": false }),
            &json!({ "isError": false }),
            &boxed_options(),
            None,
        );
        let text = tree["props"]["text"].as_str().unwrap();
        assert_eq!(text, "MCP demo/echo\nok");
    }

    #[test]
    fn identity_falls_back_to_hint_server_resource_and_requested_tool() {
        let details = json!({ "mode": "call", "hintServer": "hint" });
        assert!(format_mcp_tool_result_identity(Some(&details)).is_none());
        assert_eq!(
            format_mcp_tool_result_identity(Some(&json!(
                { "mode": "call", "server": "s", "resourceUri": "file:///x" }
            ))),
            Some("MCP s resource file:///x".to_string())
        );
        assert_eq!(
            format_mcp_tool_result_identity(Some(&json!(
                { "mode": "call", "server": "s", "requestedTool": "echo" }
            ))),
            Some("MCP s/echo".to_string())
        );
        assert!(format_mcp_tool_result_identity(Some(&json!({ "mode": "list" }))).is_none());
    }

    #[test]
    fn details_error_forces_expansion() {
        let tree = render_mcp_tool_result(
            &json!({
                "content": [text_block("one\ntwo\nthree\nfour")],
                "details": { "error": "call_failed" }
            }),
            &json!({ "expanded": false, "isPartial": false }),
            &json!({ "isError": false }),
            &boxed_options(),
            None,
        );
        let text = tree["props"]["text"].as_str().unwrap();
        assert_eq!(text, "one\ntwo\nthree\nfour");
    }

    #[test]
    fn context_is_error_forces_expansion() {
        let tree = render_mcp_tool_result(
            &json!({ "content": [text_block("one\ntwo\nthree\nfour")] }),
            &json!({ "expanded": false, "isPartial": false }),
            &json!({ "isError": true }),
            &boxed_options(),
            None,
        );
        let text = tree["props"]["text"].as_str().unwrap();
        assert_eq!(text, "one\ntwo\nthree\nfour");
    }

    #[test]
    fn is_partial_renders_running_hint() {
        let tree = render_mcp_tool_result(
            &json!({ "content": [text_block("ignored")] }),
            &json!({ "expanded": false, "isPartial": true }),
            &json!({ "isError": false }),
            &boxed_options(),
            None,
        );
        assert_eq!(tree["props"]["text"], json!("Running MCP tool..."));
    }

    #[test]
    fn image_blocks_render_placeholder_lines() {
        let tree = render_mcp_tool_result(
            &json!({
                "content": [
                    text_block("a"),
                    { "type": "image", "mimeType": "image/png" },
                    text_block("b\nc"),
                ]
            }),
            &json!({ "expanded": false, "isPartial": false }),
            &json!({ "isError": false }),
            &boxed_options(),
            None,
        );
        let text = tree["props"]["text"].as_str().unwrap();
        assert_eq!(text, "a\n[image: image/png]\nb\n…\n(Ctrl+O to expand)");
    }

    // ===== renderCall parity cases (upstream
    // __tests__/tool-result-renderer.test.ts:24-76, 281-291) =====

    const MAX: usize = DEFAULT_MAX_CALL_INPUT_CHARS;

    #[test]
    fn proxy_call_with_json_string_args_and_server() {
        // test.ts:24-35.
        let lines = format_mcp_proxy_tool_call_lines(
            &json!({
                "tool": "cf-portal_list_worker_tail_events",
                "server": "cf-portal",
                "args": "{\"accountId\":\"abc\",\"scriptName\":\"worker\"}",
            }),
            MAX,
        );
        assert_eq!(
            lines,
            vec![
                "mcp call cf-portal_list_worker_tail_events @ cf-portal".to_string(),
                "{\n  \"accountId\": \"abc\",\n  \"scriptName\": \"worker\"\n}".to_string(),
            ]
        );
    }

    #[test]
    fn proxy_call_with_object_args_and_no_server() {
        // test.ts:37-47.
        let lines = format_mcp_proxy_tool_call_lines(
            &json!({
                "tool": "cf-portal_list_worker_tail_events",
                "args": { "accountId": "abc", "limit": 10 },
            }),
            MAX,
        );
        assert_eq!(
            lines,
            vec![
                "mcp call cf-portal_list_worker_tail_events".to_string(),
                "{\n  \"accountId\": \"abc\",\n  \"limit\": 10\n}".to_string(),
            ]
        );
    }

    #[test]
    fn proxy_discovery_branches() {
        // test.ts:49-56.
        let lines = format_mcp_proxy_tool_call_lines(
            &json!({ "search": "tail events", "server": "cf-portal", "regex": true }),
            MAX,
        );
        assert_eq!(lines, vec!["mcp search tail events @ cf-portal (regex)"]);
        let lines = format_mcp_proxy_tool_call_lines(&json!({ "connect": "cf-portal" }), MAX);
        assert_eq!(lines, vec!["mcp connect cf-portal"]);
        let lines = format_mcp_proxy_tool_call_lines(&json!({ "server": "cf-portal" }), MAX);
        assert_eq!(lines, vec!["mcp list cf-portal"]);
        let lines = format_mcp_proxy_tool_call_lines(&json!({}), MAX);
        assert_eq!(lines, vec!["mcp status"]);
    }

    #[test]
    fn proxy_empty_branch_keys_are_falsy() {
        // JS truthiness on every branch key (`if (args.tool)` etc.): an
        // empty string falls through to the next branch, ending at
        // `mcp status` — a present-but-empty key must NOT match.
        let lines = format_mcp_proxy_tool_call_lines(&json!({ "tool": "" }), MAX);
        assert_eq!(lines, vec!["mcp status"]);
        let lines = format_mcp_proxy_tool_call_lines(&json!({ "connect": "" }), MAX);
        assert_eq!(lines, vec!["mcp status"]);
        let lines = format_mcp_proxy_tool_call_lines(&json!({ "describe": "" }), MAX);
        assert_eq!(lines, vec!["mcp status"]);
        let lines = format_mcp_proxy_tool_call_lines(&json!({ "search": "" }), MAX);
        assert_eq!(lines, vec!["mcp status"]);
        let lines = format_mcp_proxy_tool_call_lines(&json!({ "server": "" }), MAX);
        assert_eq!(lines, vec!["mcp status"]);
        let lines = format_mcp_proxy_tool_call_lines(&json!({ "action": "" }), MAX);
        assert_eq!(lines, vec!["mcp status"]);
        // empty `server` annotation is falsy too: no `@ ` suffix, and the
        // `server` branch itself is skipped so a lone tool call renders bare.
        let lines = format_mcp_proxy_tool_call_lines(&json!({ "tool": "t", "server": "" }), MAX);
        assert_eq!(lines, vec!["mcp call t"]);
        let lines = format_mcp_proxy_tool_call_lines(&json!({ "search": "s", "server": "" }), MAX);
        assert_eq!(lines, vec!["mcp search s"]);
    }

    #[test]
    fn proxy_ui_messages_wins_over_server() {
        // test.ts:58-60.
        let lines = format_mcp_proxy_tool_call_lines(
            &json!({ "action": "ui-messages", "server": "cf-portal" }),
            MAX,
        );
        assert_eq!(lines, vec!["mcp ui-messages"]);
    }

    #[test]
    fn proxy_search_annotations_are_exact_match() {
        // `regex` must be exactly true, `includeSchemas` exactly false
        // (upstream `===` comparisons).
        let lines = format_mcp_proxy_tool_call_lines(
            &json!({ "search": "q", "regex": false, "includeSchemas": true }),
            MAX,
        );
        assert_eq!(lines, vec!["mcp search q"]);
        let lines = format_mcp_proxy_tool_call_lines(
            &json!({ "search": "q", "includeSchemas": false }),
            MAX,
        );
        assert_eq!(lines, vec!["mcp search q (schemas hidden)"]);
    }

    #[test]
    fn proxy_args_truthiness_follows_js() {
        // `""`/0/false/null/absent skip the summary line; `{}` (any object)
        // is truthy and formats as `{}` (verified against upstream).
        assert_eq!(
            format_mcp_proxy_tool_call_lines(&json!({ "tool": "t" }), MAX),
            vec!["mcp call t"]
        );
        assert_eq!(
            format_mcp_proxy_tool_call_lines(&json!({ "tool": "t", "args": "" }), MAX),
            vec!["mcp call t"]
        );
        assert_eq!(
            format_mcp_proxy_tool_call_lines(&json!({ "tool": "t", "args": null }), MAX),
            vec!["mcp call t"]
        );
        assert_eq!(
            format_mcp_proxy_tool_call_lines(&json!({ "tool": "t", "args": 0 }), MAX),
            vec!["mcp call t"]
        );
        assert_eq!(
            format_mcp_proxy_tool_call_lines(&json!({ "tool": "t", "args": {} }), MAX),
            vec!["mcp call t", "{}"]
        );
        assert_eq!(
            format_mcp_proxy_tool_call_lines(&json!({ "tool": "t", "args": [1] }), MAX),
            vec!["mcp call t", "[\n  1\n]"]
        );
    }

    #[test]
    fn direct_tool_with_and_without_args() {
        // test.ts:62-76.
        let lines = format_mcp_direct_tool_call_lines(
            "cf-portal_list_worker_tail_events",
            &json!({ "accountId": "abc", "scriptName": "worker" }),
            MAX,
        );
        assert_eq!(
            lines,
            vec![
                "cf-portal_list_worker_tail_events".to_string(),
                "{\n  \"accountId\": \"abc\",\n  \"scriptName\": \"worker\"\n}".to_string(),
            ]
        );
        assert_eq!(
            format_mcp_direct_tool_call_lines("cf-portal_status", &json!({}), MAX),
            vec!["cf-portal_status"]
        );
    }

    #[test]
    fn jsonish_truncates_at_the_char_budget() {
        // `truncateText`: first max-1 chars + "…".
        let long = json!("x".repeat(1600));
        let out = format_jsonish(&long, 100);
        assert_eq!(out.chars().count(), 100);
        assert!(out.ends_with('…'));
        // A JSON string input is re-parsed and pretty-printed.
        let out = format_jsonish(&json!("{\"b\":1}"), 100);
        assert_eq!(out, "{\n  \"b\": 1\n}");
        // Non-JSON strings pass through unchanged (still budgeted).
        let out = format_jsonish(&json!("plain text"), 100);
        assert_eq!(out, "plain text");
    }

    #[test]
    fn jsonish_budget_counts_utf16_code_units() {
        // `value.length` is UTF-16: 80 astral chars are 160 units — over a
        // 100-unit budget even though chars().count() says 80. The budget
        // check must not under-count astral input into skipping truncation.
        let astral = "\u{1F600}".repeat(80);
        let out = format_jsonish(&json!(astral.clone()), 100);
        assert!(out.ends_with('…'), "{out}");
        // 49 whole chars (98 units) + ellipsis; the cut never splits a
        // surrogate pair (upstream's mid-pair cut only yields U+FFFD).
        assert_eq!(out.chars().count(), 50);
        assert_eq!(out.chars().filter(|c| *c == '\u{1F600}').count(), 49);
        // Under the budget in BOTH counting systems: unchanged.
        let small = "\u{1F600}".repeat(40);
        assert_eq!(format_jsonish(&json!(small.clone()), 100), small);
    }

    #[test]
    fn render_call_tree_splits_title_and_muted_rest() {
        // test.ts:281-291 render shape: first line toolTitle, rest muted.
        let tree = render_mcp_proxy_tool_call(&json!({ "tool": "test_tool", "server": "demo" }));
        assert_eq!(tree["type"], json!("column"));
        let children = tree["children"].as_array().unwrap();
        assert_eq!(
            children[0]["props"]["text"],
            json!("mcp call test_tool @ demo")
        );
        assert_eq!(children[0]["props"]["fg"], json!("toolTitle"));
        assert_eq!(children[0]["props"]["bold"], json!(true));
        assert_eq!(children.len(), 1);

        let tree = render_mcp_direct_tool_call("test_tool", &json!({ "key": "value" }));
        let children = tree["children"].as_array().unwrap();
        assert_eq!(children[0]["props"]["text"], json!("test_tool"));
        assert_eq!(
            children[1]["props"]["text"],
            json!("{\n  \"key\": \"value\"\n}")
        );
        assert_eq!(children[1]["props"]["fg"], json!("muted"));
    }

    #[test]
    fn empty_call_lines_default_the_title() {
        // renderToolCallLines destructures `[title = "mcp", ...rest]`.
        let tree = render_tool_call_lines(&[]);
        assert_eq!(tree["children"][0]["props"]["text"], json!("mcp"));
    }
}
