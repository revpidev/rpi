//! renderCall/renderResult component trees (FR-P2-E) — port of the
//! renderer half of upstream `pi-smart-fetch/src/index.ts` @ b0111612
//! (:77-468 helpers, :628-664 web_fetch renderers, :773-789 batch
//! renderers), mapped onto ComponentTree v1
//! (`rpi.component-tree.v1`).
//!
//! Protocol-driven degradations (v1 has no `row`, one `fg` per text node —
//! task file Out list: bg-block progress bars, Markdown nodes and
//! pixel-level parity are non-goals):
//! - every horizontal multi-color composition collapses to a single text
//!   node (the glyph/URL/`█░` bar line carries one foreground color);
//! - the Markdown preview renders as plain text (mcp-adapter precedent);
//! - `getOptimisticProgress` time extrapolation is dropped (Out: depends on
//!   `Date.now`) — bar fill uses the event's static progress value.
//!
//! The trees are static JSON built once per render call — the host TUI has
//! no width feedback channel in the render dispatch, so truncation uses a
//! fixed assumed width (see [`DEFAULT_RENDER_WIDTH`]; §5.5 manual-check
//! territory, not a parity surface).

use serde_json::{json, Value};

/// `SPINNER_FRAMES` (index.ts:43), verbatim.
pub const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// The assumed terminal width for the static trees (no width context rides
/// the render dispatch; the TUI wraps on its own beyond this).
pub const DEFAULT_RENDER_WIDTH: usize = 80;

/// `maxPreviewLines` (index.ts:292).
const MAX_PREVIEW_LINES: usize = 7;

// ===== ComponentTree v1 node helpers =====

fn text_node(text: &str) -> Value {
    json!({ "type": "text", "props": { "text": text } })
}

fn text_node_styled(text: &str, fg: &str, bold: bool) -> Value {
    let mut props = serde_json::Map::new();
    props.insert("text".to_string(), json!(text));
    props.insert("fg".to_string(), json!(fg));
    if bold {
        props.insert("bold".to_string(), json!(true));
    }
    json!({ "type": "text", "props": Value::Object(props) })
}

fn column_node(children: Vec<Value>) -> Value {
    json!({ "type": "column", "props": {}, "children": children })
}

fn spacer_node(lines: usize) -> Value {
    json!({ "type": "spacer", "props": { "lines": lines } })
}

// ===== shared geometry & glyphs (index.ts:77-85, 107-175, 379-417) =====

/// `truncateMiddle` (index.ts:77-85). Upstream slices UTF-16 code units;
/// this port slices chars (the same declared [VARIANT] class as truncation
/// elsewhere in the plugin).
pub fn truncate_middle(value: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let chars: Vec<char> = value.chars().collect();
    if chars.len() <= width {
        let padded: String = chars.into_iter().collect();
        return format!("{padded:<width$}");
    }
    if width == 1 {
        return "…".to_string();
    }
    let left = (width - 1).div_ceil(2);
    let right = (width - 1) / 2;
    let head: String = chars[..left].iter().collect();
    let tail: String = chars[chars.len() - right..].iter().collect();
    format!("{head}…{tail}")
}

/// The progress-bar column width shared by both renderers
/// (index.ts:197-205 / 402-410): `max(12, min(18, floor(width × 0.2)))`.
fn progress_bar_width(width: usize) -> usize {
    (width / 5).clamp(12, 18)
}

/// The URL column width (index.ts:202-205 / 407-410):
/// `max(12, width − glyph(2) − progress − 2)`.
fn url_column_width(width: usize) -> usize {
    std::cmp::max(12, width.saturating_sub(2 + progress_bar_width(width) + 2))
}

/// `renderStatusGlyph` (index.ts:152-175) without the theme calls: the
/// glyph character and its fg color name. Pending glyphs spin on
/// `SPINNER_FRAMES[index % 10]`.
fn status_glyph(status: &str, spinner_index: usize) -> (&'static str, &'static str) {
    let frame = SPINNER_FRAMES[spinner_index % SPINNER_FRAMES.len()];
    match status {
        "done" => ("✓", "success"),
        "error" => ("✗", "error"),
        "queued" => (frame, "muted"),
        _ => (frame, "accent"),
    }
}

/// `${value}` rendering for the file-info lines: JSON scalars without the
/// serde quoting (numbers bare, strings bare, null → "undefined" — a file
/// result always carries these fields, the fallback mirrors JS).
fn plain_template_value(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => "undefined".to_string(),
        Value::Bool(flag) => flag.to_string(),
        Value::Number(number) => number.to_string(),
        other => format!("{other}"),
    }
}

