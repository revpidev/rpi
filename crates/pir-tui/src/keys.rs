//! Port of `packages/tui/src/keys.ts` @ pi 0.82.1 (2efa728).
//!
//! Intentional differences:
//! - `_lastEventType` (keys.ts:520-521) is write-only upstream (assigned by
//!   `parseKittySequence`, never read — `isKeyRelease`/`isKeyRepeat` use plain
//!   substring checks) and is omitted; `ParsedKittySequence` still carries
//!   `shifted_key`/`event_type` (also never read upstream) to mirror the
//!   upstream struct, marked `#[allow(dead_code)]`.
//! - `Key` is a unit struct with associated constants/functions instead of a
//!   const object; `Key.super(...)` is `Key::super_key(...)` (`super` is a Rust
//!   keyword). `KeyId` is a plain `&str` (the TS template-literal type is
//!   erased at runtime).
//! - `parseKey` returns `Option<String>`; `decodeKittyPrintable` /
//!   `decodePrintableKey` return `Option<char>` (upstream `string | undefined`;
//!   every value upstream returns is a single code point).
//! - Kitty protocol state is an `AtomicBool`; `isWindowsTerminalSession` reads
//!   the same env vars (`WT_SESSION`, `SSH_CONNECTION`, `SSH_CLIENT`, `SSH_TTY`)
//!   via `std::env`.
//! - The `\d` regex groups of the upstream parsers are matched by a manual byte
//!   cursor parsing into `i32`; sequences whose numeric fields overflow `i32`
//!   parse as unmatched, which is observationally identical to upstream for
//!   every input a terminal emits (such fields never match upstream either).
//!
//! Keyboard input handling for terminal applications.
//!
//! Supports both legacy terminal sequences and Kitty keyboard protocol.
//! See: https://sw.kovidgoyal.net/kitty/keyboard-protocol/
//! Reference: https://github.com/sst/opentui/blob/7da92b4088aebfe27b9f691c04163a48821e49fd/packages/core/src/lib/parse.keypress.ts
//!
//! Symbol keys are also supported, however some ctrl+symbol combos
//! overlap with ASCII codes, e.g. ctrl+[ = ESC.
//! See: https://sw.kovidgoyal.net/kitty/keyboard-protocol/#legacy-ctrl-mapping-of-ascii-keys
//! Those can still be used for ctrl+shift combos.
//!
//! API:
//! - `matches_key(data, key_id)` — check if input matches a key identifier
//! - `parse_key(data)` — parse input and return the key identifier
//! - `Key` — helper object for creating typed key identifiers
//! - `set_kitty_protocol_active(active)` — set global Kitty protocol state
//! - `is_kitty_protocol_active()` — query global Kitty protocol state

use std::sync::atomic::{AtomicBool, Ordering};

// =============================================================================
// Global Kitty Protocol State
// =============================================================================

static KITTY_PROTOCOL_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Set the global Kitty keyboard protocol state.
/// Called by the terminal layer after detecting protocol support
/// (`setKittyProtocolActive`, keys.ts:31-33).
pub fn set_kitty_protocol_active(active: bool) {
    KITTY_PROTOCOL_ACTIVE.store(active, Ordering::Relaxed);
}

/// Query whether Kitty keyboard protocol is currently active
/// (`isKittyProtocolActive`, keys.ts:38-40).
pub fn is_kitty_protocol_active() -> bool {
    KITTY_PROTOCOL_ACTIVE.load(Ordering::Relaxed)
}

fn kitty_protocol_active() -> bool {
    is_kitty_protocol_active()
}

// =============================================================================
// Type-Safe Key Identifiers
// =============================================================================

/// Helper object for creating typed key identifiers with autocomplete
/// (`Key`, keys.ts:163-252).
///
/// Usage:
/// - `Key::ESCAPE`, `Key::ENTER`, `Key::TAB`, etc. for special keys
/// - `Key::BACKTICK`, `Key::COMMA`, `Key::PERIOD`, etc. for symbol keys
/// - `Key::ctrl("c")`, `Key::alt("x")`, `Key::super_key("k")` for single modifiers
/// - `Key::ctrl_shift("p")`, `Key::ctrl_alt("x")`, `Key::ctrl_super("k")` for
///   combined modifiers
///
/// Note: key identifiers are plain strings at runtime (the TS `KeyId`
/// template-literal type is compile-time only); pass them as `&str`.
pub struct Key;

impl Key {
    // Special keys
    pub const ESCAPE: &str = "escape";
    pub const ESC: &str = "esc";
    pub const ENTER: &str = "enter";
    pub const RETURN: &str = "return";
    pub const TAB: &str = "tab";
    pub const SPACE: &str = "space";
    pub const BACKSPACE: &str = "backspace";
    pub const DELETE: &str = "delete";
    pub const INSERT: &str = "insert";
    pub const CLEAR: &str = "clear";
    pub const HOME: &str = "home";
    pub const END: &str = "end";
    pub const PAGE_UP: &str = "pageUp";
    pub const PAGE_DOWN: &str = "pageDown";
    pub const UP: &str = "up";
    pub const DOWN: &str = "down";
    pub const LEFT: &str = "left";
    pub const RIGHT: &str = "right";
    pub const F1: &str = "f1";
    pub const F2: &str = "f2";
    pub const F3: &str = "f3";
    pub const F4: &str = "f4";
    pub const F5: &str = "f5";
    pub const F6: &str = "f6";
    pub const F7: &str = "f7";
    pub const F8: &str = "f8";
    pub const F9: &str = "f9";
    pub const F10: &str = "f10";
    pub const F11: &str = "f11";
    pub const F12: &str = "f12";

    // Symbol keys
    pub const BACKTICK: &str = "`";
    pub const HYPHEN: &str = "-";
    pub const EQUALS: &str = "=";
    pub const LEFT_BRACKET: &str = "[";
    pub const RIGHT_BRACKET: &str = "]";
    pub const BACKSLASH: &str = "\\";
    pub const SEMICOLON: &str = ";";
    pub const QUOTE: &str = "'";
    pub const COMMA: &str = ",";
    pub const PERIOD: &str = ".";
    pub const SLASH: &str = "/";
    pub const EXCLAMATION: &str = "!";
    pub const AT: &str = "@";
    pub const HASH: &str = "#";
    pub const DOLLAR: &str = "$";
    pub const PERCENT: &str = "%";
    pub const CARET: &str = "^";
    pub const AMPERSAND: &str = "&";
    pub const ASTERISK: &str = "*";
    pub const LEFT_PAREN: &str = "(";
    pub const RIGHT_PAREN: &str = ")";
    pub const UNDERSCORE: &str = "_";
    pub const PLUS: &str = "+";
    pub const PIPE: &str = "|";
    pub const TILDE: &str = "~";
    pub const LEFT_BRACE: &str = "{";
    pub const RIGHT_BRACE: &str = "}";
    pub const COLON: &str = ":";
    pub const LESS_THAN: &str = "<";
    pub const GREATER_THAN: &str = ">";
    pub const QUESTION: &str = "?";

    // Single modifiers
    pub fn ctrl(key: &str) -> String {
        format!("ctrl+{key}")
    }
    pub fn shift(key: &str) -> String {
        format!("shift+{key}")
    }
    pub fn alt(key: &str) -> String {
        format!("alt+{key}")
    }
    /// `Key.super` (keys.ts:233) — `super` is a Rust keyword, hence `super_key`.
    pub fn super_key(key: &str) -> String {
        format!("super+{key}")
    }

    // Combined modifiers
    pub fn ctrl_shift(key: &str) -> String {
        format!("ctrl+shift+{key}")
    }
    pub fn shift_ctrl(key: &str) -> String {
        format!("shift+ctrl+{key}")
    }
    pub fn ctrl_alt(key: &str) -> String {
        format!("ctrl+alt+{key}")
    }
    pub fn alt_ctrl(key: &str) -> String {
        format!("alt+ctrl+{key}")
    }
    pub fn shift_alt(key: &str) -> String {
        format!("shift+alt+{key}")
    }
    pub fn alt_shift(key: &str) -> String {
        format!("alt+shift+{key}")
    }
    pub fn ctrl_super(key: &str) -> String {
        format!("ctrl+super+{key}")
    }
    pub fn super_ctrl(key: &str) -> String {
        format!("super+ctrl+{key}")
    }
    pub fn shift_super(key: &str) -> String {
        format!("shift+super+{key}")
    }
    pub fn super_shift(key: &str) -> String {
        format!("super+shift+{key}")
    }
    pub fn alt_super(key: &str) -> String {
        format!("alt+super+{key}")
    }
    pub fn super_alt(key: &str) -> String {
        format!("super+alt+{key}")
    }

    // Triple modifiers
    pub fn ctrl_shift_alt(key: &str) -> String {
        format!("ctrl+shift+alt+{key}")
    }
    pub fn ctrl_shift_super(key: &str) -> String {
        format!("ctrl+shift+super+{key}")
    }
}

// =============================================================================
// Constants
// =============================================================================

fn is_symbol_key(ch: char) -> bool {
    matches!(
        ch,
        '`' | '-'
            | '='
            | '['
            | ']'
            | '\\'
            | ';'
            | '\''
            | ','
            | '.'
            | '/'
            | '!'
            | '@'
            | '#'
            | '$'
            | '%'
            | '^'
            | '&'
            | '*'
            | '('
            | ')'
            | '_'
            | '+'
            | '|'
            | '~'
            | '{'
            | '}'
            | ':'
            | '<'
            | '>'
            | '?'
    )
}

const MOD_SHIFT: i32 = 1;
const MOD_ALT: i32 = 2;
const MOD_CTRL: i32 = 4;
const MOD_SUPER: i32 = 8;

const LOCK_MASK: i32 = 64 + 128; // Caps Lock + Num Lock

const CODEPOINT_ESCAPE: i32 = 27;
const CODEPOINT_TAB: i32 = 9;
const CODEPOINT_ENTER: i32 = 13;
const CODEPOINT_SPACE: i32 = 32;
const CODEPOINT_BACKSPACE: i32 = 127;
const CODEPOINT_KP_ENTER: i32 = 57414; // Numpad Enter (Kitty protocol)

const ARROW_UP: i32 = -1;
const ARROW_DOWN: i32 = -2;
const ARROW_RIGHT: i32 = -3;
const ARROW_LEFT: i32 = -4;

const FUNCTIONAL_DELETE: i32 = -10;
const FUNCTIONAL_INSERT: i32 = -11;
const FUNCTIONAL_PAGE_UP: i32 = -12;
const FUNCTIONAL_PAGE_DOWN: i32 = -13;
const FUNCTIONAL_HOME: i32 = -14;
const FUNCTIONAL_END: i32 = -15;

/// `KITTY_FUNCTIONAL_KEY_EQUIVALENTS` (keys.ts:326-354).
fn normalize_kitty_functional_codepoint(codepoint: i32) -> i32 {
    match codepoint {
        57399 => 48, // KP_0 -> 0
        57400 => 49, // KP_1 -> 1
        57401 => 50, // KP_2 -> 2
        57402 => 51, // KP_3 -> 3
        57403 => 52, // KP_4 -> 4
        57404 => 53, // KP_5 -> 5
        57405 => 54, // KP_6 -> 6
        57406 => 55, // KP_7 -> 7
        57407 => 56, // KP_8 -> 8
        57408 => 57, // KP_9 -> 9
        57409 => 46, // KP_DECIMAL -> .
        57410 => 47, // KP_DIVIDE -> /
        57411 => 42, // KP_MULTIPLY -> *
        57412 => 45, // KP_SUBTRACT -> -
        57413 => 43, // KP_ADD -> +
        57415 => 61, // KP_EQUAL -> =
        57416 => 44, // KP_SEPARATOR -> ,
        57417 => ARROW_LEFT,
        57418 => ARROW_RIGHT,
        57419 => ARROW_UP,
        57420 => ARROW_DOWN,
        57421 => FUNCTIONAL_PAGE_UP,
        57422 => FUNCTIONAL_PAGE_DOWN,
        57423 => FUNCTIONAL_HOME,
        57424 => FUNCTIONAL_END,
        57425 => FUNCTIONAL_INSERT,
        57426 => FUNCTIONAL_DELETE,
        _ => codepoint,
    }
}

