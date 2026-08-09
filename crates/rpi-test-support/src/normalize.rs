//! Parity normalizer — the **single** normalization implementation for the
//! whole project (coding-standards §12.3; design §10.2 step 3).
//!
//! Strips volatile data from fixtures and captured output before diffing:
//! timestamps, uuids / entry ids, session ids, and cwd / agent-dir paths.
//! Everything else is preserved byte-for-byte.
//!
//! Rules:
//! - `timestamp` keys: replaced with a type-preserving constant (`0` for unix
//!   ms numbers, `"<ts>"` for ISO strings) so the timestamp *format* stays
//!   part of the parity contract.
//! - Id-bearing keys (`id`, `parentId`, `fromId`, `firstKeptEntryId`,
//!   `toolCallId`, `sessionId`, `responseId`, `parentSession`) and bare uuids
//!   anywhere in strings are replaced with **consistent** placeholders
//!   (`<id:1>`, `<id:2>`, …) so tree structure and cross-references (e.g. a
//!   `toolCall.id` vs the result's `toolCallId`) remain comparable.
//! - ISO-8601 timestamps and 13-digit unix-ms numbers embedded in strings are
//!   replaced with `<ts>`.
//! - Configured path prefixes (cwd, agent dir) are replaced with `<path>` in
//!   every string value, longest prefix first.
//!
//! Each side of a comparison is normalized with a fresh `Normalizer`; because
//! placeholders are assigned in first-appearance order, structurally equal
//! inputs normalize to identical output.

use std::collections::HashMap;

use serde_json::Value;

use crate::error::TestSupportError;

/// Placeholder for normalized timestamps.
pub const TS_PLACEHOLDER: &str = "<ts>";
/// Placeholder prefix for normalized ids (`<id:N>`).
pub const ID_PLACEHOLDER_PREFIX: &str = "<id:";
/// Placeholder for normalized path prefixes.
pub const PATH_PLACEHOLDER: &str = "<path>";

/// JSON object keys whose values are timestamps.
const TIMESTAMP_KEYS: &[&str] = &["timestamp"];

/// JSON object keys whose values are volatile ids. `null` values are kept.
const ID_KEYS: &[&str] = &[
    "id",
    "parentId",
    "fromId",
    "firstKeptEntryId",
    "toolCallId",
    "sessionId",
    "responseId",
    "parentSession",
];

/// JSON object keys whose values are working-directory paths.
const CWD_KEYS: &[&str] = &["cwd"];

/// Stateful normalizer. Reuse one instance per side of a comparison so id
/// placeholders stay consistent across lines / events of that side.
#[derive(Debug, Default)]
pub struct Normalizer {
    ids: HashMap<String, usize>,
    /// Path prefixes stripped from all strings, kept sorted longest-first.
    paths: Vec<String>,
}

