//! Completion-notification text and message renderers (TE09 FR-D).
//!
//! Port of pi-subagents `src/runs/background/notify.ts` (the wire shape of
//! `sendCompletion` :169-187 — `{customType:"subagent-notify", content,
//! display}`, details travel inside the content text, the renderer
//! re-parses) plus the three message renderers registered by
//! `src/extension/index.ts:481-539`: `subagent-notify` (:495-523),
//! `subagent_steering_notice` and `subagent_control_notice` (the constants
//! are underscore-named upstream, types via steering-notices.ts:4 /
//! control-notices.ts:5).
//!
//! The renderers return ComponentTree `column` nodes (v1 has no inline
//! spans, so each line is one styled text child — the icon/agent/status
//! title keeps upstream's icon and layout, per-line colors approximate the
//! inline ANSI mix).

use serde_json::{json, Value};

use crate::runner::display::{format_duration, shorten_path};

/// `SubagentNotifyDetails` (notify.ts:20-30) — the parsed projection of one
/// completion. `source` is always async in rpi (no detached foreground
/// face), so the foreground variant of `task_kind` is unreachable but kept
/// for parse parity.
#[derive(Debug, Clone, PartialEq)]
pub struct SubagentNotifyDetails {
    pub agent: String,
    pub status: &'static str,
    pub source: Option<&'static str>,
    pub task_info: Option<String>,
    pub result_preview: String,
    pub duration_ms: Option<u64>,
    pub handoff_path: Option<String>,
    pub session_label: Option<String>,
    pub session_value: Option<String>,
}

/// `formatSessionLine` (notify.ts:88-91).
fn format_session_line(details: &SubagentNotifyDetails) -> Option<String> {
    match (&details.session_label, &details.session_value) {
        (Some(label), Some(value)) => Some(format!("{label}: {value}")),
        (None, Some(value)) => Some(value.clone()),
        _ => None,
    }
}

/// `formatSingleCompletion` (notify.ts:93-107): the single-completion
/// content text. `taskKind` is "Background task" (async) or "Detached
/// foreground task" (:95).
pub fn format_single_completion(details: &SubagentNotifyDetails) -> String {
    let task_kind = if details.source == Some("foreground") {
        "Detached foreground task"
    } else {
        "Background task"
    };
    let mut lines: Vec<String> = vec![format!(
        "{task_kind} {}: **{}**{}",
        details.status,
        details.agent,
        details.task_info.clone().unwrap_or_default()
    )];
    lines.push(String::new());
    let preview = details.result_preview.trim();
    lines.push(if preview.is_empty() {
        "(no output)".to_string()
    } else {
        preview.to_string()
    });
    if details.handoff_path.is_some() {
        lines.push(String::new());
    }
    if let Some(handoff) = &details.handoff_path {
        lines.push(format!("Parallel handoff: {handoff}"));
    }
    if format_session_line(details).is_some() {
        lines.push(String::new());
    }
    if let Some(session) = format_session_line(details) {
        lines.push(session);
    }
    lines.join("\n")
}

