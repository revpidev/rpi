//! Child stdout/stderr stream protocol: bounded line reader, oversized-line
//! projection, stderr tail, event extraction and final-output selection
//! (FR-P0-06).
//!
//! Port of pi-subagents `src/runs/shared/child-protocol.ts` @ v0.48.0
//! (56f97234) plus `getFinalOutput` / `truncateOutput` from
//! `src/shared/types.ts` / `utils.ts` and the `message_end` handling from
//! `execution.ts` processLine (848-1007).
//!
//! Intentional differences: none in the protocol limits or parsing rules.

use serde_json::Value;

pub const MAX_CHILD_PENDING_LINE_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_CHILD_STDERR_BYTES: usize = 128 * 1024;
const MAX_PROTOCOL_DIAGNOSTIC_BYTES: usize = 4096;
const MAX_PROJECTED_JSON_DEPTH: usize = 256;

/// `DEFAULT_MAX_OUTPUT` (types.ts:1953-1956).
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 200 * 1024;
pub const DEFAULT_MAX_OUTPUT_LINES: usize = 5000;

#[derive(Debug, Clone, PartialEq)]
pub struct ProtocolOutputLimit {
    pub code: &'static str,
    pub stream: &'static str,
    pub limit_bytes: usize,
    pub observed_bytes: usize,
    pub diagnostic_prefix: String,
    pub diagnostic_tail: String,
}

impl ProtocolOutputLimit {
    /// `formatProtocolOutputLimit` (child-protocol.ts:240-242).
    pub fn message(&self) -> String {
        format!(
            "{}: child {} line exceeded {} bytes (observed at least {} bytes without a newline).",
            self.code, self.stream, self.limit_bytes, self.observed_bytes
        )
    }
}

/// `createBoundedByteTail` (child-protocol.ts:377-392): ring buffer keeping
/// the last `max_bytes` with a UTF-8-safe left boundary.
#[derive(Debug)]
pub struct BoundedByteTail {
    tail: Vec<u8>,
    max_bytes: usize,
}

impl BoundedByteTail {
    pub fn new(max_bytes: usize) -> Self {
        Self {
            tail: Vec::new(),
            max_bytes,
        }
    }

    pub fn push(&mut self, chunk: &[u8]) {
        self.tail.extend_from_slice(chunk);
        if self.tail.len() > self.max_bytes {
            let mut start = self.tail.len() - self.max_bytes;
            while start < self.tail.len() && (self.tail[start] & 0xc0) == 0x80 {
                start += 1;
            }
            self.tail.drain(..start);
        }
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.tail).to_string()
    }

    #[allow(dead_code)] // upstream API surface; exercised in tests
    pub fn byte_length(&self) -> usize {
        self.tail.len()
    }
}

/// Child lifecycle action (`projectChildLifecycle`,
/// child-protocol.ts:396-401).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChildLifecycleAction {
    StartDrain,
    CancelDrain,
    #[default]
    None,
}

pub fn project_child_lifecycle(
    event_type: Option<&str>,
    will_retry: bool,
    terminal_assistant_stop: bool,
) -> ChildLifecycleAction {
    if event_type == Some("agent_end") && will_retry {
        return ChildLifecycleAction::CancelDrain;
    }
    if event_type == Some("agent_settled") {
        return ChildLifecycleAction::StartDrain;
    }
    if terminal_assistant_stop {
        return ChildLifecycleAction::StartDrain;
    }
    ChildLifecycleAction::None
}