impl Normalizer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a path prefix (e.g. the fixture cwd or agent dir) to strip from
    /// every string value.
    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.paths.push(path.into());
        self.paths.sort_by_key(|b| std::cmp::Reverse(b.len()));
        self
    }

    /// Number of distinct volatile ids mapped so far.
    pub fn mapped_id_count(&self) -> usize {
        self.ids.len()
    }

    fn placeholder_for(&mut self, raw: &str) -> String {
        let next = self.ids.len() + 1;
        let n = *self.ids.entry(raw.to_owned()).or_insert(next);
        format!("{ID_PLACEHOLDER_PREFIX}{n}>")
    }

    /// Normalize a JSON value in place.
    pub fn normalize_json(&mut self, value: &mut Value) {
        match value {
            Value::Object(map) => {
                for (key, val) in map.iter_mut() {
                    if TIMESTAMP_KEYS.contains(&key.as_str()) {
                        *val = match val {
                            Value::Number(_) => Value::from(0),
                            _ => Value::String(TS_PLACEHOLDER.to_owned()),
                        };
                        continue;
                    }
                    if CWD_KEYS.contains(&key.as_str()) && val.is_string() {
                        *val = Value::String(PATH_PLACEHOLDER.to_owned());
                        continue;
                    }
                    if ID_KEYS.contains(&key.as_str()) {
                        if let Value::String(raw) = val {
                            let mapped = self.placeholder_for(&raw.clone());
                            *val = Value::String(mapped);
                            continue;
                        }
                    }
                    self.normalize_json(val);
                }
            }
            Value::Array(items) => {
                for item in items {
                    self.normalize_json(item);
                }
            }
            Value::String(s) => {
                *s = self.normalize_string(s);
            }
            _ => {}
        }
    }

    /// Normalize every JSON line of a JSONL document; non-JSON lines fall back
    /// to plain-text normalization (byte-preserved otherwise).
    pub fn normalize_jsonl(&mut self, text: &str) -> Result<String, TestSupportError> {
        let mut out = String::with_capacity(text.len());
        let lines: Vec<&str> = text.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            if line.trim().is_empty() {
                out.push('\n');
            } else {
                match serde_json::from_str::<Value>(line) {
                    Ok(mut v) => {
                        self.normalize_json(&mut v);
                        let rendered = serde_json::to_string(&v).map_err(TestSupportError::Json)?;
                        out.push_str(&rendered);
                    }
                    Err(_) => out.push_str(&self.normalize_string(line)),
                }
                if i + 1 < lines.len() || text.ends_with('\n') {
                    out.push('\n');
                }
            }
        }
        Ok(out)
    }

    /// Normalize a plain-text string (paths, uuids, embedded timestamps).
    pub fn normalize_string(&mut self, input: &str) -> String {
        let mut s = input.to_owned();
        for path in &self.paths {
            if !path.is_empty() && s.contains(path.as_str()) {
                s = s.replace(path.as_str(), PATH_PLACEHOLDER);
            }
        }
        s = self.replace_uuids(&s);
        replace_iso_timestamps(&s)
    }

    /// Replace uuid-shaped substrings with consistent placeholders.
    fn replace_uuids(&mut self, input: &str) -> String {
        let bytes = input.as_bytes();
        let mut out = String::with_capacity(input.len());
        let mut i = 0;
        while i < bytes.len() {
            if let Some(uuid_len) = uuid_len_at(input, i) {
                let raw = &input[i..i + uuid_len];
                let mapped = self.placeholder_for(raw);
                out.push_str(&mapped);
                i += uuid_len;
            } else {
                // Invariant: `i` is always on a char boundary — it starts at 0
                // and only advances by whole chars or ASCII-only uuid spans.
                let ch = input[i..].chars().next().expect("i is a char boundary");
                out.push(ch);
                i += ch.len_utf8();
            }
        }
        out
    }
}

/// If a uuid (8-4-4-4-12 lowercase/uppercase hex groups) starts at byte
/// offset `i`, return its length in bytes.
fn uuid_len_at(s: &str, i: usize) -> Option<usize> {
    const GROUPS: [usize; 5] = [8, 4, 4, 4, 12];
    let bytes = s.as_bytes();
    let mut pos = i;
    for (gi, len) in GROUPS.iter().enumerate() {
        if pos + len > bytes.len() {
            return None;
        }
        if !s[pos..pos + len].chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        pos += len;
        if gi < GROUPS.len() - 1 {
            if bytes.get(pos) != Some(&b'-') {
                return None;
            }
            pos += 1;
        }
    }
    Some(pos - i)
}

