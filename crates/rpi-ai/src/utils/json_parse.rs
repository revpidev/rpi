//! Port of `packages/ai/src/utils/json-parse.ts` @ pi 0.82.1 (2efa728),
//! plus an embedded port of the `partial-json` 0.1.7 npm package
//! (`node_modules/partial-json/dist/index.js`) used by `parseStreamingJson`.
//!
//! Intentional differences:
//! - `Infinity` / `-Infinity` / `NaN` literals (accepted by the JS package for
//!   JS-ism reasons) are treated as malformed: `serde_json::Value` cannot
//!   represent them and they never occur in tool-call argument streams.
//! - The parser operates on UTF-8 bytes; the JS version indexes UTF-16 code
//!   units. Every decision is an ASCII comparison, and multi-byte scalar
//!   values are opaque payload (they can never equal `"`, `\\` or a control
//!   byte), so behavior is identical while a delta no longer pays a
//!   `chars().collect::<Vec<char>>()` materialization (rpi#78 A3).
//! - Raw JSON text is pre-sanitized for unpaired `\uXXXX` surrogate escapes
//!   before `serde_json` parsing (JS `JSON.parse` accepts lone surrogates,
//!   `serde_json` rejects them). See [`strip_lone_surrogate_escapes`].

use serde_json::{Map, Value};

use std::time::{Duration, Instant};

const VALID_JSON_ESCAPES: [char; 8] = ['"', '\\', '/', 'b', 'f', 'n', 'r', 't'];

fn is_control_character(c: char) -> bool {
    (c as u32) <= 0x1f
}

fn escape_control_character(c: char) -> String {
    match c {
        '\u{08}' => "\\b".to_owned(),
        '\u{0c}' => "\\f".to_owned(),
        '\n' => "\\n".to_owned(),
        '\r' => "\\r".to_owned(),
        '\t' => "\\t".to_owned(),
        _ => format!("\\u{:04x}", c as u32),
    }
}

/// Byte-for-byte port of `repairJson`: escapes raw control characters inside
/// string literals and doubles backslashes before invalid escape characters.
///
/// The heavy caller (`parse_streaming_json_value`) runs this on every
/// streaming tool-args delta; the original always allocated a full output
/// copy even when the input was already clean (the common case while a
/// write/edit `content` string streams in). [`repair_json_if_needed`]
/// allocates only when a repair is actually required.
pub fn repair_json(json: &str) -> String {
    repair_json_if_needed(json).unwrap_or_else(|| json.to_owned())
}

/// Lazy [`repair_json`]: `None` when no repair is needed, `Some(repaired)`
/// otherwise. Output is byte-identical to [`repair_json`]. Until the first
/// repair the input is returned untouched (no allocation); from the first
/// repair onward every character flows into the materialized output.
fn repair_json_if_needed(json: &str) -> Option<String> {
    let mut out: Option<String> = None;
    let mut in_string = false;
    let mut index = 0;

    /// Emits `replacement` at `index`, materializing `out` with the untouched
    /// prefix `json[..index]` on the first repair. Every byte before `index`
    /// is a char boundary (the caller tracks chars, not bytes).
    fn repair(out: &mut Option<String>, json: &str, index: usize, replacement: &str) {
        if out.is_none() {
            let mut materialized = String::with_capacity(json.len());
            materialized.push_str(&json[..index]);
            *out = Some(materialized);
        }
        out.as_mut()
            .expect("materialized above")
            .push_str(replacement);
    }

    /// `=` in JS terms: the JSON escape character `c`.
    fn is_valid_escape(c: char) -> bool {
        VALID_JSON_ESCAPES.contains(&c)
    }

    while index < json.len() {
        let char = json[index..]
            .chars()
            .next()
            .expect("index < len is a char boundary");
        let char_len = char.len_utf8();

        if !in_string {
            if let Some(out) = out.as_mut() {
                out.push(char);
            }
            if char == '"' {
                in_string = true;
            }
            index += char_len;
            continue;
        }

        if char == '"' {
            in_string = false;
            if let Some(out) = out.as_mut() {
                out.push(char);
            }
            index += char_len;
            continue;
        }

        if char == '\\' {
            let next_char = json[index + 1..].chars().next();
            match next_char {
                None => {
                    repair(&mut out, json, index, "\\\\");
                    index += 1;
                    continue;
                }
                Some('u') => {
                    // Exactly four ASCII hex digits after `\u` (a truncated or
                    // non-hex tail is not a valid escape and falls through).
                    let hex = json
                        .get(index + 2..index + 6)
                        .filter(|digits| digits.bytes().all(|b| b.is_ascii_hexdigit()));
                    if let Some(digits) = hex {
                        if let Some(out) = out.as_mut() {
                            out.push_str("\\u");
                            out.push_str(digits);
                        }
                        index += 6;
                        continue;
                    }
                    // fall through to the generic escape check ('u' is valid)
                }
                _ => {}
            }

            let next_char = next_char.expect("None returned above");
            if is_valid_escape(next_char) {
                // Valid escape passes through untouched.
                if let Some(out) = out.as_mut() {
                    out.push('\\');
                    out.push(next_char);
                }
                index += 1 + next_char.len_utf8();
                continue;
            }

            repair(&mut out, json, index, "\\\\");
            index += 1;
            continue;
        }

        if is_control_character(char) {
            let escaped = escape_control_character(char);
            repair(&mut out, json, index, &escaped);
        } else if let Some(out) = out.as_mut() {
            out.push(char);
        }
        index += char_len;
    }

    out
}