/// `normalizeShiftedLetterIdentityCodepoint` (keys.ts:360-366).
fn normalize_shifted_letter_identity_codepoint(codepoint: i32, modifier: i32) -> i32 {
    let effective_modifier = modifier & !LOCK_MASK;
    if effective_modifier & MOD_SHIFT != 0 && (65..=90).contains(&codepoint) {
        return codepoint + 32;
    }
    codepoint
}

/// `LEGACY_KEY_SEQUENCES` (keys.ts:368-392).
fn legacy_key_sequences(key: &str) -> Option<&'static [&'static str]> {
    Some(match key {
        "up" => &["\x1b[A", "\x1bOA"],
        "down" => &["\x1b[B", "\x1bOB"],
        "right" => &["\x1b[C", "\x1bOC"],
        "left" => &["\x1b[D", "\x1bOD"],
        "home" => &["\x1b[H", "\x1bOH", "\x1b[1~", "\x1b[7~"],
        "end" => &["\x1b[F", "\x1bOF", "\x1b[4~", "\x1b[8~"],
        "insert" => &["\x1b[2~"],
        "delete" => &["\x1b[3~"],
        "pageUp" => &["\x1b[5~", "\x1b[[5~"],
        "pageDown" => &["\x1b[6~", "\x1b[[6~"],
        "clear" => &["\x1b[E", "\x1bOE"],
        "f1" => &["\x1bOP", "\x1b[11~", "\x1b[[A"],
        "f2" => &["\x1bOQ", "\x1b[12~", "\x1b[[B"],
        "f3" => &["\x1bOR", "\x1b[13~", "\x1b[[C"],
        "f4" => &["\x1bOS", "\x1b[14~", "\x1b[[D"],
        "f5" => &["\x1b[15~", "\x1b[[E"],
        "f6" => &["\x1b[17~"],
        "f7" => &["\x1b[18~"],
        "f8" => &["\x1b[19~"],
        "f9" => &["\x1b[20~"],
        "f10" => &["\x1b[21~"],
        "f11" => &["\x1b[23~"],
        "f12" => &["\x1b[24~"],
        _ => return None,
    })
}

/// `LEGACY_SHIFT_SEQUENCES` (keys.ts:394-406).
fn legacy_shift_sequences(key: &str) -> Option<&'static [&'static str]> {
    Some(match key {
        "up" => &["\x1b[a"],
        "down" => &["\x1b[b"],
        "right" => &["\x1b[c"],
        "left" => &["\x1b[d"],
        "clear" => &["\x1b[e"],
        "insert" => &["\x1b[2$"],
        "delete" => &["\x1b[3$"],
        "pageUp" => &["\x1b[5$"],
        "pageDown" => &["\x1b[6$"],
        "home" => &["\x1b[7$"],
        "end" => &["\x1b[8$"],
        _ => return None,
    })
}

/// `LEGACY_CTRL_SEQUENCES` (keys.ts:408-420).
fn legacy_ctrl_sequences(key: &str) -> Option<&'static [&'static str]> {
    Some(match key {
        "up" => &["\x1bOa"],
        "down" => &["\x1bOb"],
        "right" => &["\x1bOc"],
        "left" => &["\x1bOd"],
        "clear" => &["\x1bOe"],
        "insert" => &["\x1b[2^"],
        "delete" => &["\x1b[3^"],
        "pageUp" => &["\x1b[5^"],
        "pageDown" => &["\x1b[6^"],
        "home" => &["\x1b[7^"],
        "end" => &["\x1b[8^"],
        _ => return None,
    })
}

fn matches_legacy_sequence(data: &str, sequences: &[&str]) -> bool {
    sequences.contains(&data)
}

fn matches_legacy_modifier_sequence(data: &str, key: &str, modifier: i32) -> bool {
    if modifier == MOD_SHIFT {
        return legacy_shift_sequences(key)
            .is_some_and(|sequences| matches_legacy_sequence(data, sequences));
    }
    if modifier == MOD_CTRL {
        return legacy_ctrl_sequences(key)
            .is_some_and(|sequences| matches_legacy_sequence(data, sequences));
    }
    false
}

/// `LEGACY_SEQUENCE_KEY_IDS` (keys.ts:422-481) — used by `parse_key` only.
fn legacy_sequence_key_id(data: &str) -> Option<&'static str> {
    Some(match data {
        "\x1bOA" => "up",
        "\x1bOB" => "down",
        "\x1bOC" => "right",
        "\x1bOD" => "left",
        "\x1bOH" => "home",
        "\x1bOF" => "end",
        "\x1b[E" => "clear",
        "\x1bOE" => "clear",
        "\x1bOe" => "ctrl+clear",
        "\x1b[e" => "shift+clear",
        "\x1b[2~" => "insert",
        "\x1b[2$" => "shift+insert",
        "\x1b[2^" => "ctrl+insert",
        "\x1b[3$" => "shift+delete",
        "\x1b[3^" => "ctrl+delete",
        "\x1b[[5~" => "pageUp",
        "\x1b[[6~" => "pageDown",
        "\x1b[a" => "shift+up",
        "\x1b[b" => "shift+down",
        "\x1b[c" => "shift+right",
        "\x1b[d" => "shift+left",
        "\x1bOa" => "ctrl+up",
        "\x1bOb" => "ctrl+down",
        "\x1bOc" => "ctrl+right",
        "\x1bOd" => "ctrl+left",
        "\x1b[5$" => "shift+pageUp",
        "\x1b[6$" => "shift+pageDown",
        "\x1b[7$" => "shift+home",
        "\x1b[8$" => "shift+end",
        "\x1b[5^" => "ctrl+pageUp",
        "\x1b[6^" => "ctrl+pageDown",
        "\x1b[7^" => "ctrl+home",
        "\x1b[8^" => "ctrl+end",
        "\x1bOP" => "f1",
        "\x1bOQ" => "f2",
        "\x1bOR" => "f3",
        "\x1bOS" => "f4",
        "\x1b[11~" => "f1",
        "\x1b[12~" => "f2",
        "\x1b[13~" => "f3",
        "\x1b[14~" => "f4",
        "\x1b[[A" => "f1",
        "\x1b[[B" => "f2",
        "\x1b[[C" => "f3",
        "\x1b[[D" => "f4",
        "\x1b[[E" => "f5",
        "\x1b[15~" => "f5",
        "\x1b[17~" => "f6",
        "\x1b[18~" => "f7",
        "\x1b[19~" => "f8",
        "\x1b[20~" => "f9",
        "\x1b[21~" => "f10",
        "\x1b[23~" => "f11",
        "\x1b[24~" => "f12",
        "\x1bb" => "alt+left",
        "\x1bf" => "alt+right",
        "\x1bp" => "alt+up",
        "\x1bn" => "alt+down",
        _ => return None,
    })
}

// =============================================================================
// Kitty Protocol Parsing
// =============================================================================

/// Event types from Kitty keyboard protocol (flag 2)
/// 1 = key press, 2 = key repeat, 3 = key release (`KeyEventType`, keys.ts:505).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyEventType {
    Press,
    Repeat,
    Release,
}

/// Fields `shiftedKey` and `eventType` are parsed by the upstream but never
/// read from this struct (decoding re-parses the raw CSI-u groups;
/// `isKeyRelease`/`isKeyRepeat` use substring checks); they are kept to mirror
/// the upstream structure, hence `dead_code` is allowed.
#[allow(dead_code)]
struct ParsedKittySequence {
    codepoint: i32,
    /// Shifted version of the key (when shift is pressed).
    shifted_key: Option<i32>,
    /// Key in standard PC-101 layout (for non-Latin layouts).
    base_layout_key: Option<i32>,
    modifier: i32,
    event_type: KeyEventType,
}

struct ParsedModifyOtherKeysSequence {
    codepoint: i32,
    modifier: i32,
}

/// Check if the last parsed key event was a key release
/// (`isKeyRelease`, keys.ts:527-551).
/// Only meaningful when Kitty keyboard protocol with flag 2 is active.
pub fn is_key_release(data: &str) -> bool {
    // Don't treat bracketed paste content as key release, even if it contains
    // patterns like ":3F" (e.g., bluetooth MAC addresses like "90:62:3F:A5").
    // The terminal layer re-wraps paste content with bracketed paste markers
    // before passing to the TUI, so pasted data will always contain \x1b[200~.
    if data.contains("\x1b[200~") {
        return false;
    }

    // Quick check: release events with flag 2 contain ":3"
    // Format: \x1b[<codepoint>;<modifier>:3u
    data.contains(":3u")
        || data.contains(":3~")
        || data.contains(":3A")
        || data.contains(":3B")
        || data.contains(":3C")
        || data.contains(":3D")
        || data.contains(":3H")
        || data.contains(":3F")
}

/// Check if the last parsed key event was a key repeat
/// (`isKeyRepeat`, keys.ts:557-577).
/// Only meaningful when Kitty keyboard protocol with flag 2 is active.
pub fn is_key_repeat(data: &str) -> bool {
    // Don't treat bracketed paste content as key repeat, even if it contains
    // patterns like ":2F". See is_key_release() for details.
    if data.contains("\x1b[200~") {
        return false;
    }

    data.contains(":2u")
        || data.contains(":2~")
        || data.contains(":2A")
        || data.contains(":2B")
        || data.contains(":2C")
        || data.contains(":2D")
        || data.contains(":2H")
        || data.contains(":2F")
}

fn parse_event_type(event: Option<i32>) -> KeyEventType {
    match event {
        Some(2) => KeyEventType::Repeat,
        Some(3) => KeyEventType::Release,
        _ => KeyEventType::Press,
    }
}

/// Minimal cursor over the raw bytes of an input sequence. JS `\d` matches
/// ASCII digits only, so a byte cursor is byte-identical to the anchored
/// regexes of keys.ts.
struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn next(&mut self) -> Option<u8> {
        let byte = self.bytes.get(self.pos).copied();
        if byte.is_some() {
            self.pos += 1;
        }
        byte
    }

    fn at_end(&self) -> bool {
        self.pos >= self.bytes.len()
    }

    /// Consumes a run of ASCII digits and parses them as `i32`.
    /// Returns None when fewer than `min` digits are present (the regex
    /// groups `(\d+)` use min=1, the `(\d*)` group uses min=0).
    fn parse_digits(&mut self, min: usize) -> Option<i32> {
        let start = self.pos;
        while self.peek().is_some_and(|b| b.is_ascii_digit()) {
            self.pos += 1;
        }
        if self.pos - start < min {
            return None;
        }
        std::str::from_utf8(&self.bytes[start..self.pos])
            .ok()?
            .parse()
            .ok()
    }
}

struct RawCsiU {
    codepoint: i32,
    shifted: Option<i32>,
    base: Option<i32>,
    modifier: Option<i32>,
    event: Option<i32>,
}

/// Matches the CSI-u regex `^\x1b\[(\d+)(?::(\d*))?(?::(\d+))?(?:;(\d+))?(?::(\d+))?u$`
/// (keys.ts:598).
fn parse_csi_u_sequence(data: &str) -> Option<RawCsiU> {
    let bytes = data.as_bytes();
    if !bytes.starts_with(b"\x1b[") {
        return None;
    }
    let mut cursor = Cursor { bytes, pos: 2 };
    let codepoint = cursor.parse_digits(1)?;
    let mut shifted = None;
    let mut base = None;
    if cursor.peek() == Some(b':') {
        cursor.next();
        shifted = cursor.parse_digits(0);
        if cursor.peek() == Some(b':') {
            cursor.next();
            base = Some(cursor.parse_digits(1)?);
        }
    }
    let mut modifier = None;
    if cursor.peek() == Some(b';') {
        cursor.next();
        modifier = Some(cursor.parse_digits(1)?);
    }
    let mut event = None;
    if cursor.peek() == Some(b':') {
        cursor.next();
        event = Some(cursor.parse_digits(1)?);
    }
    if cursor.next() != Some(b'u') || !cursor.at_end() {
        return None;
    }
    Some(RawCsiU {
        codepoint,
        shifted,
        base,
        modifier,
        event,
    })
}

