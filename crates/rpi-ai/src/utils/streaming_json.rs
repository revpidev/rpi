//! Port of `packages/ai/src/utils/streaming-json.ts` @ pi v1.5.2
//! (`b4afdddf`; upstream commit `f715a15c`, fixes #9265).
//!
//! Incremental, tolerant parsing of JSON text that arrives in pieces, such as
//! streamed tool-call arguments. [`StreamingJsonParser::append`] reads each
//! piece once, so a stream costs time linear in its length, and
//! [`StreamingJsonParser::snapshot`] returns the value of the text read so far.
//! Snapshots are plain [`serde_json::Value`]s and never change once returned.
//!
//! The rules for incomplete text follow [`parse_streaming_json`]:
//!
//! - An unterminated string shows the characters read so far; an unterminated
//!   escape is held back until complete.
//! - An unterminated container shows the members and elements read so far.
//! - A member whose key or value has not started is omitted.
//! - An unterminated literal shows its value once it is a prefix of `true`,
//!   `false` or `null`; an unterminated number shows its value when the digits
//!   read so far form a number (an exponent without digits is ignored).
//!
//! Malformed text is read leniently, the way `repairJson` and
//! `parseStreamingJson` treat it: raw control characters in strings are kept,
//! an invalid escape such as `\d` stays literal text, missing commas and
//! colons are tolerated, values in invalid positions and unknown words are
//! dropped, and text after the root value is ignored. The parser never throws.
//! Callers that need the exact value of the finished text parse it once, e.g.
//! with [`parse_tool_call_arguments`].
//!
//! Intentional Rust differences:
//!
//! - The parser is byte-oriented; every decision is an ASCII comparison, so
//!   multi-byte UTF-8 scalar values are opaque payload (they can never equal
//!   `"` or `\\`).
//! - Rust strings cannot represent lone UTF-16 surrogates. A paired
//!   `\uD800\uDC00` escape is joined into the astral scalar; a lone surrogate
//!   is dropped, matching [`strip_lone_surrogate_escapes`] and the existing
//!   preview oracle.
//! - `NaN` and `Infinity` words are dropped: `serde_json::Value` cannot
//!   represent them and they never occur in tool-call argument streams. A
//!   number beyond the `f64` range (such as `1e400`) is dropped for the same
//!   reason where upstream shows `null`; the authoritative final parse rejects
//!   it with an error instead of accepting `null`.
//!
//! Cost: copying the open path costs its member and element count. Each
//! `append` earns copy credit proportional to its length plus a constant;
//! `snapshot` spends it and returns the previous snapshot while the open path
//! costs more than the credit. A document that keeps a very large container
//! open therefore shows a preview that lags a little behind the text, and no
//! input makes the total work grow faster than the text.
//!
//! `append` also returns the changes the piece made, for a writer that keeps
//! its own copy of the value and applies them instead of re-assigning
//! snapshots: `Set` when a key, an array item or a finished value appears (a
//! new string is set once with its first text, a container empty), `Append`
//! when the piece extended an open string. Unterminated numbers and literals
//! are not reported until they end. A container nested more than 32 levels
//! deep is reported as one `Set` of its whole value when it closes, so paths
//! stay short.

use serde_json::{Map, Value};

use crate::utils::json_parse::{ParsedToolCallArguments, parse_tool_call_arguments};

/// Where a value sits in its parent; absent for a value in an invalid
/// position, which is read but dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Slot {
    Root,
    Member(String),
    Element(usize),
}

/// A path from the root value: member keys and array indices.
pub type JsonPath = Vec<JsonPathSegment>;

/// One path segment of a [`JsonChange`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JsonPathSegment {
    Key(String),
    Index(usize),
}

/// One change a piece of text made to the value; see the module comment.
#[derive(Debug, Clone, PartialEq)]
pub enum JsonChange {
    Set { path: JsonPath, value: Value },
    Append { path: JsonPath, text: String },
}

/// An open object or array. Its `container` is never handed out while open;
/// snapshots get copies.
#[derive(Debug)]
struct Frame {
    container: Value,
    slot: Option<Slot>,
    /// Where the container sits, when its changes are reported.
    path: Option<JsonPath>,
    /// Whether changes inside it are reported; when not, its whole value is
    /// reported as it closes.
    reports_inside: bool,
    /// Members or elements; what a copy of the container costs.
    size: usize,
    /// Objects: the key read last, where the member value goes.
    key: Option<String>,
    phase: Phase,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Key,
    Colon,
    Value,
    Comma,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StringTarget {
    Key,
    Value,
}

/// An unterminated string.
#[derive(Debug)]
struct OpenString {
    target: StringTarget,
    slot: Option<Slot>,
    text: String,
    /// Escape read so far: empty for none, otherwise a backslash and up to five
    /// more characters.
    escape: String,
    /// Where a value string sits, when its changes are reported.
    path: Option<JsonPath>,
    /// The change this string extends, while it is the newest change of the
    /// piece.
    change: Option<usize>,
    /// High surrogate from a `\uXXXX` escape whose low half has not arrived.
    pending_high: Option<u32>,
}

// Changes are reported at most this deep, so a path costs O(1) to build.
const MAX_REPORTED_DEPTH: usize = 32;

// Copy credit per append and per appended character. The floor keeps small
// documents fresh at every snapshot.
const CREDIT_PER_APPEND: i64 = 256;
const CREDIT_PER_CHARACTER: i64 = 8;
// An unterminated number longer than this is not shown until it ends, so
// `snapshot()` stays cheap.
const MAX_PENDING_SCALAR_LENGTH: usize = 64;

/// Incremental, tolerant JSON parser; see the module comment for its rules and
/// costs.
#[derive(Debug)]
pub struct StreamingJsonParser {
    root: Option<Value>,
    done: bool,
    stack: Vec<Frame>,
    string: Option<OpenString>,
    scalar: String,
    scalar_slot: Option<Slot>,
    /// Sum of `size + 1` over the open path: what a fresh snapshot copies.
    path_cost: usize,
    credit: i64,
    dirty: bool,
    snapshot: Option<Value>,
    /// Changes of the piece being read.
    changes: Vec<JsonChange>,
    /// Bytes handed to [`Self::append`]; a test seam for the O(N) invariant.
    scanned_bytes: u64,
}

impl Default for StreamingJsonParser {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamingJsonParser {
    pub fn new() -> Self {
        Self {
            root: None,
            done: false,
            stack: Vec::new(),
            string: None,
            scalar: String::new(),
            scalar_slot: None,
            path_cost: 0,
            credit: 0,
            dirty: false,
            snapshot: None,
            changes: Vec::new(),
            scanned_bytes: 0,
        }
    }

    /// Total bytes consumed by [`Self::append`]. A test seam for proving that
    /// N bytes split into N pieces are scanned once.
    pub fn scanned_bytes(&self) -> u64 {
        self.scanned_bytes
    }

    /// Read the next piece of text; returns the changes it made, in order.
    /// Costs time linear in `text.len()`.
    pub fn append(&mut self, text: &str) -> Vec<JsonChange> {
        self.credit += CREDIT_PER_APPEND + CREDIT_PER_CHARACTER * text.len() as i64;
        self.changes = Vec::new();
        if let Some(open) = self.string.as_mut() {
            open.change = None;
        }
        let mut index = 0;
        while index < text.len() && !self.done {
            let before = index;
            if self.string.is_some() {
                index = self.read_string(text, index);
            } else if !self.scalar.is_empty() {
                index = self.read_scalar(text, index);
            } else {
                let ch = text[index..]
                    .chars()
                    .next()
                    .expect("index < len is a char boundary");
                self.structural(ch);
                index += ch.len_utf8();
            }
            self.scanned_bytes += (index - before) as u64;
        }
        std::mem::take(&mut self.changes)
    }