/// Removes unpaired `\uXXXX` surrogate escapes (a high surrogate `\uD800`–
/// `\uDBFF` not immediately followed by a low-surrogate escape, or a low
/// surrogate `\uDC00`–`\uDFFF` not immediately preceded by one) from raw JSON
/// text, mirroring the net effect of JS `JSON.parse` (which accepts lone
/// surrogates) followed by `sanitizeSurrogates` (which drops them).
///
/// Only escape sequences are inspected; raw UTF-8 text cannot contain
/// surrogates. Paired escapes (valid astral characters) are preserved.
pub fn strip_lone_surrogate_escapes(json: &str) -> String {
    strip_lone_surrogate_escapes_if_needed(json).unwrap_or_else(|| json.to_owned())
}

/// Lazy [`strip_lone_surrogate_escapes`]: `None` when nothing had to be
/// dropped, `Some(stripped)` otherwise.
///
/// The streaming hot path calls this on EVERY failed strict parse — i.e. on
/// every partial JSON fragment — so the scan must not materialize a copy of
/// the (growing) input unless an escape is actually dropped (rpi#78 A3).
fn strip_lone_surrogate_escapes_if_needed(json: &str) -> Option<String> {
    let mut out: Option<String> = None;
    let mut in_string = false;
    let mut index = 0;
    // Input offset just past the last EMITTED high-surrogate escape. A low
    // surrogate is kept only when it immediately follows one (paired escape).
    let mut last_kept_high_end: Option<usize> = None;

    /// Emits `text` only once a drop has materialized the output (before the
    /// first drop the input is kept verbatim, so there is nothing to push).
    fn emit(out: &mut Option<String>, text: &str) {
        if let Some(buffer) = out.as_mut() {
            buffer.push_str(text);
        }
    }

    /// Drops the escape at `at`: materializes the untouched prefix the first
    /// time, which is what makes the scan lazy.
    fn drop_at(out: &mut Option<String>, json: &str, at: usize) {
        if out.is_none() {
            let mut buffer = String::with_capacity(json.len());
            buffer.push_str(&json[..at]);
            *out = Some(buffer);
        }
    }

    // The four hex digits after a `\u` escape at `slash` (a byte offset).
    fn hex4(json: &str, slash: usize) -> Option<u16> {
        let digits = json
            .get(slash + 2..slash + 6)
            .filter(|digits| digits.len() == 4 && digits.bytes().all(|b| b.is_ascii_hexdigit()))?;
        u16::from_str_radix(digits, 16).ok()
    }

    while index < json.len() {
        let c = json[index..]
            .chars()
            .next()
            .expect("index < len is a char boundary");
        let char_len = c.len_utf8();
        if !in_string {
            emit(&mut out, &json[index..index + char_len]);
            if c == '"' {
                in_string = true;
            }
            index += char_len;
            last_kept_high_end = None;
            continue;
        }
        if c == '\\'
            && json[index + 1..].starts_with('u')
            && let Some(code) = hex4(json, index)
        {
            let escape = &json[index..index + 6];
            if (0xd800..0xdc00).contains(&code) {
                // High surrogate: keep only when immediately followed
                // by a low-surrogate escape.
                let next_slash = index + 6;
                let paired = json[next_slash..].starts_with("\\u")
                    && hex4(json, next_slash)
                        .map(|low| (0xdc00..0xe000).contains(&low))
                        .unwrap_or(false);
                if paired {
                    emit(&mut out, escape);
                    last_kept_high_end = Some(next_slash);
                } else {
                    drop_at(&mut out, json, index);
                    last_kept_high_end = None;
                }
                index += 6;
                continue;
            }
            if (0xdc00..0xe000).contains(&code) {
                // Low surrogate: keep only when immediately preceded
                // by an emitted high-surrogate escape.
                if last_kept_high_end == Some(index) {
                    emit(&mut out, escape);
                } else {
                    drop_at(&mut out, json, index);
                }
                last_kept_high_end = None;
                index += 6;
                continue;
            }
            emit(&mut out, escape);
            last_kept_high_end = None;
            index += 6;
            continue;
        }
        if c == '\\' {
            // Non-`\u` escape: emit both characters so an escaped quote does
            // not end the string literal (e.g. `\"` or `\\`).
            let next_len = json[index + 1..].chars().next().map_or(0, char::len_utf8);
            let end = index + 1 + next_len;
            emit(&mut out, &json[index..end]);
            index = end;
            last_kept_high_end = None;
            continue;
        }
        emit(&mut out, &json[index..index + char_len]);
        if c == '"' {
            in_string = false;
        }
        index += char_len;
        last_kept_high_end = None;
    }

    out
}