/// Matches `^\x1b\[1;(\d+)(?::(\d+))?([ABCD])$` (keys.ts:610).
fn parse_arrow_sequence(data: &str) -> Option<ParsedKittySequence> {
    let bytes = data.as_bytes();
    if !bytes.starts_with(b"\x1b[1;") {
        return None;
    }
    let mut cursor = Cursor { bytes, pos: 4 };
    let modifier = cursor.parse_digits(1)? - 1;
    let mut event = None;
    if cursor.peek() == Some(b':') {
        cursor.next();
        event = Some(cursor.parse_digits(1)?);
    }
    let codepoint = match cursor.next()? {
        b'A' => ARROW_UP,
        b'B' => ARROW_DOWN,
        b'C' => ARROW_RIGHT,
        b'D' => ARROW_LEFT,
        _ => return None,
    };
    if !cursor.at_end() {
        return None;
    }
    Some(ParsedKittySequence {
        codepoint,
        shifted_key: None,
        base_layout_key: None,
        modifier,
        event_type: parse_event_type(event),
    })
}

/// Matches `^\x1b\[(\d+)(?:;(\d+))?(?::(\d+))?~$` (keys.ts:620).
/// Unknown key numbers yield None, mirroring the upstream fall-through when
/// `funcCodes[keyNum]` is undefined.
fn parse_functional_sequence(data: &str) -> Option<ParsedKittySequence> {
    let bytes = data.as_bytes();
    if !bytes.starts_with(b"\x1b[") {
        return None;
    }
    let mut cursor = Cursor { bytes, pos: 2 };
    let key_num = cursor.parse_digits(1)?;
    let mut modifier = None;
    if cursor.peek() == Some(b';') {
        cursor.next();
        modifier = Some(cursor.parse_digits(1)? - 1);
    }
    let mut event = None;
    if cursor.peek() == Some(b':') {
        cursor.next();
        event = Some(cursor.parse_digits(1)?);
    }
    if cursor.next() != Some(b'~') || !cursor.at_end() {
        return None;
    }
    let codepoint = match key_num {
        2 => FUNCTIONAL_INSERT,
        3 => FUNCTIONAL_DELETE,
        5 => FUNCTIONAL_PAGE_UP,
        6 => FUNCTIONAL_PAGE_DOWN,
        7 => FUNCTIONAL_HOME,
        8 => FUNCTIONAL_END,
        _ => return None,
    };
    Some(ParsedKittySequence {
        codepoint,
        shifted_key: None,
        base_layout_key: None,
        modifier: modifier.unwrap_or(0),
        event_type: parse_event_type(event),
    })
}

/// Matches `^\x1b\[1;(\d+)(?::(\d+))?([HF])$` (keys.ts:641).
fn parse_home_end_sequence(data: &str) -> Option<ParsedKittySequence> {
    let bytes = data.as_bytes();
    if !bytes.starts_with(b"\x1b[1;") {
        return None;
    }
    let mut cursor = Cursor { bytes, pos: 4 };
    let modifier = cursor.parse_digits(1)? - 1;
    let mut event = None;
    if cursor.peek() == Some(b':') {
        cursor.next();
        event = Some(cursor.parse_digits(1)?);
    }
    let codepoint = match cursor.next()? {
        b'H' => FUNCTIONAL_HOME,
        b'F' => FUNCTIONAL_END,
        _ => return None,
    };
    if !cursor.at_end() {
        return None;
    }
    Some(ParsedKittySequence {
        codepoint,
        shifted_key: None,
        base_layout_key: None,
        modifier,
        event_type: parse_event_type(event),
    })
}

/// `parseKittySequence` (keys.ts:587-651).
fn parse_kitty_sequence(data: &str) -> Option<ParsedKittySequence> {
    // CSI u format with alternate keys (flag 4):
    // \x1b[<codepoint>u
    // \x1b[<codepoint>;<mod>u
    // \x1b[<codepoint>;<mod>:<event>u
    // \x1b[<codepoint>:<shifted>;<mod>u
    // \x1b[<codepoint>:<shifted>:<base>;<mod>u
    // \x1b[<codepoint>::<base>;<mod>u (no shifted key, only base)
    //
    // With flag 2, event type is appended after modifier colon: 1=press, 2=repeat, 3=release
    // With flag 4, alternate keys are appended after codepoint with colons
    if let Some(raw) = parse_csi_u_sequence(data) {
        let modifier = raw.modifier.unwrap_or(1) - 1;
        return Some(ParsedKittySequence {
            codepoint: raw.codepoint,
            shifted_key: raw.shifted,
            base_layout_key: raw.base,
            modifier,
            event_type: parse_event_type(raw.event),
        });
    }

    // Arrow keys with modifier: \x1b[1;<mod>A/B/C/D or \x1b[1;<mod>:<event>A/B/C/D
    if let Some(parsed) = parse_arrow_sequence(data) {
        return Some(parsed);
    }

    // Functional keys: \x1b[<num>~ or \x1b[<num>;<mod>~ or \x1b[<num>;<mod>:<event>~
    if let Some(parsed) = parse_functional_sequence(data) {
        return Some(parsed);
    }

    // Home/End with modifier: \x1b[1;<mod>H/F or \x1b[1;<mod>:<event>H/F
    parse_home_end_sequence(data)
}

/// `matchesKittySequence` (keys.ts:653-694).
fn matches_kitty_sequence(data: &str, expected_codepoint: i32, expected_modifier: i32) -> bool {
    let Some(parsed) = parse_kitty_sequence(data) else {
        return false;
    };
    let actual_mod = parsed.modifier & !LOCK_MASK;
    let expected_mod = expected_modifier & !LOCK_MASK;

    // Check if modifiers match
    if actual_mod != expected_mod {
        return false;
    }

    let normalized_codepoint = normalize_shifted_letter_identity_codepoint(
        normalize_kitty_functional_codepoint(parsed.codepoint),
        parsed.modifier,
    );
    let normalized_expected_codepoint = normalize_shifted_letter_identity_codepoint(
        normalize_kitty_functional_codepoint(expected_codepoint),
        expected_modifier,
    );

    // Primary match: codepoint matches directly after normalizing functional keys
    if normalized_codepoint == normalized_expected_codepoint {
        return true;
    }

    // Alternate match: use base layout key for non-Latin keyboard layouts.
    // This allows Ctrl+С (Cyrillic) to match Ctrl+c (Latin) when terminal reports
    // the base layout key (the key in standard PC-101 layout).
    //
    // Only fall back to base layout key when the codepoint is NOT already a
    // recognized Latin letter (a-z) or symbol (e.g., /, -, [, ;, etc.).
    // When the codepoint is a recognized key, it is authoritative regardless
    // of physical key position. This prevents remapped layouts (Dvorak, Colemak,
    // xremap, etc.) from causing false matches: both letters and symbols move
    // to different physical positions, so Ctrl+K could falsely match Ctrl+V
    // (letter remapping) and Ctrl+/ could falsely match Ctrl+[ (symbol remapping)
    // if the base layout key were always considered.
    if let Some(base_layout_key) = parsed.base_layout_key {
        if base_layout_key == expected_codepoint {
            let cp = normalized_codepoint;
            let is_latin_letter = (97..=122).contains(&cp); // a-z
            let is_known_symbol = char::from_u32(cp as u32).is_some_and(is_symbol_key);
            if !is_latin_letter && !is_known_symbol {
                return true;
            }
        }
    }

    false
}

/// Matches `^\x1b\[27;(\d+);(\d+)~$` (keys.ts:697).
fn parse_modify_other_keys_sequence(data: &str) -> Option<ParsedModifyOtherKeysSequence> {
    let bytes = data.as_bytes();
    if !bytes.starts_with(b"\x1b[27;") {
        return None;
    }
    let mut cursor = Cursor { bytes, pos: 5 };
    let modifier = cursor.parse_digits(1)? - 1;
    if cursor.next() != Some(b';') {
        return None;
    }
    let codepoint = cursor.parse_digits(1)?;
    if cursor.next() != Some(b'~') || !cursor.at_end() {
        return None;
    }
    Some(ParsedModifyOtherKeysSequence {
        codepoint,
        modifier,
    })
}

/// Match xterm modifyOtherKeys format: CSI 27 ; modifiers ; keycode ~
/// (`matchesModifyOtherKeys`, keys.ts:709-713).
/// This is used by terminals when Kitty protocol is not enabled.
/// Modifier values are 1-indexed: 2=shift, 3=alt, 5=ctrl, etc.
fn matches_modify_other_keys(data: &str, expected_keycode: i32, expected_modifier: i32) -> bool {
    let Some(parsed) = parse_modify_other_keys_sequence(data) else {
        return false;
    };
    parsed.codepoint == expected_keycode && parsed.modifier == expected_modifier
}

/// `isWindowsTerminalSession` (keys.ts:715-719).
fn is_windows_terminal_session() -> bool {
    env_is_set_and_nonempty("WT_SESSION")
        && env_is_unset_or_empty("SSH_CONNECTION")
        && env_is_unset_or_empty("SSH_CLIENT")
        && env_is_unset_or_empty("SSH_TTY")
}

fn env_is_set_and_nonempty(name: &str) -> bool {
    std::env::var(name).is_ok_and(|value| !value.is_empty())
}

fn env_is_unset_or_empty(name: &str) -> bool {
    !env_is_set_and_nonempty(name)
}

/// Raw 0x08 (BS) is ambiguous in legacy terminals
/// (`matchesRawBackspace`, keys.ts:730-734).
///
/// - Windows Terminal uses it for Ctrl+Backspace.
/// - Some legacy terminals and tmux setups send it for plain Backspace.
///
/// Prefer explicit Kitty / CSI-u / modifyOtherKeys sequences whenever they are
/// available. Fall back to a Windows Terminal heuristic only for raw BS bytes.
fn matches_raw_backspace(data: &str, expected_modifier: i32) -> bool {
    if data == "\x7f" {
        return expected_modifier == 0;
    }
    if data != "\x08" {
        return false;
    }
    if is_windows_terminal_session() {
        expected_modifier == MOD_CTRL
    } else {
        expected_modifier == 0
    }
}

// =============================================================================
// Generic Key Matching
// =============================================================================

/// Get the control character for a key (`rawCtrlChar`, keys.ts:749-760).
/// Uses the universal formula: code & 0x1f (mask to lower 5 bits)
///
/// Works for:
/// - Letters a-z → 1-26
/// - Symbols [\]_ → 27, 28, 29, 31
/// - Also maps - to same as _ (same physical key on US keyboards)
fn raw_ctrl_char(ch: char) -> Option<char> {
    let ch = ch.to_ascii_lowercase();
    let code = ch as u32;
    if (97..=122).contains(&code) || matches!(ch, '[' | '\\' | ']' | '_') {
        return Some((code & 0x1f) as u8 as char);
    }
    // Handle - as _ (same physical key on US keyboards)
    if ch == '-' {
        return Some('\x1f'); // Same as Ctrl+_
    }
    None
}

fn is_digit_key(ch: char) -> bool {
    ch.is_ascii_digit()
}

/// `matchesPrintableModifyOtherKeys` (keys.ts:766-774).
fn matches_printable_modify_other_keys(
    data: &str,
    expected_keycode: i32,
    expected_modifier: i32,
) -> bool {
    if expected_modifier == 0 {
        return false;
    }
    let Some(parsed) = parse_modify_other_keys_sequence(data) else {
        return false;
    };
    if parsed.modifier != expected_modifier {
        return false;
    }
    normalize_shifted_letter_identity_codepoint(parsed.codepoint, parsed.modifier)
        == normalize_shifted_letter_identity_codepoint(expected_keycode, expected_modifier)
}

/// `formatKeyNameWithModifiers` (keys.ts:776-786).
fn format_key_name_with_modifiers(key_name: &str, modifier: i32) -> Option<String> {
    let mut mods: Vec<&str> = Vec::new();
    let effective_mod = modifier & !LOCK_MASK;
    let supported_modifier_mask = MOD_SHIFT | MOD_CTRL | MOD_ALT | MOD_SUPER;
    if effective_mod & !supported_modifier_mask != 0 {
        return None;
    }
    if effective_mod & MOD_SHIFT != 0 {
        mods.push("shift");
    }
    if effective_mod & MOD_CTRL != 0 {
        mods.push("ctrl");
    }
    if effective_mod & MOD_ALT != 0 {
        mods.push("alt");
    }
    if effective_mod & MOD_SUPER != 0 {
        mods.push("super");
    }
    if mods.is_empty() {
        Some(key_name.to_string())
    } else {
        Some(format!("{}+{}", mods.join("+"), key_name))
    }
}

struct ParsedKeyId {
    key: String,
    ctrl: bool,
    shift: bool,
    alt: bool,
    super_modifier: bool,
}

