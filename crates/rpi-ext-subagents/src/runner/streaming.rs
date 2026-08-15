//! Streaming progress snapshots for the foreground child run (TE09 FR-A).
//!
//! Port of pi-subagents `src/runs/foreground/execution.ts` streaming face @
//! v0.48.0: the `AgentProgress` state (`:442-458`), the per-event
//! `fireUpdate`/`emitUpdateSnapshot` push (`:824-845`), and the snapshot
//! bounders `snapshotProgress` (`:239-247`) / `snapshotStreamResult`
//! (`:272-284`) over `src/shared/utils.ts:449-490`
//! (`boundStreamedRecentTools` / `boundStreamedRecentOutput` /
//! `boundStreamedToolCalls` + `extractToolCallSummaries` :370-387).
//!
//! Upstream pushes every event plus a 1s activity-timer tick; the
//! `deriveActivityState` engine behind the timer is out of TE09 scope, so
//! `activityState` stays unset and `controlEvents` is always empty (it only
//! ever carries `active_long_running`/`needs_attention`, both products of
//! that engine).

use serde_json::{json, Map, Value};

use super::display::{extract_text_from_content, format_tool_call};
use super::events::ChildRunState;

/// `MAX_STREAMED_RECENT_TOOLS` (utils.ts:450).
pub const MAX_STREAMED_RECENT_TOOLS: usize = 32;
/// `MAX_STREAMED_TOOL_CALLS` (utils.ts:464 area, same constant block).
pub const MAX_STREAMED_TOOL_CALLS: usize = 64;
/// `MAX_STREAMED_OUTPUT_LINE_CHARS` (utils.ts:465 area).
pub const MAX_STREAMED_OUTPUT_LINE_CHARS: usize = 2000;
/// In-memory `recentOutput` retention (execution.ts:220-226 `appendRecentOutput`).
pub const RECENT_OUTPUT_MEMORY_LINES: usize = 50;
/// Lines appended per event (execution.ts:962/978 `split("\n").slice(-10)`).
pub const RECENT_OUTPUT_APPEND_LINES: usize = 10;

/// The run-scoped facts the snapshot needs beyond the per-child state
/// (`AgentProgress` constructor inputs, execution.ts:442-458).
#[derive(Debug, Clone)]
pub struct StreamMeta {
    pub index: u32,
    pub agent: String,
    /// `shared.resolvedSkillNames` — the resolved skill list (absent → no
    /// `skills` field, upstream spreads `undefined` away).
    pub skills: Option<Vec<String>>,
    /// Launch model argument (`modelArg`) — absent when the child inherits.
    pub model: Option<String>,
    pub thinking: Option<String>,
    pub started_at_ms: u64,
}

/// `appendRecentOutput` (execution.ts:220-226): append the non-blank lines
/// (last 10 of the event's text), keep the last 50 in memory.
pub fn append_recent_output(state: &mut ChildRunState, lines: &[String]) {
    if lines.is_empty() {
        return;
    }
    state
        .recent_output
        .extend(lines.iter().filter(|line| !line.trim().is_empty()).cloned());
    if state.recent_output.len() > RECENT_OUTPUT_MEMORY_LINES {
        let overflow = state.recent_output.len() - RECENT_OUTPUT_MEMORY_LINES;
        state.recent_output.drain(..overflow);
    }
}

/// The event-text lines that feed `append_recent_output`
/// (`extractTextFromContent(...).split("\n").slice(-10)`, execution.ts:962
/// and :978).
pub fn recent_output_lines(content: &Value) -> Vec<String> {
    let text = extract_text_from_content(content);
    let lines: Vec<&str> = text.split('\n').collect();
    let start = lines.len().saturating_sub(RECENT_OUTPUT_APPEND_LINES);
    lines[start..].iter().map(|line| line.to_string()).collect()
}

/// `boundStreamedRecentTools` (utils.ts:455-457).
pub fn bound_streamed_recent_tools(state: &ChildRunState) -> Vec<Value> {
    let start = state
        .recent_tools
        .len()
        .saturating_sub(MAX_STREAMED_RECENT_TOOLS);
    state.recent_tools[start..]
        .iter()
        .map(|entry| {
            json!({
                "tool": entry.tool,
                "args": entry.args,
                "endMs": entry.end_ms,
            })
        })
        .collect()
}

