//! Diff helpers for parity comparison (design §10.2 step 5; coding-standards
//! §12.3). The single diff implementation for the whole project; all tasks
//! compare fixtures through these functions.
//!
//! Comparisons are always performed on **normalized** content (fresh
//! [`Normalizer`] per side). Assertions cover:
//! - event type sequences (`diff_event_sequence`),
//! - session JSONL structure **including line order** (`diff_jsonl`),
//! - generic transcripts / text (`diff_text`).

use rpi_ai::types::StreamEvent;
use serde_json::Value;

use crate::error::TestSupportError;
use crate::normalize::Normalizer;

/// Number of context lines included around the first difference in reports.
const CONTEXT_LINES: usize = 2;

/// A located difference between expected and actual content.
#[derive(Debug)]
pub struct DiffFailure {
    /// 1-based line number of the first difference (or line-count mismatch).
    pub line: usize,
    /// Human-readable report with context lines.
    pub report: String,
}

impl std::fmt::Display for DiffFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.report)
    }
}

impl std::error::Error for DiffFailure {}

fn line_at(lines: &[&str], idx: usize) -> String {
    lines
        .get(idx)
        .map_or("<missing>".to_owned(), |l| (*l).to_owned())
}

fn first_difference(expected: &str, actual: &str, what: &str) -> Option<DiffFailure> {
    let e: Vec<&str> = expected.lines().collect();
    let a: Vec<&str> = actual.lines().collect();
    let common = e.len().min(a.len());
    for i in 0..common {
        if e[i] != a[i] {
            let mut report = format!("{what} differs at line {}:\n", i + 1);
            let from = i.saturating_sub(CONTEXT_LINES);
            let to = (i + CONTEXT_LINES + 1).min(e.len().max(a.len()));
            for j in from..to {
                let marker = if j == i { ">>" } else { "  " };
                report.push_str(&format!(
                    "{marker} {:>4} expected: {}\n{marker} {:>4}   actual: {}\n",
                    j + 1,
                    line_at(&e, j),
                    j + 1,
                    line_at(&a, j),
                ));
            }
            return Some(DiffFailure {
                line: i + 1,
                report,
            });
        }
    }
    if e.len() != a.len() {
        let i = common;
        let report = format!(
            "{what} line count differs (expected {}, actual {}) at line {}:\n>> {:>4} expected: {}\n>> {:>4}   actual: {}\n",
            e.len(),
            a.len(),
            i + 1,
            i + 1,
            line_at(&e, i),
            i + 1,
            line_at(&a, i),
        );
        return Some(DiffFailure {
            line: i + 1,
            report,
        });
    }
    None
}

/// Compare two plain texts after normalization (each side with a fresh
/// normalizer). Returns the first located difference, if any.
pub fn diff_text(expected: &str, actual: &str) -> Result<(), DiffFailure> {
    let e = Normalizer::new().normalize_string(expected);
    let a = Normalizer::new().normalize_string(actual);
    match first_difference(&e, &a, "text") {
        Some(f) => Err(f),
        None => Ok(()),
    }
}

/// Compare two JSONL documents after normalization. Each line is parsed as
/// JSON and re-serialized before comparison, so differences in insignificant
/// whitespace are ignored while **line order** and structure must match.
pub fn diff_jsonl(expected: &str, actual: &str) -> Result<(), DiffFailure> {
    let e = Normalizer::new()
        .normalize_jsonl(expected)
        .map_err(|err| DiffFailure {
            line: 0,
            report: format!("expected side failed to normalize: {err}"),
        })?;
    let a = Normalizer::new()
        .normalize_jsonl(actual)
        .map_err(|err| DiffFailure {
            line: 0,
            report: format!("actual side failed to normalize: {err}"),
        })?;
    match first_difference(&e, &a, "JSONL") {
        Some(f) => Err(f),
        None => Ok(()),
    }
}

/// Extract the upstream event type tag of a [`StreamEvent`] (e.g. `start`,
/// `text_delta`, `toolcall_end`, `done`, `error`).
pub fn event_type_name(event: &StreamEvent) -> &'static str {
    match event {
        StreamEvent::Start { .. } => "start",
        StreamEvent::TextStart { .. } => "text_start",
        StreamEvent::TextDelta { .. } => "text_delta",
        StreamEvent::TextEnd { .. } => "text_end",
        StreamEvent::ThinkingStart { .. } => "thinking_start",
        StreamEvent::ThinkingDelta { .. } => "thinking_delta",
        StreamEvent::ThinkingEnd { .. } => "thinking_end",
        StreamEvent::ToolCallStart { .. } => "toolcall_start",
        StreamEvent::ToolCallDelta { .. } => "toolcall_delta",
        StreamEvent::ToolCallEnd { .. } => "toolcall_end",
        StreamEvent::Done { .. } => "done",
        StreamEvent::Error { .. } => "error",
    }
}