/// Parses JSON text, tolerating lone-surrogate escapes like JS `JSON.parse`.
fn json_parse_lenient(text: &str) -> Result<Value, serde_json::Error> {
    match serde_json::from_str(text) {
        Ok(value) => Ok(value),
        Err(first) => match strip_lone_surrogate_escapes_if_needed(text) {
            // A materialized strip differs from the input by construction; its
            // parse result stands. No lone surrogate (the streaming common
            // case: a truncated fragment) keeps the first error without an
            // alloc-and-compare round trip.
            Some(stripped) => serde_json::from_str(&stripped),
            None => Err(first),
        },
    }
}

/// Port of `parseJsonWithRepair`: strict parse, then one repair retry.
pub fn parse_json_with_repair(json: &str) -> Result<Value, serde_json::Error> {
    match json_parse_lenient(json) {
        Ok(value) => Ok(value),
        Err(first) => match repair_json_if_needed(json) {
            // A materialized repair differs from the input by construction;
            // its parse result stands. No repair needed (streaming-truncated
            // but otherwise clean JSON) keeps the first error without an
            // alloc-and-compare round trip.
            Some(repaired) => json_parse_lenient(&repaired),
            None => Err(first),
        },
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedToolCallArguments {
    pub arguments: Map<String, Value>,
    pub arguments_error: Option<String>,
}

/// `parseToolCallArguments` (`87f9a3641`): complete arguments never use the
/// streaming parser's prefix recovery. Repairs are limited to string escapes;
/// malformed or non-object input becomes `{}` plus a validation diagnostic.
pub fn parse_tool_call_arguments(json: Option<&str>) -> ParsedToolCallArguments {
    let Some(json) = json.filter(|json| !json.trim().is_empty()) else {
        return ParsedToolCallArguments {
            arguments: Map::new(),
            arguments_error: None,
        };
    };
    match parse_json_with_repair(json) {
        Ok(Value::Object(arguments)) => ParsedToolCallArguments {
            arguments,
            arguments_error: None,
        },
        Ok(_) => ParsedToolCallArguments {
            arguments: Map::new(),
            arguments_error: Some(format!(
                "Tool arguments must be a JSON object\n\nReceived arguments:\n{json}"
            )),
        },
        Err(error) => ParsedToolCallArguments {
            arguments: Map::new(),
            arguments_error: Some(format!("{error}\n\nReceived arguments:\n{json}")),
        },
    }
}

// ---------------------------------------------------------------------------
// partial-json 0.1.7 port (Allow.ALL)
// ---------------------------------------------------------------------------

/// Minimal recursive-descent partial JSON parser mirroring `partial-json`
/// with `Allow.ALL`: every construct may be truncated. Returns `None` where
/// the JS package throws (`PartialJSON` cannot occur with Allow.ALL at the
/// top level except through malformed input).
struct PartialParser<'a> {
    text: &'a str,
    index: usize,
}

impl PartialParser<'_> {
    fn len(&self) -> usize {
        self.text.len()
    }

    /// The byte at `index`. ASCII-only comparisons make continuation bytes
    /// inert payload, so a byte cursor is behaviorally identical to the
    /// original char cursor (rpi#78 A3).
    fn at(&self, index: usize) -> Option<u8> {
        self.text.as_bytes().get(index).copied()
    }

    /// JS `String.prototype.substring(from, to)`: clamps then swaps.
    /// Every caller passes char boundaries (quote/backslash/`e`/EOF offsets).
    fn slice(&self, from: usize, to: usize) -> &str {
        let len = self.len();
        let a = from.min(len);
        let b = to.min(len);
        let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
        &self.text[lo..hi]
    }

    fn remaining(&self) -> &str {
        &self.text[self.index..]
    }

    fn last_index_of(&self, needle: char) -> Option<usize> {
        debug_assert!(needle.is_ascii());
        self.text
            .as_bytes()
            .iter()
            .rposition(|&b| b == needle as u8)
    }

    fn skip_blank(&mut self) {
        while let Some(c) = self.at(self.index) {
            if matches!(c, b' ' | b'\n' | b'\r' | b'\t') {
                self.index += 1;
            } else {
                break;
            }
        }
    }

    fn parse_any(&mut self) -> Option<Value> {
        self.skip_blank();
        if self.index >= self.len() {
            return None; // PartialJSON "Unexpected end of input"
        }
        let c = self.at(self.index)?;
        match c {
            b'"' => self.parse_str(),
            b'{' => self.parse_obj(),
            b'[' => self.parse_arr(),
            _ => {
                let remaining = self.remaining();
                for (literal, value) in [
                    ("null", Value::Null),
                    ("true", Value::Bool(true)),
                    ("false", Value::Bool(false)),
                ] {
                    if remaining.starts_with(literal) {
                        self.index += literal.len();
                        return Some(value);
                    }
                    // Allow partial literal prefixes (Allow.NULL / Allow.BOOL).
                    if remaining.len() < literal.len() && literal.starts_with(remaining) {
                        self.index += literal.len();
                        return Some(value);
                    }
                }
                self.parse_num()
            }
        }
    }

    fn parse_str(&mut self) -> Option<Value> {
        let start = self.index;
        let mut escape = false;
        self.index += 1; // skip initial quote
        while self.index < self.len()
            && (self.at(self.index) != Some(b'"')
                || (escape && self.at(self.index.wrapping_sub(1)) == Some(b'\\')))
        {
            escape = self.at(self.index) == Some(b'\\') && !escape;
            self.index += 1;
        }
        if self.at(self.index) == Some(b'"') {
            self.index += 1;
            let end = self.index - usize::from(escape);
            return json_parse_lenient(self.slice(start, end)).ok();
        }
        // Allow.STR: unterminated strings are completed with a closing quote.
        let end = self.index - usize::from(escape);
        let candidate = format!("{}\"", self.slice(start, end));
        if let Ok(value) = json_parse_lenient(&candidate) {
            return Some(value);
        }
        // Invalid escape sequence fallback: cut at the last backslash.
        let last_backslash = self.last_index_of('\\')?;
        let candidate = format!("{}\"", self.slice(start, last_backslash));
        json_parse_lenient(&candidate).ok()
    }

    fn parse_obj(&mut self) -> Option<Value> {
        self.index += 1; // skip initial brace
        self.skip_blank();
        let mut obj = Map::new();
        loop {
            if self.at(self.index) == Some(b'}') {
                break;
            }
            self.skip_blank();
            if self.index >= self.len() {
                return Some(Value::Object(obj)); // Allow.OBJ
            }
            // Key-parse errors are caught by the object-level Allow.OBJ.
            let key = match self.parse_str() {
                Some(Value::String(key)) => key,
                _ => return Some(Value::Object(obj)),
            };
            self.skip_blank();
            self.index += 1; // skip colon (unchecked upstream)
            match self.parse_any() {
                Some(value) => {
                    obj.insert(key, value);
                }
                None => return Some(Value::Object(obj)), // Allow.OBJ
            }
            self.skip_blank();
            if self.at(self.index) == Some(b',') {
                self.index += 1;
            }
        }
        self.index += 1; // skip final brace
        Some(Value::Object(obj))
    }

    fn parse_arr(&mut self) -> Option<Value> {
        self.index += 1; // skip initial bracket
        let mut arr = Vec::new();
        loop {
            if self.at(self.index) == Some(b']') {
                break;
            }
            match self.parse_any() {
                Some(value) => arr.push(value),
                None => return Some(Value::Array(arr)), // Allow.ARR
            }
            self.skip_blank();
            if self.at(self.index) == Some(b',') {
                self.index += 1;
            }
        }
        self.index += 1; // skip final bracket
        Some(Value::Array(arr))
    }

    fn parse_num(&mut self) -> Option<Value> {
        if self.index == 0 {
            // Whole-input number (parse() called on a bare numeric string).
            if self.remaining() == "-" {
                return None; // MalformedJSON "Not sure what '-' is"
            }
            if let Ok(value) = json_parse_lenient(self.remaining()) {
                return Some(value);
            }
            // Allow.NUM: cut at the last 'e' (partial exponent).
            let last_e = self.last_index_of('e')?;
            return json_parse_lenient(self.slice(0, last_e)).ok();
        }

        let start = self.index;
        if self.at(self.index) == Some(b'-') {
            self.index += 1;
        }
        while let Some(c) = self.at(self.index) {
            if matches!(c, b',' | b']' | b'}') {
                break;
            }
            self.index += 1;
        }
        // index == len is fine under Allow.NUM.
        let slice = self.slice(start, self.index);
        if let Ok(value) = json_parse_lenient(slice) {
            return Some(value);
        }
        if slice == "-" {
            return None; // PartialJSON "Not sure what '-' is"
        }
        let last_e = self.last_index_of('e')?;
        json_parse_lenient(self.slice(start, last_e)).ok()
    }
}