/// Streaming JSON state machine that recovers `type`/`willRetry` from an
/// oversized `turn_end`/`agent_end` aggregate line
/// (`createPiAggregateProjection`, child-protocol.ts:30-224). Ported
/// character-for-character: the acceptance of a line is decided by the same
/// JSON grammar constraints.
struct AggregateProjection {
    stack: Vec<Container>,
    root_ended: bool,
    token: Option<Token>,
    valid: bool,
    event_type: Option<String>,
    will_retry: Option<bool>,
    pending_string: Option<PendingString>,
    /// Object key of the top container (JS stores it on the container).
    top_key: Option<String>,
    /// Incomplete UTF-8 sequence carried between pushes.
    utf8_pending: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Container {
    Object(ObjectState),
    Array(ArrayState),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum ObjectState {
    KeyOrEnd,
    Key,
    Colon,
    Value,
    CommaOrEnd,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum ArrayState {
    ValueOrEnd,
    Value,
    CommaOrEnd,
}

#[derive(Debug, Clone)]
enum Token {
    Literal {
        expected: &'static str,
        index: usize,
    },
    Number {
        phase: NumberPhase,
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum NumberPhase {
    Minus,
    Zero,
    Int,
    Dot,
    Frac,
    Exp,
    ExpSign,
    ExpDigits,
}

#[derive(Debug, Clone)]
struct PendingString {
    role_key: bool,
    value: String,
    capture: bool,
    escape: bool,
    unicode_digits: usize,
    unicode_value: String,
}

impl AggregateProjection {
    fn new() -> Self {
        Self {
            stack: Vec::new(),
            root_ended: false,
            token: None,
            valid: true,
            event_type: None,
            will_retry: None,
            pending_string: None,
            top_key: None,
            utf8_pending: Vec::new(),
        }
    }

    fn key_of_top(&self) -> Option<String> {
        match self.stack.last() {
            Some(Container::Object { .. }) => self.top_key.clone(),
            _ => None,
        }
    }

    fn push(&mut self, chunk: &[u8]) -> bool {
        // Streaming UTF-8 decode with fatal errors (upstream TextDecoder
        // `{stream: true}`): carry incomplete sequences between pushes.
        self.utf8_pending.extend_from_slice(chunk);
        let valid_up_to = match std::str::from_utf8(&self.utf8_pending) {
            Ok(_) => self.utf8_pending.len(),
            Err(error) => {
                let valid = error.valid_up_to();
                if valid == 0 {
                    return true; // incomplete code point, wait for more bytes
                }
                valid
            }
        };
        let text = String::from_utf8_lossy(&self.utf8_pending[..valid_up_to]).to_string();
        self.utf8_pending.drain(..valid_up_to);
        if !self.process_text(&text) {
            self.valid = false;
            return false;
        }
        true
    }

    fn finish(mut self) -> Option<String> {
        if !self.utf8_pending.is_empty() {
            let text = String::from_utf8_lossy(&self.utf8_pending).to_string();
            self.utf8_pending.clear();
            if !self.process_text(&text) {
                self.valid = false;
            }
        }
        if let Some(Token::Number { phase }) = &self.token {
            if matches!(
                phase,
                NumberPhase::Zero | NumberPhase::Int | NumberPhase::Frac | NumberPhase::ExpDigits
            ) {
                self.token = None;
                self.complete_value(None);
            }
        }
        if !self.valid
            || self.token.is_some()
            || self.pending_string.is_some()
            || !self.stack.is_empty()
            || !self.root_ended
        {
            return None;
        }
        if self.event_type.as_deref() == Some("turn_end") {
            return Some(r#"{"type":"turn_end"}"#.to_string());
        }
        if self.event_type.as_deref() == Some("agent_end") {
            if let Some(will_retry) = self.will_retry {
                return Some(
                    serde_json::json!({"type": "agent_end", "willRetry": will_retry}).to_string(),
                );
            }
        }
        None
    }

    fn process_text(&mut self, text: &str) -> bool {
        for ch in text.chars() {
            if !self.process_char(ch) {
                return false;
            }
        }
        true
    }

    fn process_char(&mut self, ch: char) -> bool {
        if self.pending_string.is_some() {
            return self.process_string_char(ch);
        }
        match &mut self.token {
            Some(Token::Literal { expected, index }) => {
                let expected = *expected;
                let bytes: Vec<char> = expected.chars().collect();
                let index = *index;
                if bytes.get(index) != Some(&ch) {
                    return false;
                }
                let next = index + 1;
                if next == bytes.len() {
                    let value = match expected {
                        "true" => Some(true),
                        "false" => Some(false),
                        _ => None,
                    };
                    self.token = None;
                    self.complete_value(value.map(Value::Bool));
                } else {
                    self.token = Some(Token::Literal {
                        expected,
                        index: next,
                    });
                }
                true
            }
            Some(Token::Number { phase }) => {
                let phase = *phase;
                let next = match phase {
                    NumberPhase::Minus => match ch {
                        '0' => Some(NumberPhase::Zero),
                        '1'..='9' => Some(NumberPhase::Int),
                        _ => None,
                    },
                    NumberPhase::Zero | NumberPhase::Int => match ch {
                        '0'..='9' if phase == NumberPhase::Int => Some(NumberPhase::Int),
                        '0'..='9' => return false,
                        '.' => Some(NumberPhase::Dot),
                        'e' | 'E' => Some(NumberPhase::Exp),
                        _ => None,
                    },
                    NumberPhase::Dot => match ch {
                        '0'..='9' => Some(NumberPhase::Frac),
                        _ => return false,
                    },
                    NumberPhase::Frac => match ch {
                        '0'..='9' => Some(NumberPhase::Frac),
                        'e' | 'E' => Some(NumberPhase::Exp),
                        _ => None,
                    },
                    NumberPhase::Exp => match ch {
                        '+' | '-' => Some(NumberPhase::ExpSign),
                        '0'..='9' => Some(NumberPhase::ExpDigits),
                        _ => return false,
                    },
                    NumberPhase::ExpSign => match ch {
                        '0'..='9' => Some(NumberPhase::ExpDigits),
                        _ => return false,
                    },
                    NumberPhase::ExpDigits => match ch {
                        '0'..='9' => Some(NumberPhase::ExpDigits),
                        _ => None,
                    },
                };
                match next {
                    Some(next_phase) => {
                        self.token = Some(Token::Number { phase: next_phase });
                        true
                    }
                    None => {
                        if !matches!(
                            phase,
                            NumberPhase::Zero
                                | NumberPhase::Int
                                | NumberPhase::Frac
                                | NumberPhase::ExpDigits
                        ) {
                            return false;
                        }
                        self.token = None;
                        self.complete_value(None);
                        self.process_char(ch)
                    }
                }
            }
            None => self.process_structure_char(ch),
        }
    }

    fn process_string_char(&mut self, ch: char) -> bool {
        let Some(pending) = self.pending_string.as_mut() else {
            return false;
        };
        if pending.unicode_digits > 0 {
            if !ch.is_ascii_hexdigit() {
                return false;
            }
            pending.unicode_value.push(ch);
            pending.unicode_digits -= 1;
            if pending.unicode_digits == 0 && pending.capture {
                if pending.value.len() >= 64 {
                    return false;
                }
                let code = u32::from_str_radix(&pending.unicode_value, 16).unwrap_or(0);
                pending
                    .value
                    .push(char::from_u32(code).unwrap_or('\u{fffd}'));
            }
            return true;
        }
        if pending.escape {
            pending.escape = false;
            if ch == 'u' {
                pending.unicode_digits = 4;
                pending.unicode_value.clear();
                return true;
            }
            if !matches!(ch, '"' | '\\' | '/' | 'b' | 'f' | 'n' | 'r' | 't') {
                return false;
            }
            if pending.capture {
                if pending.value.len() >= 64 {
                    return false;
                }
                pending.value.push(match ch {
                    'b' => '\u{8}',
                    'f' => '\u{c}',
                    'n' => '\n',
                    'r' => '\r',
                    't' => '\t',
                    other => other,
                });
            }
            return true;
        }
        if ch == '\\' {
            pending.escape = true;
            return true;
        }
        if ch == '"' {
            let finished = self.pending_string.take().expect("checked above");
            if finished.role_key {
                let Some(Container::Object(state)) = self.stack.last_mut() else {
                    return false;
                };
                self.top_key = Some(finished.value.clone());
                *state = ObjectState::Colon;
            } else {
                self.complete_value(
                    finished
                        .capture
                        .then(|| Value::String(finished.value.clone())),
                );
            }
            return true;
        }
        if (ch as u32) < 0x20 {
            return false;
        }
        if pending.capture {
            if pending.value.len() >= 64 {
                return false;
            }
            pending.value.push(ch);
        }
        true
    }

    fn process_structure_char(&mut self, ch: char) -> bool {
        if matches!(ch, ' ' | '\t' | '\r' | '\n') {
            return true;
        }
        let key = self.key_of_top();
        let is_top_level_field = self.stack.len() == 1
            && matches!(self.stack.last(), Some(Container::Object { .. }))
            && matches!(key.as_deref(), Some("type") | Some("willRetry"));
        if is_top_level_field {
            match key.as_deref() {
                Some("type") => self.event_type = None,
                Some("willRetry") => self.will_retry = None,
                _ => {}
            }
        }
        match self.stack.last_mut() {
            None => {
                if self.root_ended {
                    return false;
                }
                self.start_value(ch)
            }
            Some(Container::Object(state)) => match state {
                ObjectState::KeyOrEnd => {
                    if ch == '}' {
                        self.close_container();
                        return true;
                    }
                    if ch != '"' {
                        return false;
                    }
                    self.pending_string = Some(PendingString {
                        role_key: true,
                        value: String::new(),
                        capture: self.stack.len() == 1,
                        escape: false,
                        unicode_digits: 0,
                        unicode_value: String::new(),
                    });
                    true
                }
                ObjectState::Key => {
                    if ch != '"' {
                        return false;
                    }
                    self.pending_string = Some(PendingString {
                        role_key: true,
                        value: String::new(),
                        capture: self.stack.len() == 1,
                        escape: false,
                        unicode_digits: 0,
                        unicode_value: String::new(),
                    });
                    true
                }
                ObjectState::Colon => {
                    if ch != ':' {
                        return false;
                    }
                    *state = ObjectState::Value;
                    true
                }
                ObjectState::Value => self.start_value(ch),
                ObjectState::CommaOrEnd => {
                    if ch == ',' {
                        *state = ObjectState::Key;
                        true
                    } else if ch == '}' {
                        self.close_container();
                        true
                    } else {
                        false
                    }
                }
            },
            Some(Container::Array(state)) => match state {
                ArrayState::ValueOrEnd => {
                    if ch == ']' {
                        self.close_container();
                        return true;
                    }
                    self.start_value(ch)
                }
                ArrayState::Value => self.start_value(ch),
                ArrayState::CommaOrEnd => {
                    if ch == ',' {
                        *state = ArrayState::Value;
                        true
                    } else if ch == ']' {
                        self.close_container();
                        true
                    } else {
                        false
                    }
                }
            },
        }
    }

    fn start_value(&mut self, ch: char) -> bool {
        let key = self.key_of_top();
        let capture_value = self.stack.len() == 1 && key.as_deref() == Some("type");
        match ch {
            '{' => {
                if self.stack.len() >= MAX_PROJECTED_JSON_DEPTH {
                    return false;
                }
                self.stack.push(Container::Object(ObjectState::KeyOrEnd));
                true
            }
            '[' => {
                if self.stack.len() >= MAX_PROJECTED_JSON_DEPTH {
                    return false;
                }
                self.stack.push(Container::Array(ArrayState::ValueOrEnd));
                true
            }
            '"' => {
                self.pending_string = Some(PendingString {
                    role_key: false,
                    value: String::new(),
                    capture: capture_value,
                    escape: false,
                    unicode_digits: 0,
                    unicode_value: String::new(),
                });
                true
            }
            't' => {
                self.token = Some(Token::Literal {
                    expected: "true",
                    index: 1,
                });
                true
            }
            'f' => {
                self.token = Some(Token::Literal {
                    expected: "false",
                    index: 1,
                });
                true
            }
            'n' => {
                self.token = Some(Token::Literal {
                    expected: "null",
                    index: 1,
                });
                true
            }
            '-' => {
                self.token = Some(Token::Number {
                    phase: NumberPhase::Minus,
                });
                true
            }
            '0' => {
                self.token = Some(Token::Number {
                    phase: NumberPhase::Zero,
                });
                true
            }
            '1'..='9' => {
                self.token = Some(Token::Number {
                    phase: NumberPhase::Int,
                });
                true
            }
            _ => false,
        }
    }

    fn close_container(&mut self) {
        self.stack.pop();
        self.complete_value(None);
    }

    fn complete_value(&mut self, value: Option<Value>) {
        let stack_len = self.stack.len();
        let Some(container) = self.stack.last_mut() else {
            self.root_ended = true;
            return;
        };
        let top_key = self.top_key.clone();
        if matches!(container, Container::Object(_)) && stack_len == 1 {
            if top_key.as_deref() == Some("type") {
                if let Some(Value::String(event)) = &value {
                    self.event_type = Some(event.clone());
                }
            }
            if top_key.as_deref() == Some("willRetry") {
                if let Some(Value::Bool(flag)) = &value {
                    self.will_retry = Some(*flag);
                }
            }
            self.top_key = None;
        }
        match container {
            Container::Object(state) => *state = ObjectState::CommaOrEnd,
            Container::Array(state) => *state = ArrayState::CommaOrEnd,
        }
    }
}

/// `createBoundedLineReader` (child-protocol.ts:244-368): byte-level `\n`
/// splitting, per-line limit, oversized `turn_end`/`agent_end` projection,
/// 4 KiB diagnostic prefix/tail. The callbacks are generic so callers can
/// borrow their accumulation state directly (the reader never escapes the
/// scope that created it).
pub struct BoundedLineReader<OnLine, OnLimit>
where
    OnLine: FnMut(&str),
    OnLimit: FnMut(&ProtocolOutputLimit),
{
    stream: &'static str,
    max_pending_line_bytes: usize,
    on_line: OnLine,
    on_limit: OnLimit,
    pending: Vec<u8>,
    projecting: Option<AggregateProjection>,
    projected_bytes: usize,
    projected_prefix: Vec<u8>,
    projected_tail: Vec<u8>,
    limit_exceeded: bool,
}

impl<OnLine, OnLimit> BoundedLineReader<OnLine, OnLimit>
where
    OnLine: FnMut(&str),
    OnLimit: FnMut(&ProtocolOutputLimit),
{
    pub fn new(
        stream: &'static str,
        max_pending_line_bytes: usize,
        on_line: OnLine,
        on_limit: OnLimit,
    ) -> Self {
        Self {
            stream,
            max_pending_line_bytes,
            on_line,
            on_limit,
            pending: Vec::new(),
            projecting: None,
            projected_bytes: 0,
            projected_prefix: Vec::new(),
            projected_tail: Vec::new(),
            limit_exceeded: false,
        }
    }

    pub fn push(&mut self, chunk: &[u8]) {
        if self.limit_exceeded {
            return;
        }
        let mut start = 0;
        for (index, byte) in chunk.iter().enumerate() {
            if *byte != b'\n' {
                continue;
            }
            if !self.append(&chunk[start..index]) {
                return;
            }
            self.finish_line();
            if self.limit_exceeded {
                return;
            }
            start = index + 1;
        }
        self.append(&chunk[start..]);
    }

    pub fn end(&mut self) {
        if !self.limit_exceeded {
            self.finish_line();
        }
    }

    pub fn exceeded(&self) -> bool {
        self.limit_exceeded
    }

    fn diagnostic_tail(&self, prior: &[u8], segment: &[u8]) -> Vec<u8> {
        let tail_from_segment =
            &segment[segment.len().saturating_sub(MAX_PROTOCOL_DIAGNOSTIC_BYTES)..];
        if tail_from_segment.len() == MAX_PROTOCOL_DIAGNOSTIC_BYTES {
            tail_from_segment.to_vec()
        } else {
            let need = MAX_PROTOCOL_DIAGNOSTIC_BYTES - tail_from_segment.len();
            let mut out = prior[prior.len().saturating_sub(need)..].to_vec();
            out.extend_from_slice(tail_from_segment);
            out
        }
    }

    fn fail_limit(&mut self, observed_bytes: usize, prefix: Vec<u8>, tail: Vec<u8>) {
        self.limit_exceeded = true;
        self.pending.clear();
        self.projecting = None;
        self.projected_bytes = 0;
        self.projected_prefix.clear();
        self.projected_tail.clear();
        let limit = ProtocolOutputLimit {
            code: "protocol_output_limit",
            stream: self.stream,
            limit_bytes: self.max_pending_line_bytes,
            observed_bytes,
            diagnostic_prefix: String::from_utf8_lossy(&prefix).to_string(),
            diagnostic_tail: String::from_utf8_lossy(&tail).to_string(),
        };
        (self.on_limit)(&limit);
    }

    fn finish_line(&mut self) {
        if let Some(projection) = self.projecting.take() {
            match projection.finish() {
                Some(projected) => (self.on_line)(&projected),
                None => {
                    let prefix = std::mem::take(&mut self.projected_prefix);
                    let tail = std::mem::take(&mut self.projected_tail);
                    let bytes = self.projected_bytes;
                    self.projected_bytes = 0;
                    self.fail_limit(bytes, prefix, tail);
                }
            }
        } else if !self.pending.is_empty() {
            let line = String::from_utf8_lossy(&self.pending).to_string();
            self.pending.clear();
            (self.on_line)(&line);
        }
        self.pending.clear();
        self.projected_bytes = 0;
        self.projected_prefix.clear();
        self.projected_tail.clear();
    }

    fn append(&mut self, segment: &[u8]) -> bool {
        if segment.is_empty() {
            return true;
        }
        if self.projecting.is_some() {
            self.projected_bytes += segment.len();
            let prior = self.projected_tail.clone();
            let tail = self.diagnostic_tail(&prior, segment);
            self.projected_tail = tail;
            let ok = self
                .projecting
                .as_mut()
                .map(|projection| projection.push(segment))
                .unwrap_or(false);
            if !ok {
                let prefix = std::mem::take(&mut self.projected_prefix);
                let tail = std::mem::take(&mut self.projected_tail);
                let bytes = self.projected_bytes;
                self.projected_bytes = 0;
                self.fail_limit(bytes, prefix, tail);
            }
            return ok;
        }
        let observed = self.pending.len() + segment.len();
        if observed > self.max_pending_line_bytes {
            let mut prefix: Vec<u8> =
                self.pending[..self.pending.len().min(MAX_PROTOCOL_DIAGNOSTIC_BYTES)].to_vec();
            if prefix.len() < MAX_PROTOCOL_DIAGNOSTIC_BYTES {
                let need = MAX_PROTOCOL_DIAGNOSTIC_BYTES - prefix.len();
                prefix.extend_from_slice(&segment[..need.min(segment.len())]);
            }
            let prior = self.pending.clone();
            let tail = self.diagnostic_tail(&prior, segment);
            let prefix_text = String::from_utf8_lossy(&prefix).to_string();
            if let Some(mut candidate) = aggregate_projector_accepts(&prefix_text) {
                let prior = std::mem::take(&mut self.pending);
                if !candidate.push(&prior) || !candidate.push(segment) {
                    self.fail_limit(observed, prefix, tail);
                    return false;
                }
                self.projecting = Some(candidate);
                self.projected_prefix = prefix;
                self.projected_tail = tail;
                self.projected_bytes = observed;
                return true;
            }
            self.fail_limit(observed, prefix, tail);
            return false;
        }
        self.pending.extend_from_slice(segment);
        true
    }
}

fn aggregate_projector_accepts(prefix: &str) -> Option<AggregateProjection> {
    // PI_AGGREGATE_EVENT_PROJECTOR.accepts (child-protocol.ts:233-238).
    if prefix.starts_with(r#"{"type":"turn_end""#) || prefix.starts_with(r#"{"type":"agent_end""#) {
        Some(AggregateProjection::new())
    } else {
        None
    }
}

/// Accumulated child facts extracted from the JSONL stream (the `processLine`
/// subset, execution.ts:848-1007).
#[derive(Debug, Default, Clone)]
pub struct ChildRunState {
    pub messages: Vec<Value>,
    pub usage_input: u64,
    pub usage_output: u64,
    pub usage_cache_read: u64,
    pub usage_cache_write: u64,
    pub usage_cost_total: f64,
    pub model: Option<String>,
    pub tool_count: u64,
    pub turns: u64,
    pub agent_settled_received: bool,
    pub assistant_error: Option<String>,
}

/// One parsed line's contribution (mirrors processLine's per-event handling).
#[derive(Debug, Default, Clone)]
pub struct LineOutcome {
    pub lifecycle: ChildLifecycleAction,
}

impl ChildRunState {
    /// Process one stdout line. Non-JSON / non-object lines are ignored by the
    /// caller (they only feed the raw tail), same as upstream.
    pub fn process_line(&mut self, line: &str) -> LineOutcome {
        let mut outcome = LineOutcome::default();
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            return outcome;
        };
        let Some(event_type) = value.get("type").and_then(Value::as_str) else {
            return outcome;
        };
        match event_type {
            "agent_settled" => {
                self.agent_settled_received = true;
                outcome.lifecycle = project_child_lifecycle(Some(event_type), false, false);
            }
            "agent_end" => {
                outcome.lifecycle = project_child_lifecycle(
                    Some(event_type),
                    value.get("willRetry") == Some(&Value::Bool(true)),
                    false,
                );
            }
            "tool_execution_start" => {
                self.tool_count += 1;
            }
            "message_end" | "tool_result_end" => {
                self.process_message_end(&value, &mut outcome);
            }
            _ => {}
        }
        outcome
    }

    fn process_message_end(&mut self, event: &Value, outcome: &mut LineOutcome) {
        // `tool_result_end` carries the toolResult message under `message`.
        let Some(message) = event.get("message") else {
            return;
        };
        let role = message.get("role").and_then(Value::as_str).unwrap_or("");
        if role == "assistant" {
            self.turns += 1;
            if let Some(usage) = message.get("usage") {
                self.usage_input += usage.get("input").and_then(Value::as_u64).unwrap_or(0);
                self.usage_output += usage.get("output").and_then(Value::as_u64).unwrap_or(0);
                self.usage_cache_read +=
                    usage.get("cacheRead").and_then(Value::as_u64).unwrap_or(0);
                self.usage_cache_write +=
                    usage.get("cacheWrite").and_then(Value::as_u64).unwrap_or(0);
                self.usage_cost_total += usage
                    .get("cost")
                    .and_then(|cost| cost.get("total"))
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0);
            }
            if let Some(model) = message.get("model").and_then(Value::as_str) {
                self.model = Some(model.to_string());
            }
            if let Some(error) = message
                .get("errorMessage")
                .and_then(Value::as_str)
                .filter(|e| !e.is_empty())
            {
                self.assistant_error = Some(error.to_string());
            }
            // terminalAssistantStop (execution.ts:964-969): stopReason "stop"
            // with no toolCall blocks in the final message.
            let has_tool_call = message
                .get("content")
                .and_then(Value::as_array)
                .map(|blocks| {
                    blocks
                        .iter()
                        .any(|b| b.get("type").and_then(Value::as_str) == Some("toolCall"))
                })
                .unwrap_or(false);
            if message.get("stopReason").and_then(Value::as_str) == Some("stop") && !has_tool_call {
                outcome.lifecycle = project_child_lifecycle(None, false, true);
            }
        }
        self.messages.push(message.clone());
    }

    pub fn usage_json(&self) -> Value {
        json_usage(
            self.usage_input,
            self.usage_output,
            self.usage_cache_read,
            self.usage_cache_write,
            self.usage_cost_total,
        )
    }
}

pub fn json_usage(input: u64, output: u64, cache_read: u64, cache_write: u64, cost: f64) -> Value {
    serde_json::json!({
        "input": input,
        "output": output,
        "cacheRead": cache_read,
        "cacheWrite": cache_write,
        "cost": cost,
    })
}

/// `getFinalOutput` (utils.ts:301-328). Walks assistant messages from the end,
/// skipping errored ones; returns the whole message text when an acceptance
/// report shape is present, else the last non-empty text part seen (parts are
/// pushed in reverse order, so `valid_text_parts[0]` is the latest).
pub fn get_final_output(messages: &[Value]) -> String {
    let mut valid_text_parts: Vec<String> = Vec::new();
    for message in messages.iter().rev() {
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let has_error = message
            .get("errorMessage")
            .and_then(Value::as_str)
            .is_some_and(|e| !e.is_empty())
            || message.get("stopReason").and_then(Value::as_str) == Some("error");
        if has_error {
            continue;
        }
        let Some(blocks) = message.get("content").and_then(Value::as_array) else {
            continue;
        };
        let text_parts: Vec<&str> = blocks
            .iter()
            .filter_map(|block| {
                if block.get("type").and_then(Value::as_str) == Some("text") {
                    block.get("text").and_then(Value::as_str)
                } else {
                    None
                }
            })
            .collect();
        let message_text = text_parts
            .iter()
            .filter(|text| !text.trim().is_empty())
            .copied()
            .collect::<Vec<_>>()
            .join("\n");
        for block in blocks.iter().rev() {
            if block.get("type").and_then(Value::as_str) != Some("text") {
                continue;
            }
            let Some(text) = block.get("text").and_then(Value::as_str) else {
                continue;
            };
            if text.trim().is_empty() {
                continue;
            }
            valid_text_parts.push(text.to_string());
            if is_acceptance_report(text) {
                return message_text;
            }
        }
    }
    valid_text_parts.first().cloned().unwrap_or_default()
}

/// Acceptance report detection (utils.ts:314-327): fenced ```acceptance-report
/// blocks, an acceptance-shaped JSON fence, or an ACCEPTANCE_REPORT marker.
fn is_acceptance_report(text: &str) -> bool {
    if acceptance_fence(text).is_some() {
        return true;
    }
    for body in json_fences(text) {
        if body.contains("\"criteriaSatisfied\"") || body.contains("\"criteria_satisfied\"") {
            let markers = [
                "\"changedFiles\"",
                "\"changed_files\"",
                "\"testsAddedOrUpdated\"",
                "\"tests_added_or_updated\"",
                "\"commandsRun\"",
                "\"commands_run\"",
                "\"validationOutput\"",
                "\"validation_output\"",
                "\"residualRisks\"",
                "\"residual_risks\"",
                "\"noStagedFiles\"",
                "\"no_staged_files\"",
                "\"diffSummary\"",
                "\"diff_summary\"",
                "\"reviewFindings\"",
                "\"review_findings\"",
                "\"manualNotes\"",
                "\"manual_notes\"",
            ];
            if markers.iter().any(|marker| body.contains(marker)) {
                return true;
            }
        }
    }
    text.to_uppercase().contains("ACCEPTANCE_REPORT")
}

fn acceptance_fence(text: &str) -> Option<&str> {
    // /```acceptance[-_]report\s*\n[\s\S]*?```/i — a case-insensitive prefix
    // scan is sufficient for detection.
    let lowered = text.to_lowercase();
    let mut search_from = 0;
    while let Some(offset) = lowered[search_from..].find("```acceptance") {
        let start = search_from + offset;
        let rest = &lowered[start + "```acceptance".len()..];
        let rest = rest
            .strip_prefix('-')
            .or_else(|| rest.strip_prefix('_'))
            .unwrap_or(rest);
        let after = rest.trim_start();
        if after.starts_with("report") {
            return Some(&text[start..]);
        }
        search_from = start + 3;
    }
    None
}

fn json_fences(text: &str) -> Vec<String> {
    let mut fences = Vec::new();
    let lowered = text.to_lowercase();
    let mut search_from = 0;
    while let Some(offset) = lowered[search_from..].find("```json") {
        let start = search_from + offset + 7;
        let Some(end) = lowered[start..].find("```") else {
            break;
        };
        fences.push(text[start..start + end].to_string());
        search_from = start + end;
    }
    fences
}

#[derive(Debug, Clone, PartialEq)]
pub struct TruncationResult {
    pub text: String,
    pub truncated: bool,
    pub original_bytes: usize,
    pub original_lines: usize,
}

fn format_bytes(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// `truncateOutput` (types.ts:2162-2204): line cap first, then byte cap by
/// prefix, prepend the `[TRUNCATED: …]` marker.
pub fn truncate_output(
    output: &str,
    max_bytes: usize,
    max_lines: usize,
    artifact_path: Option<&str>,
) -> TruncationResult {
    let lines: Vec<&str> = output.split('\n').collect();
    let bytes = output.len();
    if bytes <= max_bytes && lines.len() <= max_lines {
        return TruncationResult {
            text: output.to_string(),
            truncated: false,
            original_bytes: bytes,
            original_lines: lines.len(),
        };
    }
    let truncated_lines: Vec<&str> = if lines.len() > max_lines {
        lines[..max_lines].to_vec()
    } else {
        lines.clone()
    };
    let mut result = truncated_lines.join("\n");
    if result.len() > max_bytes {
        // Binary search the longest prefix whose UTF-8 length fits.
        let mut low = 0usize;
        let mut high = result.len();
        while low < high {
            let mid = (low + high + 1).div_ceil(2);
            // Only test at char boundaries; skip mid not on a boundary.
            if !result.is_char_boundary(mid) {
                high = mid - 1;
                continue;
            }
            if result[..mid].len() <= max_bytes {
                low = mid;
            } else {
                high = mid - 1;
            }
        }
        let mut cut = low;
        while cut > 0 && !result.is_char_boundary(cut) {
            cut -= 1;
        }
        result.truncate(cut);
    }
    let kept_lines = result.split('\n').count();
    let marker = format!(
        "[TRUNCATED: showing first {} of {} lines, {} of {}{}]\n",
        kept_lines,
        lines.len(),
        format_bytes(result.len()),
        format_bytes(bytes),
        artifact_path
            .map(|path| format!(" - full output at {path}"))
            .unwrap_or_default()
    );
    TruncationResult {
        text: format!("{marker}{result}"),
        truncated: true,
        original_bytes: bytes,
        original_lines: lines.len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_tail_keeps_utf8_boundary() {
        let mut tail = BoundedByteTail::new(8);
        tail.push("abcdefgh".as_bytes());
        assert_eq!(tail.text(), "abcdefgh");
        tail.push("é".as_bytes()); // 2 bytes
                                   // 10 bytes > 8: drop 2 from the front ("ab"), é stays whole.
        assert_eq!(tail.text(), "cdefghé");
        assert_eq!(tail.byte_length(), 8);
    }

    #[test]
    fn lifecycle_projection_rules() {
        assert_eq!(
            project_child_lifecycle(Some("agent_end"), true, false),
            ChildLifecycleAction::CancelDrain
        );
        assert_eq!(
            project_child_lifecycle(Some("agent_settled"), false, false),
            ChildLifecycleAction::StartDrain
        );
        assert_eq!(
            project_child_lifecycle(None, false, true),
            ChildLifecycleAction::StartDrain
        );
        assert_eq!(
            project_child_lifecycle(Some("agent_end"), false, false),
            ChildLifecycleAction::None
        );
    }

    #[test]
    fn line_reader_splits_on_newline_across_chunks() {
        let mut lines = Vec::new();
        let mut exceeded = false;
        {
            let mut reader = BoundedLineReader::new(
                "stdout",
                MAX_CHILD_PENDING_LINE_BYTES,
                |line| lines.push(line.to_string()),
                |_| exceeded = true,
            );
            reader.push(b"{\"a\":1}\n{\"b\"");
            reader.push(b":2}\n");
            reader.end();
        }
        assert_eq!(lines, vec![r#"{"a":1}"#, r#"{"b":2}"#]);
        assert!(!exceeded);
    }

    #[test]
    fn line_reader_enforces_limit_with_diagnostics() {
        let mut limits = Vec::new();
        {
            let mut reader =
                BoundedLineReader::new("stdout", 16, |_| {}, |limit| limits.push(limit.clone()));
            reader.push(b"0123456789abcdef");
            reader.push(b"GH\n");
            reader.end();
        }
        assert!(limits[0].observed_bytes >= 18);
        assert!(limits[0]
            .message()
            .starts_with("protocol_output_limit: child stdout line exceeded 16 bytes"));
    }

    #[test]
    fn oversized_agent_end_projects_lifecycle_fields() {
        // An agent_end line larger than the limit but parseable by the
        // projection comes through with only type/willRetry.
        let mut lines = Vec::new();
        let mut limits = 0;
        {
            let mut reader = BoundedLineReader::new(
                "stdout",
                32,
                |line| lines.push(line.to_string()),
                |_| limits += 1,
            );
            let payload = format!(
                r#"{{"type":"agent_end","messages":["{}"],"willRetry":false}}"#,
                "x".repeat(64)
            );
            assert!(payload.len() > 32);
            reader.push(payload.as_bytes());
            reader.push(b"\n");
            reader.end();
        }
        assert_eq!(limits, 0);
        assert_eq!(lines, vec![r#"{"type":"agent_end","willRetry":false}"#]);
    }

    #[test]
    fn oversized_garbage_fails_the_limit() {
        let mut limits = 0;
        {
            let mut reader = BoundedLineReader::new("stdout", 16, |_| {}, |_| limits += 1);
            reader.push(b"not json at all!!!");
            reader.push(b"more junk\n");
        }
        assert_eq!(limits, 1);
    }

    fn assistant_message(texts: &[&str], stop_reason: &str, error: Option<&str>) -> Value {
        let content: Vec<Value> = texts
            .iter()
            .map(|t| serde_json::json!({"type": "text", "text": t}))
            .collect();
        serde_json::json!({
            "role": "assistant",
            "content": content,
            "stopReason": stop_reason,
            "errorMessage": error,
        })
    }

    #[test]
    fn final_output_takes_last_text_part_of_last_good_message() {
        let messages = vec![
            assistant_message(&["first"], "stop", None),
            assistant_message(&["middle", "the answer"], "stop", None),
        ];
        assert_eq!(get_final_output(&messages), "the answer");
        // Errored trailing messages are skipped.
        let messages = vec![
            assistant_message(&["good"], "stop", None),
            assistant_message(&["bad"], "error", Some("boom")),
        ];
        assert_eq!(get_final_output(&messages), "good");
        // Empty output for empty content.
        assert_eq!(get_final_output(&[]), "");
    }

    #[test]
    fn final_output_whole_message_for_acceptance_report() {
        let report = "Summary\n```acceptance-report\n{\"criteriaSatisfied\": true}\n```";
        let messages = vec![assistant_message(&[report, "final part"], "stop", None)];
        assert_eq!(get_final_output(&messages), format!("{report}\nfinal part"));
    }

    #[test]
    fn process_line_accumulates_usage_and_terminal_stop() {
        let mut state = ChildRunState::default();
        let outcome = state.process_line(
            r#"{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"hi"}],"usage":{"input":10,"output":5,"cacheRead":2,"cacheWrite":1,"cost":{"total":0.25}},"model":"faux/1","stopReason":"stop"}}"#,
        );
        assert_eq!(outcome.lifecycle, ChildLifecycleAction::StartDrain);
        assert_eq!(state.usage_input, 10);
        assert_eq!(state.usage_output, 5);
        assert_eq!(state.usage_cost_total, 0.25);
        assert_eq!(state.model.as_deref(), Some("faux/1"));
        assert_eq!(state.messages.len(), 1);
        let outcome = state.process_line(r#"{"type":"agent_settled"}"#);
        assert_eq!(outcome.lifecycle, ChildLifecycleAction::StartDrain);
        let outcome = state.process_line(r#"{"type":"agent_end","willRetry":true}"#);
        assert_eq!(outcome.lifecycle, ChildLifecycleAction::CancelDrain);
        let outcome = state.process_line("not json");
        assert_eq!(outcome.lifecycle, ChildLifecycleAction::None);
    }

    #[test]
    fn tool_call_in_final_message_is_not_terminal() {
        let mut state = ChildRunState::default();
        let message = serde_json::json!({
            "role": "assistant",
            "content": [{"type": "toolCall", "id": "t", "name": "read", "arguments": {}}],
            "stopReason": "stop"
        });
        let outcome = state.process_line(
            &serde_json::json!({
                "type": "message_end", "message": message
            })
            .to_string(),
        );
        assert_eq!(outcome.lifecycle, ChildLifecycleAction::None);
    }

    #[test]
    fn truncation_marker_and_caps() {
        let long = "a\n".repeat(10);
        let result = truncate_output(&long, 200_000, 5, Some("/tmp/out.md"));
        assert!(result.truncated);
        assert!(result
            .text
            .starts_with("[TRUNCATED: showing first 5 of 11 lines"));
        assert!(result.text.contains("- full output at /tmp/out.md]"));
        let big = "x".repeat(300 * 1024);
        let result = truncate_output(&big, 200 * 1024, 5000, None);
        assert!(result.truncated);
        assert_eq!(result.original_bytes, big.len());
        let short = "fine";
        let result = truncate_output(short, 200 * 1024, 5000, None);
        assert!(!result.truncated);
        assert_eq!(result.text, "fine");
    }
}
