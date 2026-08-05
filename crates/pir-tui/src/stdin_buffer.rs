//! Port of `packages/tui/src/stdin-buffer.ts` @ pi 0.82.1 (2efa728).
//!
//! `StdinBuffer` buffers input and emits complete sequences. stdin data can
//! arrive in partial chunks, especially for escape sequences like mouse
//! events; without buffering, partial sequences can be misinterpreted as
//! regular keypresses. `process()` feeds input data.
//!
//! Based on code from OpenTUI (https://github.com/anomalyco/opentui)
//! MIT License - Copyright (c) 2025 opentui
//!
//! Intentional differences:
//! - The EventEmitter `data`/`paste` events become [`StdinBufferEvent`] values
//!   returned by `process()`; the internal `setTimeout` flush timer becomes
//!   the explicit [`StdinBuffer::flush_deadline`] / [`StdinBuffer::flush_expired`]
//!   pair driven by the TUI event loop (crossterm poll with a timeout) instead
//!   of a background timer. Deadline-based flushing has exactly the upstream
//!   semantics: `process()` cancels a pending flush and reschedules it when an
//!   incomplete tail remains; `flush_expired(now)` flushes only once the
//!   deadline has passed.
//! - `process()` takes `&str`; the upstream `Buffer` input path (a single byte
//!   above 127 becomes ESC + (byte - 128), otherwise UTF-8 lossy decode) is
//!   exposed as `process_bytes()`.
//! - The Kitty duplicate-codepoint drop measures a sequence by UTF-16
//!   code-unit length (upstream `sequence.length === 1`). BMP characters
//!   such as `à` are one code unit and are still deduplicated. Astral
//!   characters are two code units and never enter the dedup check, which
//!   now matches upstream: there they are split into two lone surrogate
//!   halves (each `length === 1`), but `codePointAt(0)` yields a surrogate
//!   value that can never equal a pending printable codepoint, so neither
//!   half is ever dropped either. The only remaining difference is
//!   representational: upstream emits two lone surrogate half events
//!   (reassembled by downstream JS string concatenation), while Rust
//!   strings cannot hold lone surrogates and emit one complete `char`
//!   event instead — an accepted representation difference, with the
//!   character passed through to the consumer in both cases.

use std::time::{Duration, Instant};

const ESC: &str = "\x1b";
const BRACKETED_PASTE_START: &str = "\x1b[200~";
const BRACKETED_PASTE_END: &str = "\x1b[201~";

/// Whether a string is a complete escape sequence or needs more data
/// (upstream `isCompleteSequence`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SequenceStatus {
    Complete,
    Incomplete,
    NotEscape,
}

/// Check if a string is a complete escape sequence or needs more data.
fn is_complete_sequence(data: &str) -> SequenceStatus {
    if !data.starts_with(ESC) {
        return SequenceStatus::NotEscape;
    }

    if data.len() == 1 {
        return SequenceStatus::Incomplete;
    }

    let after_esc = &data[1..];

    // CSI sequences: ESC [
    if after_esc.starts_with('[') {
        // Check for old-style mouse sequence: ESC[M + 3 bytes
        if after_esc.starts_with("[M") {
            // Old-style mouse needs ESC[M + 3 bytes = 6 total
            return if data.len() >= 6 {
                SequenceStatus::Complete
            } else {
                SequenceStatus::Incomplete
            };
        }
        return if is_complete_csi_sequence(data) {
            SequenceStatus::Complete
        } else {
            SequenceStatus::Incomplete
        };
    }

    // OSC sequences: ESC ]
    if after_esc.starts_with(']') {
        return if is_complete_osc_sequence(data) {
            SequenceStatus::Complete
        } else {
            SequenceStatus::Incomplete
        };
    }

    // DCS sequences: ESC P ... ESC \ (includes XTVersion responses)
    if after_esc.starts_with('P') {
        return if is_complete_dcs_sequence(data) {
            SequenceStatus::Complete
        } else {
            SequenceStatus::Incomplete
        };
    }

    // APC sequences: ESC _ ... ESC \ (includes Kitty graphics responses)
    if after_esc.starts_with('_') {
        return if is_complete_apc_sequence(data) {
            SequenceStatus::Complete
        } else {
            SequenceStatus::Incomplete
        };
    }

    // SS3 sequences: ESC O
    if after_esc.starts_with('O') {
        // ESC O followed by a single character
        return if after_esc.len() >= 2 {
            SequenceStatus::Complete
        } else {
            SequenceStatus::Incomplete
        };
    }

    // Meta key sequences: ESC followed by a single character
    if after_esc.len() == 1 {
        return SequenceStatus::Complete;
    }

    // Unknown escape sequence - treat as complete
    SequenceStatus::Complete
}