    /// The value of the text read so far: `None` before a value starts,
    /// otherwise a plain JSON value that never changes afterwards.
    ///
    /// Returns the previous snapshot while the text read since did not change
    /// the value, or while a fresh copy of the open path is not yet paid for
    /// (see the module comment). Callers that need the exact value of the
    /// finished text parse it once with [`parse_tool_call_arguments`].
    pub fn snapshot(&mut self) -> (Option<&Value>, bool) {
        if !self.dirty {
            return (self.snapshot.as_ref(), false);
        }
        let pending = self.pending_scalar();
        let cost = self.path_cost as i64 + i64::from(pending.is_some());
        if cost > self.credit {
            return (self.snapshot.as_ref(), false);
        }
        self.credit -= cost;
        self.dirty = false;
        self.snapshot = self.copy_open_path(pending);
        (self.snapshot.as_ref(), true)
    }

    /// Read string characters from `start` until the string ends or the text
    /// does; returns the next index.
    fn read_string(&mut self, text: &str, start: usize) -> usize {
        let mut run_start = start;
        let mut index = start;
        while index < text.len() {
            let byte = text.as_bytes()[index];
            let escape_active = self
                .string
                .as_ref()
                .is_some_and(|open| !open.escape.is_empty());
            if escape_active {
                let ch = text[index..]
                    .chars()
                    .next()
                    .expect("index < len is a char boundary");
                if self.continue_escape(ch) {
                    index += ch.len_utf8();
                    run_start = index;
                    continue;
                }
                // Not part of the escape: read it as a normal string character
                // below.
                run_start = index;
            }
            if byte == b'"' {
                if index > run_start {
                    self.append_string(&text[run_start..index]);
                }
                self.end_string();
                return index + 1;
            }
            if byte == b'\\' {
                if index > run_start {
                    self.append_string(&text[run_start..index]);
                }
                if let Some(open) = self.string.as_mut() {
                    open.escape = "\\".to_owned();
                }
                run_start = index + 1;
                index += 1;
                continue;
            }
            index += 1;
        }
        if text.len() > run_start {
            self.append_string(&text[run_start..]);
        }
        text.len()
    }

    /// Read number or literal characters from `start` up to the next
    /// delimiter; returns the delimiter's index.
    fn read_scalar(&mut self, text: &str, start: usize) -> usize {
        let mut end = start;
        while end < text.len() && !is_delimiter(text.as_bytes()[end]) {
            end += 1;
        }
        if end > start {
            self.scalar.push_str(&text[start..end]);
            if self.scalar_slot.is_some() {
                self.dirty = true;
            }
        }
        if end < text.len() {
            self.end_scalar();
        }
        end
    }

    /// Handle one character outside strings.
    fn structural(&mut self, ch: char) {
        match ch {
            ' ' | '\t' | '\n' | '\r' => {}
            '"' => self.start_string(),
            '{' | '[' => {
                let container = if ch == '{' {
                    Value::Object(Map::new())
                } else {
                    Value::Array(Vec::new())
                };
                self.open_container(container);
            }
            '}' | ']' => self.close_container(),
            ':' => {
                if let Some(frame) = self.stack.last_mut()
                    && frame.container.is_object()
                    && frame.phase == Phase::Colon
                {
                    frame.phase = Phase::Value;
                }
            }
            ',' => {
                if let Some(frame) = self.stack.last_mut() {
                    frame.phase = if frame.container.is_array() {
                        Phase::Value
                    } else {
                        Phase::Key
                    };
                }
            }
            _ => {
                self.scalar.clear();
                self.scalar.push(ch);
                self.scalar_slot = self.value_slot();
                if self.scalar_slot.is_some() {
                    self.dirty = true;
                }
            }
        }
    }

    /// Where a value starting now goes, or `None` when this position takes no
    /// value.
    fn value_slot(&self) -> Option<Slot> {
        let Some(frame) = self.stack.last() else {
            return if self.root.is_none() {
                Some(Slot::Root)
            } else {
                None
            };
        };
        if let Value::Array(elements) = &frame.container {
            return Some(Slot::Element(elements.len()));
        }
        // A missing colon is tolerated: `{"a" 1}` reads as `{"a": 1}`.
        if matches!(frame.phase, Phase::Value | Phase::Colon)
            && let Some(key) = &frame.key
        {
            return Some(Slot::Member(key.clone()));
        }
        None
    }

    fn start_string(&mut self) {
        let key_position = self.stack.last().is_some_and(|frame| {
            frame.container.is_object() && matches!(frame.phase, Phase::Key | Phase::Comma)
        });
        // A missing comma is tolerated: `{"a": 1 "b": 2}` reads the second
        // member.
        if key_position {
            self.string = Some(OpenString {
                target: StringTarget::Key,
                slot: None,
                text: String::new(),
                escape: String::new(),
                path: None,
                change: None,
                pending_high: None,
            });
            return;
        }
        let slot = self.value_slot();
        let path = self.reported_path(slot.as_ref());
        self.string = Some(OpenString {
            target: StringTarget::Value,
            slot: slot.clone(),
            text: String::new(),
            escape: String::new(),
            path: path.clone(),
            change: None,
            pending_high: None,
        });
        self.place(slot, Value::String(String::new()));
        if let Some(path) = path {
            let index = self.report(JsonChange::Set {
                path,
                value: Value::String(String::new()),
            });
            if let Some(open) = self.string.as_mut() {
                open.change = Some(index);
            }
        }
    }

    /// The path of a value at `slot` in the innermost open container, when its
    /// changes are reported.
    fn reported_path(&self, slot: Option<&Slot>) -> Option<JsonPath> {
        let slot = slot?;
        if *slot == Slot::Root {
            return Some(Vec::new());
        }
        let frame = self.stack.last()?;
        if !frame.reports_inside {
            return None;
        }
        let mut path = frame.path.clone()?;
        match slot {
            Slot::Member(key) => path.push(JsonPathSegment::Key(key.clone())),
            Slot::Element(index) => path.push(JsonPathSegment::Index(*index)),
            Slot::Root => return Some(Vec::new()),
        }
        Some(path)
    }

    fn report(&mut self, change: JsonChange) -> usize {
        self.changes.push(change);
        self.changes.len() - 1
    }

    /// Continue an escape with `ch`; returns false when `ch` is not part of it
    /// and must be read normally.
    fn continue_escape(&mut self, ch: char) -> bool {
        let Some(open) = self.string.as_mut() else {
            return false;
        };
        if open.escape == "\\" {
            let decoded = match ch {
                '"' => Some('"'),
                '\\' => Some('\\'),
                '/' => Some('/'),
                'b' => Some('\u{08}'),
                'f' => Some('\u{0c}'),
                'n' => Some('\n'),
                'r' => Some('\r'),
                't' => Some('\t'),
                _ => None,
            };
            if let Some(decoded) = decoded {
                open.escape.clear();
                self.append_string(&decoded.to_string());
                return true;
            }
            if ch == 'u' {
                open.escape = "\\u".to_owned();
                return true;
            }
            // Invalid escape: keep the backslash as text and read the
            // character normally, as repairJson() does.
            open.escape.clear();
            self.append_string("\\");
            return false;
        }
        if ch.is_ascii_hexdigit() {
            open.escape.push(ch);
            if open.escape.len() < 6 {
                return true;
            }
            let code = u32::from_str_radix(&open.escape[2..], 16).unwrap_or(0xfffd);
            open.escape.clear();
            self.append_unicode_escape(code);
            return true;
        }
        // `\u` without four hex digits: keep it as text and read the character
        // normally, as repairJson() does.
        let literal = std::mem::take(&mut open.escape);
        open.pending_high = None;
        self.append_string(&literal);
        false
    }