/// Port of `partial-json`'s `parse` with `Allow.ALL`.
fn partial_parse(partial_json: &str) -> Option<Value> {
    let trimmed = partial_json.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut parser = PartialParser {
        text: trimmed,
        index: 0,
    };
    parser.parse_any()
}

/// Port of `parseStreamingJson`: parses potentially incomplete JSON during
/// streaming. Always returns an object (`{}` when nothing parses), like the
/// upstream default generic `Record<string, unknown>`.
pub fn parse_streaming_json(partial_json: Option<&str>) -> Map<String, Value> {
    match parse_streaming_json_value(partial_json) {
        Value::Object(map) => map,
        // Upstream assigns whatever partial-json returns (even a bare string);
        // `ToolCall.arguments` is an object in rpi, so non-object partials
        // degrade to `{}` (transient partial display only).
        _ => Map::new(),
    }
}

/// [`parse_streaming_json`] variant preserving non-object results.
pub fn parse_streaming_json_value(partial_json: Option<&str>) -> Value {
    let Some(text) = partial_json else {
        return Value::Object(Map::new());
    };
    if text.trim().is_empty() {
        return Value::Object(Map::new());
    }
    if let Ok(value) = parse_json_with_repair(text) {
        return value;
    }
    if let Some(value) = partial_parse(text) {
        return value;
    }
    if let Some(value) = partial_parse(&repair_json(text)) {
        return value;
    }
    Value::Object(Map::new())
}

