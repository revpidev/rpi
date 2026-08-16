//! TE11 FR-A/FR-B: `subagent` tool renderers over the host render protocol
//! (`{"kind":"render","what":"toolCall"|"toolResult"}` dispatch,
//! host_call.rs) — ComponentTree v1 output, rpi-own design: information
//! parity with the pi experience, no byte/visual parity (TE11 取代 TE10).
//!
//! Input shapes (the plugin's own data faces, TE09):
//! - toolCall: `context.args` — the tool params (`agent`+`task` / `tasks` /
//!   `steps` / `action`).
//! - toolResult: `result` (`{content[], details, isError?}`) where `details`
//!   is either a terminal payload (`{mode, runId, results[], ...}`) or a
//!   TE09 streaming frame (`{mode, results[], progress[], controlEvents}` —
//!   `isPartial` distinguishes them).
//!
//! Spinner: the foreground ticker pushes one snapshot frame per second
//! (ACTIVITY_TICK_MS), so the frame source is the run's `durationMs` —
//! no protocol change to the TE09 details shape.
//!
//! Theme color names used (present in both bundled themes): `toolTitle`,
//! `accent`, `success`, `error`, `warning`, `muted`, `dim`, `toolOutput`.

use serde_json::{json, Value};

/// Braille spinner frames (same glyphs as the TE08 smart-fetch renderer).
pub const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// One spinner frame per pushed snapshot (1/s — the ticker period).
const SPINNER_FRAME_MS: u64 = 1000;

/// The spinner glyph for a run of `duration_ms` (advances with every pushed
/// frame because the duration grows monotonically).
pub fn spinner_frame(duration_ms: u64) -> &'static str {
    SPINNER_FRAMES[((duration_ms / SPINNER_FRAME_MS) % SPINNER_FRAMES.len() as u64) as usize]
}

/// The aborted marker (`foreground.rs` settles user-aborted runs with this
/// error text — render treats it as the ■ terminal family, not ✗).
const ABORTED_ERROR: &str = "Subagent run aborted by user.";

// ===== FR-A: the invocation title line =======================================

/// `renderSubagentToolCall`: title line for a `subagent` invocation, four
/// shapes — single delegation (`subagent · {agent}` + `[async]` suffix),
/// parallel (`subagent · N tasks`), chain (`subagent · chain · N steps`),
/// management action (`subagent · {action} {target?}`). A muted summary
/// line carries the delegated task text (single) or the item keys
/// (composite) when available.
pub fn render_subagent_tool_call(args: &Value) -> Value {
    let mut children = vec![json!({
        "type": "text",
        "props": {
            "text": call_title(args),
            "fg": "toolTitle",
            "bold": true,
            "truncate": true,
        },
    })];
    if let Some(summary) = call_summary(args) {
        children.push(json!({
            "type": "text",
            "props": { "text": summary, "fg": "muted", "truncate": true },
        }));
    }
    json!({ "type": "column", "props": {}, "children": children })
}

/// The title text of a call (upstream `renderCall` non-workflow branches;
/// exact wording is rpi-own).
pub fn call_title(args: &Value) -> String {
    let array_len = |key: &str| args.get(key).and_then(Value::as_array).map_or(0, Vec::len);
    if let Some(action) = args.get("action").and_then(Value::as_str) {
        let target = ["id", "runId", "target"]
            .iter()
            .find_map(|key| args.get(*key).and_then(Value::as_str))
            .filter(|t| !t.is_empty());
        return match target {
            Some(target) => format!("subagent · {action} {target}"),
            None => format!("subagent · {action}"),
        };
    }
    if array_len("tasks") > 0 {
        return format!("subagent · {} tasks", array_len("tasks"));
    }
    if array_len("steps") > 0 || args.get("chainName").is_some() {
        let steps = if array_len("steps") > 0 {
            array_len("steps").to_string()
        } else {
            "?".to_string()
        };
        return format!("subagent · chain · {steps} steps");
    }
    let mut title = match args.get("agent").and_then(Value::as_str) {
        Some(agent) => format!("subagent · {agent}"),
        None => "subagent".to_string(),
    };
    // The explicit `async: true` marker (composite shapes are async by
    // construction and stay unmarked — the shape itself says it).
    if args.get("async").and_then(Value::as_bool) == Some(true) {
        title.push_str(" [async]");
    }
    title
}