    /// Append the decoded scalar of a complete `\uXXXX` escape, joining
    /// surrogate pairs and dropping lone surrogates (Rust strings cannot hold
    /// them).
    fn append_unicode_escape(&mut self, code: u32) {
        if (0xd800..0xdc00).contains(&code) {
            if let Some(open) = self.string.as_mut() {
                open.pending_high = Some(code);
            }
            return;
        }
        if (0xdc00..0xe000).contains(&code) {
            let high = self
                .string
                .as_mut()
                .and_then(|open| open.pending_high.take());
            if let Some(high) = high {
                let scalar = 0x10000 + ((high - 0xd800) << 10) + (code - 0xdc00);
                if let Some(ch) = char::from_u32(scalar) {
                    self.append_string(&ch.to_string());
                }
            }
            return;
        }
        if let Some(open) = self.string.as_mut() {
            open.pending_high = None;
        }
        if let Some(ch) = char::from_u32(code) {
            self.append_string(&ch.to_string());
        }
    }

    fn append_string(&mut self, text: &str) {
        let (target, slot, path, change) = {
            let Some(open) = self.string.as_mut() else {
                return;
            };
            // A high surrogate whose low half never arrived is dropped,
            // matching `strip_lone_surrogate_escapes`.
            if open.pending_high.is_some() && !text.is_empty() {
                open.pending_high = None;
            }
            open.text.push_str(text);
            (
                open.target,
                open.slot.clone(),
                open.path.clone(),
                open.change,
            )
        };
        if target != StringTarget::Value {
            return;
        }
        self.append_open_string_value(slot, text);
        let Some(path) = path else {
            return;
        };
        // Extend this string's newest change while nothing came after it;
        // otherwise report an append.
        if let Some(change) = change
            && change + 1 == self.changes.len()
        {
            match &mut self.changes[change] {
                JsonChange::Set { value, .. } => {
                    if let Value::String(current) = value {
                        current.push_str(text);
                    }
                }
                JsonChange::Append { text: current, .. } => current.push_str(text),
            }
            return;
        }
        let index = self.report(JsonChange::Append {
            path,
            text: text.to_owned(),
        });
        if let Some(open) = self.string.as_mut() {
            open.change = Some(index);
        }
    }

    /// Append `text` to the placeholder string of an open value string, in
    /// place, so a long string costs O(delta) rather than a full copy.
    fn append_open_string_value(&mut self, slot: Option<Slot>, text: &str) {
        let Some(slot) = slot else {
            return;
        };
        self.dirty = true;
        match slot {
            Slot::Root => {
                if let Some(Value::String(current)) = self.root.as_mut() {
                    current.push_str(text);
                }
            }
            Slot::Member(key) => {
                if let Some(frame) = self.stack.last_mut()
                    && let Value::Object(members) = &mut frame.container
                    && let Some(Value::String(current)) = members.get_mut(&key)
                {
                    current.push_str(text);
                }
            }
            Slot::Element(index) => {
                if let Some(frame) = self.stack.last_mut()
                    && let Value::Array(elements) = &mut frame.container
                    && let Some(Value::String(current)) = elements.get_mut(index)
                {
                    current.push_str(text);
                }
            }
        }
    }

    fn end_string(&mut self) {
        let Some(open) = self.string.take() else {
            return;
        };
        if open.target == StringTarget::Key {
            if let Some(frame) = self.stack.last_mut() {
                frame.key = Some(open.text);
                frame.phase = Phase::Colon;
            }
            return;
        }
        self.value_ended();
    }

    fn end_scalar(&mut self) {
        let token = std::mem::take(&mut self.scalar);
        let slot = self.scalar_slot.take();
        if let Some(value) = scalar_value(&token) {
            self.place(slot.clone(), value.clone());
            if let Some(path) = self.reported_path(slot.as_ref()) {
                self.report(JsonChange::Set { path, value });
            }
        }
        self.value_ended();
    }

    /// The value of the unterminated scalar at the end of the text, if it
    /// shows.
    fn pending_scalar(&self) -> Option<(Slot, Value)> {
        let slot = self.scalar_slot.clone()?;
        let token = self.scalar.as_str();
        if token.is_empty() || token.len() > MAX_PENDING_SCALAR_LENGTH {
            return None;
        }
        pending_scalar_value(token).map(|value| (slot, value))
    }

    fn open_container(&mut self, container: Value) {
        let slot = self.value_slot();
        let path = self.reported_path(slot.as_ref());
        let empty = if container.is_array() {
            Value::Array(Vec::new())
        } else {
            Value::Object(Map::new())
        };
        // The open frame owns the working container; the parent keeps this
        // placeholder until the container closes and is moved into it.
        self.place(slot.clone(), empty.clone());
        let phase = if container.is_array() {
            Phase::Value
        } else {
            Phase::Key
        };
        let reports_inside = path
            .as_ref()
            .is_some_and(|path| path.len() < MAX_REPORTED_DEPTH);
        self.stack.push(Frame {
            container,
            slot,
            path: path.clone(),
            reports_inside,
            size: 0,
            key: None,
            phase,
        });
        self.path_cost += 1;
        self.dirty = true;
        // A fresh container: the working one keeps changing, and a change
        // never does.
        if let Some(path) = path {
            self.report(JsonChange::Set { path, value: empty });
        }
    }

    fn close_container(&mut self) {
        let Some(frame) = self.stack.pop() else {
            // A stray closer outside any container is ignored.
            return;
        };
        self.path_cost -= frame.size + 1;
        self.dirty = true;
        // Too deep to report inside: the whole value, final now that it is
        // closed.
        if let Some(path) = &frame.path
            && !frame.reports_inside
        {
            self.report(JsonChange::Set {
                path: path.clone(),
                value: frame.container.clone(),
            });
        }
        // Move the completed container into its parent (or the root).
        self.place(frame.slot.clone(), frame.container);
        self.value_ended();
    }

    /// A value in the innermost frame, or the root value when there is no
    /// frame, has ended.
    fn value_ended(&mut self) {
        if self.stack.is_empty() {
            // Text after the root value is ignored, but only once a root value
            // exists.
            if self.root.is_some() {
                self.done = true;
            }
            return;
        }
        if let Some(frame) = self.stack.last_mut() {
            frame.phase = Phase::Comma;
        }
    }

    /// Set the value at `slot` in the open path; nothing for a value in an
    /// invalid position.
    fn place(&mut self, slot: Option<Slot>, value: Value) {
        let Some(slot) = slot else {
            return;
        };
        self.dirty = true;
        if slot == Slot::Root {
            self.root = Some(value);
            return;
        }
        let mut added = false;
        {
            let Some(frame) = self.stack.last_mut() else {
                return;
            };
            match slot {
                Slot::Element(index) => {
                    let Value::Array(elements) = &mut frame.container else {
                        return;
                    };
                    if index == elements.len() {
                        frame.size += 1;
                        added = true;
                        elements.push(value);
                    } else if let Some(element) = elements.get_mut(index) {
                        *element = value;
                    }
                }
                Slot::Member(key) => {
                    let Value::Object(members) = &mut frame.container else {
                        return;
                    };
                    if !members.contains_key(&key) {
                        frame.size += 1;
                        added = true;
                    }
                    members.insert(key, value);
                }
                Slot::Root => unreachable!("handled above"),
            }
        }
        if added {
            self.path_cost += 1;
        }
    }