/// Check if CSI sequence is complete.
/// CSI sequences: ESC [ ... followed by a final byte (0x40-0x7E)
fn is_complete_csi_sequence(data: &str) -> bool {
    if !data.starts_with("\x1b[") {
        return true;
    }

    // Need at least ESC [ and one more character
    if data.len() < 3 {
        return false;
    }

    let payload = &data[2..];

    // CSI sequences end with a byte in the range 0x40-0x7E (@-~)
    // This includes all letters and several special characters
    let last_char = match payload.chars().next_back() {
        Some(c) => c,
        None => return false,
    };
    let last_char_code = last_char as u32;

    if (0x40..=0x7e).contains(&last_char_code) {
        // Special handling for SGR mouse sequences
        // Format: ESC[<B;X;Ym or ESC[<B;X;YM
        if payload.starts_with('<') {
            // Must have format: <digits;digits;digits[Mm]
            if is_complete_mouse_sgr(payload) {
                return true;
            }
            // If it ends with M or m but doesn't match the pattern, still incomplete
            if last_char == 'M' || last_char == 'm' {
                // Check if we have the right structure
                let inner = &payload[1..payload.len() - last_char.len_utf8()];
                let parts: Vec<&str> = inner.split(';').collect();
                if parts.len() == 3 && parts.iter().all(|p| is_all_digits(p)) {
                    return true;
                }
            }

            return false;
        }

        return true;
    }

    false
}

/// Regex equivalent of `/^<\d+;\d+;\d+[Mm]$/` (JS `\d` is ASCII digits).
fn is_complete_mouse_sgr(payload: &str) -> bool {
    let Some(body) = payload.strip_prefix('<') else {
        return false;
    };
    let Some(last) = body.chars().next_back() else {
        return false;
    };
    if last != 'M' && last != 'm' {
        return false;
    }
    let inner = &body[..body.len() - last.len_utf8()];
    let parts: Vec<&str> = inner.split(';').collect();
    parts.len() == 3 && parts.iter().all(|p| is_all_digits(p))
}

/// Regex equivalent of `/^\d+$/` (non-empty ASCII digits).
fn is_all_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// Check if OSC sequence is complete.
/// OSC sequences: ESC ] ... ST (where ST is ESC \ or BEL)
fn is_complete_osc_sequence(data: &str) -> bool {
    if !data.starts_with("\x1b]") {
        return true;
    }

    // OSC sequences end with ST (ESC \) or BEL (\x07)
    data.ends_with("\x1b\\") || data.ends_with('\x07')
}

/// Check if DCS (Device Control String) sequence is complete.
/// DCS sequences: ESC P ... ST (where ST is ESC \)
/// Used for XTVersion responses like ESC P >| ... ESC \
fn is_complete_dcs_sequence(data: &str) -> bool {
    if !data.starts_with("\x1bP") {
        return true;
    }

    // DCS sequences end with ST (ESC \)
    data.ends_with("\x1b\\")
}

/// Check if APC (Application Program Command) sequence is complete.
/// APC sequences: ESC _ ... ST (where ST is ESC \)
/// Used for Kitty graphics responses like ESC _ G ... ESC \
fn is_complete_apc_sequence(data: &str) -> bool {
    if !data.starts_with("\x1b_") {
        return true;
    }

    // APC sequences end with ST (ESC \)
    data.ends_with("\x1b\\")
}

/// Regex equivalent of `/^\x1b\[(\d+)(?::\d*)?(?::\d+)?u$/` — an unmodified
/// Kitty CSI-u press sequence; returns the printable codepoint when >= 32.
fn parse_unmodified_kitty_printable_codepoint(sequence: &str) -> Option<u32> {
    let rest = sequence.strip_prefix("\x1b[")?.strip_suffix('u')?;

    let digits = rest.bytes().take_while(|b| b.is_ascii_digit()).count();
    if digits == 0 {
        return None;
    }
    let codepoint: u32 = rest[..digits].parse().ok()?;
    let tail = &rest[digits..];

    // The remainder must match `(?::\d*)?(?::\d+)?` in full: empty, or a
    // colon with optional digits, optionally followed by a colon with at
    // least one digit. (JS regex backtracking yields the same language.)
    let matches_tail = if tail.is_empty() {
        true
    } else if let Some(rest_tail) = tail.strip_prefix(':') {
        match rest_tail.find(':') {
            None => rest_tail.bytes().all(|b| b.is_ascii_digit()),
            Some(second_colon) => {
                let before = &rest_tail[..second_colon];
                let after = &rest_tail[second_colon + 1..];
                before.bytes().all(|b| b.is_ascii_digit())
                    && !after.is_empty()
                    && after.bytes().all(|b| b.is_ascii_digit())
            }
        }
    } else {
        false
    };

    if matches_tail && codepoint >= 32 {
        Some(codepoint)
    } else {
        None
    }
}

