//! Tool-result renderer (renderResult): builds the ComponentTree for MCP
//! tool results so the host collapses long output instead of the generic
//! fallback full expansion.
//!
//! Port of `tool-result-renderer.ts` @ 3d953f90: `renderMcpToolResult`
//! (:269-297) + `collectCollapsedResultLines` (:188-238) +
//! `formatMcpToolResultIdentity` (:240-252).
//!
//! Upstream returns rpi-tui components (Text/CollapsibleText with theme
//! colors); the native ABI renders declaratively, so this module returns a
//! static ComponentTree text node (`rpi.component-tree.v1`). The theme
//! colors and the width-dependent CollapsibleText re-clamping are dropped:
//! the static text carries the final line/char-clamped output, with the
//! "(Ctrl+O to expand)" hint folded into the text when truncated.

use serde_json::{json, Value};

/// `DEFAULT_MAX_COLLAPSED_LINES` (tool-result-renderer.ts:36).
pub const DEFAULT_MAX_COLLAPSED_LINES: usize = 3;
/// `DEFAULT_MAX_COLLAPSED_CHARS` (tool-result-renderer.ts:37).
pub const DEFAULT_MAX_COLLAPSED_CHARS: usize = 8000;

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

/// `renderMcpToolResult` (tool-result-renderer.ts:269-297), mapped onto a
/// ComponentTree text node. Pure and synchronous: only the result/options/
/// context JSON is consulted.
pub fn render_mcp_tool_result(result: &Value, options: &Value, context: &Value) -> Value {
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
    let identity = format_mcp_tool_result_identity(details);
    let display = format_mcp_tool_result_lines(
        result,
        expanded,
        DEFAULT_MAX_COLLAPSED_LINES,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn text_block(text: &str) -> Value {
        json!({ "type": "text", "text": text })
    }

    fn call(text: &str, expanded: bool) -> Value {
        render_mcp_tool_result(
            &json!({ "content": [text_block(text)] }),
            &json!({ "expanded": expanded, "isPartial": false }),
            &json!({ "isError": false }),
        )
    }

    #[test]
    fn collapsed_caps_at_three_lines_with_expand_hint() {
        let tree = call("one\ntwo\nthree\nfour", false);
        let text = tree["props"]["text"].as_str().unwrap();
        assert_eq!(text, "one\ntwo\nthree\n…\n(Ctrl+O to expand)");
    }

    #[test]
    fn short_output_collapses_without_hint() {
        let tree = call("one\ntwo", false);
        let text = tree["props"]["text"].as_str().unwrap();
        assert_eq!(text, "one\ntwo");
    }

    #[test]
    fn expanded_shows_all_lines_without_hint() {
        let tree = call("one\ntwo\nthree\nfour", true);
        let text = tree["props"]["text"].as_str().unwrap();
        assert_eq!(text, "one\ntwo\nthree\nfour");
    }

    #[test]
    fn empty_content_renders_empty_result() {
        let tree = render_mcp_tool_result(
            &json!({ "content": [] }),
            &json!({ "expanded": false, "isPartial": false }),
            &json!({ "isError": false }),
        );
        assert_eq!(tree["props"]["text"], json!("(empty result)"));
    }

    #[test]
    fn char_budget_truncates_last_line() {
        let long = "x".repeat(9000);
        let tree = call(&long, false);
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
        );
        let text = tree["props"]["text"].as_str().unwrap();
        assert_eq!(text, "a\n[image: image/png]\nb\n…\n(Ctrl+O to expand)");
    }
}