/// `parseSubagentNotifyContent` (notify.ts:109-144): re-derive the details
/// from the content text. Returns None when the first line does not match
/// the completion header.
pub fn parse_subagent_notify_content(content: &str) -> Option<SubagentNotifyDetails> {
    let lines: Vec<&str> = content.split('\n').collect();
    let first = lines.first().copied().unwrap_or_default();

    // ^(Background task|Detached foreground task)
    //   (completed|failed|paused|stopped): \*\*(.+?)\*\*(?:\s+(\([^)]*\)))?$
    let foreground = first.strip_prefix("Detached foreground task ");
    let background = first.strip_prefix("Background task ");
    let (source, after_kind) = match (foreground, background) {
        (Some(rest), _) => (Some("foreground"), rest),
        (None, Some(rest)) => (None, rest),
        (None, None) => return None,
    };
    let (status_str, after_status) = after_kind.split_once(": ")?;
    let status: &'static str = match status_str {
        "completed" => "completed",
        "failed" => "failed",
        "paused" => "paused",
        "stopped" => "stopped",
        _ => return None,
    };
    let after_agent = after_status.strip_prefix("**")?;
    let agent_end = after_agent.find("**")?;
    let agent = after_agent[..agent_end].to_string();
    let tail = after_agent[agent_end + 2..].trim();
    let task_info = (!tail.is_empty()).then(|| tail.to_string());

    let body: Vec<&str> = lines.iter().skip(2).copied().collect();

    // Session line: last "Label: value" preceded by a blank line.
    let mut session_index: Option<usize> = None;
    if body.len() >= 2 {
        for i in (1..body.len()).rev() {
            if body[i - 1].trim().is_empty()
                && (body[i].starts_with("Session: ")
                    || body[i].starts_with("Session file: ")
                    || body[i].starts_with("Session share error: "))
            {
                session_index = Some(i);
                break;
            }
        }
    }
    let session_line = session_index.map(|i| body[i]);
    let handoff_index = body
        .iter()
        .position(|line| line.starts_with("Parallel handoff: "));

    // resultPreview: body up to the first metadata block (trailing blank
    // line before it dropped).
    let first_metadata = [session_index, handoff_index]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(body.len());
    let result_end = if first_metadata > 0
        && body
            .get(first_metadata - 1)
            .is_some_and(|l| l.trim().is_empty())
    {
        first_metadata - 1
    } else {
        first_metadata
    };
    let result_preview = body[..result_end].join("\n").trim().to_string();
    let result_preview = if result_preview.is_empty() {
        "(no output)".to_string()
    } else {
        result_preview
    };

    let handoff_path = handoff_index.map(|i| {
        body[i]
            .strip_prefix("Parallel handoff: ")
            .unwrap_or_default()
            .trim()
            .to_string()
    });
    let (session_label, session_value) = match session_line {
        Some(line) => match line.find(':') {
            Some(separator) => (
                Some(line[..separator].to_lowercase()),
                Some(line[separator + 1..].trim().to_string()),
            ),
            None => (None, Some(line.to_string())),
        },
        None => (None, None),
    };

    Some(SubagentNotifyDetails {
        agent,
        status,
        source,
        task_info,
        result_preview,
        duration_ms: None,
        handoff_path,
        session_label,
        session_value,
    })
}