/// Split accumulated buffer into complete sequences.
fn extract_complete_sequences(buffer: &str) -> (Vec<String>, String) {
    let mut sequences: Vec<String> = Vec::new();
    let mut pos = 0;

    while pos < buffer.len() {
        let remaining = &buffer[pos..];

        // Try to extract a sequence starting at this position
        if remaining.starts_with(ESC) {
            // Find the end of this escape sequence
            let mut seq_end = 1;
            while seq_end <= remaining.len() {
                let candidate = &remaining[..seq_end];
                let status = is_complete_sequence(candidate);

                match status {
                    SequenceStatus::Complete => {
                        // WezTerm with enable_kitty_keyboard sends the Escape key
                        // press as a raw '\x1b' byte (simple text path in
                        // encode_kitty, ignoring DISAMBIGUATE_ESCAPE_CODES) and the
                        // release as a full Kitty CSI-u sequence. These arrive
                        // concatenated as '\x1b\x1b[27;...u'. The buffer would
                        // normally treat '\x1b\x1b' as a complete meta-key sequence
                        // (ESC + single char), leaving '[27;...u' to be typed as
                        // plain text. If the character immediately following
                        // '\x1b\x1b' would begin a new escape sequence, emit only
                        // the first ESC and restart from the second.
                        if candidate == "\x1b\x1b" {
                            let next_char = remaining[seq_end..].chars().next();
                            if matches!(
                                next_char,
                                Some('[') | Some(']') | Some('O') | Some('P') | Some('_')
                            ) {
                                sequences.push(ESC.to_string());
                                pos += 1;
                                break;
                            }
                        }
                        sequences.push(candidate.to_string());
                        pos += seq_end;
                        break;
                    }
                    SequenceStatus::Incomplete => {
                        // Advance by one full character (keeps byte indices on
                        // char boundaries; upstream iterates UTF-16 code units).
                        seq_end += remaining[seq_end..]
                            .chars()
                            .next()
                            .map_or(1, char::len_utf8);
                    }
                    SequenceStatus::NotEscape => {
                        // Should not happen when starting with ESC
                        sequences.push(candidate.to_string());
                        pos += seq_end;
                        break;
                    }
                }
            }

            if seq_end > remaining.len() {
                return (sequences, remaining.to_string());
            }
        } else {
            // Not an escape sequence - take a single character
            let ch = match remaining.chars().next() {
                Some(ch) => ch,
                // Unreachable: pos < buffer.len() guarantees a char here.
                None => break,
            };
            sequences.push(ch.to_string());
            pos += ch.len_utf8();
        }
    }

    (sequences, String::new())
}

/// Options for [`StdinBuffer`] (upstream `StdinBufferOptions`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StdinBufferOptions {
    /// Maximum time to wait for sequence completion (default: 10ms).
    /// After this time, the buffer is flushed even if incomplete.
    pub timeout: Duration,
}

impl Default for StdinBufferOptions {
    fn default() -> Self {
        Self {
            timeout: Duration::from_millis(10),
        }
    }
}

/// An event emitted by [`StdinBuffer::process`], mirroring the upstream
/// EventEmitter `data`/`paste` events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StdinBufferEvent {
    /// A complete sequence (`data` event).
    Data(String),
    /// A bracketed paste payload (`paste` event).
    Paste(String),
}

/// Buffers stdin input and emits complete sequences via `process()`.
/// Handles partial escape sequences that arrive across multiple chunks.
pub struct StdinBuffer {
    buffer: String,
    /// Rust equivalent of the upstream `setTimeout` flush: `Some` while a
    /// flush is scheduled; the buffer must be flushed once `now >= deadline`.
    deadline: Option<Instant>,
    timeout: Duration,
    paste_mode: bool,
    paste_buffer: String,
    pending_kitty_printable_codepoint: Option<u32>,
}

impl StdinBuffer {
    /// Creates a buffer with the given options (upstream constructor).
    pub fn new(options: StdinBufferOptions) -> Self {
        Self {
            buffer: String::new(),
            deadline: None,
            timeout: options.timeout,
            paste_mode: false,
            paste_buffer: String::new(),
            pending_kitty_printable_codepoint: None,
        }
    }