/// The character progress bar (`renderProgressBar`, index.ts:107-150,
/// re-based from bg blocks to `█`/`░` fills — Out: pixel-level parity).
/// Fill is the static event progress (`getOptimisticProgress` dropped).
fn char_progress_bar(progress: f64, width: usize) -> String {
    let inner = std::cmp::max(10, width.saturating_sub(2));
    let filled = ((progress.clamp(0.0, 1.0)) * inner as f64).round() as usize;
    let filled = filled.min(inner);
    format!("[{}{}]", "█".repeat(filled), "░".repeat(inner - filled))
}

/// The single-line pending row: `<glyph> <url> <bar>` (index.ts:209-220 /
/// 412-416). One text node — the multi-color split is not expressible in
/// v1 (no row, one fg per node).
fn progress_row(
    url: &str,
    status: &str,
    progress: f64,
    spinner_index: usize,
    width: usize,
) -> Value {
    let (glyph, fg) = status_glyph(status, spinner_index);
    let truncated = truncate_middle(url, url_column_width(width));
    let bar = char_progress_bar(progress, progress_bar_width(width));
    text_node_styled(&format!("{glyph} {truncated} {bar}"), fg, false)
}

// ===== renderCall (index.ts:628-635, 773-781) =====

/// `web_fetch` call line: `web_fetch <url>` (non-string/absent url →
/// `...`). Upstream splits toolTitle/accent; v1 carries one color.
pub fn render_web_fetch_call(args: &Value) -> Value {
    let url = args.get("url").and_then(Value::as_str).unwrap_or("...");
    text_node_styled(&format!("web_fetch {url}"), "toolTitle", false)
}

/// `batch_web_fetch` call line: `batch_web_fetch <n> urls`
/// (non-array requests → 0).
pub fn render_batch_call(args: &Value) -> Value {
    let count = args
        .get("requests")
        .and_then(Value::as_array)
        .map(|requests| requests.len())
        .unwrap_or(0);
    text_node_styled(&format!("batch_web_fetch {count} urls"), "toolTitle", true)
}

// ===== renderResult: web_fetch (index.ts:637-664) =====

/// The partial (pending) result: the single-item progress row
/// (`createResponsiveSingleFetchProgressComponent` /
/// `renderSingleFetchProgressText`, index.ts:379-437).
fn render_web_fetch_partial(details: &Value) -> Value {
    let status = details
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("connecting");
    let url = details
        .get("url")
        .or_else(|| {
            details
                .get("fetchResult")
                .and_then(|result| result.get("finalUrl").or_else(|| result.get("url")))
        })
        .and_then(Value::as_str)
        .unwrap_or("");
    let progress = details
        .get("progress")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let spinner_tick = details
        .get("spinnerTick")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    progress_row(
        url,
        status,
        progress,
        spinner_tick as usize,
        DEFAULT_RENDER_WIDTH,
    )
}

/// `buildWebFetchMetadataLines` (index.ts:238-261): Title/Published when
/// non-empty. Upstream colors each half (syntaxKeyword/syntaxString); v1
/// renders the muted pair.
fn web_fetch_metadata_lines(details: &Value) -> Vec<String> {
    let Some(result) = details.get("fetchResult") else {
        return Vec::new();
    };
    let mut lines = Vec::new();
    for (label, key) in [("Title", "title"), ("Published", "published")] {
        let value = result.get(key).and_then(Value::as_str);
        if let Some(value) = value.filter(|value| !value.is_empty()) {
            lines.push(format!("{label}: {value}"));
        }
    }
    lines
}

/// `buildWebFetchCollapsedPreview` (index.ts:282-299): drop the trailing
/// empty lines, keep the first 7.
fn web_fetch_collapsed_preview(content: &str) -> (String, usize) {
    let lines: Vec<&str> = content.split('\n').collect();
    let content_lines: Vec<&str> = lines
        .iter()
        .enumerate()
        .filter(|(index, line)| !line.is_empty() || *index == 0 || *index + 1 < lines.len())
        .map(|(_, line)| *line)
        .collect();
    let preview: Vec<&str> = content_lines
        .iter()
        .take(MAX_PREVIEW_LINES)
        .copied()
        .collect();
    let remaining = content_lines.len().saturating_sub(preview.len());
    (preview.join("\n"), remaining)
}