/// `parseKeyId` (keys.ts:788-801). Key identifiers are matched
/// case-insensitively; modifier flags come from the split parts.
fn parse_key_id(key_id: &str) -> Option<ParsedKeyId> {
    let lower = key_id.to_lowercase();
    let parts: Vec<&str> = lower.split('+').collect();
    let key = *parts.last()?;
    if key.is_empty() {
        return None;
    }
    Some(ParsedKeyId {
        key: key.to_string(),
        ctrl: parts.contains(&"ctrl"),
        shift: parts.contains(&"shift"),
        alt: parts.contains(&"alt"),
        super_modifier: parts.contains(&"super"),
    })
}

/// Match input data against a key identifier string (`matchesKey`, keys.ts:820-1204).
///
/// Supported key identifiers:
/// - Single keys: "escape", "tab", "enter", "backspace", "delete", "home", "end", "space"
/// - Arrow keys: "up", "down", "left", "right"
/// - Ctrl combinations: "ctrl+c", "ctrl+z", etc.
/// - Shift combinations: "shift+tab", "shift+enter"
/// - Alt combinations: "alt+enter", "alt+backspace"
/// - Super combinations: "super+k", "super+enter"
/// - Combined modifiers: "shift+ctrl+p", "ctrl+alt+x", "ctrl+super+k"
///
/// Use the `Key` helper for constructing identifiers:
/// `Key::ctrl("c")`, `Key::ESCAPE`, `Key::ctrl_shift("p")`, `Key::super_key("k")`.
///
/// `key_id` — key identifier (e.g., "ctrl+c", "escape", `Key::ctrl("c")`).
pub fn matches_key(data: &str, key_id: &str) -> bool {
    let Some(parsed) = parse_key_id(key_id) else {
        return false;
    };

    let key = parsed.key;
    let mut modifier = 0;
    if parsed.shift {
        modifier |= MOD_SHIFT;
    }
    if parsed.alt {
        modifier |= MOD_ALT;
    }
    if parsed.ctrl {
        modifier |= MOD_CTRL;
    }
    if parsed.super_modifier {
        modifier |= MOD_SUPER;
    }

    match key.as_str() {
        "escape" | "esc" => {
            if modifier != 0 {
                return false;
            }
            data == "\x1b"
                || matches_kitty_sequence(data, CODEPOINT_ESCAPE, 0)
                || matches_modify_other_keys(data, CODEPOINT_ESCAPE, 0)
        }
        "space" => {
            if !kitty_protocol_active() {
                if modifier == MOD_CTRL && data == "\x00" {
                    return true;
                }
                if modifier == MOD_ALT && data == "\x1b " {
                    return true;
                }
            }
            if modifier == 0 {
                return data == " "
                    || matches_kitty_sequence(data, CODEPOINT_SPACE, 0)
                    || matches_modify_other_keys(data, CODEPOINT_SPACE, 0);
            }
            matches_kitty_sequence(data, CODEPOINT_SPACE, modifier)
                || matches_modify_other_keys(data, CODEPOINT_SPACE, modifier)
        }
        "tab" => {
            if modifier == MOD_SHIFT {
                return data == "\x1b[Z"
                    || matches_kitty_sequence(data, CODEPOINT_TAB, MOD_SHIFT)
                    || matches_modify_other_keys(data, CODEPOINT_TAB, MOD_SHIFT);
            }
            if modifier == 0 {
                return data == "\t" || matches_kitty_sequence(data, CODEPOINT_TAB, 0);
            }
            matches_kitty_sequence(data, CODEPOINT_TAB, modifier)
                || matches_modify_other_keys(data, CODEPOINT_TAB, modifier)
        }
        "enter" | "return" => {
            if modifier == MOD_SHIFT {
                // CSI u sequences (standard Kitty protocol)
                if matches_kitty_sequence(data, CODEPOINT_ENTER, MOD_SHIFT)
                    || matches_kitty_sequence(data, CODEPOINT_KP_ENTER, MOD_SHIFT)
                {
                    return true;
                }
                // xterm modifyOtherKeys format (fallback when Kitty protocol not enabled)
                if matches_modify_other_keys(data, CODEPOINT_ENTER, MOD_SHIFT) {
                    return true;
                }
                // When Kitty protocol is active, legacy sequences are custom terminal mappings
                // \x1b\r = Kitty's "map shift+enter send_text all \e\r"
                // \n = Ghostty's "keybind = shift+enter=text:\n"
                if kitty_protocol_active() {
                    return data == "\x1b\r" || data == "\n";
                }
                return false;
            }
            if modifier == MOD_ALT {
                // CSI u sequences (standard Kitty protocol)
                if matches_kitty_sequence(data, CODEPOINT_ENTER, MOD_ALT)
                    || matches_kitty_sequence(data, CODEPOINT_KP_ENTER, MOD_ALT)
                {
                    return true;
                }
                // xterm modifyOtherKeys format (fallback when Kitty protocol not enabled)
                if matches_modify_other_keys(data, CODEPOINT_ENTER, MOD_ALT) {
                    return true;
                }
                // \x1b\r is alt+enter only in legacy mode (no Kitty protocol)
                // When Kitty protocol is active, alt+enter comes as CSI u sequence
                if !kitty_protocol_active() {
                    return data == "\x1b\r";
                }
                return false;
            }
            if modifier == 0 {
                return data == "\r"
                    || (!kitty_protocol_active() && data == "\n")
                    || data == "\x1bOM" // SS3 M (numpad enter in some terminals)
                    || matches_kitty_sequence(data, CODEPOINT_ENTER, 0)
                    || matches_kitty_sequence(data, CODEPOINT_KP_ENTER, 0);
            }
            matches_kitty_sequence(data, CODEPOINT_ENTER, modifier)
                || matches_kitty_sequence(data, CODEPOINT_KP_ENTER, modifier)
                || matches_modify_other_keys(data, CODEPOINT_ENTER, modifier)
        }
        "backspace" => {
            if modifier == MOD_ALT {
                if data == "\x1b\x7f" || data == "\x1b\x08" {
                    return true;
                }
                return matches_kitty_sequence(data, CODEPOINT_BACKSPACE, MOD_ALT)
                    || matches_modify_other_keys(data, CODEPOINT_BACKSPACE, MOD_ALT);
            }
            if modifier == MOD_CTRL {
                // Legacy raw 0x08 is ambiguous: it can be Ctrl+Backspace on Windows
                // Terminal or plain Backspace on other terminals, while also
                // overlapping with Ctrl+H.
                if matches_raw_backspace(data, MOD_CTRL) {
                    return true;
                }
                return matches_kitty_sequence(data, CODEPOINT_BACKSPACE, MOD_CTRL)
                    || matches_modify_other_keys(data, CODEPOINT_BACKSPACE, MOD_CTRL);
            }
            if modifier == 0 {
                return matches_raw_backspace(data, 0)
                    || matches_kitty_sequence(data, CODEPOINT_BACKSPACE, 0)
                    || matches_modify_other_keys(data, CODEPOINT_BACKSPACE, 0);
            }
            matches_kitty_sequence(data, CODEPOINT_BACKSPACE, modifier)
                || matches_modify_other_keys(data, CODEPOINT_BACKSPACE, modifier)
        }
        "insert" => {
            if modifier == 0 {
                return legacy_key_sequences("insert")
                    .is_some_and(|sequences| matches_legacy_sequence(data, sequences))
                    || matches_kitty_sequence(data, FUNCTIONAL_INSERT, 0);
            }
            if matches_legacy_modifier_sequence(data, "insert", modifier) {
                return true;
            }
            matches_kitty_sequence(data, FUNCTIONAL_INSERT, modifier)
        }
        "delete" => {
            if modifier == 0 {
                return legacy_key_sequences("delete")
                    .is_some_and(|sequences| matches_legacy_sequence(data, sequences))
                    || matches_kitty_sequence(data, FUNCTIONAL_DELETE, 0);
            }
            if matches_legacy_modifier_sequence(data, "delete", modifier) {
                return true;
            }
            matches_kitty_sequence(data, FUNCTIONAL_DELETE, modifier)
        }
        "clear" => {
            if modifier == 0 {
                return legacy_key_sequences("clear")
                    .is_some_and(|sequences| matches_legacy_sequence(data, sequences));
            }
            matches_legacy_modifier_sequence(data, "clear", modifier)
        }
        "home" => {
            if modifier == 0 {
                return legacy_key_sequences("home")
                    .is_some_and(|sequences| matches_legacy_sequence(data, sequences))
                    || matches_kitty_sequence(data, FUNCTIONAL_HOME, 0);
            }
            if matches_legacy_modifier_sequence(data, "home", modifier) {
                return true;
            }
            matches_kitty_sequence(data, FUNCTIONAL_HOME, modifier)
        }
        "end" => {
            if modifier == 0 {
                return legacy_key_sequences("end")
                    .is_some_and(|sequences| matches_legacy_sequence(data, sequences))
                    || matches_kitty_sequence(data, FUNCTIONAL_END, 0);
            }
            if matches_legacy_modifier_sequence(data, "end", modifier) {
                return true;
            }
            matches_kitty_sequence(data, FUNCTIONAL_END, modifier)
        }
        "pageup" => {
            if modifier == 0 {
                return legacy_key_sequences("pageUp")
                    .is_some_and(|sequences| matches_legacy_sequence(data, sequences))
                    || matches_kitty_sequence(data, FUNCTIONAL_PAGE_UP, 0);
            }
            if matches_legacy_modifier_sequence(data, "pageUp", modifier) {
                return true;
            }
            matches_kitty_sequence(data, FUNCTIONAL_PAGE_UP, modifier)
        }
        "pagedown" => {
            if modifier == 0 {
                return legacy_key_sequences("pageDown")
                    .is_some_and(|sequences| matches_legacy_sequence(data, sequences))
                    || matches_kitty_sequence(data, FUNCTIONAL_PAGE_DOWN, 0);
            }
            if matches_legacy_modifier_sequence(data, "pageDown", modifier) {
                return true;
            }
            matches_kitty_sequence(data, FUNCTIONAL_PAGE_DOWN, modifier)
        }
        "up" => {
            if modifier == MOD_ALT {
                return data == "\x1bp" || matches_kitty_sequence(data, ARROW_UP, MOD_ALT);
            }
            if modifier == 0 {
                return legacy_key_sequences("up")
                    .is_some_and(|sequences| matches_legacy_sequence(data, sequences))
                    || matches_kitty_sequence(data, ARROW_UP, 0);
            }
            if matches_legacy_modifier_sequence(data, "up", modifier) {
                return true;
            }
            matches_kitty_sequence(data, ARROW_UP, modifier)
        }
        "down" => {
            if modifier == MOD_ALT {
                return data == "\x1bn" || matches_kitty_sequence(data, ARROW_DOWN, MOD_ALT);
            }
            if modifier == 0 {
                return legacy_key_sequences("down")
                    .is_some_and(|sequences| matches_legacy_sequence(data, sequences))
                    || matches_kitty_sequence(data, ARROW_DOWN, 0);
            }
            if matches_legacy_modifier_sequence(data, "down", modifier) {
                return true;
            }
            matches_kitty_sequence(data, ARROW_DOWN, modifier)
        }
        "left" => {
            if modifier == MOD_ALT {
                return data == "\x1b[1;3D"
                    || (!kitty_protocol_active() && data == "\x1bB")
                    || data == "\x1bb"
                    || matches_kitty_sequence(data, ARROW_LEFT, MOD_ALT);
            }
            if modifier == MOD_CTRL {
                return data == "\x1b[1;5D"
                    || matches_legacy_modifier_sequence(data, "left", MOD_CTRL)
                    || matches_kitty_sequence(data, ARROW_LEFT, MOD_CTRL);
            }
            if modifier == 0 {
                return legacy_key_sequences("left")
                    .is_some_and(|sequences| matches_legacy_sequence(data, sequences))
                    || matches_kitty_sequence(data, ARROW_LEFT, 0);
            }
            if matches_legacy_modifier_sequence(data, "left", modifier) {
                return true;
            }
            matches_kitty_sequence(data, ARROW_LEFT, modifier)
        }
        "right" => {
            if modifier == MOD_ALT {
                return data == "\x1b[1;3C"
                    || (!kitty_protocol_active() && data == "\x1bF")
                    || data == "\x1bf"
                    || matches_kitty_sequence(data, ARROW_RIGHT, MOD_ALT);
            }
            if modifier == MOD_CTRL {
                return data == "\x1b[1;5C"
                    || matches_legacy_modifier_sequence(data, "right", MOD_CTRL)
                    || matches_kitty_sequence(data, ARROW_RIGHT, MOD_CTRL);
            }
            if modifier == 0 {
                return legacy_key_sequences("right")
                    .is_some_and(|sequences| matches_legacy_sequence(data, sequences))
                    || matches_kitty_sequence(data, ARROW_RIGHT, 0);
            }
            if matches_legacy_modifier_sequence(data, "right", modifier) {
                return true;
            }
            matches_kitty_sequence(data, ARROW_RIGHT, modifier)
        }
        "f1" | "f2" | "f3" | "f4" | "f5" | "f6" | "f7" | "f8" | "f9" | "f10" | "f11" | "f12" => {
            if modifier != 0 {
                return false;
            }
            legacy_key_sequences(key.as_str())
                .is_some_and(|sequences| matches_legacy_sequence(data, sequences))
        }
        _ => {
            // Handle single letter/digit keys and symbols
            if key.len() == 1 {
                let ch = key.as_bytes()[0] as char;
                let is_letter = ch.is_ascii_lowercase();
                let is_digit = is_digit_key(ch);
                if is_letter || is_digit || is_symbol_key(ch) {
                    let codepoint = ch as i32;
                    let raw_ctrl = raw_ctrl_char(ch);

                    if modifier == MOD_CTRL + MOD_ALT && !kitty_protocol_active() {
                        // Legacy: ctrl+alt+key is ESC followed by the control character.
                        // If that legacy form does not match, continue so CSI-u and
                        // modifyOtherKeys sequences from tmux can still be recognized.
                        if let Some(ctrl_char) = raw_ctrl {
                            if data.len() == 2
                                && data.as_bytes()[0] == b'\x1b'
                                && data.as_bytes()[1] == ctrl_char as u8
                            {
                                return true;
                            }
                        }
                    }

                    if modifier == MOD_ALT
                        && !kitty_protocol_active()
                        && (is_letter || is_digit || is_symbol_key(ch))
                    {
                        // Legacy: alt+printable key is ESC followed by the key
                        if data.len() == 2
                            && data.as_bytes()[0] == b'\x1b'
                            && data.as_bytes()[1] == ch as u8
                        {
                            return true;
                        }
                    }

                    if modifier == MOD_CTRL {
                        // Legacy: ctrl+key sends the control character
                        if let Some(ctrl_char) = raw_ctrl {
                            if data.len() == 1 && data.as_bytes()[0] == ctrl_char as u8 {
                                return true;
                            }
                        }
                        return matches_kitty_sequence(data, codepoint, MOD_CTRL)
                            || matches_printable_modify_other_keys(data, codepoint, MOD_CTRL);
                    }

                    if modifier == MOD_SHIFT + MOD_CTRL {
                        return matches_kitty_sequence(data, codepoint, MOD_SHIFT + MOD_CTRL)
                            || matches_printable_modify_other_keys(
                                data,
                                codepoint,
                                MOD_SHIFT + MOD_CTRL,
                            );
                    }

                    if modifier == MOD_SHIFT {
                        // Legacy: shift+letter produces uppercase
                        if is_letter
                            && data.len() == 1
                            && data.as_bytes()[0] == ch.to_ascii_uppercase() as u8
                        {
                            return true;
                        }
                        return matches_kitty_sequence(data, codepoint, MOD_SHIFT)
                            || matches_printable_modify_other_keys(data, codepoint, MOD_SHIFT);
                    }

                    if modifier != 0 {
                        return matches_kitty_sequence(data, codepoint, modifier)
                            || matches_printable_modify_other_keys(data, codepoint, modifier);
                    }

                    // Check both raw char and Kitty sequence (needed for release events)
                    return (data.len() == 1 && data.as_bytes()[0] == ch as u8)
                        || matches_kitty_sequence(data, codepoint, 0);
                }
            }
            false
        }
    }
}