/// `SubagentNotifyDetails` from a `message.details` payload (the injected
/// form; upstream `buildCompletionDetails` output shape).
fn details_from_message(message: &Value) -> Option<SubagentNotifyDetails> {
    let details = message.get("details")?;
    if !details.is_object() {
        return None;
    }
    Some(SubagentNotifyDetails {
        agent: details
            .get("agent")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        status: match details.get("status").and_then(Value::as_str) {
            Some("completed") => "completed",
            Some("paused") => "paused",
            Some("stopped") => "stopped",
            _ => "failed",
        },
        source: None,
        task_info: details
            .get("taskInfo")
            .and_then(Value::as_str)
            .map(str::to_string),
        result_preview: details
            .get("resultPreview")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        duration_ms: details.get("durationMs").and_then(Value::as_u64),
        handoff_path: details
            .get("handoffPath")
            .and_then(Value::as_str)
            .map(str::to_string),
        session_label: details
            .get("sessionLabel")
            .and_then(Value::as_str)
            .map(str::to_string),
        session_value: details
            .get("sessionValue")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

/// The `subagent-notify` renderer (extension/index.ts:495-523): icon by
/// status (✓ success / ■ warning / ✗ error), bold agent, dim status and
/// taskInfo/duration parts, `⎿`-indented preview (first line collapsed,
/// all lines expanded), Ctrl+O hint when collapsed, muted session line.
pub fn render_subagent_notify(message: &Value, options: &Value) -> Value {
    let content = message
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default();
    // `message.details ?? parse(content)` (extension/index.ts:499-500): a
    // details-carrying message renders with full fields (durationMs), the
    // text-only wire shape re-parses (no duration upstream either).
    let details = match details_from_message(message) {
        Some(details) => Some(details),
        None => parse_subagent_notify_content(content),
    };
    let Some(details) = details else {
        // Upstream falls back to the raw content text (Text(content)).
        return json!({ "type": "text", "props": { "text": content } });
    };
    let expanded = options.get("expanded").and_then(Value::as_bool) == Some(true);

    let icon = match details.status {
        "completed" => "✓",
        "paused" => "■",
        _ => "✗",
    };
    let icon_color = match details.status {
        "completed" => "success",
        "paused" => "warning",
        _ => "error",
    };
    let mut parts: Vec<String> = Vec::new();
    if let Some(task_info) = &details.task_info {
        parts.push(task_info.clone());
    }
    if let Some(duration) = details.duration_ms {
        parts.push(format_duration(duration));
    }
    let title = if parts.is_empty() {
        format!("{icon} {} {}", details.agent, details.status)
    } else {
        format!(
            "{icon} {} {} · {}",
            details.agent,
            details.status,
            parts.join(" · ")
        )
    };

    let mut children = vec![json!({
        "type": "text",
        "props": { "text": title, "fg": icon_color, "bold": true },
    })];

    let trimmed_preview = details.result_preview.trim();
    let preview_lines: Vec<&str> = if expanded {
        trimmed_preview
            .split('\n')
            .filter(|line| !line.trim().is_empty())
            .collect()
    } else {
        vec![trimmed_preview.split('\n').next().unwrap_or("")]
            .into_iter()
            .filter(|line| !line.trim().is_empty())
            .collect()
    };
    let preview_lines = if preview_lines.is_empty() {
        vec!["(no output)"]
    } else {
        preview_lines
    };
    let mut preview_text = String::new();
    for line in &preview_lines {
        if !preview_text.is_empty() {
            preview_text.push('\n');
        }
        preview_text.push_str(&format!("  ⎿  {line}"));
    }
    children.push(json!({
        "type": "text",
        "props": { "text": preview_text, "dim": true },
    }));

    if !expanded && trimmed_preview.contains('\n') {
        children.push(json!({
            "type": "text",
            "props": { "text": "  Ctrl+O full notification", "dim": true },
        }));
    }
    if let (Some(_), Some(value)) = (&details.session_label, &details.session_value) {
        children.push(json!({
            "type": "text",
            "props": {
                "text": format!("  {}: {}", details.session_label.clone().unwrap_or_default(), shorten_path(value)),
                "fg": "muted",
            },
        }));
    }
    json!({ "type": "column", "props": {}, "children": children })
}

/// The steering-notice renderer (extension/index.ts:525-529): a single
/// warning/error line from the notice text (`formatSteeringNotice` upstream;
/// rpi has no injector yet, the renderer honors any injected message).
pub fn render_subagent_steering(message: &Value) -> Value {
    let has_details = message.get("details").is_some_and(|d| !d.is_null());
    let content = message
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !has_details {
        return Value::Null;
    }
    let state = message
        .get("details")
        .and_then(|d| d.get("state"))
        .and_then(Value::as_str)
        .unwrap_or("failed");
    let color = if state == "recovered" {
        "warning"
    } else {
        "error"
    };
    json!({
        "type": "text",
        "props": { "text": content, "fg": color },
    })
}

/// The control-notice renderer (extension/index.ts:531-539): the control
/// notice text, single line (upstream wraps it in the bordered
/// SubagentControlNoticeComponent — approximated as plain text per the TE09
/// scope note).
pub fn render_subagent_control(message: &Value) -> Value {
    let details = message.get("details");
    let has_event = details
        .and_then(|d| d.get("event"))
        .is_some_and(|e| !e.is_null());
    if !has_event {
        return Value::Null;
    }
    let content = message.get("content").and_then(Value::as_str);
    let text = match content {
        Some(content) => content.to_string(),
        None => {
            let event = details.and_then(|d| d.get("event")).unwrap_or(&Value::Null);
            format!(
                "⚠ Subagent {}: {}",
                event
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("event")
                    .replace('_', " "),
                event
                    .get("agent")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            )
        }
    };
    json!({
        "type": "text",
        "props": { "text": text, "fg": "warning" },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn details(agent: &str, status: &'static str, preview: &str) -> SubagentNotifyDetails {
        SubagentNotifyDetails {
            agent: agent.to_string(),
            status,
            source: None,
            task_info: None,
            result_preview: preview.to_string(),
            duration_ms: Some(185_000),
            handoff_path: None,
            session_label: Some("Session file".to_string()),
            session_value: Some("/home/user/sessions/abc.jsonl".to_string()),
        }
    }

    #[test]
    fn single_completion_text_shape() {
        let text = format_single_completion(&details("scout", "completed", "all mapped"));
        assert_eq!(
            text,
            "Background task completed: **scout**\n\nall mapped\n\nSession file: /home/user/sessions/abc.jsonl"
        );
        // Empty preview → (no output); handoff rides before the session line.
        let mut d = details("scout", "failed", "  ");
        d.handoff_path = Some("/tmp/handoff.md".to_string());
        d.session_label = None;
        d.session_value = None;
        assert_eq!(
            format_single_completion(&d),
            "Background task failed: **scout**\n\n(no output)\n\nParallel handoff: /tmp/handoff.md"
        );
    }

    #[test]
    fn parse_round_trips_the_content_text() {
        let d = details("scout", "completed", "line one\nline two");
        let text = format_single_completion(&d);
        let parsed = parse_subagent_notify_content(&text).unwrap();
        assert_eq!(parsed.agent, "scout");
        assert_eq!(parsed.status, "completed");
        assert_eq!(parsed.result_preview, "line one\nline two");
        assert_eq!(parsed.session_label.as_deref(), Some("session file"));
        assert_eq!(
            parsed.session_value.as_deref(),
            Some("/home/user/sessions/abc.jsonl")
        );
        // Non-notify content → None.
        assert!(parse_subagent_notify_content("random text").is_none());
    }

    #[test]
    fn renderer_tree_shapes() {
        let d = details("scout", "completed", "line one\nline two");
        let message = json!({
            "customType": "subagent-notify",
            "content": format_single_completion(&d),
        });
        // Parsed wire shape carries no durationMs (upstream parse drops
        // it too); a details-carrying message renders with it.
        let collapsed = render_subagent_notify(&message, &json!({ "expanded": false }));
        let children = collapsed["children"].as_array().unwrap();
        assert_eq!(children[0]["props"]["text"], json!("✓ scout completed"));
        let with_details = render_subagent_notify(
            &json!({
                "content": "x",
                "details": {
                    "agent": "scout",
                    "status": "completed",
                    "resultPreview": "line one\nline two",
                    "durationMs": 185_000,
                }
            }),
            &json!({ "expanded": false }),
        );
        assert_eq!(
            with_details["children"][0]["props"]["text"],
            json!("✓ scout completed · 3m5s")
        );
        assert_eq!(children[0]["props"]["fg"], json!("success"));
        // Collapsed: first preview line + the expand hint.
        assert_eq!(children[1]["props"]["text"], json!("  ⎿  line one"));
        assert_eq!(
            children[2]["props"]["text"],
            json!("  Ctrl+O full notification")
        );
        let expanded = render_subagent_notify(&message, &json!({ "expanded": true }));
        let children = expanded["children"].as_array().unwrap();
        assert_eq!(
            children[1]["props"]["text"],
            json!("  ⎿  line one\n  ⎿  line two")
        );
        assert_eq!(children.len(), 3); // title, preview, session — no hint
    }

    #[test]
    fn renderer_status_icons() {
        for (status, icon, color) in [
            ("completed", "✓", "success"),
            ("paused", "■", "warning"),
            ("failed", "✗", "error"),
        ] {
            let d = details("a", status, "x");
            let message = json!({ "content": format_single_completion(&d) });
            let tree = render_subagent_notify(&message, &json!({ "expanded": true }));
            let text = tree["children"][0]["props"]["text"].as_str().unwrap();
            assert!(text.starts_with(icon), "{status}: {text}");
            assert_eq!(tree["children"][0]["props"]["fg"], json!(color));
        }
    }

    #[test]
    fn steering_and_control_renderers() {
        assert_eq!(
            render_subagent_steering(&json!({ "content": "notice" })),
            Value::Null
        );
        let tree = render_subagent_steering(&json!({
            "content": "steering failed",
            "details": { "state": "failed" },
        }));
        assert_eq!(tree["props"]["fg"], json!("error"));
        let tree = render_subagent_steering(&json!({
            "content": "recovered",
            "details": { "state": "recovered" },
        }));
        assert_eq!(tree["props"]["fg"], json!("warning"));
        assert_eq!(
            render_subagent_control(&json!({ "content": "x" })),
            Value::Null
        );
        let tree = render_subagent_control(&json!({
            "content": "control notice",
            "details": { "event": { "type": "needs_attention", "agent": "scout" } },
        }));
        assert_eq!(tree["props"]["text"], json!("control notice"));
    }
}