/// `createWebFetchResultComponent` (index.ts:301-377) as a column:
/// metadata lines → (file info | preview/full content) → the expand hint.
/// Markdown rendering degrades to plain text (Out).
fn render_web_fetch_final(details: &Value, expanded: bool) -> Value {
    let Some(fetch_result) = details.get("fetchResult") else {
        return text_node_styled("No fetch result available.", "muted", false);
    };

    let mut children: Vec<Value> = Vec::new();
    let metadata = web_fetch_metadata_lines(details);
    if !metadata.is_empty() {
        children.push(text_node_styled(&metadata.join("\n"), "muted", false));
    }

    let is_file = fetch_result.get("kind").and_then(Value::as_str) == Some("file");
    if is_file {
        if !metadata.is_empty() {
            children.push(spacer_node(1));
        }
        let mut file_lines = Vec::new();
        // JS template values: numbers render bare, missing fields render
        // "undefined" — `Value`'s Display would quote strings.
        file_lines.push(format!(
            "File size: {}",
            fetch_result
                .get("fileSize")
                .map(plain_template_value)
                .unwrap_or_else(|| "undefined".to_string())
        ));
        if let Some(mime_type) = fetch_result
            .get("mimeType")
            .and_then(Value::as_str)
            .filter(|mime| !mime.is_empty())
        {
            file_lines.push(format!("Mime type: {mime_type}"));
        }
        file_lines.push(format!(
            "File path: {}",
            fetch_result
                .get("filePath")
                .map(plain_template_value)
                .unwrap_or_else(|| "undefined".to_string())
        ));
        children.push(text_node_styled(&file_lines.join("\n"), "muted", false));
        return column_node(children);
    }

    let content = fetch_result
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or("");
    let (preview, remaining) = web_fetch_collapsed_preview(content);
    let shown = if expanded { content } else { &preview };
    if !metadata.is_empty() && !shown.is_empty() {
        children.push(spacer_node(1));
    }
    if !shown.is_empty() {
        children.push(text_node(shown));
    }
    if !expanded && remaining > 0 {
        if !shown.is_empty() {
            children.push(spacer_node(1));
        }
        children.push(text_node_styled(
            &format!("... ({remaining} more lines, Ctrl+O to expand)"),
            "muted",
            false,
        ));
    }
    column_node(children)
}