/// `formatParsedKey` (keys.ts:1212-1249).
fn format_parsed_key(
    codepoint: i32,
    modifier: i32,
    base_layout_key: Option<i32>,
) -> Option<String> {
    let normalized_codepoint = normalize_kitty_functional_codepoint(codepoint);
    let identity_codepoint =
        normalize_shifted_letter_identity_codepoint(normalized_codepoint, modifier);

    // Use base layout key only when codepoint is not a recognized Latin
    // letter (a-z), digit (0-9), or symbol (/, -, [, ;, etc.). For those,
    // the codepoint is authoritative regardless of physical key position.
    // This prevents remapped layouts (Dvorak, Colemak, xremap, etc.) from
    // reporting the wrong key name based on the QWERTY physical position.
    let is_latin_letter = (97..=122).contains(&identity_codepoint); // a-z
    let is_digit = (48..=57).contains(&identity_codepoint); // 0-9
    let is_known_symbol = char::from_u32(identity_codepoint as u32).is_some_and(is_symbol_key);
    let effective_codepoint = if is_latin_letter || is_digit || is_known_symbol {
        identity_codepoint
    } else {
        base_layout_key.unwrap_or(identity_codepoint)
    };

    let key_name: Option<String> = match effective_codepoint {
        CODEPOINT_ESCAPE => Some("escape".to_string()),
        CODEPOINT_TAB => Some("tab".to_string()),
        CODEPOINT_ENTER | CODEPOINT_KP_ENTER => Some("enter".to_string()),
        CODEPOINT_SPACE => Some("space".to_string()),
        CODEPOINT_BACKSPACE => Some("backspace".to_string()),
        FUNCTIONAL_DELETE => Some("delete".to_string()),
        FUNCTIONAL_INSERT => Some("insert".to_string()),
        FUNCTIONAL_HOME => Some("home".to_string()),
        FUNCTIONAL_END => Some("end".to_string()),
        FUNCTIONAL_PAGE_UP => Some("pageUp".to_string()),
        FUNCTIONAL_PAGE_DOWN => Some("pageDown".to_string()),
        ARROW_UP => Some("up".to_string()),
        ARROW_DOWN => Some("down".to_string()),
        ARROW_LEFT => Some("left".to_string()),
        ARROW_RIGHT => Some("right".to_string()),
        cp if (48..=57).contains(&cp)
            || (97..=122).contains(&cp)
            || char::from_u32(cp as u32).is_some_and(is_symbol_key) =>
        {
            char::from_u32(cp as u32).map(|c| c.to_string())
        }
        _ => None,
    };
    let key_name = key_name?;
    format_key_name_with_modifiers(&key_name, modifier)
}

/// Parse input data and return the key identifier if recognized (`parseKey`, keys.ts:1251-1327).
///
/// Returns the key identifier string (e.g., "ctrl+c") or None.
pub fn parse_key(data: &str) -> Option<String> {
    let kitty = parse_kitty_sequence(data);
    if let Some(kitty) = kitty {
        return format_parsed_key(kitty.codepoint, kitty.modifier, kitty.base_layout_key);
    }

    let modify_other_keys = parse_modify_other_keys_sequence(data);
    if let Some(modify_other_keys) = modify_other_keys {
        return format_parsed_key(
            modify_other_keys.codepoint,
            modify_other_keys.modifier,
            None,
        );
    }

    // Mode-aware legacy sequences
    // When Kitty protocol is active, ambiguous sequences are interpreted as custom terminal mappings:
    // - \x1b\r = shift+enter (Kitty mapping), not alt+enter
    // - \n = shift+enter (Ghostty mapping)
    if kitty_protocol_active() && (data == "\x1b\r" || data == "\n") {
        return Some("shift+enter".to_string());
    }

    if let Some(legacy_sequence_key_id) = legacy_sequence_key_id(data) {
        return Some(legacy_sequence_key_id.to_string());
    }

    // Legacy sequences (used when Kitty protocol is not active, or for unambiguous sequences)
    if data == "\x1b" {
        return Some("escape".to_string());
    }
    if data == "\x1c" {
        return Some("ctrl+\\".to_string());
    }
    if data == "\x1d" {
        return Some("ctrl+]".to_string());
    }
    if data == "\x1f" {
        return Some("ctrl+-".to_string());
    }
    if data == "\x1b\x1b" {
        return Some("ctrl+alt+[".to_string());
    }
    if data == "\x1b\x1c" {
        return Some("ctrl+alt+\\".to_string());
    }
    if data == "\x1b\x1d" {
        return Some("ctrl+alt+]".to_string());
    }
    if data == "\x1b\x1f" {
        return Some("ctrl+alt+-".to_string());
    }
    if data == "\t" {
        return Some("tab".to_string());
    }
    if data == "\r" || (!kitty_protocol_active() && data == "\n") || data == "\x1bOM" {
        return Some("enter".to_string());
    }
    if data == "\x00" {
        return Some("ctrl+space".to_string());
    }
    if data == " " {
        return Some("space".to_string());
    }
    if data == "\x7f" {
        return Some("backspace".to_string());
    }
    if data == "\x08" {
        return Some(
            if is_windows_terminal_session() {
                "ctrl+backspace"
            } else {
                "backspace"
            }
            .to_string(),
        );
    }
    if data == "\x1b[Z" {
        return Some("shift+tab".to_string());
    }
    if !kitty_protocol_active() && data == "\x1b\r" {
        return Some("alt+enter".to_string());
    }
    if !kitty_protocol_active() && data == "\x1b " {
        return Some("alt+space".to_string());
    }
    if data == "\x1b\x7f" || data == "\x1b\x08" {
        return Some("alt+backspace".to_string());
    }
    if !kitty_protocol_active() && data == "\x1bB" {
        return Some("alt+left".to_string());
    }
    if !kitty_protocol_active() && data == "\x1bF" {
        return Some("alt+right".to_string());
    }
    if !kitty_protocol_active() && data.len() == 2 && data.starts_with('\x1b') {
        let code = data.as_bytes()[1] as i32;
        if (1..=26).contains(&code) {
            return Some(format!("ctrl+alt+{}", (code as u8 + 96) as char));
        }
        // Legacy alt+letter/digit/symbol (ESC followed by the key)
        let key = data.as_bytes()[1] as char;
        if (97..=122).contains(&code) || (48..=57).contains(&code) || is_symbol_key(key) {
            return Some(format!("alt+{key}"));
        }
    }
    if data == "\x1b[A" {
        return Some("up".to_string());
    }
    if data == "\x1b[B" {
        return Some("down".to_string());
    }
    if data == "\x1b[C" {
        return Some("right".to_string());
    }
    if data == "\x1b[D" {
        return Some("left".to_string());
    }
    if data == "\x1b[H" || data == "\x1bOH" {
        return Some("home".to_string());
    }
    if data == "\x1b[F" || data == "\x1bOF" {
        return Some("end".to_string());
    }
    if data == "\x1b[3~" {
        return Some("delete".to_string());
    }
    if data == "\x1b[5~" {
        return Some("pageUp".to_string());
    }
    if data == "\x1b[6~" {
        return Some("pageDown".to_string());
    }

    // Raw Ctrl+letter
    if data.len() == 1 {
        let code = data.as_bytes()[0] as i32;
        if (1..=26).contains(&code) {
            return Some(format!("ctrl+{}", (code as u8 + 96) as char));
        }
        if (32..=126).contains(&code) {
            return Some(data.to_string());
        }
    }

    None
}

// =============================================================================
// Kitty CSI-u Printable Decoding
// =============================================================================

const KITTY_PRINTABLE_ALLOWED_MODIFIERS: i32 = MOD_SHIFT | LOCK_MASK;