    /// A fresh snapshot: copies of the open containers, innermost first, each
    /// holding the copy of its open child.
    fn copy_open_path(&self, pending: Option<(Slot, Value)>) -> Option<Value> {
        if self.stack.is_empty() {
            return match pending {
                Some((Slot::Root, value)) => Some(value),
                _ => self.root.clone(),
            };
        }
        let mut child: Option<(Option<Slot>, Value)> = None;
        for depth in (0..self.stack.len()).rev() {
            let frame = &self.stack[depth];
            let mut copy = frame.container.clone();
            if depth + 1 == self.stack.len()
                && let Some((slot, value)) = pending.clone()
            {
                set_slot(&mut copy, &slot, value);
            }
            if let Some((slot, value)) = child.take()
                && let Some(slot) = slot
            {
                set_slot(&mut copy, &slot, value);
            }
            child = Some((frame.slot.clone(), copy));
        }
        match child {
            Some((Some(Slot::Root), value)) => Some(value),
            _ => self.root.clone(),
        }
    }
}

fn set_slot(container: &mut Value, slot: &Slot, value: Value) {
    match slot {
        Slot::Element(index) => {
            if let Value::Array(elements) = container {
                if *index == elements.len() {
                    elements.push(value);
                } else if let Some(element) = elements.get_mut(*index) {
                    *element = value;
                }
            }
        }
        Slot::Member(key) => {
            if let Value::Object(members) = container {
                members.insert(key.clone(), value);
            }
        }
        Slot::Root => {}
    }
}

fn is_delimiter(byte: u8) -> bool {
    matches!(
        byte,
        b' ' | b'\t' | b'\n' | b'\r' | b',' | b':' | b'{' | b'}' | b'[' | b']' | b'"'
    )
}

/// The value of a complete number or literal token; `None` for anything else,
/// which is dropped.
fn scalar_value(token: &str) -> Option<Value> {
    match token {
        "true" => return Some(Value::Bool(true)),
        "false" => return Some(Value::Bool(false)),
        "null" => return Some(Value::Null),
        _ => {}
    }
    if !is_number(token) {
        return None;
    }
    serde_json::from_str::<Value>(token)
        .ok()
        .filter(Value::is_number)
}

/// The value an unterminated number or literal token shows.
fn pending_scalar_value(token: &str) -> Option<Value> {
    for (literal, value) in [
        ("true", Value::Bool(true)),
        ("false", Value::Bool(false)),
        ("null", Value::Null),
    ] {
        if literal.starts_with(token) {
            return Some(value);
        }
    }
    if is_number(token) {
        return serde_json::from_str::<Value>(token)
            .ok()
            .filter(Value::is_number);
    }
    // An exponent without digits shows the mantissa, as the upstream
    // `NUMBER_WITHOUT_EXPONENT_DIGITS` regex
    // `/^(-?(?:0|[1-9]\d*)(?:\.\d+)?)[eE][+-]?$/`: the marker is the first
    // `e` or `E`, the sign after it is optional, and nothing but the marker
    // and that sign may follow the mantissa (`1e5e` is not a number).
    let marker = token.find(['e', 'E'])?;
    let (mantissa, exponent) = (&token[..marker], &token[marker..]);
    if !matches!(exponent, "e" | "E" | "e+" | "e-" | "E+" | "E-") || !is_number(mantissa) {
        return None;
    }
    serde_json::from_str::<Value>(mantissa)
        .ok()
        .filter(Value::is_number)
}

/// `-?(0|[1-9]\d*)(\.\d+)?([eE][+-]?\d+)?`, the JSON number grammar.
fn is_number(token: &str) -> bool {
    let bytes = token.as_bytes();
    let mut index = 0;
    if bytes.get(index) == Some(&b'-') {
        index += 1;
    }
    match bytes.get(index) {
        Some(b'0') => index += 1,
        Some(b'1'..=b'9') => {
            index += 1;
            while matches!(bytes.get(index), Some(b'0'..=b'9')) {
                index += 1;
            }
        }
        _ => return false,
    }
    if bytes.get(index) == Some(&b'.') {
        index += 1;
        if !matches!(bytes.get(index), Some(b'0'..=b'9')) {
            return false;
        }
        while matches!(bytes.get(index), Some(b'0'..=b'9')) {
            index += 1;
        }
    }
    if matches!(bytes.get(index), Some(b'e' | b'E')) {
        index += 1;
        if matches!(bytes.get(index), Some(b'+' | b'-')) {
            index += 1;
        }
        if !matches!(bytes.get(index), Some(b'0'..=b'9')) {
            return false;
        }
        while matches!(bytes.get(index), Some(b'0'..=b'9')) {
            index += 1;
        }
    }
    index == bytes.len()
}

/// The arguments of one streaming tool call: the raw text, for the one full
/// parse when the call ends, and a [`StreamingJsonParser`] for the partial
/// arguments in between. Providers keep one per call in their own stream state,
/// never on the content block, so the raw text never reaches partial messages
/// or transcripts.
#[derive(Debug, Default)]
pub struct ToolCallArgumentsStream {
    text: String,
    parser: StreamingJsonParser,
    arguments: Map<String, Value>,
}

impl ToolCallArgumentsStream {
    /// The raw argument text received so far.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Append a piece of argument text. The parser reads only `delta`; the
    /// partial arguments are materialized by [`Self::arguments`] when the
    /// display asks for them.
    pub fn append(&mut self, delta: &str) {
        self.text.push_str(delta);
        self.parser.append(delta);
    }

    /// The partial arguments of the text read so far. Tool arguments are an
    /// object; any other partial value shows as no arguments yet.
    pub fn arguments(&mut self) -> &Map<String, Value> {
        let (snapshot, fresh) = self.parser.snapshot();
        if fresh {
            let next = match snapshot {
                Some(Value::Object(members)) => members.clone(),
                _ => Map::new(),
            };
            self.arguments = next;
        }
        &self.arguments
    }

    /// Take the provider's complete argument text. When it continues the text
    /// so far, returns the missing piece as `delta`; otherwise starts over with
    /// `text`. Either way the partial arguments of `text` are materialized.
    pub fn replace(&mut self, text: &str) -> (Option<String>, &Map<String, Value>) {
        if text.starts_with(&self.text) {
            let delta = text[self.text.len()..].to_owned();
            if !delta.is_empty() {
                self.append(&delta);
            }
            let arguments = self.arguments();
            return (if delta.is_empty() { None } else { Some(delta) }, arguments);
        }
        self.text.clear();
        self.parser = StreamingJsonParser::new();
        self.arguments = Map::new();
        self.append(text);
        (None, self.arguments())
    }

    /// The final arguments: one full parse of the complete text, which reports
    /// malformed arguments.
    pub fn finish(&self) -> ParsedToolCallArguments {
        parse_tool_call_arguments(Some(&self.text))
    }