/// Compare two event streams by their **event type sequence** (design §10.2
/// step 5: the event-type sequence must match).
pub fn diff_event_sequence(
    expected: &[StreamEvent],
    actual: &[StreamEvent],
) -> Result<(), DiffFailure> {
    let render = |events: &[StreamEvent]| -> String {
        events
            .iter()
            .map(event_type_name)
            .collect::<Vec<_>>()
            .join("\n")
    };
    match first_difference(&render(expected), &render(actual), "event sequence") {
        Some(f) => Err(f),
        None => Ok(()),
    }
}

/// Compare two event streams fully: serialized JSON per event, normalized
/// (strips timestamps / ids embedded in partials), line order enforced.
pub fn diff_events_normalized(
    expected: &[StreamEvent],
    actual: &[StreamEvent],
) -> Result<(), DiffFailure> {
    let render = |events: &[StreamEvent]| -> Result<String, TestSupportError> {
        let mut s = String::new();
        for event in events {
            let v: Value = serde_json::to_value(event)?;
            s.push_str(&serde_json::to_string(&v)?);
            s.push('\n');
        }
        Ok(s)
    };
    let e = render(expected).map_err(|err| DiffFailure {
        line: 0,
        report: format!("failed to serialize expected events: {err}"),
    })?;
    let a = render(actual).map_err(|err| DiffFailure {
        line: 0,
        report: format!("failed to serialize actual events: {err}"),
    })?;
    diff_jsonl(&e, &a)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::faux::faux_assistant_message;

    #[test]
    fn test_diff_text_equal_after_normalization() {
        let a = "session 9b2c1d4e-1234-4abc-9def-001122334455 at 2024-12-03T14:00:00.000Z\nok\n";
        let b = "session 11111111-2222-3333-4444-555555555555 at 2025-01-01T00:00:00.000Z\nok\n";
        diff_text(a, b).unwrap();
    }

    #[test]
    fn test_diff_text_locates_difference() {
        let err = diff_text("one\ntwo\nthree\n", "one\nTWO\nthree\n").unwrap_err();
        assert_eq!(err.line, 2);
        assert!(err.report.contains("expected: two"));
        assert!(err.report.contains("actual: TWO"));
    }

    #[test]
    fn test_diff_text_line_count_mismatch() {
        let err = diff_text("one\ntwo\n", "one\n").unwrap_err();
        assert_eq!(err.line, 2);
        assert!(err.report.contains("line count differs"));
    }

    #[test]
    fn test_diff_jsonl_line_order_matters() {
        let a = "{\"type\":\"a\",\"v\":1}\n{\"type\":\"b\",\"v\":2}\n";
        let swapped = "{\"type\":\"b\",\"v\":2}\n{\"type\":\"a\",\"v\":1}\n";
        let err = diff_jsonl(a, swapped).unwrap_err();
        assert_eq!(err.line, 1);
    }

    #[test]
    fn test_diff_jsonl_whitespace_insensitive() {
        diff_jsonl("{ \"a\": 1 }\n", "{\"a\":1}\n").unwrap();
    }

    #[test]
    fn test_diff_event_sequence_equal() {
        let msg = faux_assistant_message("hi", Default::default());
        let events = vec![
            StreamEvent::Start {
                partial: msg.clone(),
            },
            StreamEvent::Done {
                reason: rpi_ai::types::DoneReason::Stop,
                message: msg,
            },
        ];
        diff_event_sequence(&events, &events.clone()).unwrap();
    }

    #[test]
    fn test_diff_event_sequence_reports_mismatch() {
        let msg = faux_assistant_message("hi", Default::default());
        let expected = vec![StreamEvent::Start {
            partial: msg.clone(),
        }];
        let actual = vec![StreamEvent::Done {
            reason: rpi_ai::types::DoneReason::Stop,
            message: msg,
        }];
        let err = diff_event_sequence(&expected, &actual).unwrap_err();
        assert_eq!(err.line, 1);
        assert!(err.report.contains("expected: start"));
        assert!(err.report.contains("actual: done"));
    }
}