/// Decode a Kitty CSI-u sequence into a printable character, if applicable
/// (`decodeKittyPrintable`, keys.ts:1350-1383).
///
/// When Kitty keyboard protocol flag 1 (disambiguate) is active, terminals send
/// CSI-u sequences for all keys, including plain printable characters. This
/// function extracts the printable character from such sequences.
///
/// Only accepts plain or Shift-modified keys. Rejects Ctrl, Alt, and unsupported
/// modifier combinations (those are handled by keybinding matching instead).
/// Prefers the shifted keycode when Shift is held and a shifted key is reported.
pub fn decode_kitty_printable(data: &str) -> Option<char> {
    let raw = parse_csi_u_sequence(data)?;

    // CSI-u groups: <codepoint>[:<shifted>[:<base>]];<mod>[:<event>]u
    let codepoint = raw.codepoint;
    let shifted_key = raw.shifted;
    // Modifiers are 1-indexed in CSI-u; normalize to our bitmask.
    let modifier = raw.modifier.unwrap_or(1) - 1;

    // Only accept printable CSI-u input for plain or Shift-modified text keys.
    // Reject unsupported modifier bits (e.g. Super/Meta) to avoid inserting
    // characters from modifier-only terminal events.
    if modifier & !KITTY_PRINTABLE_ALLOWED_MODIFIERS != 0 {
        return None;
    }
    if modifier & (MOD_ALT | MOD_CTRL) != 0 {
        return None;
    }

    // Prefer the shifted keycode when Shift is held.
    let mut effective_codepoint = codepoint;
    if modifier & MOD_SHIFT != 0 {
        if let Some(shifted_key) = shifted_key {
            effective_codepoint = shifted_key;
        }
    }
    effective_codepoint = normalize_kitty_functional_codepoint(effective_codepoint);
    // Drop control characters or invalid codepoints.
    if effective_codepoint < 32 {
        return None;
    }

    char::from_u32(effective_codepoint as u32)
}

fn decode_modify_other_keys_printable(data: &str) -> Option<char> {
    let parsed = parse_modify_other_keys_sequence(data)?;
    let modifier = parsed.modifier & !LOCK_MASK;
    if modifier & !MOD_SHIFT != 0 {
        return None;
    }
    if parsed.codepoint < 32 {
        return None;
    }

    char::from_u32(parsed.codepoint as u32)
}