/// Refresh cadence for the streamed tool-call argument PREVIEW (rpi#78 A3).
///
/// `parse_streaming_json` re-parses the whole accumulated argument text, so a
/// per-delta call is O(args) work per delta and O(args²) per tool call. The
/// preview only has to keep up with the renderer (a 16 ms frame tick), and the
/// authoritative arguments always come from [`parse_tool_call_arguments`] when
/// the block closes, so adapters may skip intermediate refreshes.
pub const STREAMING_ARGS_PREVIEW_INTERVAL: Duration = Duration::from_millis(33);

/// Argument payloads up to this size are parsed on every delta: the parse is
/// cheaper than the bookkeeping and keeps the preview's onset exact.
const STREAMING_ARGS_PREVIEW_ALWAYS_BYTES: usize = 2048;

/// Per-tool-call preview-parse throttle (see
/// [`STREAMING_ARGS_PREVIEW_INTERVAL`]).
#[derive(Debug, Default)]
pub struct StreamingArgsPreview {
    last: Option<Instant>,
}

impl StreamingArgsPreview {
    /// Whether this delta should refresh `block.arguments` for an accumulated
    /// argument text of `args_len` bytes.
    pub fn should_parse(&mut self, args_len: usize) -> bool {
        if args_len <= STREAMING_ARGS_PREVIEW_ALWAYS_BYTES {
            // Small payloads refresh unconditionally and do not arm the
            // throttle, so the first large delta after a small run still
            // refreshes immediately.
            return true;
        }
        let now = Instant::now();
        match self.last {
            Some(last) if now.duration_since(last) < STREAMING_ARGS_PREVIEW_INTERVAL => false,
            _ => {
                self.last = Some(now);
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    fn parse(text: &str) -> Value {
        parse_streaming_json_value(Some(text))
    }

    /// The lazy repair must be byte-identical to the eager original on both
    /// clean inputs (no allocation, `None`) and dirty inputs.
    #[test]
    fn test_lazy_repair_matches_eager_output() {
        let clean = r#"{"path":"src/main.rs","content":"fn main() {}\n"}"#;
        assert_eq!(
            repair_json_if_needed(clean),
            None,
            "clean input needs no repair"
        );
        assert_eq!(repair_json(clean), clean.to_owned());

        let dirty_inputs = [
            // Raw control characters inside strings.
            "{\"a\":\"line1\nline2\t\"}",
            "{\"a\":\"x\u{0c}y\"}",
            // Control char before any repair (prefix must be preserved).
            "{\"key\":\"value\",\"other\":\"z\rz\"}",
            // Invalid escape doubled; valid escapes untouched.
            "{\"a\":\"c:\\x\"}",
            "{\"a\":\"c:\\n\\t\"}",
            // Trailing lone backslash.
            "{\"a\":\"c:\\\"",
            // Valid \uXXXX kept; invalid hex after \u doubled.
            "{\"a\":\"\\u00e9\"}",
            "{\"a\":\"\\uZZZZ\"}",
            // Escapes outside strings are untouched.
            "{\"a\":\"b\"}",
        ];
        for dirty in dirty_inputs {
            let eager = eager_repair_reference(dirty);
            match repair_json_if_needed(dirty) {
                None => assert_eq!(
                    eager,
                    dirty.to_owned(),
                    "no lazy repair ⇒ eager output must equal the input for {dirty:?}"
                ),
                Some(lazy) => assert_eq!(
                    lazy, eager,
                    "lazy repair must match the eager original for {dirty:?}"
                ),
            }
            assert_eq!(repair_json(dirty), eager);
        }
    }

    /// The pre-lazy eager implementation, kept as the behavioral reference
    /// (always builds the full output).
    fn eager_repair_reference(json: &str) -> String {
        let chars: Vec<char> = json.chars().collect();
        let mut repaired = String::new();
        let mut in_string = false;
        let mut index = 0;
        while index < chars.len() {
            let char = chars[index];
            if !in_string {
                repaired.push(char);
                if char == '"' {
                    in_string = true;
                }
                index += 1;
                continue;
            }
            if char == '"' {
                repaired.push(char);
                in_string = false;
                index += 1;
                continue;
            }
            if char == '\\' {
                let next_char = chars.get(index + 1).copied();
                match next_char {
                    None => {
                        repaired.push_str("\\\\");
                        index += 1;
                        continue;
                    }
                    Some('u') => {
                        let unicode_digits: String = chars.iter().skip(index + 2).take(4).collect();
                        if unicode_digits.len() == 4
                            && unicode_digits.chars().all(|c| c.is_ascii_hexdigit())
                        {
                            repaired.push_str("\\u");
                            repaired.push_str(&unicode_digits);
                            index += 6;
                            continue;
                        }
                    }
                    _ => {}
                }
                let next_char = next_char.unwrap();
                if VALID_JSON_ESCAPES.contains(&next_char) {
                    repaired.push('\\');
                    repaired.push(next_char);
                    index += 2;
                    continue;
                }
                repaired.push_str("\\\\");
                index += 1;
                continue;
            }
            if is_control_character(char) {
                repaired.push_str(&escape_control_character(char));
            } else {
                repaired.push(char);
            }
            index += 1;
        }
        repaired
    }

    #[test]
    fn test_repair_json_escapes_control_characters() {
        let input = "{\"a\":\"line1\nline2\t\"}";
        let repaired = repair_json(input);
        assert_eq!(repaired, "{\"a\":\"line1\\nline2\\t\"}");
        let value = parse_json_with_repair(input).expect("repaired parses");
        assert_eq!(value, json!({"a": "line1\nline2\t"}));
    }

    #[test]
    fn test_repair_json_doubles_invalid_backslash() {
        // `\x` is not a valid JSON escape: the backslash is doubled.
        let input = "{\"a\":\"c:\\x\"}";
        assert!(serde_json::from_str::<Value>(input).is_err());
        let value = parse_json_with_repair(input).expect("repaired parses");
        assert_eq!(value, json!({"a": "c:\\x"}));
    }

    #[test]
    fn test_repair_json_preserves_valid_escapes() {
        let input = "{\"a\":\"\\n\\u00e9\"}";
        assert_eq!(repair_json(input), input);
    }

    #[test]
    fn test_parse_streaming_json_empty_inputs() {
        assert_eq!(parse_streaming_json(None), Map::new());
        assert_eq!(parse_streaming_json(Some("")), Map::new());
        assert_eq!(parse_streaming_json(Some("   ")), Map::new());
    }

    #[test]
    fn test_parse_streaming_json_complete() {
        assert_eq!(parse("{\"a\":1}"), json!({"a": 1}));
    }

    #[test]
    fn test_parse_streaming_json_partial_any_split() {
        // Arbitrary splits of a tool-call payload must parse to something
        // object-shaped; the full string must match the strict parse.
        let full = "{\"command\":\"ls -la\",\"timeout\":30,\"nested\":{\"x\":[1,2,3]}}";
        let value: Value = serde_json::from_str(full).expect("valid");
        for split in 0..=full.len() {
            if !full.is_char_boundary(split) {
                continue;
            }
            let partial = parse(&full[..split]);
            if split == full.len() {
                assert_eq!(partial, value);
            }
        }
        assert_eq!(parse("{\"command\":\"ls -l"), json!({"command": "ls -l"}));
        assert_eq!(parse("{\"command\":"), json!({}));
        assert_eq!(
            parse("{\"command\":\"ls -la\",\"timeout\":3"),
            json!({"command": "ls -la", "timeout": 3})
        );
        assert_eq!(
            parse("{\"nested\":{\"x\":[1,2"),
            json!({"nested": {"x": [1, 2]}})
        );
    }

    #[test]
    fn test_parse_streaming_json_partial_literals() {
        assert_eq!(parse("{\"a\":tru"), json!({"a": true}));
        assert_eq!(parse("{\"a\":nul"), json!({"a": null}));
        assert_eq!(parse("{\"a\":fals"), json!({"a": false}));
        assert_eq!(parse("{\"a\":1e"), json!({"a": 1}));
    }

    #[test]
    fn test_parse_streaming_json_trailing_backslash() {
        assert_eq!(parse("{\"a\":\"abc\\"), json!({"a": "abc"}));
        assert_eq!(parse("{\"a\":\"abc"), json!({"a": "abc"}));
    }

    #[test]
    fn test_parse_streaming_json_malformed_returns_empty() {
        assert_eq!(parse("not json at all"), json!({}));
        assert_eq!(parse("{\"a\":@}"), json!({}));
        assert_eq!(parse_streaming_json(Some("not json at all")), Map::new());
    }

    #[test]
    fn final_tool_arguments_require_complete_json_objects() {
        let valid = parse_tool_call_arguments(Some(r#"{"kind":"seek","note":"\url"}"#));
        assert_eq!(valid.arguments["note"], json!("\\url"));
        assert_eq!(valid.arguments_error, None);
        assert_eq!(parse_tool_call_arguments(None).arguments, Map::new());
        assert_eq!(parse_tool_call_arguments(Some(" ")).arguments_error, None);

        for raw in [
            r#"{"action":"apply","actions":[{"kind":"seek","time":.5}]}"#,
            r#"{"kind":"seek","clipId":"clip-fl"#,
        ] {
            let parsed = parse_tool_call_arguments(Some(raw));
            assert!(parsed.arguments.is_empty(), "{raw}");
            assert!(
                parsed.arguments_error.as_deref().is_some_and(|error| error
                    .contains("Received arguments:\n")
                    && error.ends_with(raw)),
                "{parsed:?}"
            );
        }
        let parsed = parse_tool_call_arguments(Some("[1,2]"));
        assert!(parsed.arguments.is_empty());
        assert_eq!(
            parsed.arguments_error.as_deref(),
            Some("Tool arguments must be a JSON object\n\nReceived arguments:\n[1,2]")
        );
    }

    #[test]
    fn test_strip_lone_surrogate_escapes() {
        // Paired surrogate escapes are preserved.
        let paired = "{\"a\":\"\\ud83d\\ude00\"}";
        assert_eq!(strip_lone_surrogate_escapes(paired), paired);
        // Lone high / low surrogates are dropped.
        assert_eq!(
            strip_lone_surrogate_escapes("{\"a\":\"x\\ud83dy\"}"),
            "{\"a\":\"xy\"}"
        );
        assert_eq!(
            strip_lone_surrogate_escapes("{\"a\":\"x\\ude00y\"}"),
            "{\"a\":\"xy\"}"
        );
        // High surrogate followed by a non-escape character: dropped.
        assert_eq!(
            strip_lone_surrogate_escapes("{\"a\":\"\\ud83dx\"}"),
            "{\"a\":\"x\"}"
        );
        let value = parse_json_with_repair("{\"a\":\"x\\ud83dy\"}").expect("lenient parse");
        assert_eq!(value, json!({"a": "xy"}));
    }

    /// rpi#78 A3: the lazy strip must report `None` (no allocation) when the
    /// input has no lone surrogate escape — the streaming hot path.
    #[test]
    fn strip_is_lazy_when_nothing_is_dropped() {
        assert_eq!(
            strip_lone_surrogate_escapes_if_needed("{\"a\":\"b\"}"),
            None
        );
        let paired = "{\"a\":\"\\ud83d\\ude00\"}";
        assert_eq!(strip_lone_surrogate_escapes_if_needed(paired), None);
        assert!(strip_lone_surrogate_escapes_if_needed("{\"a\":\"x\\ud83dy\"}").is_some());
    }

    /// The byte-cursor partial parser must behave exactly like the previous
    /// char-cursor one for multi-byte payloads: no panic at any char boundary,
    /// and the decoded `content` grows monotonically (the write-preview
    /// highlight cache relies on the prefix property — rpi#78 A4).
    #[test]
    fn partial_parse_byte_cursor_is_boundary_safe_and_monotonic() {
        let content = "fn main() {\n    let s = \"hi\\there\";\n\tprintln!(\"a\\nb\");\n}\n// 中文注释 🚀\n/* x */\n";
        let full = json!({"path": "a.rs", "content": content}).to_string();
        let mut last = String::new();
        let mut saw_cjk = false;
        for (index, _) in full.char_indices() {
            let prefix = &full[..index];
            let map = parse_streaming_json(Some(prefix));
            if let Some(decoded) = map.get("content").and_then(Value::as_str) {
                assert!(
                    decoded.starts_with(&last),
                    "prefix property broke at {index}: {last:?} -> {decoded:?}"
                );
                saw_cjk |= decoded.contains('中') || decoded.contains('🚀');
                last = decoded.to_owned();
            }
        }
        assert_eq!(last, content);
        assert!(saw_cjk, "the multi-byte payload must have been exercised");
    }

    /// rpi#78 A3: small argument payloads parse every delta; large ones are
    /// throttled to the display cadence (the authoritative arguments come from
    /// `parse_tool_call_arguments` at block end).
    #[test]
    fn streaming_args_preview_throttles_large_payloads() {
        let mut preview = StreamingArgsPreview::default();
        for _ in 0..10 {
            assert!(preview.should_parse(64), "small payloads always refresh");
        }
        assert!(
            preview.should_parse(4096),
            "the first large delta refreshes"
        );
        assert!(!preview.should_parse(8192), "immediate re-parse is skipped");
        std::thread::sleep(STREAMING_ARGS_PREVIEW_INTERVAL + std::time::Duration::from_millis(5));
        assert!(preview.should_parse(8192), "the next window refreshes");
    }
}