/// `boundStreamedRecentOutput` (utils.ts:459-463): cap each line at 2000
/// chars, marking truncation.
pub fn bound_streamed_recent_output(state: &ChildRunState) -> Vec<Value> {
    state
        .recent_output
        .iter()
        .map(|line| {
            if line.encode_utf16().count() > MAX_STREAMED_OUTPUT_LINE_CHARS {
                let prefix =
                    super::display::truncate_display_text(line, MAX_STREAMED_OUTPUT_LINE_CHARS);
                json!(format!("{prefix}… [truncated]"))
            } else {
                json!(line)
            }
        })
        .collect()
}

/// `extractToolCallSummaries` (utils.ts:370-387) over the accumulated
/// messages — the rpi result has no precomputed `toolCalls` list, so the
/// message-derived summary is always the source.
pub fn extract_tool_call_summaries(messages: &[Value]) -> Vec<Value> {
    let mut summaries = Vec::new();
    for message in messages {
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(blocks) = message.get("content").and_then(Value::as_array) else {
            continue;
        };
        for block in blocks {
            if block.get("type").and_then(Value::as_str) != Some("toolCall") {
                continue;
            }
            let name = block
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let arguments = match block.get("arguments") {
                Some(value) if value.as_object().is_some() => value.clone(),
                _ => json!({}),
            };
            summaries.push(json!({
                "text": format_tool_call(name, &arguments, false),
                "expandedText": format_tool_call(name, &arguments, true),
            }));
        }
    }
    summaries
}

/// `boundStreamedToolCalls` (utils.ts:483-490): most recent 64 summaries,
/// `undefined` (field dropped) when empty.
pub fn bound_streamed_tool_calls(messages: &[Value]) -> Option<Vec<Value>> {
    let summaries = extract_tool_call_summaries(messages);
    if summaries.is_empty() {
        return None;
    }
    let start = summaries.len().saturating_sub(MAX_STREAMED_TOOL_CALLS);
    Some(summaries[start..].to_vec())
}

/// `snapshotProgress` (execution.ts:239-247) over the live progress state:
/// task redacted, skills cloned, recentTools/recentOutput bounded. Field
/// presence follows upstream — optional fields drop when unset.
pub fn snapshot_progress(state: &ChildRunState, meta: &StreamMeta, now_ms: u64) -> Value {
    let mut map = Map::new();
    map.insert("index".to_string(), json!(meta.index));
    map.insert("agent".to_string(), json!(meta.agent));
    map.insert("status".to_string(), json!("running"));
    map.insert("task".to_string(), json!("[prompt redacted]"));
    if let Some(skills) = &meta.skills {
        map.insert("skills".to_string(), json!(skills));
    }
    if let Some(current_tool) = &state.current_tool {
        map.insert("currentTool".to_string(), json!(current_tool));
        if let Some(args) = &state.current_tool_args {
            map.insert("currentToolArgs".to_string(), json!(args));
        }
        if let Some(started_at) = state.current_tool_started_at {
            map.insert("currentToolStartedAt".to_string(), json!(started_at));
        }
    }
    if let Some(path) = &state.current_path {
        map.insert("currentPath".to_string(), json!(path));
    }
    map.insert(
        "recentTools".to_string(),
        json!(bound_streamed_recent_tools(state)),
    );
    map.insert(
        "recentOutput".to_string(),
        json!(bound_streamed_recent_output(state)),
    );
    map.insert("toolCount".to_string(), json!(state.tool_count));
    map.insert("turnCount".to_string(), json!(state.turns));
    map.insert(
        "tokens".to_string(),
        json!(state.usage_input + state.usage_output),
    );
    if let Some(model) = state.model.clone().or_else(|| meta.model.clone()) {
        map.insert("model".to_string(), json!(model));
    }
    if let Some(thinking) = &meta.thinking {
        map.insert("thinking".to_string(), json!(thinking));
    }
    map.insert("inputTokens".to_string(), json!(state.usage_input));
    map.insert("outputTokens".to_string(), json!(state.usage_output));
    map.insert(
        "durationMs".to_string(),
        json!(now_ms.saturating_sub(meta.started_at_ms)),
    );
    if let Some(last_activity) = state.last_activity_at {
        map.insert("lastActivityAt".to_string(), json!(last_activity));
    }
    Value::Object(map)
}