    /// The current tolerant snapshot, for previews and tests that compare
    /// against [`parse_streaming_json`].
    pub fn snapshot(&mut self) -> Option<Value> {
        self.parser.snapshot().0.cloned()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::utils::json_parse::parse_streaming_json;

    fn snapshot(text: &str) -> Option<Value> {
        let mut parser = StreamingJsonParser::new();
        parser.append(text);
        parser.snapshot().0.cloned()
    }

    #[test]
    fn reads_complete_documents_like_serde_json() {
        for text in [
            r#"{"a":1,"b":[true,false,null],"c":{"d":"e"}}"#,
            r#"  {"path":"src/main.rs","content":"fn main() {}\n"}  "#,
            r#"{"a":"x\u00e9y","b":"😀"}"#,
        ] {
            assert_eq!(
                snapshot(text),
                Some(serde_json::from_str(text).expect("valid")),
                "{text}"
            );
        }
    }

    #[test]
    fn shows_unterminated_values() {
        assert_eq!(
            snapshot(r#"{"command":"ls -l"#),
            Some(json!({"command": "ls -l"}))
        );
        assert_eq!(snapshot(r#"{"command":"#), Some(json!({})));
        assert_eq!(snapshot(r#"{"a":tru"#), Some(json!({"a": true})));
        assert_eq!(snapshot(r#"{"a":nul"#), Some(json!({"a": null})));
        assert_eq!(snapshot(r#"{"a":1e"#), Some(json!({"a": 1})));
        assert_eq!(snapshot(r#"{"a":1.5e+"#), Some(json!({"a": 1.5})));
        assert_eq!(
            snapshot(r#"{"nested":{"x":[1,2"#),
            Some(json!({"nested": {"x": [1, 2]}}))
        );
    }

    /// Upstream `NUMBER_WITHOUT_EXPONENT_DIGITS`: an exponent marker with an
    /// optional sign and no digits yet shows the mantissa, whatever the case
    /// of `e`/`E`, with or without a sign, and nothing else does (`1e5e` is
    /// dropped).
    #[test]
    fn pending_exponent_without_digits_shows_the_mantissa() {
        assert_eq!(snapshot(r#"{"a":1E"#), Some(json!({"a": 1})));
        assert_eq!(snapshot(r#"{"a":1E+"#), Some(json!({"a": 1})));
        assert_eq!(snapshot(r#"{"a":1E-"#), Some(json!({"a": 1})));
        assert_eq!(snapshot(r#"{"a":1.5E-"#), Some(json!({"a": 1.5})));
        assert_eq!(snapshot(r#"{"a":-2.5E+"#), Some(json!({"a": -2.5})));
        assert_eq!(snapshot(r#"{"a":[1E+"#), Some(json!({"a": [1]})));
        assert_eq!(snapshot(r#"{"a":1e5e"#), Some(json!({})));
        assert_eq!(snapshot(r#"{"a":1E5E"#), Some(json!({})));
    }

    #[test]
    fn holds_partial_escapes_back() {
        assert_eq!(snapshot(r#"{"a":"x\"#), Some(json!({"a": "x"})));
        assert_eq!(snapshot(r#"{"a":"x\u00"#), Some(json!({"a": "x"})));
        assert_eq!(snapshot(r#"{"a":"x\u00e9"#), Some(json!({"a": "xé"})));
        assert_eq!(
            snapshot(r#"{"a":"x\ud83d\ude00!"#),
            Some(json!({"a": "x😀!"}))
        );
        // Lone surrogates cannot exist in a Rust string; they are dropped.
        assert_eq!(snapshot(r#"{"a":"x\ud83d!"#), Some(json!({"a": "x!"})));
    }

    #[test]
    fn keeps_malformed_text_lenient() {
        assert_eq!(snapshot(r#"{"a":"x\ny \d"#), Some(json!({"a": "x\ny \\d"})));
        assert_eq!(snapshot(r#"{"a":1 "b":2}"#), Some(json!({"a": 1, "b": 2})));
        assert_eq!(snapshot(r#"{"a":1,}"#), Some(json!({"a": 1})));
        assert_eq!(
            snapshot(r#"{"__proto__":{"polluted":true}}"#),
            Some(json!({"__proto__": {"polluted": true}}))
        );
        assert_eq!(snapshot(r#"{"a":truex"#), Some(json!({})));
    }

    #[test]
    fn changes_keep_a_mirror_in_sync() {
        let mut parser = StreamingJsonParser::new();
        let mut mirror = Value::Object(Map::new());
        let first = parser.append(r#"{"n":1"#);
        assert_eq!(
            first,
            vec![JsonChange::Set {
                path: vec![],
                value: json!({})
            }]
        );
        for change in first {
            apply_change(&mut mirror, &change);
        }
        for change in parser.append(r#"2,"s":"ab"#) {
            apply_change(&mut mirror, &change);
        }
        assert_eq!(mirror, json!({"n": 12, "s": "ab"}));
        for change in parser.append(r#"c","t":tru"#) {
            apply_change(&mut mirror, &change);
        }
        assert_eq!(mirror, json!({"n": 12, "s": "abc"}));
        for change in parser.append("e}") {
            apply_change(&mut mirror, &change);
        }
        assert_eq!(mirror, json!({"n": 12, "s": "abc", "t": true}));
    }

    fn apply_change(target: &mut Value, change: &JsonChange) {
        match change {
            JsonChange::Set { path, value } => set_path(target, path, value.clone()),
            JsonChange::Append { path, text } => {
                if let Some(Value::String(current)) = get_path_mut(target, path) {
                    current.push_str(text);
                }
            }
        }
    }

    fn get_path_mut<'a>(target: &'a mut Value, path: &[JsonPathSegment]) -> Option<&'a mut Value> {
        let mut current = target;
        for segment in path {
            current = match segment {
                JsonPathSegment::Key(key) => current.as_object_mut()?.get_mut(key)?,
                JsonPathSegment::Index(index) => current.as_array_mut()?.get_mut(*index)?,
            };
        }
        Some(current)
    }

    fn set_path(target: &mut Value, path: &[JsonPathSegment], value: Value) {
        if path.is_empty() {
            *target = value;
            return;
        }
        let mut current = target;
        for (depth, segment) in path.iter().enumerate() {
            let last = depth + 1 == path.len();
            current = match segment {
                JsonPathSegment::Key(key) => {
                    let object = current.as_object_mut().expect("object path");
                    if last {
                        object.insert(key.clone(), value);
                        return;
                    }
                    object.entry(key.clone()).or_insert(Value::Null)
                }
                JsonPathSegment::Index(index) => {
                    let array = current.as_array_mut().expect("array path");
                    if last {
                        if *index == array.len() {
                            array.push(value);
                        } else {
                            array[*index] = value;
                        }
                        return;
                    }
                    &mut array[*index]
                }
            };
        }
    }

    #[test]
    fn tool_stream_materializes_partial_arguments() {
        let mut stream = ToolCallArgumentsStream::default();
        stream.append("  ");
        assert_eq!(stream.arguments(), &Map::new());
        stream.append(r#"{"path":"a.ts","content":"x"#);
        assert_eq!(stream.arguments()["path"], json!("a.ts"));
        assert_eq!(stream.arguments()["content"], json!("x"));
        stream.append("y");
        assert_eq!(stream.arguments()["content"], json!("xy"));
        assert_eq!(stream.text(), r#"  {"path":"a.ts","content":"xy"#);
        stream.append(r#""}"#);
        assert_eq!(stream.finish().arguments["content"], json!("xy"));
    }

    #[test]
    fn tool_stream_replace_continues_or_restarts() {
        let mut stream = ToolCallArgumentsStream::default();
        stream.append(r#"{"a":"x"#);
        let (delta, arguments) = stream.replace(r#"{"a":"xyz"}"#);
        assert_eq!(delta.as_deref(), Some(r#"yz"}"#));
        assert_eq!(arguments["a"], json!("xyz"));
        let (delta, arguments) = stream.replace(r#"{"b":2}"#);
        assert_eq!(delta, None);
        assert_eq!(arguments["b"], json!(2));
        assert_eq!(stream.text(), r#"{"b":2}"#);
    }

    #[test]
    fn scans_each_byte_once() {
        let mut parser = StreamingJsonParser::new();
        let text = r#"{"command":"ls -la","nested":{"x":[1,2,3]}}"#;
        for byte in text.as_bytes() {
            parser.append(std::str::from_utf8(std::slice::from_ref(byte)).unwrap_or(""));
        }
        assert_eq!(parser.scanned_bytes(), text.len() as u64);
    }

    /// R3.4.4: every UTF-8 prefix of the corpus must match the existing
    /// tolerant preview oracle, except the documented D-112 categories
    /// (trailing whitespace, raw control characters/invalid escapes while a
    /// string is open, malformed literal suffixes).
    #[test]
    fn preview_matches_parse_streaming_json_on_every_prefix() {
        let corpus = [
            r#"{"path":"src/main.rs","content":"fn main() {}\n"}"#,
            r#"{"command":"ls -la","timeout":30,"nested":{"x":[1,2,3]}}"#,
            r#"{"a":"unterminated"#,
            r#"{"a":"x\"#,
            r#"{"a":"x\u00e9y"}"#,
            r#"{"a":"x\u12"#,
            r#"{"a":tru"#,
            r#"{"a":nul"#,
            r#"{"a":fals"#,
            r#"{"a":1e"#,
            r#"{"a":1.5e+"#,
            r#"{"a":1,}"#,
            r#"{"a":{"b":[{"c":1},"#,
            r#"{"__proto__":{"polluted":true},"b":{"__proto__":"x"}}"#,
            r#"{"a":"中文😀","b":"é"}"#,
            r#"{"a":"x\ud83d\ude00"}"#,
        ];
        let mut failures = Vec::new();
        for text in corpus {
            let mut stream = ToolCallArgumentsStream::default();
            let mut previous = 0;
            for (index, ch) in text.char_indices() {
                let end = index + ch.len_utf8();
                stream.append(&text[previous..end]);
                previous = end;
                let prefix = &text[..end];
                if prefix.ends_with(char::is_whitespace) || d112_preview_category(prefix) {
                    continue;
                }
                let actual = stream.arguments().clone();
                let expected = crate::utils::json_parse::parse_streaming_json(Some(prefix));
                if actual != expected {
                    failures.push((prefix.to_owned(), actual, expected));
                }
            }
        }
        if !failures.is_empty() {
            for (prefix, actual, expected) in failures.iter().take(40) {
                eprintln!("prefix: {prefix:?}\n  actual:   {actual:?}\n  expected: {expected:?}");
            }
            panic!("{} preview prefixes differ", failures.len());
        }
    }

    /// D-112: the upstream #9265 preview keeps an open string's raw control
    /// characters and invalid escapes visible, keeps trailing whitespace, and
    /// drops malformed literal/number suffixes. Each row asserts both sides,
    /// so the deviation is reproducible without trusting the new parser.
    #[test]
    fn d112_preview_differences_are_explicit() {
        type PreviewCase = (
            &'static str,
            serde_json::Map<String, Value>,
            serde_json::Map<String, Value>,
        );
        let cases: &[PreviewCase] = &[
            (
                r#"{"content":"fn "#,
                json!({"content": "fn"})
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
                json!({"content": "fn "})
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
            ),
            (
                "{\"a\":\"x\n",
                json!({"a": "x"}).as_object().cloned().unwrap_or_default(),
                json!({"a": "x\n"}).as_object().cloned().unwrap_or_default(),
            ),
            (
                "{\"a\":\"x\ny",
                json!({}).as_object().cloned().unwrap_or_default(),
                json!({"a": "x\ny"})
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
            ),
            (
                r#"{"a":"x\d"#,
                json!({"a": "x"}).as_object().cloned().unwrap_or_default(),
                json!({"a": "x\\d"})
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
            ),
            (
                r#"{"a":"x\dy"#,
                json!({"a": "x"}).as_object().cloned().unwrap_or_default(),
                json!({"a": "x\\dy"})
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
            ),
            (
                r#"{"a":truex}"#,
                json!({"a": true}).as_object().cloned().unwrap_or_default(),
                json!({}).as_object().cloned().unwrap_or_default(),
            ),
            (
                r#"{"a":nullx}"#,
                json!({"a": null}).as_object().cloned().unwrap_or_default(),
                json!({}).as_object().cloned().unwrap_or_default(),
            ),
            (
                r#"{"a":tru "#,
                json!({"a": true}).as_object().cloned().unwrap_or_default(),
                json!({}).as_object().cloned().unwrap_or_default(),
            ),
            (
                r#"{"a":1e}"#,
                json!({"a": 1}).as_object().cloned().unwrap_or_default(),
                json!({}).as_object().cloned().unwrap_or_default(),
            ),
            (
                r#"{"a":1 "b":2}"#,
                json!({}).as_object().cloned().unwrap_or_default(),
                json!({"a": 1, "b": 2})
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
            ),
            (
                r#"{"a" 1}"#,
                json!({}).as_object().cloned().unwrap_or_default(),
                json!({"a": 1}).as_object().cloned().unwrap_or_default(),
            ),
            (
                r#"{"a":[1 2]}"#,
                json!({"a": []}).as_object().cloned().unwrap_or_default(),
                json!({"a": [1, 2]})
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
            ),
            (
                r#"{"a":1,,"b":2}"#,
                json!({"a": 1}).as_object().cloned().unwrap_or_default(),
                json!({"a": 1, "b": 2})
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
            ),
            // A value where a key is expected is read and dropped, and the
            // parser continues; the old oracle stops at the first one.
            (
                r#"{"a" []}"#,
                json!({}).as_object().cloned().unwrap_or_default(),
                json!({"a": []}).as_object().cloned().unwrap_or_default(),
            ),
            (
                r#"{1 "a":2}"#,
                json!({}).as_object().cloned().unwrap_or_default(),
                json!({"a": 2}).as_object().cloned().unwrap_or_default(),
            ),
            (
                r#"{,"a":1}"#,
                json!({}).as_object().cloned().unwrap_or_default(),
                json!({"a": 1}).as_object().cloned().unwrap_or_default(),
            ),
            (
                r#"{[]""["#,
                json!({}).as_object().cloned().unwrap_or_default(),
                json!({"": []}).as_object().cloned().unwrap_or_default(),
            ),
            // A number beyond the `f64` range: upstream shows `null`, the old
            // oracle showed the mantissa, the new parser drops the value.
            (
                r#"{"a":2e315"#,
                json!({"a": 2}).as_object().cloned().unwrap_or_default(),
                json!({}).as_object().cloned().unwrap_or_default(),
            ),
            (
                r#"{"a":2e315,"b":1}"#,
                json!({"a": 2, "b": 1})
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
                json!({"b": 1}).as_object().cloned().unwrap_or_default(),
            ),
            // An exponent marker with a sign and no digits yet shows the
            // mantissa (upstream regex), whatever the case of `e`/`E`; a token
            // with digits after the marker and another marker (`1e5e`) is
            // dropped, where the old oracle showed the shorter number.
            (
                r#"{"a":1E+"#,
                json!({}).as_object().cloned().unwrap_or_default(),
                json!({"a": 1}).as_object().cloned().unwrap_or_default(),
            ),
            (
                r#"{"a":1e5e"#,
                json!({"a": 100000.0})
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
                json!({}).as_object().cloned().unwrap_or_default(),
            ),
            // An empty array with one blank inside: the old `parse_arr` did
            // not skip blanks before its `]` check, never consumed the `]` and
            // made the enclosing object stop there; the new parser and
            // upstream read on.
            (
                r#"{"a":[ ],"b":3"#,
                json!({"a": []}).as_object().cloned().unwrap_or_default(),
                json!({"a": [], "b": 3})
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
            ),
            // The same array without a following member is not a difference.
            (
                r#"{"a":[ ],"#,
                json!({"a": []}).as_object().cloned().unwrap_or_default(),
                json!({"a": []}).as_object().cloned().unwrap_or_default(),
            ),
        ];
        for (input, old, new) in cases {
            assert_eq!(
                parse_streaming_json(Some(input)),
                *old,
                "old preview oracle for {input:?}"
            );
            let mut stream = ToolCallArgumentsStream::default();
            stream.append(input);
            assert_eq!(stream.arguments(), new, "new preview for {input:?}");
        }

        // The "same, not a difference" rows are machine-checked too. The
        // multi-byte row used to panic inside the old oracle before its
        // unchecked colon skip learned to step over a whole character.
        for (input, expected) in [
            (r#"{"a":"x\dy"}"#, json!({"a": "x\\dy"})),
            (r#"{"a":tru}"#, json!({})),
            ("{\"a\"é", json!({})),
        ] {
            let old = parse_streaming_json(Some(input));
            let mut stream = ToolCallArgumentsStream::default();
            stream.append(input);
            let new = stream.arguments().clone();
            assert_eq!(
                old,
                expected.as_object().cloned().unwrap_or_default(),
                "{input}"
            );
            assert_eq!(new, old, "{input}");
        }

        // The authoritative final parse is unchanged: complete valid text is
        // identical to the old oracle, and a number beyond the `f64` range is
        // still an error (upstream would accept it as `null`).
        let mut stream = ToolCallArgumentsStream::default();
        stream.append("{\"a\":\"x\ny\"}");
        assert_eq!(
            stream.finish().arguments["a"],
            json!("x\ny"),
            "complete documents still repair like parse_tool_call_arguments"
        );
        let mut stream = ToolCallArgumentsStream::default();
        stream.append(r#"{"a":2e315,"b":1}"#);
        let parsed = stream.finish();
        assert!(parsed.arguments.is_empty());
        assert!(
            parsed.arguments_error.is_some(),
            "out-of-range numbers never execute"
        );
    }

    /// D-112 (credit lag): a very large open container makes the preview lag
    /// behind the text until the credit catches up; the final value is exact.
    #[test]
    fn d112_credit_lag_is_explicit() {
        let mut stream = ToolCallArgumentsStream::default();
        stream.append(r#"{"items":["#);
        let mut saw_lag = false;
        for index in 0..2000 {
            stream.append(&format!("{index},"));
            if saw_lag {
                continue;
            }
            let prefix = stream.text().to_owned();
            let actual = stream.arguments().clone();
            let expected = parse_streaming_json(Some(&prefix));
            if actual != expected {
                saw_lag = true;
            }
        }
        assert!(saw_lag, "a large open array must lag at some prefix");
        stream.append("0]}");
        let parsed = stream.finish();
        assert_eq!(
            parsed.arguments["items"].as_array().map(Vec::len),
            Some(2001),
            "the final strict parse is exact"
        );
    }

    /// R3.4.4: the same text fed in any chunk size produces the same snapshot
    /// at every UTF-8 boundary, and the same authoritative final value.
    #[test]
    fn arbitrary_chunks_produce_the_same_snapshot_at_every_prefix() {
        let corpus = [
            r#"{"path":"src/main.rs","content":"fn main() {}\n"}"#,
            r#"{"command":"ls -la","timeout":30,"nested":{"x":[1,2,3]}}"#,
            r#"{"a":"unterminated"#,
            r#"{"a":"x\u00e9y"}"#,
            r#"{"a":"x\u12"#,
            r#"{"a":tru"#,
            r#"{"a":1e"#,
            r#"{"a":1.5e+"#,
            r#"{"a":1,}"#,
            r#"{"a":{"b":[{"c":1},"#,
            r#"{"__proto__":{"polluted":true},"b":{"__proto__":"x"}}"#,
            r#"{"a":"中文😀","b":"é"}"#,
            r#"{"a":"x\ud83d\ude00"}"#,
        ];
        for text in corpus {
            let (reference, reference_final) = snapshots_at_chunk_ends(text, 1);
            let reference_by_position: std::collections::HashMap<_, _> =
                reference.iter().cloned().collect();
            for chunk in [2, 3, 7, 64] {
                let (candidate, candidate_final) = snapshots_at_chunk_ends(text, chunk);
                assert!(
                    candidate.len() < reference.len(),
                    "chunk {chunk} must actually group pieces for {text:?}"
                );
                for (position, snapshot) in &candidate {
                    assert_eq!(
                        snapshot,
                        reference_by_position
                            .get(position)
                            .unwrap_or_else(|| panic!("missing reference position {position}")),
                        "snapshot mismatch at position {position} for {text:?} chunk {chunk}"
                    );
                }
                assert_eq!(
                    candidate_final, reference_final,
                    "authoritative final for {text:?} chunk {chunk}"
                );
            }
        }
    }

    /// Feed `text` in whole-character pieces of at most `chunk` bytes and
    /// record `(end_position, snapshot)` after each piece. The chunk=1 run
    /// records every UTF-8 boundary; other sizes record a subset, which the
    /// caller compares at the shared positions.
    type ChunkSnapshots = Vec<(usize, serde_json::Map<String, Value>)>;

    fn snapshots_at_chunk_ends(
        text: &str,
        chunk: usize,
    ) -> (ChunkSnapshots, ParsedToolCallArguments) {
        let mut stream = ToolCallArgumentsStream::default();
        let mut out = Vec::new();
        let mut fed = 0;
        while fed < text.len() {
            let mut end = fed;
            let mut bytes = 0;
            for (offset, ch) in text[fed..].char_indices() {
                if bytes > 0 && bytes + ch.len_utf8() > chunk {
                    break;
                }
                bytes += ch.len_utf8();
                end = fed + offset + ch.len_utf8();
            }
            if end == fed {
                end = fed + text[fed..].chars().next().expect("non-empty").len_utf8();
            }
            stream.append(&text[fed..end]);
            fed = end;
            out.push((fed, stream.arguments().clone()));
        }
        (out, stream.finish())
    }

    /// In the credit-lag region the preview can depend on how the same bytes
    /// were grouped (D-112); the authoritative final value never does.
    #[test]
    fn chunk_grouping_does_not_change_the_final_value() {
        let mut text = String::from(r#"{"items":["#);
        for index in 0..2500 {
            text.push_str(&format!("{index},"));
        }
        text.push_str("0]}");
        let mut reference = None;
        for chunk in [1, 2, 7, 64] {
            let (_, parsed) = snapshots_at_chunk_ends(&text, chunk);
            if let Some(reference) = &reference {
                assert_eq!(&parsed, reference, "chunk {chunk} final value");
            } else {
                reference = Some(parsed);
            }
        }
    }

    /// A discarded unterminated scalar does not invalidate the previous
    /// preview until the next change; the next change refreshes it (D-112).
    #[test]
    fn d112_discarded_scalar_keeps_the_previous_preview_until_the_next_change() {
        let mut stream = ToolCallArgumentsStream::default();
        stream.append(r#"{"a":tru"#);
        assert_eq!(stream.arguments()["a"], json!(true));
        stream.append(" ");
        assert_eq!(
            stream.arguments()["a"],
            json!(true),
            "the stale preview is retained until the next change"
        );
        stream.append(r#","b":1}"#);
        assert_eq!(stream.arguments()["b"], json!(1));
        assert!(
            stream.arguments().get("a").is_none(),
            "the next value change drops the stale scalar"
        );

        let mut fresh = ToolCallArgumentsStream::default();
        fresh.append(r#"{"a":tru "#);
        assert!(
            fresh.arguments().is_empty(),
            "a fresh one-shot parser drops the scalar"
        );
    }

    /// A snapshot is an owned value: later appends never mutate an earlier
    /// one (the parser's cached snapshot is only handed out by reference and
    /// cloned by the caller).
    #[test]
    fn snapshots_are_immutable() {
        let mut stream = ToolCallArgumentsStream::default();
        stream.append(r#"{"a":"x"#);
        let first = stream.snapshot();
        stream.append("y");
        assert_eq!(first, Some(json!({"a": "x"})));
        assert_eq!(stream.snapshot(), Some(json!({"a": "xy"})));
    }

    /// `scanned_bytes` counts bytes the parser actually consumed, not the
    /// length handed to `append`: text after the root value is not scanned.
    #[test]
    fn scanned_bytes_counts_consumed_bytes_not_input_length() {
        let mut parser = StreamingJsonParser::new();
        let text = r#"{"a":1}"#;
        parser.append(text);
        assert_eq!(parser.scanned_bytes(), text.len() as u64);
        parser.append(" trailing text after the root");
        assert_eq!(
            parser.scanned_bytes(),
            text.len() as u64,
            "bytes after the root value are not consumed"
        );
    }

    /// Duplicate keys keep the first key's position and the last value, like
    /// `JSON.parse` and `serde_json` with `preserve_order`.
    #[test]
    fn duplicate_keys_keep_the_first_position() {
        let mut stream = ToolCallArgumentsStream::default();
        stream.append(r#"{"a":1,"b":2,"a":"three"}"#);
        let arguments = stream.arguments();
        assert_eq!(arguments["a"], json!("three"));
        assert_eq!(
            arguments.keys().cloned().collect::<Vec<_>>(),
            vec!["a".to_owned(), "b".to_owned()]
        );
    }

    /// Changes deeper than 32 levels are reported once, whole, when the
    /// container closes; shallower changes are reported incrementally.
    #[test]
    fn deep_container_changes_report_the_whole_value_on_close() {
        let depth = 40;
        let text = format!("{}1{}", "[".repeat(depth), "]".repeat(depth));
        let mut parser = StreamingJsonParser::new();
        let changes = parser.append(&text);
        assert!(
            changes.iter().any(|change| matches!(
                change,
                JsonChange::Set { path, .. } if path.len() == 32
            )),
            "the 32nd-level container must be reported whole at close"
        );
    }

    /// The parser keeps document state, not per-delta history: the changes
    /// vector is per piece, the scalar buffer is bounded, and the only growing
    /// field is the open string itself.
    #[test]
    fn parser_state_is_bounded_by_the_document_not_the_delta_count() {
        let mut parser = StreamingJsonParser::new();
        parser.append(r#"{"content":""#);
        for _ in 0..10_000 {
            let changes = parser.append("x");
            assert!(
                changes.is_empty() || matches!(changes.first(), Some(JsonChange::Append { .. })),
                "an open string only reports its appended text"
            );
            assert_eq!(changes.len(), 1, "one open-string append per piece");
            assert_eq!(
                parser.changes.capacity(),
                0,
                "no per-delta history retained"
            );
        }
        assert_eq!(parser.stack.len(), 1);
        assert!(parser.scalar.is_empty());
        assert_eq!(
            parser.string.as_ref().map(|open| open.text.len()),
            Some(10_000)
        );
    }

    /// A skipped preview does not stop the parser: every delta is appended,
    /// and the next materialization covers every byte received so far.
    #[test]
    fn skipped_snapshots_still_advance_and_the_next_snapshot_is_complete() {
        let mut stream = ToolCallArgumentsStream::default();
        let full = r#"{"path":"a.ts","content":"hello world"}"#;
        let mut shown = 0;
        for (index, chunk) in full.as_bytes().chunks(3).enumerate() {
            stream.append(std::str::from_utf8(chunk).expect("ASCII chunks"));
            // Materialize only every fifth piece; the skipped pieces must
            // still advance the parser, and the next snapshot must cover them.
            if index % 5 == 0 {
                let content = stream
                    .arguments()
                    .get("content")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                assert!(content.len() >= shown, "the preview only moves forward");
                shown = content.len();
            }
        }
        assert_eq!(stream.text(), full);
        assert_eq!(stream.arguments()["content"], json!("hello world"));
    }

    /// N bytes split into N pieces are scanned once: `scanned_bytes` equals the
    /// input length for every chunk size, and the final value is exact.
    #[test]
    fn scanned_bytes_are_linear_for_every_chunk_size() {
        let text = format!(r#"{{"path":"a.ts","content":"{}"}}"#, "x".repeat(4096));
        for chunk in [1, 2, 7, 64, 4096] {
            let mut stream = ToolCallArgumentsStream::default();
            let mut offset = 0;
            while offset < text.len() {
                let end = (offset + chunk).min(text.len());
                stream.append(&text[offset..end]);
                offset = end;
            }
            assert_eq!(
                stream.finish().arguments["content"],
                json!("x".repeat(4096))
            );
        }
        // The parser seam counts every consumed byte once.
        let mut parser = StreamingJsonParser::new();
        for chunk in text.as_bytes().chunks(7) {
            parser.append(std::str::from_utf8(chunk).expect("ASCII chunks"));
        }
        assert_eq!(parser.scanned_bytes(), text.len() as u64);
    }

    /// Whether a prefix contains a D-112 preview category: a raw control
    /// character or an invalid escape inside a string.
    fn d112_preview_category(text: &str) -> bool {
        let bytes = text.as_bytes();
        let mut in_string = false;
        let mut index = 0;
        while index < bytes.len() {
            let byte = bytes[index];
            if !in_string {
                if byte == b'"' {
                    in_string = true;
                }
                index += 1;
                continue;
            }
            if byte == b'"' {
                in_string = false;
                index += 1;
                continue;
            }
            if byte == b'\\' {
                let Some(next) = bytes.get(index + 1) else {
                    return false;
                };
                if *next == b'u' {
                    let digits = bytes.get(index + 2..index + 6);
                    match digits {
                        Some(digits) if digits.iter().all(u8::is_ascii_hexdigit) => {
                            index += 6;
                            continue;
                        }
                        // A complete but non-hex escape is an invalid escape.
                        Some(_) => return true,
                        // An incomplete `\u` whose digits are still hex is
                        // held back by both parsers; once a non-hex byte
                        // appears it is an invalid escape.
                        None => return false,
                    }
                }
                if !matches!(
                    *next,
                    b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't'
                ) {
                    return true;
                }
                index += 2;
                continue;
            }
            if byte < 0x20 {
                return true;
            }
            index += 1;
        }
        false
    }
}