/// `decodePrintableKey` (keys.ts:1399-1401).
pub fn decode_printable_key(data: &str) -> Option<char> {
    decode_kitty_printable(data).or_else(|| decode_modify_other_keys_printable(data))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};

    /// Serializes tests that depend on the module-global Kitty protocol flag and
    /// on process env vars. The upstream suite runs sequentially; Rust test
    /// threads run in parallel, so every test in this module takes this lock to
    /// keep the globals deterministic.
    static GLOBALS_LOCK: Mutex<()> = Mutex::new(());

    fn lock_globals() -> MutexGuard<'static, ()> {
        GLOBALS_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// RAII guard restoring the previous Kitty protocol state on drop.
    struct KittyGuard {
        previous: bool,
    }

    impl Drop for KittyGuard {
        fn drop(&mut self) {
            set_kitty_protocol_active(self.previous);
        }
    }

    fn with_kitty(active: bool) -> KittyGuard {
        let previous = is_kitty_protocol_active();
        set_kitty_protocol_active(active);
        KittyGuard { previous }
    }

    /// RAII guard restoring the previous env var value on drop.
    struct EnvGuard {
        name: &'static str,
        previous: Option<String>,
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(previous) => std::env::set_var(self.name, previous),
                None => std::env::remove_var(self.name),
            }
        }
    }

    fn with_env(name: &'static str, value: Option<&str>) -> EnvGuard {
        let previous = std::env::var(name).ok();
        match value {
            Some(value) => std::env::set_var(name, value),
            None => std::env::remove_var(name),
        }
        EnvGuard { name, previous }
    }

    fn with_env_vars(vars: &[(&'static str, Option<&str>)]) -> Vec<EnvGuard> {
        vars.iter()
            .map(|(name, value)| with_env(name, *value))
            .collect()
    }

    mod kitty_alternate_keys {
        use super::*;

        #[test]
        fn matches_ctrl_c_when_pressing_cyrillic_s_with_base_layout_key() {
            let _globals = lock_globals();
            let _kitty = with_kitty(true);
            // Cyrillic 'с' = codepoint 1089, Latin 'c' = codepoint 99
            // Format: CSI 1089::99;5u (codepoint::base;modifier with ctrl=4, +1=5)
            assert!(matches_key("\x1b[1089::99;5u", "ctrl+c"));
        }

        #[test]
        fn matches_ctrl_d_when_pressing_cyrillic_v_with_base_layout_key() {
            let _globals = lock_globals();
            let _kitty = with_kitty(true);
            // Cyrillic 'в' = codepoint 1074, Latin 'd' = codepoint 100
            assert!(matches_key("\x1b[1074::100;5u", "ctrl+d"));
        }

        #[test]
        fn matches_ctrl_z_when_pressing_cyrillic_ya_with_base_layout_key() {
            let _globals = lock_globals();
            let _kitty = with_kitty(true);
            // Cyrillic 'я' = codepoint 1103, Latin 'z' = codepoint 122
            assert!(matches_key("\x1b[1103::122;5u", "ctrl+z"));
        }

        #[test]
        fn matches_ctrl_shift_p_with_base_layout_key() {
            let _globals = lock_globals();
            let _kitty = with_kitty(true);
            // Cyrillic 'з' = codepoint 1079, Latin 'p' = codepoint 112
            // ctrl=4, shift=1, +1 = 6
            assert!(matches_key("\x1b[1079::112;6u", "ctrl+shift+p"));
        }

        #[test]
        fn matches_direct_codepoint_when_no_base_layout_key() {
            let _globals = lock_globals();
            let _kitty = with_kitty(true);
            // Latin ctrl+c without base layout key (terminal doesn't support flag 4)
            assert!(matches_key("\x1b[99;5u", "ctrl+c"));
        }

        #[test]
        fn matches_super_modified_bindings_including_combined_modifiers() {
            let _globals = lock_globals();
            let _kitty = with_kitty(true);
            assert!(matches_key("\x1b[107;9u", "super+k"));
            assert!(matches_key("\x1b[13;9u", "super+enter"));
            assert!(matches_key("\x1b[107;13u", &Key::ctrl_super("k")));
            assert!(matches_key("\x1b[107;13u", "ctrl+super+k"));
            assert!(matches_key("\x1b[107;14u", "ctrl+shift+super+k"));
            assert!(!matches_key("\x1b[107;13u", "super+k"));
            assert_eq!(parse_key("\x1b[107;9u").as_deref(), Some("super+k"));
            assert_eq!(parse_key("\x1b[13;9u").as_deref(), Some("super+enter"));
            assert_eq!(parse_key("\x1b[107;13u").as_deref(), Some("ctrl+super+k"));
            assert_eq!(
                parse_key("\x1b[107;14u").as_deref(),
                Some("shift+ctrl+super+k")
            );
        }

        #[test]
        fn matches_digit_bindings_via_kitty_csi_u() {
            let _globals = lock_globals();
            let _kitty = with_kitty(true);
            assert!(matches_key("\x1b[49u", "1"));
            assert!(matches_key("\x1b[49;5u", "ctrl+1"));
            assert!(!matches_key("\x1b[49;5u", "ctrl+2"));
            assert_eq!(parse_key("\x1b[49u").as_deref(), Some("1"));
            assert_eq!(parse_key("\x1b[49;5u").as_deref(), Some("ctrl+1"));
        }

        #[test]
        fn normalizes_kitty_keypad_functional_keys_to_logical_keys() {
            let _globals = lock_globals();
            let _kitty = with_kitty(true);
            assert!(matches_key("\x1b[57400u", "1"));
            assert!(matches_key("\x1b[57410u", "/"));
            assert!(matches_key("\x1b[57417u", "left"));
            assert!(matches_key("\x1b[57426u", "delete"));
            assert_eq!(parse_key("\x1b[57399u").as_deref(), Some("0"));
            assert_eq!(parse_key("\x1b[57409u").as_deref(), Some("."));
            assert_eq!(parse_key("\x1b[57413u").as_deref(), Some("+"));
            assert_eq!(parse_key("\x1b[57416u").as_deref(), Some(","));
            assert_eq!(parse_key("\x1b[57417u").as_deref(), Some("left"));
            assert_eq!(parse_key("\x1b[57418u").as_deref(), Some("right"));
            assert_eq!(parse_key("\x1b[57419u").as_deref(), Some("up"));
            assert_eq!(parse_key("\x1b[57420u").as_deref(), Some("down"));
            assert_eq!(parse_key("\x1b[57421u").as_deref(), Some("pageUp"));
            assert_eq!(parse_key("\x1b[57422u").as_deref(), Some("pageDown"));
            assert_eq!(parse_key("\x1b[57423u").as_deref(), Some("home"));
            assert_eq!(parse_key("\x1b[57424u").as_deref(), Some("end"));
            assert_eq!(parse_key("\x1b[57425u").as_deref(), Some("insert"));
            assert_eq!(parse_key("\x1b[57426u").as_deref(), Some("delete"));
        }

        #[test]
        fn handles_shifted_key_in_format() {
            let _globals = lock_globals();
            let _kitty = with_kitty(true);
            // Format with shifted key: CSI codepoint:shifted:base;modifier u
            // Latin 'c' with shifted 'C' (67) and base 'c' (99)
            let shifted_key = "\x1b[99:67:99;2u"; // shift modifier = 1, +1 = 2
            assert!(matches_key(shifted_key, "shift+c"));
        }

        #[test]
        fn handles_event_type_in_format() {
            let _globals = lock_globals();
            let _kitty = with_kitty(true);
            // Format with event type: CSI codepoint::base;modifier:event u
            // Cyrillic ctrl+c release event (event type 3)
            let release_event = "\x1b[1089::99;5:3u";
            assert!(matches_key(release_event, "ctrl+c"));
        }

        #[test]
        fn handles_full_format_with_shifted_base_and_event() {
            let _globals = lock_globals();
            let _kitty = with_kitty(true);
            // Full format: CSI codepoint:shifted:base;modifier:event u
            // Cyrillic 'С' (shifted) with base 'c', Ctrl+Shift pressed, repeat event
            // Cyrillic 'с' = 1089, Cyrillic 'С' = 1057, Latin 'c' = 99
            // ctrl=4, shift=1, +1 = 6, repeat event = 2
            let full_format = "\x1b[1089:1057:99;6:2u";
            assert!(matches_key(full_format, "ctrl+shift+c"));
        }

        #[test]
        fn prefers_codepoint_for_latin_letters_even_when_base_layout_differs() {
            let _globals = lock_globals();
            let _kitty = with_kitty(true);
            // Dvorak Ctrl+K reports codepoint 'k' (107) and base layout 'v' (118)
            let dvorak_ctrl_k = "\x1b[107::118;5u";
            assert!(matches_key(dvorak_ctrl_k, "ctrl+k"));
            assert!(!matches_key(dvorak_ctrl_k, "ctrl+v"));
        }

        #[test]
        fn prefers_codepoint_for_symbol_keys_even_when_base_layout_differs() {
            let _globals = lock_globals();
            let _kitty = with_kitty(true);
            // Dvorak Ctrl+/ reports codepoint '/' (47) and base layout '[' (91)
            let dvorak_ctrl_slash = "\x1b[47::91;5u";
            assert!(matches_key(dvorak_ctrl_slash, "ctrl+/"));
            assert!(!matches_key(dvorak_ctrl_slash, "ctrl+["));
        }

        #[test]
        fn does_not_match_wrong_key_even_with_base_layout() {
            let _globals = lock_globals();
            let _kitty = with_kitty(true);
            // Cyrillic ctrl+с with base 'c' should NOT match ctrl+d
            let cyrillic_ctrl_c = "\x1b[1089::99;5u";
            assert!(!matches_key(cyrillic_ctrl_c, "ctrl+d"));
        }

        #[test]
        fn does_not_match_wrong_modifiers_even_with_base_layout() {
            let _globals = lock_globals();
            let _kitty = with_kitty(true);
            // Cyrillic ctrl+с should NOT match ctrl+shift+c
            let cyrillic_ctrl_c = "\x1b[1089::99;5u";
            assert!(!matches_key(cyrillic_ctrl_c, "ctrl+shift+c"));
        }
    }

    mod modify_other_keys {
        use super::*;

        #[test]
        fn matches_xterm_modify_other_keys_ctrl_c() {
            let _globals = lock_globals();
            let _kitty = with_kitty(false);
            assert!(matches_key("\x1b[27;5;99~", "ctrl+c"));
            assert_eq!(parse_key("\x1b[27;5;99~").as_deref(), Some("ctrl+c"));
        }

        #[test]
        fn matches_xterm_modify_other_keys_ctrl_d() {
            let _globals = lock_globals();
            let _kitty = with_kitty(false);
            assert!(matches_key("\x1b[27;5;100~", "ctrl+d"));
            assert_eq!(parse_key("\x1b[27;5;100~").as_deref(), Some("ctrl+d"));
        }

        #[test]
        fn matches_xterm_modify_other_keys_ctrl_z() {
            let _globals = lock_globals();
            let _kitty = with_kitty(false);
            assert!(matches_key("\x1b[27;5;122~", "ctrl+z"));
            assert_eq!(parse_key("\x1b[27;5;122~").as_deref(), Some("ctrl+z"));
        }

        #[test]
        fn matches_xterm_modify_other_keys_enter_variants() {
            let _globals = lock_globals();
            let _kitty = with_kitty(false);
            assert!(matches_key("\x1b[27;5;13~", "ctrl+enter"));
            assert!(matches_key("\x1b[27;2;13~", "shift+enter"));
            assert!(matches_key("\x1b[27;3;13~", "alt+enter"));
            assert_eq!(parse_key("\x1b[27;5;13~").as_deref(), Some("ctrl+enter"));
            assert_eq!(parse_key("\x1b[27;2;13~").as_deref(), Some("shift+enter"));
            assert_eq!(parse_key("\x1b[27;3;13~").as_deref(), Some("alt+enter"));
        }

        #[test]
        fn matches_xterm_modify_other_keys_tab_variants() {
            let _globals = lock_globals();
            let _kitty = with_kitty(false);
            assert!(matches_key("\x1b[27;2;9~", "shift+tab"));
            assert!(matches_key("\x1b[27;5;9~", "ctrl+tab"));
            assert!(matches_key("\x1b[27;3;9~", "alt+tab"));
            assert_eq!(parse_key("\x1b[27;2;9~").as_deref(), Some("shift+tab"));
            assert_eq!(parse_key("\x1b[27;5;9~").as_deref(), Some("ctrl+tab"));
            assert_eq!(parse_key("\x1b[27;3;9~").as_deref(), Some("alt+tab"));
        }

        #[test]
        fn matches_xterm_modify_other_keys_backspace_variants() {
            let _globals = lock_globals();
            let _kitty = with_kitty(false);
            assert!(matches_key("\x1b[27;1;127~", "backspace"));
            assert!(matches_key("\x1b[27;5;127~", "ctrl+backspace"));
            assert!(matches_key("\x1b[27;3;127~", "alt+backspace"));
            assert_eq!(parse_key("\x1b[27;1;127~").as_deref(), Some("backspace"));
            assert_eq!(
                parse_key("\x1b[27;5;127~").as_deref(),
                Some("ctrl+backspace")
            );
            assert_eq!(
                parse_key("\x1b[27;3;127~").as_deref(),
                Some("alt+backspace")
            );
        }

        #[test]
        fn matches_xterm_modify_other_keys_escape() {
            let _globals = lock_globals();
            let _kitty = with_kitty(false);
            assert!(matches_key("\x1b[27;1;27~", "escape"));
            assert_eq!(parse_key("\x1b[27;1;27~").as_deref(), Some("escape"));
        }

        #[test]
        fn matches_xterm_modify_other_keys_space_variants() {
            let _globals = lock_globals();
            let _kitty = with_kitty(false);
            assert!(matches_key("\x1b[27;1;32~", "space"));
            assert!(matches_key("\x1b[27;5;32~", "ctrl+space"));
            assert_eq!(parse_key("\x1b[27;1;32~").as_deref(), Some("space"));
            assert_eq!(parse_key("\x1b[27;5;32~").as_deref(), Some("ctrl+space"));
        }

        #[test]
        fn matches_xterm_modify_other_keys_symbol_combos() {
            let _globals = lock_globals();
            let _kitty = with_kitty(false);
            assert!(matches_key("\x1b[27;5;47~", "ctrl+/"));
            assert_eq!(parse_key("\x1b[27;5;47~").as_deref(), Some("ctrl+/"));
        }

        #[test]
        fn matches_xterm_modify_other_keys_digit_combos() {
            let _globals = lock_globals();
            let _kitty = with_kitty(false);
            assert!(matches_key("\x1b[27;5;49~", "ctrl+1"));
            assert!(matches_key("\x1b[27;2;49~", "shift+1"));
            assert_eq!(parse_key("\x1b[27;5;49~").as_deref(), Some("ctrl+1"));
            assert_eq!(parse_key("\x1b[27;2;49~").as_deref(), Some("shift+1"));
        }

        #[test]
        fn matches_xterm_modify_other_keys_shifted_uppercase_letters() {
            let _globals = lock_globals();
            let _kitty = with_kitty(false);
            assert!(matches_key("\x1b[27;2;69~", "shift+e"));
            assert!(matches_key("\x1b[27;6;69~", "ctrl+shift+e"));
            assert_eq!(parse_key("\x1b[27;2;69~").as_deref(), Some("shift+e"));
            assert_eq!(parse_key("\x1b[27;6;69~").as_deref(), Some("shift+ctrl+e"));
        }

        #[test]
        fn matches_ctrl_alt_letter_via_csi_u_when_kitty_inactive() {
            let _globals = lock_globals();
            let _kitty = with_kitty(false);
            assert!(matches_key("\x1b[104;7u", "ctrl+alt+h"));
            assert_eq!(parse_key("\x1b[104;7u").as_deref(), Some("ctrl+alt+h"));
        }

        #[test]
        fn matches_ctrl_alt_letter_via_xterm_modify_other_keys() {
            let _globals = lock_globals();
            let _kitty = with_kitty(false);
            assert!(matches_key("\x1b[27;7;104~", "ctrl+alt+h"));
            assert_eq!(parse_key("\x1b[27;7;104~").as_deref(), Some("ctrl+alt+h"));
        }
    }

    mod legacy {
        use super::*;

        #[test]
        fn matches_legacy_ctrl_c() {
            let _globals = lock_globals();
            let _kitty = with_kitty(false);
            // Ctrl+c sends ASCII 3 (ETX)
            assert!(matches_key("\x03", "ctrl+c"));
        }

        #[test]
        fn matches_legacy_ctrl_d() {
            let _globals = lock_globals();
            let _kitty = with_kitty(false);
            // Ctrl+d sends ASCII 4 (EOT)
            assert!(matches_key("\x04", "ctrl+d"));
        }

        #[test]
        fn matches_escape_key() {
            let _globals = lock_globals();
            assert!(matches_key("\x1b", "escape"));
        }

        #[test]
        fn matches_legacy_linefeed_as_enter() {
            let _globals = lock_globals();
            let _kitty = with_kitty(false);
            assert!(matches_key("\n", "enter"));
            assert_eq!(parse_key("\n").as_deref(), Some("enter"));
        }

        #[test]
        fn treats_linefeed_as_shift_enter_when_kitty_active() {
            let _globals = lock_globals();
            let _kitty = with_kitty(true);
            assert!(matches_key("\n", "shift+enter"));
            assert!(!matches_key("\n", "enter"));
            assert_eq!(parse_key("\n").as_deref(), Some("shift+enter"));
        }

        #[test]
        fn parses_ctrl_space() {
            let _globals = lock_globals();
            let _kitty = with_kitty(false);
            assert!(matches_key("\x00", "ctrl+space"));
            assert_eq!(parse_key("\x00").as_deref(), Some("ctrl+space"));
        }

        #[test]
        fn matches_legacy_ctrl_symbol() {
            let _globals = lock_globals();
            let _kitty = with_kitty(false);
            // Ctrl+\ sends ASCII 28 (File Separator) in legacy terminals
            assert!(matches_key("\x1c", "ctrl+\\"));
            assert_eq!(parse_key("\x1c").as_deref(), Some("ctrl+\\"));
            // Ctrl+] sends ASCII 29 (Group Separator) in legacy terminals
            assert!(matches_key("\x1d", "ctrl+]"));
            assert_eq!(parse_key("\x1d").as_deref(), Some("ctrl+]"));
            // Ctrl+_ sends ASCII 31 (Unit Separator) in legacy terminals
            // Ctrl+- is on the same physical key on US keyboards
            assert!(matches_key("\x1f", "ctrl+_"));
            assert!(matches_key("\x1f", "ctrl+-"));
            assert_eq!(parse_key("\x1f").as_deref(), Some("ctrl+-"));
        }

        #[test]
        fn matches_legacy_ctrl_alt_symbol() {
            let _globals = lock_globals();
            let _kitty = with_kitty(false);
            // Ctrl+Alt+[ sends ESC followed by ESC (Ctrl+[ = ESC)
            assert!(matches_key("\x1b\x1b", "ctrl+alt+["));
            assert_eq!(parse_key("\x1b\x1b").as_deref(), Some("ctrl+alt+["));
            // Ctrl+Alt+\ sends ESC followed by ASCII 28
            assert!(matches_key("\x1b\x1c", "ctrl+alt+\\"));
            assert_eq!(parse_key("\x1b\x1c").as_deref(), Some("ctrl+alt+\\"));
            // Ctrl+Alt+] sends ESC followed by ASCII 29
            assert!(matches_key("\x1b\x1d", "ctrl+alt+]"));
            assert_eq!(parse_key("\x1b\x1d").as_deref(), Some("ctrl+alt+]"));
            // Ctrl+_ sends ASCII 31 (Unit Separator) in legacy terminals
            // Ctrl+- is on the same physical key on US keyboards
            assert!(matches_key("\x1b\x1f", "ctrl+alt+_"));
            assert!(matches_key("\x1b\x1f", "ctrl+alt+-"));
            assert_eq!(parse_key("\x1b\x1f").as_deref(), Some("ctrl+alt+-"));
        }

        #[test]
        fn treats_raw_0x08_as_plain_backspace_outside_windows_terminal() {
            let _globals = lock_globals();
            let _kitty = with_kitty(false);
            let _env = with_env("WT_SESSION", None);
            assert!(matches_key("\x7f", "backspace"));
            assert!(!matches_key("\x7f", "ctrl+backspace"));
            assert_eq!(parse_key("\x7f").as_deref(), Some("backspace"));
            assert!(matches_key("\x08", "backspace"));
            assert!(!matches_key("\x08", "ctrl+backspace"));
            assert_eq!(parse_key("\x08").as_deref(), Some("backspace"));
            assert!(matches_key("\x08", "ctrl+h"));
        }

        #[test]
        fn treats_raw_0x08_as_ctrl_backspace_in_local_windows_terminal() {
            let _globals = lock_globals();
            let _kitty = with_kitty(false);
            let _env = with_env_vars(&[
                ("WT_SESSION", Some("test-session")),
                ("SSH_CONNECTION", None),
                ("SSH_CLIENT", None),
                ("SSH_TTY", None),
            ]);
            assert!(matches_key("\x08", "ctrl+backspace"));
            assert!(!matches_key("\x08", "backspace"));
            assert_eq!(parse_key("\x08").as_deref(), Some("ctrl+backspace"));
            assert!(matches_key("\x08", "ctrl+h"));
        }

        #[test]
        fn treats_raw_0x08_as_plain_backspace_in_windows_terminal_over_ssh() {
            let _globals = lock_globals();
            let _kitty = with_kitty(false);
            let _env = with_env_vars(&[
                ("WT_SESSION", Some("test-session")),
                ("SSH_CONNECTION", Some("1 2 3 4")),
                ("SSH_CLIENT", Some("1 2 3")),
                ("SSH_TTY", Some("/dev/pts/1")),
            ]);
            assert!(!matches_key("\x08", "ctrl+backspace"));
            assert!(matches_key("\x08", "backspace"));
            assert_eq!(parse_key("\x08").as_deref(), Some("backspace"));
            assert!(matches_key("\x08", "ctrl+h"));
        }

        #[test]
        fn parses_legacy_alt_prefixed_sequences_when_kitty_inactive() {
            let _globals = lock_globals();
            let _kitty = with_kitty(false);
            assert!(matches_key("\x1b ", "alt+space"));
            assert_eq!(parse_key("\x1b ").as_deref(), Some("alt+space"));
            assert!(matches_key("\x1b\x08", "alt+backspace"));
            assert_eq!(parse_key("\x1b\x08").as_deref(), Some("alt+backspace"));
            assert!(matches_key("\x1b\x03", "ctrl+alt+c"));
            assert_eq!(parse_key("\x1b\x03").as_deref(), Some("ctrl+alt+c"));
            assert!(matches_key("\x1bB", "alt+left"));
            assert_eq!(parse_key("\x1bB").as_deref(), Some("alt+left"));
            assert!(matches_key("\x1bF", "alt+right"));
            assert_eq!(parse_key("\x1bF").as_deref(), Some("alt+right"));
            assert!(matches_key("\x1ba", "alt+a"));
            assert_eq!(parse_key("\x1ba").as_deref(), Some("alt+a"));
            assert!(matches_key("\x1b1", "alt+1"));
            assert_eq!(parse_key("\x1b1").as_deref(), Some("alt+1"));
            assert!(matches_key("\x1b,", "alt+,"));
            assert_eq!(parse_key("\x1b,").as_deref(), Some("alt+,"));
            assert!(matches_key("\x1b.", "alt+."));
            assert_eq!(parse_key("\x1b.").as_deref(), Some("alt+."));
            assert!(matches_key("\x1by", "alt+y"));
            assert_eq!(parse_key("\x1by").as_deref(), Some("alt+y"));
            assert!(matches_key("\x1bz", "alt+z"));
            assert_eq!(parse_key("\x1bz").as_deref(), Some("alt+z"));

            drop(_kitty);
            let _kitty = with_kitty(true);
            assert!(!matches_key("\x1b ", "alt+space"));
            assert_eq!(parse_key("\x1b "), None);
            assert!(matches_key("\x1b\x08", "alt+backspace"));
            assert_eq!(parse_key("\x1b\x08").as_deref(), Some("alt+backspace"));
            assert!(!matches_key("\x1b\x03", "ctrl+alt+c"));
            assert_eq!(parse_key("\x1b\x03"), None);
            assert!(!matches_key("\x1bB", "alt+left"));
            assert_eq!(parse_key("\x1bB"), None);
            assert!(!matches_key("\x1bF", "alt+right"));
            assert_eq!(parse_key("\x1bF"), None);
            assert!(!matches_key("\x1ba", "alt+a"));
            assert_eq!(parse_key("\x1ba"), None);
            assert!(!matches_key("\x1b1", "alt+1"));
            assert_eq!(parse_key("\x1b1"), None);
            assert!(!matches_key("\x1b,", "alt+,"));
            assert_eq!(parse_key("\x1b,"), None);
            assert!(!matches_key("\x1b.", "alt+."));
            assert_eq!(parse_key("\x1b."), None);
            assert!(!matches_key("\x1by", "alt+y"));
            assert_eq!(parse_key("\x1by"), None);
        }

        #[test]
        fn matches_arrow_keys() {
            let _globals = lock_globals();
            assert!(matches_key("\x1b[A", "up"));
            assert!(matches_key("\x1b[B", "down"));
            assert!(matches_key("\x1b[C", "right"));
            assert!(matches_key("\x1b[D", "left"));
        }

        #[test]
        fn matches_ss3_arrows_and_home_end() {
            let _globals = lock_globals();
            assert!(matches_key("\x1bOA", "up"));
            assert!(matches_key("\x1bOB", "down"));
            assert!(matches_key("\x1bOC", "right"));
            assert!(matches_key("\x1bOD", "left"));
            assert!(matches_key("\x1bOH", "home"));
            assert!(matches_key("\x1bOF", "end"));
        }

        #[test]
        fn matches_legacy_function_keys_and_clear() {
            let _globals = lock_globals();
            assert!(matches_key("\x1bOP", "f1"));
            assert!(matches_key("\x1b[24~", "f12"));
            assert!(matches_key("\x1b[E", "clear"));
        }

        #[test]
        fn matches_alt_arrows() {
            let _globals = lock_globals();
            assert!(matches_key("\x1bp", "alt+up"));
            assert!(!matches_key("\x1bp", "up"));
        }

        #[test]
        fn matches_rxvt_modifier_sequences() {
            let _globals = lock_globals();
            assert!(matches_key("\x1b[a", "shift+up"));
            assert!(matches_key("\x1bOa", "ctrl+up"));
            assert!(matches_key("\x1b[2$", "shift+insert"));
            assert!(matches_key("\x1b[2^", "ctrl+insert"));
            assert!(matches_key("\x1b[7$", "shift+home"));
        }
    }

    mod decode_kitty_printable {
        use super::*;

        #[test]
        fn decodes_kitty_keypad_functional_keys_to_printable_characters() {
            let _globals = lock_globals();
            assert_eq!(decode_kitty_printable("\x1b[57399u"), Some('0'));
            assert_eq!(decode_kitty_printable("\x1b[57400u"), Some('1'));
            assert_eq!(decode_kitty_printable("\x1b[57409u"), Some('.'));
            assert_eq!(decode_kitty_printable("\x1b[57410u"), Some('/'));
            assert_eq!(decode_kitty_printable("\x1b[57411u"), Some('*'));
            assert_eq!(decode_kitty_printable("\x1b[57412u"), Some('-'));
            assert_eq!(decode_kitty_printable("\x1b[57413u"), Some('+'));
            assert_eq!(decode_kitty_printable("\x1b[57415u"), Some('='));
            assert_eq!(decode_kitty_printable("\x1b[57416u"), Some(','));
            assert_eq!(decode_kitty_printable("\x1b[57417u"), None);
        }
    }

    mod decode_printable_key {
        use super::*;

        #[test]
        fn decodes_printable_xterm_modify_other_keys_sequences() {
            let _globals = lock_globals();
            assert_eq!(decode_printable_key("\x1b[27;2;69~"), Some('E'));
            assert_eq!(decode_printable_key("\x1b[27;2;196~"), Some('Ä'));
            assert_eq!(decode_printable_key("\x1b[27;2;32~"), Some(' '));
            assert_eq!(decode_printable_key("\x1b[27;2;13~"), None);
            assert_eq!(decode_printable_key("\x1b[27;6;69~"), None);
        }
    }

    mod parse_key {
        use super::*;

        #[test]
        fn returns_latin_key_name_when_base_layout_key_is_present() {
            let _globals = lock_globals();
            let _kitty = with_kitty(true);
            // Cyrillic ctrl+с with base layout 'c'
            let cyrillic_ctrl_c = "\x1b[1089::99;5u";
            assert_eq!(parse_key(cyrillic_ctrl_c).as_deref(), Some("ctrl+c"));
        }

        #[test]
        fn prefers_codepoint_for_latin_letters_when_base_layout_differs() {
            let _globals = lock_globals();
            let _kitty = with_kitty(true);
            // Dvorak Ctrl+K reports codepoint 'k' (107) and base layout 'v' (118)
            let dvorak_ctrl_k = "\x1b[107::118;5u";
            assert_eq!(parse_key(dvorak_ctrl_k).as_deref(), Some("ctrl+k"));
        }

        #[test]
        fn prefers_codepoint_for_symbol_keys_when_base_layout_differs() {
            let _globals = lock_globals();
            let _kitty = with_kitty(true);
            // Dvorak Ctrl+/ reports codepoint '/' (47) and base layout '[' (91)
            let dvorak_ctrl_slash = "\x1b[47::91;5u";
            assert_eq!(parse_key(dvorak_ctrl_slash).as_deref(), Some("ctrl+/"));
        }

        #[test]
        fn returns_key_name_from_codepoint_when_no_base_layout() {
            let _globals = lock_globals();
            let _kitty = with_kitty(true);
            let latin_ctrl_c = "\x1b[99;5u";
            assert_eq!(parse_key(latin_ctrl_c).as_deref(), Some("ctrl+c"));
        }

        #[test]
        fn parses_shifted_uppercase_csi_u_letters_as_shift_letter() {
            let _globals = lock_globals();
            let _kitty = with_kitty(true);
            assert!(matches_key("\x1b[69;2u", "shift+e"));
            assert_eq!(parse_key("\x1b[69;2u").as_deref(), Some("shift+e"));
        }

        #[test]
        fn ignores_kitty_csi_u_with_unsupported_modifiers() {
            let _globals = lock_globals();
            let _kitty = with_kitty(true);
            assert_eq!(parse_key("\x1b[99;17u"), None);
        }

        #[test]
        fn parses_legacy_ctrl_letter() {
            let _globals = lock_globals();
            let _kitty = with_kitty(false);
            assert_eq!(parse_key("\x03").as_deref(), Some("ctrl+c"));
            assert_eq!(parse_key("\x04").as_deref(), Some("ctrl+d"));
        }

        #[test]
        fn parses_special_keys() {
            let _globals = lock_globals();
            let _kitty = with_kitty(false);
            assert_eq!(parse_key("\x1b").as_deref(), Some("escape"));
            assert_eq!(parse_key("\t").as_deref(), Some("tab"));
            assert_eq!(parse_key("\r").as_deref(), Some("enter"));
            assert_eq!(parse_key("\n").as_deref(), Some("enter"));
            assert_eq!(parse_key("\x00").as_deref(), Some("ctrl+space"));
            assert_eq!(parse_key(" ").as_deref(), Some("space"));
            assert_eq!(parse_key("1").as_deref(), Some("1"));
            assert!(matches_key("1", "1"));
        }

        #[test]
        fn parses_arrow_keys() {
            let _globals = lock_globals();
            assert_eq!(parse_key("\x1b[A").as_deref(), Some("up"));
            assert_eq!(parse_key("\x1b[B").as_deref(), Some("down"));
            assert_eq!(parse_key("\x1b[C").as_deref(), Some("right"));
            assert_eq!(parse_key("\x1b[D").as_deref(), Some("left"));
        }

        #[test]
        fn parses_ss3_arrows_and_home_end() {
            let _globals = lock_globals();
            assert_eq!(parse_key("\x1bOA").as_deref(), Some("up"));
            assert_eq!(parse_key("\x1bOB").as_deref(), Some("down"));
            assert_eq!(parse_key("\x1bOC").as_deref(), Some("right"));
            assert_eq!(parse_key("\x1bOD").as_deref(), Some("left"));
            assert_eq!(parse_key("\x1bOH").as_deref(), Some("home"));
            assert_eq!(parse_key("\x1bOF").as_deref(), Some("end"));
        }

        #[test]
        fn parses_legacy_function_and_modifier_sequences() {
            let _globals = lock_globals();
            assert_eq!(parse_key("\x1bOP").as_deref(), Some("f1"));
            assert_eq!(parse_key("\x1b[24~").as_deref(), Some("f12"));
            assert_eq!(parse_key("\x1b[E").as_deref(), Some("clear"));
            assert_eq!(parse_key("\x1b[2^").as_deref(), Some("ctrl+insert"));
            assert_eq!(parse_key("\x1bp").as_deref(), Some("alt+up"));
        }

        #[test]
        fn parses_double_bracket_page_up() {
            let _globals = lock_globals();
            assert_eq!(parse_key("\x1b[[5~").as_deref(), Some("pageUp"));
        }
    }

    mod key_events {
        use super::*;

        #[test]
        fn is_key_release_and_is_key_repeat_detect_event_suffixes() {
            let _globals = lock_globals();
            // Release event: flag 2 appends ":3" after the modifier
            assert!(is_key_release("\x1b[99;5:3u"));
            assert!(!is_key_repeat("\x1b[99;5:3u"));
            // Repeat event
            assert!(is_key_repeat("\x1b[99;5:2u"));
            assert!(!is_key_release("\x1b[99;5:2u"));
            // Plain press events carry no suffix
            assert!(!is_key_release("\x1b[99;5u"));
            assert!(!is_key_repeat("\x1b[99;5u"));
            // Bracketed paste content must never count as release/repeat,
            // even when it contains ":3F"-style patterns (e.g. MAC addresses).
            assert!(!is_key_release("\x1b[200~90:62:3F:A5\x1b[201~"));
            assert!(!is_key_repeat("\x1b[200~90:62:2F:A5\x1b[201~"));
        }
    }

    mod key_helper {
        use super::*;

        #[test]
        fn builds_typed_key_identifiers() {
            assert_eq!(Key::ESCAPE, "escape");
            assert_eq!(Key::PAGE_UP, "pageUp");
            assert_eq!(Key::BACKSLASH, "\\");
            assert_eq!(Key::ctrl("c"), "ctrl+c");
            assert_eq!(Key::alt("x"), "alt+x");
            assert_eq!(Key::super_key("k"), "super+k");
            assert_eq!(Key::ctrl_shift("p"), "ctrl+shift+p");
            assert_eq!(Key::ctrl_super("k"), "ctrl+super+k");
            assert_eq!(Key::ctrl_shift_alt("x"), "ctrl+shift+alt+x");
            assert_eq!(Key::ctrl_shift_super("x"), "ctrl+shift+super+x");
        }
    }
}
