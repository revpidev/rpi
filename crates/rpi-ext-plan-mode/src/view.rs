//! Rendering surfaces: the Plan-mode editor hint lines and the
//! `write_plan` transcript renderers (ComponentTree v1 — the declarative
//! schema the host maps onto rpi-tui components).
//!
//! The renderers are theme-free by design: a single bold text row for the
//! call and a plain row for the result keep the frames deterministic and
//! independent of the active theme (G14: zero existing-frame changes; the
//! new frames are pinned by the unit tests below).

use serde_json::{Value, json};

use crate::i18n;

/// The editor hint line shown while Plan mode is active.
pub fn hint_lines() -> Vec<String> {
    vec![i18n::HINT_PLAN_MODE.to_owned()]
}

/// Human-sized byte count for the transcript rows (`1234` → `1.2 KiB`).
fn human_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let kib = bytes as f64 / 1024.0;
    if kib < 1024.0 {
        return format!("{kib:.1} KiB");
    }
    format!("{:.1} MiB", kib / 1024.0)
}

/// `renderCall` for `write_plan`: `✎ write_plan · 1.2 KiB plan`.
pub fn render_call(context: &Value) -> Value {
    let content = context
        .get("args")
        .and_then(|args| args.get("content"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    text_node(
        &format!("✎ write_plan · {} plan", human_bytes(content.len() as u64)),
        true,
    )
}

/// `renderResult` for `write_plan`: path + size + review outcome.
pub fn render_result(result: &Value) -> Value {
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        return text_node("✎ write_plan · failed", false);
    }
    let details = result.get("details").cloned().unwrap_or(Value::Null);
    let path = details
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or("plan file");
    let bytes = details.get("bytes").and_then(Value::as_u64);
    let outcome = details
        .get("outcome")
        .and_then(Value::as_str)
        .unwrap_or("saved");
    match bytes {
        Some(bytes) => text_node(
            &format!("✎ {path} · {} · {outcome}", human_bytes(bytes)),
            false,
        ),
        None => text_node(&format!("✎ {path} · {outcome}"), false),
    }
}

fn text_node(text: &str, bold: bool) -> Value {
    json!({
        "type": "text",
        "props": {
            "text": text,
            "bold": bold,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hint_is_a_single_plan_mode_line() {
        let lines = hint_lines();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("plan mode"));
    }

    #[test]
    fn call_renders_a_size_row() {
        let context = json!({"args": {"content": "x".repeat(1536)}});
        let node = render_call(&context);
        assert_eq!(node["type"], "text");
        assert_eq!(node["props"]["text"], "✎ write_plan · 1.5 KiB plan");
        assert_eq!(node["props"]["bold"], true);
    }

    #[test]
    fn call_tolerates_missing_args() {
        assert_eq!(
            render_call(&json!({}))["props"]["text"],
            "✎ write_plan · 0 B plan"
        );
    }

    #[test]
    fn result_renders_path_size_and_outcome() {
        let node = render_result(&json!({
            "details": {"path": "/w/.rpi/plans/s-1.md", "bytes": 2048, "outcome": "approved"},
        }));
        assert_eq!(
            node["props"]["text"],
            "✎ /w/.rpi/plans/s-1.md · 2.0 KiB · approved"
        );
    }

    #[test]
    fn error_result_renders_failure() {
        let node = render_result(&json!({"isError": true}));
        assert_eq!(node["props"]["text"], "✎ write_plan · failed");
    }

    #[test]
    fn human_bytes_boundaries() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(1024 * 1024), "1.0 MiB");
    }
}