    /// Feed input data; returns the events emitted synchronously (upstream
    /// `data`/`paste` emissions, in order). Mirrors upstream `process()`:
    /// a pending flush deadline is cancelled first, and rescheduled at the
    /// end when an incomplete tail remains.
    pub fn process(&mut self, data: &str) -> Vec<StdinBufferEvent> {
        // Clear any pending timeout
        self.deadline = None;

        // Empty input with an empty buffer emits a single empty data event.
        if data.is_empty() && self.buffer.is_empty() {
            let mut events = Vec::new();
            self.emit_data_sequence("", &mut events);
            return events;
        }

        self.buffer.push_str(data);
        let mut events = Vec::new();

        if self.paste_mode {
            self.paste_buffer.push_str(&self.buffer);
            self.buffer.clear();

            if let Some(end_index) = self.paste_buffer.find(BRACKETED_PASTE_END) {
                let pasted_content = self.paste_buffer[..end_index].to_string();
                let remaining =
                    self.paste_buffer[end_index + BRACKETED_PASTE_END.len()..].to_string();

                self.paste_mode = false;
                self.paste_buffer.clear();
                self.pending_kitty_printable_codepoint = None;

                events.push(StdinBufferEvent::Paste(pasted_content));

                if !remaining.is_empty() {
                    events.extend(self.process(&remaining));
                }
            }
            return events;
        }

        let start_index = self.buffer.find(BRACKETED_PASTE_START);
        if let Some(start_index) = start_index {
            if start_index > 0 {
                let before_paste = &self.buffer[..start_index];
                let (sequences, _remainder) = extract_complete_sequences(before_paste);
                // Upstream discards `result.remainder` here: an incomplete
                // tail right before the paste start is dropped.
                for sequence in sequences {
                    self.emit_data_sequence(&sequence, &mut events);
                }
            }

            self.pending_kitty_printable_codepoint = None;
            self.buffer
                .drain(..start_index + BRACKETED_PASTE_START.len());
            self.paste_mode = true;
            self.paste_buffer = std::mem::take(&mut self.buffer);

            if let Some(end_index) = self.paste_buffer.find(BRACKETED_PASTE_END) {
                let pasted_content = self.paste_buffer[..end_index].to_string();
                let remaining =
                    self.paste_buffer[end_index + BRACKETED_PASTE_END.len()..].to_string();

                self.paste_mode = false;
                self.paste_buffer.clear();
                self.pending_kitty_printable_codepoint = None;

                events.push(StdinBufferEvent::Paste(pasted_content));

                if !remaining.is_empty() {
                    events.extend(self.process(&remaining));
                }
            }
            return events;
        }

        let (sequences, remainder) = extract_complete_sequences(&self.buffer);
        self.buffer = remainder;

        for sequence in sequences {
            self.emit_data_sequence(&sequence, &mut events);
        }

        if !self.buffer.is_empty() {
            self.deadline = Some(Instant::now() + self.timeout);
        }

        events
    }

    /// Feed raw bytes; the upstream `Buffer` input path: a single byte > 127
    /// becomes ESC + (byte - 128) (high-bit meta key), otherwise the bytes
    /// are decoded as UTF-8 with lossy replacement (upstream `toString()`).
    pub fn process_bytes(&mut self, data: &[u8]) -> Vec<StdinBufferEvent> {
        if data.len() == 1 && data[0] > 127 {
            let byte = data[0] - 128;
            let mut s = String::with_capacity(2);
            s.push('\x1b');
            s.push(char::from(byte));
            self.process(&s)
        } else {
            let s = String::from_utf8_lossy(data);
            self.process(&s)
        }
    }

    /// When `Some`, a flush is scheduled for the incomplete tail and the
    /// caller (TUI event loop) should poll input only until this instant,
    /// then call [`Self::flush_expired`]. Rust equivalent of the upstream
    /// `setTimeout` flush timer.
    pub fn flush_deadline(&self) -> Option<Instant> {
        self.deadline
    }

    /// Flush the buffer when the deadline (see [`Self::flush_deadline`]) has
    /// elapsed, returning the flushed data sequences; no-op before the
    /// deadline or when nothing is buffered. Rust equivalent of the upstream
    /// timer callback (flush + emit `data`).
    pub fn flush_expired(&mut self, now: Instant) -> Vec<String> {
        match self.deadline {
            Some(deadline) if now >= deadline => self.flush(),
            _ => Vec::new(),
        }
    }

    /// Flush the buffered tail as a single sequence (upstream `flush()`).
    pub fn flush(&mut self) -> Vec<String> {
        self.deadline = None;

        if self.buffer.is_empty() {
            return Vec::new();
        }

        let sequences = vec![std::mem::take(&mut self.buffer)];
        self.pending_kitty_printable_codepoint = None;
        sequences
    }

    /// Clear all buffered state without emitting anything (upstream
    /// `clear()`).
    pub fn clear(&mut self) {
        self.deadline = None;
        self.buffer.clear();
        self.paste_mode = false;
        self.paste_buffer.clear();
        self.pending_kitty_printable_codepoint = None;
    }

    /// The buffered, not yet emitted tail (upstream `getBuffer()`).
    pub fn get_buffer(&self) -> &str {
        &self.buffer
    }

    /// Release the buffer (upstream `destroy()`); identical to
    /// [`Self::clear`].
    pub fn destroy(&mut self) {
        self.clear();
    }

    /// Upstream `emitDataSequence`: drops a single raw character that
    /// duplicates the codepoint of the immediately preceding unmodified
    /// Kitty CSI-u sequence.
    fn emit_data_sequence(&mut self, sequence: &str, events: &mut Vec<StdinBufferEvent>) {
        let raw_codepoint = if sequence.encode_utf16().count() == 1 {
            sequence.chars().next().map(|c| c as u32)
        } else {
            None
        };
        if raw_codepoint.is_some() && raw_codepoint == self.pending_kitty_printable_codepoint {
            self.pending_kitty_printable_codepoint = None;
            return;
        }

        self.pending_kitty_printable_codepoint =
            parse_unmodified_kitty_printable_codepoint(sequence);
        events.push(StdinBufferEvent::Data(sequence.to_string()));
    }
}