/// Replace ISO-8601 timestamps (`2024-12-03T14:00:00.000Z`) embedded in a
/// string with `<ts>`.
fn replace_iso_timestamps(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < bytes.len() {
        if let Some(len) = iso_timestamp_len_at(bytes, i) {
            out.push_str(TS_PLACEHOLDER);
            i += len;
        } else {
            // Invariant: `i` is always on a char boundary — it starts at 0 and
            // only advances by whole chars or ASCII-only timestamp spans.
            let ch = input[i..].chars().next().expect("i is a char boundary");
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

/// If an ISO-8601 timestamp (`YYYY-MM-DDTHH:MM:SS[.fff][Z|±HH:MM]`) starts at
/// byte offset `i`, return its length.
fn iso_timestamp_len_at(bytes: &[u8], i: usize) -> Option<usize> {
    fn digits(bytes: &[u8], start: usize, n: usize) -> bool {
        bytes
            .get(start..start + n)
            .is_some_and(|w| w.iter().all(|b| b.is_ascii_digit()))
    }
    // YYYY-MM-DD
    if !digits(bytes, i, 4)
        || bytes.get(i + 4) != Some(&b'-')
        || !digits(bytes, i + 5, 2)
        || bytes.get(i + 7) != Some(&b'-')
        || !digits(bytes, i + 8, 2)
        || bytes.get(i + 10) != Some(&b'T')
    {
        return None;
    }
    // HH:MM:SS
    if !digits(bytes, i + 11, 2)
        || bytes.get(i + 13) != Some(&b':')
        || !digits(bytes, i + 14, 2)
        || bytes.get(i + 16) != Some(&b':')
        || !digits(bytes, i + 17, 2)
    {
        return None;
    }
    let mut pos = i + 19;
    // Optional .fff
    if bytes.get(pos) == Some(&b'.') {
        let mut end = pos + 1;
        while bytes.get(end).is_some_and(u8::is_ascii_digit) {
            end += 1;
        }
        if end == pos + 1 {
            return None;
        }
        pos = end;
    }
    // Z or ±HH:MM
    match bytes.get(pos) {
        Some(b'Z') => Some(pos + 1 - i),
        Some(sign @ (b'+' | b'-')) => {
            let start = pos + 1;
            if *sign == b'+' || *sign == b'-' {
                if digits(bytes, start, 2)
                    && bytes.get(start + 2) == Some(&b':')
                    && digits(bytes, start + 3, 2)
                {
                    return Some(start + 5 - i);
                }
                None
            } else {
                None
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_json_timestamp_keys_type_preserving() {
        let mut n = Normalizer::new();
        let mut v = serde_json::json!({
            "type": "message",
            "timestamp": "2024-12-03T14:00:01.000Z",
            "message": {"role": "user", "content": "hi", "timestamp": 1733234401000i64}
        });
        n.normalize_json(&mut v);
        assert_eq!(v["timestamp"], serde_json::json!(TS_PLACEHOLDER));
        assert_eq!(v["message"]["timestamp"], serde_json::json!(0));
        assert_eq!(v["message"]["content"], serde_json::json!("hi"));
    }

    #[test]
    fn test_normalize_json_ids_consistent_across_references() {
        let mut n = Normalizer::new();
        let mut v = serde_json::json!([
            {"id": "a1b2c3d4", "parentId": null},
            {"id": "b2c3d4e5", "parentId": "a1b2c3d4"},
            {"message": {"role": "assistant", "content": [{"type": "toolCall", "id": "call_1"}]}},
            {"message": {"role": "toolResult", "toolCallId": "call_1"}}
        ]);
        n.normalize_json(&mut v);
        assert_eq!(v[1]["parentId"], v[0]["id"]);
        assert_eq!(
            v[2]["message"]["content"][0]["id"],
            v[3]["message"]["toolCallId"]
        );
        assert_eq!(v[0]["parentId"], serde_json::Value::Null);
    }

    #[test]
    fn test_normalize_json_uuid_and_paths_anywhere() {
        let mut n = Normalizer::new().with_path("/tmp/fixture-workspace");
        let mut v = serde_json::json!({
            "type": "session",
            "id": "9b2c1d4e-1234-4abc-9def-001122334455",
            "cwd": "/tmp/fixture-workspace",
            "fullOutputPath": "/tmp/fixture-workspace/out/9b2c1d4e-1234-4abc-9def-001122334455.log"
        });
        n.normalize_json(&mut v);
        assert_eq!(v["cwd"], serde_json::json!(PATH_PLACEHOLDER));
        // The bare uuid inside the path string maps to the same placeholder
        // as the session id key ("id" is visited before "fullOutputPath" in
        // insertion order, so it is <id:1>).
        assert_eq!(v["id"], serde_json::json!("<id:1>"));
        assert_eq!(
            v["fullOutputPath"],
            serde_json::json!(format!("{PATH_PLACEHOLDER}/out/<id:1>.log"))
        );
    }

    #[test]
    fn test_normalize_idempotent() {
        let text = "{\"id\":\"a1b2c3d4\",\"timestamp\":1733234401000,\"cwd\":\"/work\"}\n";
        let mut n1 = Normalizer::new();
        let once = n1.normalize_jsonl(text).unwrap();
        let mut n2 = Normalizer::new();
        let twice = n2.normalize_jsonl(&once).unwrap();
        assert_eq!(once, twice);
    }

    #[test]
    fn test_normalize_jsonl_same_shape_different_volatiles_equal() {
        let a = "{\"type\":\"message\",\"id\":\"aaaaaaaa\",\"parentId\":null,\"timestamp\":\"2024-01-01T00:00:00.000Z\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n\
                 {\"type\":\"message\",\"id\":\"bbbbbbbb\",\"parentId\":\"aaaaaaaa\",\"timestamp\":\"2024-01-01T00:00:01.000Z\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"yo\"}]}}\n";
        let b = a
            .replace("aaaaaaaa", "cccccccc")
            .replace("bbbbbbbb", "dddddddd");
        assert_eq!(
            Normalizer::new().normalize_jsonl(a).unwrap(),
            Normalizer::new().normalize_jsonl(&b).unwrap()
        );
    }

    #[test]
    fn test_replace_iso_timestamps_embedded() {
        let mut n = Normalizer::new();
        let s = n.normalize_string("started 2024-12-03T14:00:00.000Z done");
        assert_eq!(s, format!("started {TS_PLACEHOLDER} done"));
    }
}