/// `renderResult` for `web_fetch` (index.ts:637-664). Pure and
/// synchronous: result/options/context JSON only.
pub fn render_web_fetch_result(result: &Value, options: &Value, _context: &Value) -> Value {
    let is_partial = options
        .get("isPartial")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let expanded = options
        .get("expanded")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let details = result.get("details").cloned().unwrap_or(Value::Null);

    if is_partial {
        return render_web_fetch_partial(&details);
    }
    if details.get("error").and_then(Value::as_bool) == Some(true) {
        // index.ts:649-661: the user-facing summary wins; then the first
        // text block, then the raw error text.
        let output_text = result
            .get("content")
            .and_then(Value::as_array)
            .and_then(|blocks| blocks.first())
            .and_then(|block| block.get("text"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let message = details
            .get("userErrorSummary")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .or_else(|| Some(output_text).filter(|text| !text.is_empty()))
            .or_else(|| {
                details
                    .get("errorText")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
            })
            .unwrap_or("Error");
        return text_node_styled(message, "error", false);
    }
    render_web_fetch_final(&details, expanded)
}

// ===== renderResult: batch (index.ts:177-223, 439-468, 783-789) =====

/// `renderBatchProgressText` (index.ts:177-223) as a column: the bold
/// summary line, then one row per item (error rows gain their detail line
/// when expanded). One color per line (v1).
pub fn render_batch_result(result: &Value, options: &Value, _context: &Value) -> Value {
    let expanded = options
        .get("expanded")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let details = result.get("details").cloned().unwrap_or(Value::Null);
    let Some(snapshot) = details.get("batchProgress") else {
        return text_node_styled("No batch progress available.", "muted", false);
    };
    let spinner_tick = details
        .get("spinnerTick")
        .and_then(Value::as_u64)
        .unwrap_or(0);

    let mut children = Vec::new();
    let summary = format!(
        "batch_web_fetch {}/{} done · ok {} · err {} · concurrency {}",
        snapshot.get("completed").cloned().unwrap_or(json!(0)),
        snapshot.get("total").cloned().unwrap_or(json!(0)),
        snapshot.get("succeeded").cloned().unwrap_or(json!(0)),
        snapshot.get("failed").cloned().unwrap_or(json!(0)),
        snapshot
            .get("batchConcurrency")
            .cloned()
            .unwrap_or(json!(0)),
    );
    children.push(text_node_styled(&summary, "toolTitle", true));

    if let Some(items) = snapshot.get("items").and_then(Value::as_array) {
        for item in items {
            let index = item.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
            let url = item.get("url").and_then(Value::as_str).unwrap_or("");
            let status = item
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("queued");
            let progress = item.get("progress").and_then(Value::as_f64).unwrap_or(0.0);
            children.push(progress_row(
                url,
                status,
                progress,
                spinner_tick as usize + index,
                DEFAULT_RENDER_WIDTH,
            ));
            // index.ts:215-219: the expanded error detail line.
            if expanded && status == "error" {
                if let Some(error) = item
                    .get("error")
                    .and_then(Value::as_str)
                    .filter(|error| !error.is_empty())
                {
                    children.push(text_node_styled(
                        &format!("  error: {error}"),
                        "error",
                        false,
                    ));
                }
            }
        }
    }
    column_node(children)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_middle_boundaries() {
        assert_eq!(truncate_middle("abc", 0), "");
        assert_eq!(truncate_middle("abc", 5), "abc  ");
        assert_eq!(truncate_middle("abcdefgh", 5), "ab…gh");
        assert_eq!(truncate_middle("long-value-here", 1), "…");
        // left gets the ceiling: width 6 → 3 chars + … + 2 chars
        assert_eq!(truncate_middle("abcdefgh", 6), "abc…gh");
    }

    #[test]
    fn progress_bar_fill_counts() {
        assert_eq!(
            char_progress_bar(0.0, 14),
            format!("[{}{}]", "", "░".repeat(12))
        );
        let full = char_progress_bar(1.0, 14);
        assert_eq!(full.matches('█').count(), 12);
        assert!(!full.contains('░'));
        let half = char_progress_bar(0.5, 14);
        assert_eq!(half.matches('█').count(), 6);
        assert_eq!(half.matches('░').count(), 6);
    }

    #[test]
    fn web_fetch_call_line() {
        let tree = render_web_fetch_call(&json!({ "url": "https://example.com/x" }));
        assert_eq!(
            tree["props"]["text"],
            json!("web_fetch https://example.com/x")
        );
        assert_eq!(tree["props"]["fg"], json!("toolTitle"));
        let missing = render_web_fetch_call(&json!({}));
        assert_eq!(missing["props"]["text"], json!("web_fetch ..."));
    }

    #[test]
    fn batch_call_line() {
        let tree = render_batch_call(&json!({ "requests": [{ "url": "a" }, { "url": "b" }] }));
        assert_eq!(tree["props"]["text"], json!("batch_web_fetch 2 urls"));
        assert_eq!(tree["props"]["bold"], json!(true));
        assert_eq!(
            render_batch_call(&json!({}))["props"]["text"],
            json!("batch_web_fetch 0 urls")
        );
    }

    #[test]
    fn partial_result_renders_progress_row() {
        let tree = render_web_fetch_result(
            &json!({
                "content": [],
                "details": { "status": "loading", "progress": 0.51, "url": "https://ex.com/a", "spinnerTick": 3 }
            }),
            &json!({ "isPartial": true, "expanded": false }),
            &json!({}),
        );
        let text = tree["props"]["text"].as_str().unwrap();
        // glyph frame 3 = ⠸; url present; bar inner = 14 at width 80,
        // 0.51×14 = 7.14 → 7 filled
        assert!(text.starts_with("⠸ https://ex.com/a "), "{text}");
        assert_eq!(text.matches('█').count(), 7);
        assert_eq!(tree["props"]["fg"], json!("accent"));
    }

    #[test]
    fn partial_error_status_uses_error_glyph() {
        let tree = render_web_fetch_result(
            &json!({
                "content": [],
                "details": { "status": "error", "progress": 1.0, "url": "https://ex.com/bad", "spinnerTick": 0 }
            }),
            &json!({ "isPartial": true }),
            &json!({}),
        );
        assert!(tree["props"]["text"]
            .as_str()
            .unwrap()
            .starts_with("✗ https://ex.com/bad"));
        assert_eq!(tree["props"]["fg"], json!("error"));
    }

    #[test]
    fn error_result_renders_user_summary() {
        let tree = render_web_fetch_result(
            &json!({
                "content": [{ "type": "text", "text": "Error: boom" }],
                "details": { "error": true, "userErrorSummary": "The request failed." }
            }),
            &json!({ "isPartial": false, "expanded": false }),
            &json!({}),
        );
        assert_eq!(tree["props"]["text"], json!("The request failed."));
        assert_eq!(tree["props"]["fg"], json!("error"));
        // summary missing → the content text
        let fallback = render_web_fetch_result(
            &json!({
                "content": [{ "type": "text", "text": "Error: boom" }],
                "details": { "error": true }
            }),
            &json!({ "isPartial": false, "expanded": false }),
            &json!({}),
        );
        assert_eq!(fallback["props"]["text"], json!("Error: boom"));
    }

    #[test]
    fn final_result_metadata_preview_and_hint() {
        let content = "para one\npara two\npara three\npara four\npara five\npara six\npara seven\npara eight\npara nine";
        let tree = render_web_fetch_result(
            &json!({
                "content": [{ "type": "text", "text": "agent text" }],
                "details": {
                    "fetchResult": {
                        "kind": "content",
                        "title": "T",
                        "published": "2026-01-01",
                        "content": content,
                    }
                }
            }),
            &json!({ "isPartial": false, "expanded": false }),
            &json!({}),
        );
        assert_eq!(tree["type"], json!("column"));
        let children = tree["children"].as_array().unwrap();
        assert_eq!(
            children[0]["props"]["text"],
            json!("Title: T\nPublished: 2026-01-01")
        );
        assert_eq!(children[1]["type"], json!("spacer"));
        let preview = children[2]["props"]["text"].as_str().unwrap();
        assert_eq!(preview.matches('\n').count(), 6, "7 preview lines");
        assert!(preview.starts_with("para one"));
        assert_eq!(
            children[4]["props"]["text"],
            json!("... (2 more lines, Ctrl+O to expand)")
        );
    }

    #[test]
    fn final_result_expanded_shows_everything() {
        let content = "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nl10";
        let tree = render_web_fetch_result(
            &json!({
                "content": [],
                "details": { "fetchResult": { "kind": "content", "content": content } }
            }),
            &json!({ "isPartial": false, "expanded": true }),
            &json!({}),
        );
        let texts: Vec<&str> = tree["children"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|node| node["type"] == json!("text"))
            .map(|node| node["props"]["text"].as_str().unwrap())
            .collect();
        assert_eq!(texts, vec!["l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nl10"]);
    }

    #[test]
    fn file_result_lines() {
        let tree = render_web_fetch_result(
            &json!({
                "content": [],
                "details": {
                    "fetchResult": {
                        "kind": "file",
                        "fileSize": 12345,
                        "mimeType": "application/pdf",
                        "filePath": "/tmp/smart-fetch-rpi/report.pdf",
                        "content": ""
                    }
                }
            }),
            &json!({ "isPartial": false, "expanded": false }),
            &json!({}),
        );
        let children = tree["children"].as_array().unwrap();
        assert_eq!(
            children[0]["props"]["text"],
            json!("File size: 12345\nMime type: application/pdf\nFile path: /tmp/smart-fetch-rpi/report.pdf")
        );
        assert_eq!(children[0]["props"]["fg"], json!("muted"));
    }

    #[test]
    fn batch_result_summary_items_and_expanded_error() {
        let result = json!({
            "content": [],
            "details": {
                "batchProgress": {
                    "items": [
                        { "index": 0, "url": "https://ex.com/a", "status": "done", "progress": 1.0 },
                        { "index": 1, "url": "https://ex.com/b", "status": "error", "progress": 1.0, "error": "Invalid URL" },
                        { "index": 2, "url": "https://ex.com/c", "status": "loading", "progress": 0.51 }
                    ],
                    "total": 3, "completed": 2, "succeeded": 1, "failed": 1, "batchConcurrency": 8
                },
                "spinnerTick": 4
            }
        });
        let collapsed = render_batch_result(&result, &json!({ "expanded": false }), &json!({}));
        let children = collapsed["children"].as_array().unwrap();
        assert_eq!(children.len(), 4, "summary + 3 items, no error lines");
        assert_eq!(
            children[0]["props"]["text"],
            json!("batch_web_fetch 2/3 done · ok 1 · err 1 · concurrency 8")
        );
        assert!(children[1]["props"]["text"]
            .as_str()
            .unwrap()
            .starts_with("✓ "));
        assert!(children[2]["props"]["text"]
            .as_str()
            .unwrap()
            .starts_with("✗ "));
        assert!(children[3]["props"]["text"]
            .as_str()
            .unwrap()
            .starts_with("⠦ "));

        let expanded = render_batch_result(&result, &json!({ "expanded": true }), &json!({}));
        let children = expanded["children"].as_array().unwrap();
        assert_eq!(children.len(), 5);
        assert_eq!(children[3]["props"]["text"], json!("  error: Invalid URL"));
        assert_eq!(children[3]["props"]["fg"], json!("error"));
    }

    #[test]
    fn batch_result_without_snapshot() {
        let tree = render_batch_result(&json!({ "details": {} }), &json!({}), &json!({}));
        assert_eq!(tree["props"]["text"], json!("No batch progress available."));
    }
}