/// `snapshotStreamResult` (execution.ts:272-284): the streaming `single`
/// result — `snapshotResult` spread (the terminal-details shape) minus the
/// unbounded `messages`, plus the bounded `toolCalls` summary and the
/// progress snapshot. The runtime `SingleResult` starts with
/// `exitCode: 0` and `outputState: "absent"` (execution.ts:406-407 — both
/// only move at terminal processing, so every streaming frame carries the
/// initial values); `timedOut` is NOT set while running (upstream only
/// assigns it on timeout, so JSON.stringify drops it from frames), and
/// error fields likewise stay absent.
pub fn snapshot_stream_result(
    state: &ChildRunState,
    meta: &StreamMeta,
    context: &str,
    progress: Value,
) -> Value {
    let mut map = Map::new();
    map.insert("index".to_string(), json!(meta.index));
    map.insert("agent".to_string(), json!(meta.agent));
    map.insert("task".to_string(), json!("[prompt redacted]"));
    map.insert("context".to_string(), json!(context));
    map.insert("exitCode".to_string(), json!(0));
    map.insert("outputState".to_string(), json!("absent"));
    map.insert("usage".to_string(), state.usage_json());
    if let Some(model) = state.model.clone().or_else(|| meta.model.clone()) {
        map.insert("model".to_string(), json!(model));
    }
    if let Some(tool_calls) = bound_streamed_tool_calls(&state.messages) {
        map.insert("toolCalls".to_string(), json!(tool_calls));
    }
    map.insert("progress".to_string(), progress);
    Value::Object(map)
}

/// The content text of one streaming frame (`fireUpdate`, execution.ts
/// :837-840): the latest final output, or `(running...)` before any.
pub fn streaming_update_text(state: &ChildRunState) -> String {
    let output = super::events::get_final_output(&state.messages);
    if output.is_empty() {
        "(running...)".to_string()
    } else {
        output
    }
}