impl Default for StdinBuffer {
    fn default() -> Self {
        Self::new(StdinBufferOptions::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Upstream tests construct `new StdinBuffer({ timeout: 10 })`; 10ms is
    /// also the default.
    fn new_buffer() -> StdinBuffer {
        StdinBuffer::default()
    }

    /// Upstream `emittedSequences` (the `data` events).
    fn data_events(events: &[StdinBufferEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|e| match e {
                StdinBufferEvent::Data(s) => Some(s.clone()),
                StdinBufferEvent::Paste(_) => None,
            })
            .collect()
    }

    /// Upstream `emittedPaste` (the `paste` events).
    fn paste_events(events: &[StdinBufferEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|e| match e {
                StdinBufferEvent::Paste(s) => Some(s.clone()),
                StdinBufferEvent::Data(_) => None,
            })
            .collect()
    }

    /// Simulates waiting longer than the 10ms flush timeout (upstream
    /// `await wait(15)`).
    fn past_timeout() -> Instant {
        Instant::now() + Duration::from_millis(100)
    }

    // --- Regular characters (upstream describe "Regular Characters") ---

    #[test]
    fn pass_through_regular_characters_immediately() {
        let mut buffer = new_buffer();
        assert_eq!(data_events(&buffer.process("a")), ["a"]);
    }

    #[test]
    fn pass_through_multiple_regular_characters() {
        let mut buffer = new_buffer();
        assert_eq!(data_events(&buffer.process("abc")), ["a", "b", "c"]);
    }

    #[test]
    fn handle_unicode_characters() {
        let mut buffer = new_buffer();
        assert_eq!(
            data_events(&buffer.process("hello 世界")),
            ["h", "e", "l", "l", "o", " ", "世", "界"]
        );
    }

    // --- Complete escape sequences (upstream describe "Complete Escape Sequences") ---

    #[test]
    fn pass_through_complete_mouse_sgr_sequences() {
        let mut buffer = new_buffer();
        let mouse_seq = "\x1b[<35;20;5m";
        assert_eq!(data_events(&buffer.process(mouse_seq)), [mouse_seq]);
    }

    #[test]
    fn pass_through_complete_arrow_key_sequences() {
        let mut buffer = new_buffer();
        let up_arrow = "\x1b[A";
        assert_eq!(data_events(&buffer.process(up_arrow)), [up_arrow]);
    }

    #[test]
    fn pass_through_complete_function_key_sequences() {
        let mut buffer = new_buffer();
        let f1 = "\x1b[11~";
        assert_eq!(data_events(&buffer.process(f1)), [f1]);
    }

    #[test]
    fn pass_through_meta_key_sequences() {
        let mut buffer = new_buffer();
        let meta_a = "\x1ba";
        assert_eq!(data_events(&buffer.process(meta_a)), [meta_a]);
    }

    #[test]
    fn pass_through_ss3_sequences() {
        let mut buffer = new_buffer();
        let ss3 = "\x1bOA";
        assert_eq!(data_events(&buffer.process(ss3)), [ss3]);
    }

    // --- Partial escape sequences (upstream describe "Partial Escape Sequences") ---

    #[test]
    fn buffer_incomplete_mouse_sgr_sequence() {
        let mut buffer = new_buffer();

        assert_eq!(buffer.process("\x1b"), []);
        assert_eq!(buffer.get_buffer(), "\x1b");

        assert_eq!(buffer.process("[<35"), []);
        assert_eq!(buffer.get_buffer(), "\x1b[<35");

        assert_eq!(data_events(&buffer.process(";20;5m")), ["\x1b[<35;20;5m"]);
        assert_eq!(buffer.get_buffer(), "");
    }

    #[test]
    fn buffer_incomplete_csi_sequence() {
        let mut buffer = new_buffer();
        assert_eq!(buffer.process("\x1b["), []);
        assert_eq!(buffer.process("1;"), []);
        assert_eq!(data_events(&buffer.process("5H")), ["\x1b[1;5H"]);
    }

    #[test]
    fn buffer_split_across_many_chunks() {
        let mut buffer = new_buffer();
        let mut events = Vec::new();
        for chunk in ["\x1b", "[", "<", "3", "5", ";", "2", "0", ";", "5", "m"] {
            events.extend(buffer.process(chunk));
        }
        assert_eq!(data_events(&events), ["\x1b[<35;20;5m"]);
    }

    #[test]
    fn flush_incomplete_sequence_after_timeout() {
        let mut buffer = new_buffer();
        assert_eq!(buffer.process("\x1b[<35"), []);

        // Upstream: `await wait(15)` then assert the flushed sequence.
        assert_eq!(buffer.flush_expired(past_timeout()), ["\x1b[<35"]);
    }

    // --- Mixed content (upstream describe "Mixed Content") ---

    #[test]
    fn handle_characters_followed_by_escape_sequence() {
        let mut buffer = new_buffer();
        assert_eq!(
            data_events(&buffer.process("abc\x1b[A")),
            ["a", "b", "c", "\x1b[A"]
        );
    }

    #[test]
    fn handle_escape_sequence_followed_by_characters() {
        let mut buffer = new_buffer();
        assert_eq!(
            data_events(&buffer.process("\x1b[Aabc")),
            ["\x1b[A", "a", "b", "c"]
        );
    }

    #[test]
    fn handle_multiple_complete_sequences() {
        let mut buffer = new_buffer();
        assert_eq!(
            data_events(&buffer.process("\x1b[A\x1b[B\x1b[C")),
            ["\x1b[A", "\x1b[B", "\x1b[C"]
        );
    }

    #[test]
    fn handle_partial_sequence_with_preceding_characters() {
        let mut buffer = new_buffer();
        // Upstream `emittedSequences` accumulates across process() calls.
        let mut events = buffer.process("abc\x1b[<35");
        assert_eq!(data_events(&events), ["a", "b", "c"]);
        assert_eq!(buffer.get_buffer(), "\x1b[<35");

        events.extend(buffer.process(";20;5m"));
        assert_eq!(data_events(&events), ["a", "b", "c", "\x1b[<35;20;5m"]);
    }

    // --- Kitty keyboard protocol (upstream describe "Kitty Keyboard Protocol") ---

    #[test]
    fn handle_kitty_csi_u_press_events() {
        let mut buffer = new_buffer();
        assert_eq!(data_events(&buffer.process("\x1b[97u")), ["\x1b[97u"]);
    }

    #[test]
    fn handle_kitty_csi_u_release_events() {
        let mut buffer = new_buffer();
        assert_eq!(
            data_events(&buffer.process("\x1b[97;1:3u")),
            ["\x1b[97;1:3u"]
        );
    }

    #[test]
    fn handle_batched_kitty_press_and_release() {
        let mut buffer = new_buffer();
        assert_eq!(
            data_events(&buffer.process("\x1b[97u\x1b[97;1:3u")),
            ["\x1b[97u", "\x1b[97;1:3u"]
        );
    }

    #[test]
    fn handle_multiple_batched_kitty_events() {
        let mut buffer = new_buffer();
        assert_eq!(
            data_events(&buffer.process("\x1b[97u\x1b[97;1:3u\x1b[98u\x1b[98;1:3u")),
            ["\x1b[97u", "\x1b[97;1:3u", "\x1b[98u", "\x1b[98;1:3u"]
        );
    }

    #[test]
    fn handle_kitty_arrow_keys_with_event_type() {
        let mut buffer = new_buffer();
        assert_eq!(data_events(&buffer.process("\x1b[1;1:1A")), ["\x1b[1;1:1A"]);
    }

    #[test]
    fn handle_kitty_functional_keys_with_event_type() {
        let mut buffer = new_buffer();
        assert_eq!(data_events(&buffer.process("\x1b[3;1:3~")), ["\x1b[3;1:3~"]);
    }

    #[test]
    fn split_esc_esc_csi_into_standalone_esc_and_csi_sequence() {
        // WezTerm with enable_kitty_keyboard sends Escape key press as raw \x1b
        // and the release as a full Kitty CSI-u sequence, concatenated.
        // The buffer must not treat \x1b\x1b as a complete meta-key when the
        // following byte starts a new escape sequence.
        let mut buffer = new_buffer();
        assert_eq!(
            data_events(&buffer.process("\x1b\x1b[27;129:3u")),
            ["\x1b", "\x1b[27;129:3u"]
        );
    }

    #[test]
    fn split_esc_esc_csi_with_no_modifier() {
        let mut buffer = new_buffer();
        assert_eq!(
            data_events(&buffer.process("\x1b\x1b[27;1:3u")),
            ["\x1b", "\x1b[27;1:3u"]
        );
    }

    #[test]
    fn still_emit_esc_esc_as_single_sequence_when_not_followed_by_new_escape() {
        // \x1b\x1b alone (no following CSI) stays as-is — e.g. ctrl+alt+[
        let mut buffer = new_buffer();
        assert_eq!(data_events(&buffer.process("\x1b\x1b")), ["\x1b\x1b"]);
    }

    #[test]
    fn handle_plain_characters_mixed_with_kitty_sequences() {
        let mut buffer = new_buffer();
        assert_eq!(
            data_events(&buffer.process("a\x1b[97;1:3u")),
            ["a", "\x1b[97;1:3u"]
        );
    }

    #[test]
    fn drop_raw_duplicate_character_after_matching_kitty_printable_sequence() {
        let mut buffer = new_buffer();
        assert_eq!(data_events(&buffer.process("\x1b[224uà")), ["\x1b[224u"]);
    }

    #[test]
    fn drop_raw_duplicate_character_after_matching_kitty_printable_sequence_across_chunks() {
        let mut buffer = new_buffer();
        assert_eq!(data_events(&buffer.process("\x1b[64u")), ["\x1b[64u"]);
        assert_eq!(buffer.process("@"), []);
    }

    #[test]
    fn keep_astral_character_after_matching_kitty_printable_sequence() {
        // Upstream splits an astral character into two lone surrogate halves
        // (each `length === 1`), whose surrogate code points never match the
        // pending printable codepoint, so the character is never dropped;
        // Rust emits it whole as a single char event.
        let mut buffer = new_buffer();
        assert_eq!(
            data_events(&buffer.process("\x1b[128512u😀")),
            ["\x1b[128512u", "😀"]
        );
    }

    #[test]
    fn keep_non_matching_plain_character_after_kitty_printable_sequence() {
        let mut buffer = new_buffer();
        assert_eq!(data_events(&buffer.process("\x1b[97ub")), ["\x1b[97u", "b"]);
    }

    #[test]
    fn keep_raw_character_after_modified_kitty_printable_sequence() {
        let mut buffer = new_buffer();
        assert_eq!(
            data_events(&buffer.process("\x1b[64;3u@")),
            ["\x1b[64;3u", "@"]
        );
    }

    #[test]
    fn handle_rapid_typing_simulation_with_kitty_protocol() {
        // Simulates typing "hi" quickly with releases interleaved
        let mut buffer = new_buffer();
        assert_eq!(
            data_events(&buffer.process("\x1b[104u\x1b[104;1:3u\x1b[105u\x1b[105;1:3u")),
            ["\x1b[104u", "\x1b[104;1:3u", "\x1b[105u", "\x1b[105;1:3u"]
        );
    }

    // --- Mouse events (upstream describe "Mouse Events") ---

    #[test]
    fn handle_mouse_press_event() {
        let mut buffer = new_buffer();
        assert_eq!(
            data_events(&buffer.process("\x1b[<0;10;5M")),
            ["\x1b[<0;10;5M"]
        );
    }

    #[test]
    fn handle_mouse_release_event() {
        let mut buffer = new_buffer();
        assert_eq!(
            data_events(&buffer.process("\x1b[<0;10;5m")),
            ["\x1b[<0;10;5m"]
        );
    }

    #[test]
    fn handle_mouse_move_event() {
        let mut buffer = new_buffer();
        assert_eq!(
            data_events(&buffer.process("\x1b[<35;20;5m")),
            ["\x1b[<35;20;5m"]
        );
    }

    #[test]
    fn handle_split_mouse_events() {
        let mut buffer = new_buffer();
        buffer.process("\x1b[<3");
        buffer.process("5;1");
        buffer.process("5;");
        assert_eq!(data_events(&buffer.process("10m")), ["\x1b[<35;15;10m"]);
    }

    #[test]
    fn handle_multiple_mouse_events() {
        let mut buffer = new_buffer();
        assert_eq!(
            data_events(&buffer.process("\x1b[<35;1;1m\x1b[<35;2;2m\x1b[<35;3;3m")),
            ["\x1b[<35;1;1m", "\x1b[<35;2;2m", "\x1b[<35;3;3m"]
        );
    }

    #[test]
    fn handle_old_style_mouse_sequence() {
        let mut buffer = new_buffer();
        assert_eq!(
            data_events(&buffer.process("\x1b[M abc")),
            ["\x1b[M ab", "c"]
        );
    }

    #[test]
    fn buffer_incomplete_old_style_mouse_sequence() {
        let mut buffer = new_buffer();
        buffer.process("\x1b[M");
        assert_eq!(buffer.get_buffer(), "\x1b[M");

        buffer.process(" a");
        assert_eq!(buffer.get_buffer(), "\x1b[M a");

        assert_eq!(data_events(&buffer.process("b")), ["\x1b[M ab"]);
    }

    // --- Edge cases (upstream describe "Edge Cases") ---

    #[test]
    fn handle_empty_input() {
        let mut buffer = new_buffer();
        // Empty input emits an empty data event
        assert_eq!(data_events(&buffer.process("")), [""]);
    }

    #[test]
    fn handle_lone_escape_character_with_timeout() {
        let mut buffer = new_buffer();
        assert_eq!(buffer.process("\x1b"), []);

        // After timeout, should emit
        assert_eq!(buffer.flush_expired(past_timeout()), ["\x1b"]);
    }

    #[test]
    fn handle_lone_escape_character_with_explicit_flush() {
        let mut buffer = new_buffer();
        assert_eq!(buffer.process("\x1b"), []);

        let flushed = buffer.flush();
        assert_eq!(flushed, ["\x1b"]);
    }

    #[test]
    fn handle_buffer_input() {
        let mut buffer = new_buffer();
        assert_eq!(data_events(&buffer.process_bytes(b"\x1b[A")), ["\x1b[A"]);
    }

    #[test]
    fn handle_single_high_byte_buffer_input() {
        // Upstream converts a single buffer byte > 127 to ESC + (byte - 128)
        // (high-bit meta key encoding), like parseKeypress expects.
        let mut buffer = new_buffer();
        assert_eq!(data_events(&buffer.process_bytes(&[0x89])), ["\x1b\t"]);
    }

    #[test]
    fn handle_very_long_sequences() {
        let mut buffer = new_buffer();
        let long_seq = format!("\x1b[{}H", "1;".repeat(50));
        assert_eq!(data_events(&buffer.process(&long_seq)), [long_seq.as_str()]);
    }

    // --- Flush (upstream describe "Flush") ---

    #[test]
    fn flush_incomplete_sequences() {
        let mut buffer = new_buffer();
        buffer.process("\x1b[<35");
        let flushed = buffer.flush();
        assert_eq!(flushed, ["\x1b[<35"]);
        assert_eq!(buffer.get_buffer(), "");
    }

    #[test]
    fn return_empty_array_if_nothing_to_flush() {
        let mut buffer = new_buffer();
        let flushed = buffer.flush();
        assert!(flushed.is_empty());
    }

    #[test]
    fn emit_flushed_data_via_timeout() {
        let mut buffer = new_buffer();
        assert_eq!(buffer.process("\x1b[<35"), []);

        // Wait for timeout to flush
        assert_eq!(buffer.flush_expired(past_timeout()), ["\x1b[<35"]);
    }

    #[test]
    fn does_not_flush_before_deadline() {
        // Rust-equivalent API check (upstream timer could not fire early):
        // flush_expired is a no-op until the 10ms timeout elapses.
        let mut buffer = new_buffer();
        buffer.process("\x1b[<35");
        assert!(buffer.flush_expired(Instant::now()).is_empty());
        assert_eq!(buffer.get_buffer(), "\x1b[<35");
    }

    // --- Clear (upstream describe "Clear") ---

    #[test]
    fn clear_buffered_content_without_emitting() {
        let mut buffer = new_buffer();
        buffer.process("\x1b[<35");
        assert_eq!(buffer.get_buffer(), "\x1b[<35");

        buffer.clear();
        assert_eq!(buffer.get_buffer(), "");
        // No pending flush survives clear() either.
        assert!(buffer.flush_expired(past_timeout()).is_empty());
    }

    // --- Bracketed paste (upstream describe "Bracketed Paste") ---

    #[test]
    fn emit_paste_event_for_complete_bracketed_paste() {
        let mut buffer = new_buffer();
        let events = buffer.process("\x1b[200~hello world\x1b[201~");
        assert_eq!(paste_events(&events), ["hello world"]);
        // No data events during paste
        assert!(data_events(&events).is_empty());
    }

    #[test]
    fn handle_paste_arriving_in_chunks() {
        let mut buffer = new_buffer();
        assert_eq!(buffer.process("\x1b[200~"), []);
        assert_eq!(buffer.process("hello "), []);
        let events = buffer.process("world\x1b[201~");
        assert_eq!(paste_events(&events), ["hello world"]);
        assert!(data_events(&events).is_empty());
    }

    #[test]
    fn handle_paste_with_input_before_and_after() {
        let mut buffer = new_buffer();
        assert_eq!(data_events(&buffer.process("a")), ["a"]);
        let events = buffer.process("\x1b[200~pasted\x1b[201~");
        assert_eq!(paste_events(&events), ["pasted"]);
        assert_eq!(data_events(&buffer.process("b")), ["b"]);
    }

    #[test]
    fn handle_paste_with_newlines() {
        let mut buffer = new_buffer();
        let events = buffer.process("\x1b[200~line1\nline2\nline3\x1b[201~");
        assert_eq!(paste_events(&events), ["line1\nline2\nline3"]);
        assert!(data_events(&events).is_empty());
    }

    #[test]
    fn handle_paste_with_unicode() {
        let mut buffer = new_buffer();
        let events = buffer.process("\x1b[200~Hello 世界 🎉\x1b[201~");
        assert_eq!(paste_events(&events), ["Hello 世界 🎉"]);
        assert!(data_events(&events).is_empty());
    }

    // --- Destroy (upstream describe "Destroy") ---

    #[test]
    fn clear_buffer_on_destroy() {
        let mut buffer = new_buffer();
        buffer.process("\x1b[<35");
        assert_eq!(buffer.get_buffer(), "\x1b[<35");

        buffer.destroy();
        assert_eq!(buffer.get_buffer(), "");
    }

    #[test]
    fn clear_pending_timeouts_on_destroy() {
        let mut buffer = new_buffer();
        buffer.process("\x1b[<35");
        buffer.destroy();

        // Wait longer than timeout: nothing is flushed.
        assert!(buffer.flush_expired(past_timeout()).is_empty());
    }
}