/// The muted second line: the delegated task (single) or the item keys
/// (composite). `None` when the call carries nothing worth a summary.
fn call_summary(args: &Value) -> Option<String> {
    if let Some(task) = args.get("task").and_then(Value::as_str) {
        let task = task.trim();
        if !task.is_empty() {
            return Some(bounded(first_line(task), 100));
        }
    }
    let items = ["tasks", "steps"]
        .iter()
        .find_map(|key| args.get(*key).and_then(Value::as_array))?;
    let keys: Vec<String> = items
        .iter()
        .filter_map(|item| {
            item.get("key")
                .or_else(|| item.get("agent"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .collect();
    if keys.is_empty() {
        return None;
    }
    Some(bounded(&keys.join(", "), 100))
}

// ===== FR-B: the run card ====================================================

/// `renderSubagentToolResult`: the run card over the terminal or partial
/// details. Structure: status glyph + status line, stats line, per-child
/// lines (composite shapes), current-activity line (running), artifact
/// path line, and — expanded (Ctrl+O) — the recent-output tail window.
/// `call_args` is the invocation's `context.args` — the agent name source
/// when the details carry no per-run rows (management actions, async
/// receipts, partial frames before the first row lands).
pub fn render_subagent_tool_result(result: &Value, options: &Value, call_args: &Value) -> Value {
    let is_partial = options.get("isPartial").and_then(Value::as_bool) == Some(true);
    let expanded = options.get("expanded").and_then(Value::as_bool) == Some(true);
    let details = result.get("details").unwrap_or(&Value::Null);
    let mode = details
        .get("mode")
        .and_then(Value::as_str)
        .unwrap_or("single");
    let fallback_agent = call_args
        .get("agent")
        .and_then(Value::as_str)
        .unwrap_or("subagent");
    let results = details
        .get("results")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let progress = details
        .get("progress")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut lines: Vec<(String, &'static str)> = Vec::new();

    // Async receipt (FR-B "detached" family, found missing in the 2026-08-16
    // live-session check): the dispatch already returned — the run continues
    // in the background and this card never transitions again (completion
    // arrives as a session message). A spinner here would read "still in
    // this call" forever, which is exactly what the live session showed.
    if mode == "async" && !is_partial {
        let run_id = details.get("runId").and_then(Value::as_str).unwrap_or("");
        lines.push((
            format!("■ {fallback_agent} · background · {run_id}"),
            "warning",
        ));
        lines.push((
            "completion arrives as a session message; inspect with subagent({action:\"status\"})"
                .to_string(),
            "muted",
        ));
        if let Some(status_file) = details.get("statusFile").and_then(Value::as_str) {
            lines.push((format!("status: {status_file}"), "dim"));
        }
        return card_tree(lines);
    }

    match mode {
        "parallel" | "chain" => {
            // Composite head: one aggregate line, then per-child lines.
            let label = if mode == "parallel" {
                format!("{} tasks", results.len().max(1))
            } else {
                format!("chain · {} steps", results.len().max(1))
            };
            let (glyph, status, tone) = composite_status(&results, &progress, is_partial);
            lines.push((format!("{glyph} {label} · {status}"), tone));
            lines.push((composite_stats(&results, &progress), "muted"));
            for (index, child) in results.iter().enumerate() {
                let child_progress = progress.get(index);
                let child = child_view(child, child_progress, is_partial);
                lines.push((
                    format!("  {} {} · {}", child.glyph, child.agent, child.status_text),
                    child.tone,
                ));
                if let Some(stats) = &child.stats {
                    lines.push((format!("    {stats}"), "dim"));
                }
            }
        }
        _ => {
            // Single: everything merged onto the main lines. No rows yet
            // (management actions, pre-first-frame partials) → the fallback
            // running view named after the delegated agent, not a bare
            // "subagent".
            let child = results
                .first()
                .map(|entry| child_view(entry, progress.first(), is_partial))
                .unwrap_or_else(|| ChildView::running(fallback_agent, 0));
            lines.push((
                format!("{} {} · {}", child.glyph, child.agent, child.status_text),
                child.tone,
            ));
            if let Some(stats) = &child.stats {
                lines.push((stats.clone(), "muted"));
            }
            if let Some(activity) = &child.activity {
                lines.push((activity.clone(), "toolOutput"));
            }
        }
    }

    // Artifact path line (terminal frames; partial frames carry none).
    if !is_partial {
        for entry in &results {
            if let Some(path) = artifact_output_path(entry) {
                lines.push((format!("output: {path}"), "dim"));
                break;
            }
        }
    }

    // Expanded: the recent-output tail window.
    if expanded {
        let tail = expanded_tail(result, &results, is_partial);
        if !tail.is_empty() {
            lines.push((tail, "toolOutput"));
        }
    }

    card_tree(lines)
}

/// Wrap `(text, fg)` rows into the card's column tree (every row truncates).
fn card_tree(lines: Vec<(String, &'static str)>) -> Value {
    json!({
        "type": "column",
        "props": {},
        "children": lines
            .into_iter()
            .map(|(text, fg)| json!({
                "type": "text",
                "props": { "text": text, "fg": fg, "truncate": true },
            }))
            .collect::<Vec<_>>(),
    })
}

/// One child's projection onto card fields.
struct ChildView {
    glyph: String,
    agent: String,
    status_text: String,
    tone: &'static str,
    stats: Option<String>,
    /// `⎿ {currentTool} {args}` while running.
    activity: Option<String>,
}

impl ChildView {
    fn running(agent: &str, duration_ms: u64) -> Self {
        ChildView {
            glyph: spinner_frame(duration_ms).to_string(),
            agent: agent.to_string(),
            status_text: "running".to_string(),
            tone: "accent",
            stats: None,
            activity: None,
        }
    }
}

fn child_view(entry: &Value, progress: Option<&Value>, is_partial: bool) -> ChildView {
    let agent = entry
        .get("agent")
        .and_then(Value::as_str)
        .unwrap_or("subagent")
        .to_string();
    let duration_ms = progress
        .and_then(|p| p.get("durationMs"))
        .and_then(Value::as_u64)
        .unwrap_or(0);

    if is_partial || progress.is_some_and(|p| p.get("status") == Some(&json!("running"))) {
        // Streaming frame: always the running projection (the ticker and
        // per-event pushes drive the spinner + stats).
        let mut view = ChildView::running(&agent, duration_ms);
        view.stats = progress.map(progress_stats);
        view.activity = progress.and_then(current_activity);
        return view;
    }

    // Terminal: exitCode / error / final output decide the family.
    let exit_code = entry.get("exitCode").and_then(Value::as_i64).unwrap_or(0);
    let error = entry.get("error").and_then(Value::as_str);
    let final_output = entry
        .get("finalOutput")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let stats = terminal_stats(entry);
    if let Some(error) = error {
        if error.contains("aborted by user") || error == ABORTED_ERROR {
            return ChildView {
                glyph: "■".to_string(),
                agent,
                status_text: "Aborted by user".to_string(),
                tone: "warning",
                stats,
                activity: None,
            };
        }
        return ChildView {
            glyph: "✗".to_string(),
            agent,
            status_text: format!("Error: {}", first_line(error)),
            tone: "error",
            stats,
            activity: None,
        };
    }
    if exit_code != 0 {
        return ChildView {
            glyph: "✗".to_string(),
            agent,
            status_text: format!("exit {exit_code}"),
            tone: "error",
            stats,
            activity: None,
        };
    }
    let no_output = final_output.trim().is_empty()
        || entry.get("outputState").and_then(Value::as_str) == Some("absent");
    ChildView {
        glyph: "✓".to_string(),
        agent,
        status_text: if no_output {
            "Done (no text output)".to_string()
        } else {
            "Done".to_string()
        },
        tone: if no_output { "warning" } else { "success" },
        stats,
        activity: None,
    }
}

/// Aggregate (glyph, status, tone) for a composite call.
fn composite_status(
    results: &[Value],
    progress: &[Value],
    is_partial: bool,
) -> (&'static str, String, &'static str) {
    if is_partial {
        let max_duration = progress
            .iter()
            .filter_map(|p| p.get("durationMs").and_then(Value::as_u64))
            .max()
            .unwrap_or(0);
        return (spinner_frame(max_duration), "running".to_string(), "accent");
    }
    let any_aborted = results.iter().any(|entry| {
        entry
            .get("error")
            .and_then(Value::as_str)
            .is_some_and(|error| error.contains("aborted by user"))
    });
    if any_aborted {
        return ("■", "aborted".to_string(), "warning");
    }
    let failed = results
        .iter()
        .filter(|entry| entry.get("exitCode").and_then(Value::as_i64) != Some(0))
        .count();
    if failed == 0 {
        ("✓", "all steps done".to_string(), "success")
    } else {
        (
            "✗",
            format!("{failed}/{} failed", results.len().max(1)),
            "error",
        )
    }
}

/// Totals across children: tools · tokens · in/out. The tools segment only
/// appears while streaming frames carry it (terminal composite details have
/// no toolCount aggregate).
fn composite_stats(results: &[Value], progress: &[Value]) -> String {
    let tools: u64 = progress
        .iter()
        .filter_map(|p| p.get("toolCount").and_then(Value::as_u64))
        .sum();
    let (input, output) = usage_totals(results, progress);
    if input + output == 0 && tools == 0 {
        return String::new();
    }
    if tools > 0 {
        format!(
            "{} tools · {} token · in:{} out:{}",
            tools,
            input + output,
            input,
            output
        )
    } else {
        format!("{} token · in:{} out:{}", input + output, input, output)
    }
}

/// Stats line from a streaming progress snapshot.
fn progress_stats(progress: &Value) -> String {
    let tools = progress
        .get("toolCount")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let tokens = progress.get("tokens").and_then(Value::as_u64).unwrap_or(0);
    let input = progress
        .get("inputTokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output = progress
        .get("outputTokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let duration = progress
        .get("durationMs")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    format!(
        "{} tools · {} token · {} · in:{} out:{}",
        tools,
        tokens,
        format_duration(duration),
        input,
        output
    )
}

/// Stats line from a terminal child entry (usage lives on the entry).
fn terminal_stats(entry: &Value) -> Option<String> {
    let usage = entry.get("usage")?;
    let input = usage.get("input").and_then(Value::as_u64).unwrap_or(0);
    let output = usage.get("output").and_then(Value::as_u64).unwrap_or(0);
    let turns = usage.get("turns").and_then(Value::as_u64).unwrap_or(0);
    Some(format!(
        "{} turns · {} token · in:{} out:{}",
        turns,
        input + output,
        input,
        output
    ))
}

/// `⎿ {currentTool} {args}` from a progress snapshot (running only).
fn current_activity(progress: &Value) -> Option<String> {
    let tool = progress.get("currentTool").and_then(Value::as_str)?;
    let mut line = format!("⎿ {tool}");
    if let Some(args) = progress.get("currentToolArgs").and_then(Value::as_str) {
        let args = args.trim();
        if !args.is_empty() {
            line.push(' ');
            line.push_str(&args.chars().take(60).collect::<String>());
        }
    }
    Some(line)
}

/// `output:` path source: `artifactPaths.outputPath`, then `savedOutputPath`.
fn artifact_output_path(entry: &Value) -> Option<String> {
    let from_paths = entry
        .get("artifactPaths")
        .and_then(|paths| paths.get("outputPath"))
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty())
        .map(str::to_owned);
    from_paths.or_else(|| {
        entry
            .get("savedOutputPath")
            .and_then(Value::as_str)
            .filter(|p| !p.is_empty())
            .map(str::to_owned)
    })
}

/// The expanded tail: recentOutput while running, the final output at
/// terminal (bounded to a few lines).
fn expanded_tail(result: &Value, results: &[Value], is_partial: bool) -> String {
    if is_partial {
        let tail = results
            .first()
            .and_then(|entry| entry.get("progress"))
            .and_then(|p| p.get("recentOutput"))
            .or_else(|| results.first().and_then(|_| result.get("recentOutput")))
            .and_then(Value::as_str)
            .unwrap_or_default();
        return tail.trim_end().to_string();
    }
    let output = results
        .first()
        .and_then(|entry| entry.get("finalOutput"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let lines: Vec<&str> = output.lines().collect();
    let tail: Vec<&str> = if lines.len() > 10 {
        lines[lines.len() - 10..].to_vec()
    } else {
        lines
    };
    tail.join("\n")
}

fn usage_totals(results: &[Value], progress: &[Value]) -> (u64, u64) {
    let mut input = 0;
    let mut output = 0;
    for entry in results {
        if let Some(usage) = entry.get("usage") {
            input += usage.get("input").and_then(Value::as_u64).unwrap_or(0);
            output += usage.get("output").and_then(Value::as_u64).unwrap_or(0);
        }
    }
    for snapshot in progress {
        input += snapshot
            .get("inputTokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        output += snapshot
            .get("outputTokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
    }
    (input, output)
}

fn first_line(text: &str) -> &str {
    text.split('\n').next().unwrap_or("")
}

/// `formatDuration`-style one-decimal seconds (TE09 `formatDuration`
/// semantics, applied to milliseconds).
pub fn format_duration(duration_ms: u64) -> String {
    format!("{:.1}s", duration_ms as f64 / 1000.0)
}

// ===== bounded summary (payload bound, not width) =============================

/// First line only, capped to `max_chars` with an ellipsis (the render
/// width truncation happens host-side via `truncate`; this bounds the
/// payload itself).
fn bounded(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_string()
    } else {
        let mut cut: String = text.chars().take(max_chars.saturating_sub(1)).collect();
        cut.push('…');
        cut
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ===== FR-A =====

    #[test]
    fn call_title_four_shapes() {
        assert_eq!(
            call_title(&json!({"agent": "researcher", "task": "x"})),
            "subagent · researcher"
        );
        assert_eq!(
            call_title(&json!({"agent": "researcher", "async": true})),
            "subagent · researcher [async]"
        );
        assert_eq!(
            call_title(&json!({"tasks": [{"agent": "a"}, {"agent": "b"}]})),
            "subagent · 2 tasks"
        );
        assert_eq!(
            call_title(&json!({"steps": [{}, {}, {}]})),
            "subagent · chain · 3 steps"
        );
        assert_eq!(
            call_title(&json!({"action": "stop", "id": "run-abc"})),
            "subagent · stop run-abc"
        );
    }

    #[test]
    fn call_summary_carries_task_first_line() {
        let args = json!({"agent": "researcher", "task": "study the parity checklist\nand report"});
        let tree = render_subagent_tool_call(&args);
        let children = tree["children"].as_array().unwrap();
        assert_eq!(children.len(), 2);
        assert_eq!(
            children[1]["props"]["text"].as_str().unwrap(),
            "study the parity checklist"
        );
    }

    #[test]
    fn call_title_line_is_single_bold_truncating_text() {
        let tree = render_subagent_tool_call(&json!({"agent": "scout"}));
        let title = &tree["children"][0];
        assert_eq!(title["type"], "text");
        assert_eq!(title["props"]["bold"], json!(true));
        assert_eq!(title["props"]["truncate"], json!(true));
        assert_eq!(title["props"]["fg"], "toolTitle");
    }

    // ===== FR-B: partial frames =====

    fn partial_details() -> Value {
        json!({
            "mode": "single",
            "results": [{
                "index": 0,
                "agent": "researcher",
                "exitCode": 0,
                "outputState": "absent",
                "usage": {"input": 10, "output": 5, "turns": 1},
                "progress": {
                    "status": "running",
                    "currentTool": "grep",
                    "currentToolArgs": "-rn te11 src",
                    "toolCount": 3,
                    "tokens": 15,
                    "inputTokens": 10,
                    "outputTokens": 5,
                    "durationMs": 12_500,
                }
            }],
            "progress": [{
                "status": "running",
                "currentTool": "grep",
                "currentToolArgs": "-rn te11 src",
                "toolCount": 3,
                "tokens": 15,
                "inputTokens": 10,
                "outputTokens": 5,
                "durationMs": 12_500,
            }],
        })
    }

    fn card_lines(tree: &Value) -> Vec<(String, String)> {
        tree["children"]
            .as_array()
            .unwrap()
            .iter()
            .map(|node| {
                (
                    node["props"]["text"].as_str().unwrap_or("").to_string(),
                    node["props"]["fg"].as_str().unwrap_or("").to_string(),
                )
            })
            .collect()
    }

    #[test]
    fn partial_card_shows_spinner_stats_and_activity() {
        let tree = render_subagent_tool_result(
            &json!({"details": partial_details()}),
            &json!({"isPartial": true, "expanded": false}),
            &json!({"agent": "researcher"}),
        );
        let lines = card_lines(&tree);
        assert!(lines[0].0.contains("researcher · running"), "{lines:?}");
        assert!(SPINNER_FRAMES.contains(&lines[0].0.split(' ').next().unwrap()));
        assert_eq!(lines[0].1, "accent");
        assert!(
            lines
                .iter()
                .any(|(text, _)| text.starts_with("3 tools · 15 token · 12.5s")),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|(text, _)| text.starts_with("⎿ grep")),
            "{lines:?}"
        );
    }

    #[test]
    fn spinner_advances_with_duration() {
        assert_eq!(spinner_frame(0), SPINNER_FRAMES[0]);
        assert_eq!(spinner_frame(1000), SPINNER_FRAMES[1]);
        assert_eq!(spinner_frame(12_500), SPINNER_FRAMES[2]);
    }

    // ===== FR-B: terminal families =====

    #[test]
    #[test]
    fn async_receipt_renders_detached_not_running() {
        // The 2026-08-16 live-session defect: the receipt is the call's
        // FINAL render, so a spinner here reads "still running" forever.
        let receipt = json!({
            "mode": "async",
            "runId": "23ea5c12",
            "status": "running",
            "statusFile": "/tmp/rpi-subagents-uid-1000/async-subagent-runs/23ea5c12/status.json",
        });
        let tree = render_subagent_tool_result(
            &json!({"details": receipt}),
            &json!({}),
            &json!({"agent": "researcher", "async": true}),
        );
        let lines = card_lines(&tree);
        assert!(
            lines[0]
                .0
                .starts_with("■ researcher · background · 23ea5c12"),
            "{lines:?}"
        );
        assert_eq!(lines[0].1, "warning");
        assert!(
            lines
                .iter()
                .any(|(text, _)| text.contains("session message")),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|(text, _)| text.starts_with("status: ")),
            "{lines:?}"
        );
        // No spinner anywhere: the call itself has returned.
        assert!(
            !lines.iter().any(|(text, _)| text.contains("running")),
            "{lines:?}"
        );
    }

    #[test]
    fn fallback_running_view_uses_the_delegated_agent_name() {
        // Management action / pre-first-frame partial: no result rows, the
        // args' agent names the card.
        let tree = render_subagent_tool_result(
            &json!({"details": {"mode": "single", "results": []}}),
            &json!({"isPartial": true}),
            &json!({"agent": "scout"}),
        );
        let lines = card_lines(&tree);
        assert!(lines[0].0.contains("scout · running"), "{lines:?}");
    }

    fn terminal_success_card() {
        let details = json!({
            "mode": "single",
            "results": [{
                "agent": "researcher",
                "exitCode": 0,
                "finalOutput": "the report",
                "usage": {"input": 100, "output": 50, "turns": 4},
                "artifactPaths": {"outputPath": "/tmp/art/out.md"},
            }],
        });
        let tree =
            render_subagent_tool_result(&json!({"details": details}), &json!({}), &json!({}));
        let lines = card_lines(&tree);
        assert!(lines[0].0.starts_with("✓ researcher · Done"), "{lines:?}");
        assert_eq!(lines[0].1, "success");
        assert!(lines
            .iter()
            .any(|(text, _)| text == "output: /tmp/art/out.md"));
    }

    #[test]
    fn terminal_no_output_warns() {
        let details = json!({
            "mode": "single",
            "results": [{
                "agent": "worker",
                "exitCode": 0,
                "finalOutput": "",
                "usage": {"input": 1, "output": 0, "turns": 1},
            }],
        });
        let tree =
            render_subagent_tool_result(&json!({"details": details}), &json!({}), &json!({}));
        let lines = card_lines(&tree);
        assert!(lines[0].0.contains("Done (no text output)"), "{lines:?}");
        assert_eq!(lines[0].1, "warning");
    }

    #[test]
    fn terminal_error_and_aborted_families() {
        let error_details = |error: &str| {
            json!({
                "mode": "single",
                "results": [{
                    "agent": "oracle",
                    "exitCode": 1,
                    "error": error,
                    "usage": {"input": 0, "output": 0, "turns": 0},
                }],
            })
        };
        let tree = render_subagent_tool_result(
            &json!({"details": error_details("boom at step 2\ntrace")}),
            &json!({}),
            &json!({}),
        );
        let lines = card_lines(&tree);
        assert!(
            lines[0].0.starts_with("✗ oracle · Error: boom at step 2"),
            "{lines:?}"
        );
        assert_eq!(lines[0].1, "error");

        let tree = render_subagent_tool_result(
            &json!({"details": error_details("Subagent run aborted by user.")}),
            &json!({}),
            &json!({}),
        );
        let lines = card_lines(&tree);
        assert!(
            lines[0].0.starts_with("■ oracle · Aborted by user"),
            "{lines:?}"
        );
        assert_eq!(lines[0].1, "warning");
    }

    // ===== FR-B: composite =====

    #[test]
    fn parallel_card_lists_each_child() {
        let details = json!({
            "mode": "parallel",
            "results": [
                {"agent": "scout", "exitCode": 0, "finalOutput": "ok", "usage": {"input": 10, "output": 5, "turns": 2}},
                {"agent": "mapper", "exitCode": 0, "finalOutput": "map", "usage": {"input": 20, "output": 8, "turns": 3}},
            ],
        });
        let tree =
            render_subagent_tool_result(&json!({"details": details}), &json!({}), &json!({}));
        let lines = card_lines(&tree);
        assert!(
            lines[0].0.starts_with("✓ 2 tasks · all steps done"),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|(text, _)| text.contains("scout · Done")),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|(text, _)| text.contains("mapper · Done")),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|(text, _)| text.starts_with("43 token")),
            "{lines:?}"
        );
    }

    #[test]
    fn chain_partial_card_marks_running_children() {
        let details = json!({
            "mode": "chain",
            "results": [
                {"agent": "researcher", "exitCode": 0, "finalOutput": "done", "usage": {"input": 5, "output": 2, "turns": 1}},
                {"agent": "writer", "exitCode": 0, "outputState": "absent", "usage": {"input": 0, "output": 0, "turns": 0},
                 "progress": {"status": "running", "durationMs": 3000, "toolCount": 1, "tokens": 0, "inputTokens": 0, "outputTokens": 0}},
            ],
            "progress": [{}, {"status": "running", "durationMs": 3000, "toolCount": 1, "tokens": 0, "inputTokens": 0, "outputTokens": 0}],
        });
        let tree = render_subagent_tool_result(
            &json!({"details": details}),
            &json!({"isPartial": true}),
            &json!({}),
        );
        let lines = card_lines(&tree);
        assert!(lines[0].0.contains("chain · 2 steps"), "{lines:?}");
        // The in-flight child shows a spinner; the finished one shows ✓.
        assert!(
            lines
                .iter()
                .any(|(text, _)| text.contains("writer · running")),
            "{lines:?}"
        );
    }

    #[test]
    fn expanded_terminal_card_carries_output_tail() {
        let details = json!({
            "mode": "single",
            "results": [{
                "agent": "researcher",
                "exitCode": 0,
                "finalOutput": "l1\nl2\nl3",
                "usage": {"input": 1, "output": 1, "turns": 1},
            }],
        });
        let tree = render_subagent_tool_result(
            &json!({"details": details}),
            &json!({"expanded": true}),
            &json!({}),
        );
        let lines = card_lines(&tree);
        assert!(
            lines
                .iter()
                .any(|(text, fg)| text.contains("l3") && fg == "toolOutput"),
            "{lines:?}"
        );
    }
}