/// One full `emitUpdateSnapshot` details payload (execution.ts:824-836):
/// `{mode:"single", results:[stream result], progress:[snapshot],
/// controlEvents}` — `controlEvents` is always empty here (the derivation
/// engine that fills it is out of scope; see the module docs).
pub fn single_update_details(
    state: &ChildRunState,
    meta: &StreamMeta,
    context: &str,
    now_ms: u64,
) -> (String, Value) {
    let progress = snapshot_progress(state, meta, now_ms);
    let result = snapshot_stream_result(state, meta, context, progress.clone());
    let details = json!({
        "mode": "single",
        "results": [result],
        "progress": [progress],
        "controlEvents": [],
    });
    (streaming_update_text(state), details)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::events::ChildRunState;

    fn meta() -> StreamMeta {
        StreamMeta {
            index: 0,
            agent: "scout".to_string(),
            skills: Some(vec!["repo-map".to_string()]),
            model: None,
            thinking: Some("low".to_string()),
            started_at_ms: 1_000,
        }
    }

    fn feed_tool_start(state: &mut ChildRunState, tool: &str, args: &Value) {
        state.process_line(
            &json!({
                "type": "tool_execution_start",
                "toolName": tool,
                "args": args,
            })
            .to_string(),
            1_200,
        );
    }

    fn feed_message_end(state: &mut ChildRunState, text: &str) {
        state.process_line(
            &json!({
                "type": "message_end",
                "message": {
                    "role": "assistant",
                    "content": [
                        {"type": "toolCall", "id": "t1", "name": "bash", "arguments": {"command": "cargo test"}},
                        {"type": "text", "text": text},
                    ],
                    "usage": {"input": 10, "output": 5, "cost": {"total": 0.1}},
                    "model": "faux/1",
                    "stopReason": "stop",
                },
            })
            .to_string(),
            1_300,
        );
    }

    #[test]
    fn tool_events_drive_current_tool_and_recent_tools() {
        let mut state = ChildRunState::default();
        feed_tool_start(&mut state, "read", &json!({ "path": "/tmp/a.rs" }));
        assert_eq!(state.current_tool.as_deref(), Some("read"));
        assert_eq!(state.current_path.as_deref(), Some("/tmp/a.rs"));
        assert_eq!(state.tool_count, 1);

        // tool_execution_end moves the entry into recentTools and clears
        // the current* fields (execution.ts:916-929).
        state.process_line(
            &json!({ "type": "tool_execution_end", "toolName": "read" }).to_string(),
            1_250,
        );
        assert_eq!(state.current_tool, None);
        assert_eq!(state.current_path, None);
        assert_eq!(state.recent_tools.len(), 1);
        assert_eq!(state.recent_tools[0].tool, "read");
        assert_eq!(state.recent_tools[0].end_ms, 1_250);
    }

    #[test]
    fn message_end_appends_bounded_recent_output() {
        let mut state = ChildRunState::default();
        feed_message_end(&mut state, "line one\nline two\n\nline three");
        assert_eq!(
            state.recent_output,
            vec!["line one", "line two", "line three"]
        );
        // Memory cap: 50 lines.
        let bulk: Vec<String> = (0..80).map(|i| format!("bulk {i}")).collect();
        append_recent_output(&mut state, &bulk);
        assert_eq!(state.recent_output.len(), RECENT_OUTPUT_MEMORY_LINES);
        assert_eq!(state.recent_output.last().unwrap(), "bulk 79");
    }

    #[test]
    fn progress_snapshot_shape_matches_upstream() {
        let mut state = ChildRunState::default();
        feed_tool_start(&mut state, "bash", &json!({ "command": "cargo test" }));
        feed_message_end(&mut state, "working on it");
        let (text, details) = single_update_details(&state, &meta(), "fresh", 2_000);
        assert_eq!(text, "working on it");
        assert_eq!(details["mode"], json!("single"));
        assert_eq!(details["controlEvents"], json!([]));
        let progress = &details["progress"][0];
        assert_eq!(progress["agent"], json!("scout"));
        assert_eq!(progress["status"], json!("running"));
        assert_eq!(progress["task"], json!("[prompt redacted]"));
        assert_eq!(progress["skills"], json!(["repo-map"]));
        assert_eq!(progress["currentTool"], json!("bash"));
        assert_eq!(progress["currentToolArgs"], json!("cargo test"));
        assert_eq!(progress["currentToolStartedAt"], json!(1200));
        assert_eq!(progress["durationMs"], json!(1000));
        assert_eq!(progress["lastActivityAt"], json!(1300));
        assert_eq!(progress["tokens"], json!(15));
        assert_eq!(progress["inputTokens"], json!(10));
        assert_eq!(progress["outputTokens"], json!(5));
        assert_eq!(progress["turnCount"], json!(1));
        assert_eq!(progress["toolCount"], json!(1));
        assert_eq!(progress["model"], json!("faux/1"));
        assert_eq!(progress["recentOutput"], json!(["working on it"]));
        // The streaming result carries the bounded toolCalls summary and no
        // messages (snapshotStreamResult drops them by construction).
        let result = &details["results"][0];
        assert!(result.get("messages").is_none());
        assert_eq!(result["toolCalls"][0]["text"], json!("$ cargo test"));
        assert_eq!(result["progress"], *progress);
        // Runtime SingleResult initials carried into every streaming frame
        // (execution.ts:406-407): exitCode 0 / outputState "absent" — both
        // only move at terminal processing. `timedOut` is NOT in the frame:
        // upstream assigns it only on timeout, undefined drops from JSON.
        assert_eq!(result["exitCode"], json!(0));
        assert_eq!(result["outputState"], json!("absent"));
        assert!(result.get("timedOut").is_none());
        // usage carries the emptyUsage() shape (execution.ts:112-114),
        // turns included.
        assert_eq!(
            result["usage"],
            json!({"input": 10, "output": 5, "cacheRead": 0, "cacheWrite": 0, "cost": 0.1, "turns": 1})
        );
    }

    #[test]
    fn empty_output_frame_says_running() {
        let state = ChildRunState::default();
        let (text, _) = single_update_details(&state, &meta(), "fresh", 1_500);
        assert_eq!(text, "(running...)");
    }

    #[test]
    fn bounded_windows_enforce_upstream_caps() {
        let mut state = ChildRunState::default();
        for i in 0..40 {
            state
                .recent_tools
                .push(crate::runner::events::RecentToolEntry {
                    tool: format!("tool{i}"),
                    args: String::new(),
                    end_ms: i,
                });
        }
        let bounded = bound_streamed_recent_tools(&state);
        assert_eq!(bounded.len(), MAX_STREAMED_RECENT_TOOLS);
        assert_eq!(bounded[0]["tool"], json!("tool8"));

        state.recent_output = vec![format!("{}", "x".repeat(2500))];
        let bounded = bound_streamed_recent_output(&state);
        assert!(bounded[0]
            .as_str()
            .unwrap()
            .ends_with("\u{2026} [truncated]"));
        assert_eq!(
            bounded[0].as_str().unwrap().encode_utf16().count(),
            MAX_STREAMED_OUTPUT_LINE_CHARS + "… [truncated]".encode_utf16().count()
        );
    }

    #[test]
    fn tool_calls_cap_at_64() {
        let mut messages = Vec::new();
        for _ in 0..70 {
            messages.push(json!({
                "role": "assistant",
                "content": [{"type": "toolCall", "id": "t", "name": "read", "arguments": {"path": "/x"}}],
            }));
        }
        let bounded = bound_streamed_tool_calls(&messages).unwrap();
        assert_eq!(bounded.len(), MAX_STREAMED_TOOL_CALLS);
        assert_eq!(bounded[0]["text"], json!("read /x"));
        assert_eq!(bound_streamed_tool_calls(&[]), None);
    }
}
