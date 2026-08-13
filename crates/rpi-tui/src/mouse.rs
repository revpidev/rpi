//! Mouse-sequence parsing and mouse mode sequences — port of the mouse
//! handling in `packages/tui/src/tui-alt-screen.ts` @ pi 0.84.1 (4181f66):
//! wheel-event parsing (:462-487), SGR mouse-event parsing (:503-512),
//! mouse-sequence recognition (:965-967), terminal-multiplexer detection
//! (:236-246), and the mouse enable/disable byte sequences (:44-52).
//!
//! Intentional differences:
//! - `WheelEvent.x/y` are `u32` (upstream `number`): upstream JS yields
//!   negative coordinates for out-of-range input (X10 coordinate bytes < 33,
//!   SGR coordinate "0" after the `- 1` shift); this port saturates to 0.
//!   Real terminals never emit those inputs.
//! - SGR fields beyond `u32::MAX` fail to parse and yield `None` (upstream
//!   parses them into arbitrary-precision floats).
//! - `is_multiplexer` takes an injected `get_env` closure instead of reading
//!   `process.env` directly (coding-standards §12.4: tests never touch the
//!   real environment); [`is_multiplexer_env`] wraps `std::env::var`.

use std::sync::LazyLock;

use regex::Regex;

/// SGR mouse shape `ESC[<b;x;yM` / `ESC[<b;x;ym` (tui-alt-screen.ts:463, 504,
/// 966), shared by wheel parsing, SGR mouse parsing, and sequence
/// recognition. The pattern is a static literal, so the `expect` encodes a
/// compile-time invariant (any breakage fails every mouse test instantly).
static SGR_MOUSE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\x1b\[<(\d+);(\d+);(\d+)([Mm])$").expect("static SGR mouse regex is valid")
});

/// `ENABLE_BUTTON_MOTION_MOUSE` (tui-alt-screen.ts:48): click tracking,
/// button-motion tracking, focus events, and SGR mouse encoding. Used inside
/// terminal multiplexers, which can lag when every pointer movement is
/// forwarded (tui-alt-screen.ts:237-238).
pub const ENABLE_BUTTON_MOTION_MOUSE: &str = "\x1b[?1000h\x1b[?1002h\x1b[?1004h\x1b[?1006h";

/// `ENABLE_ALL_MOTION_MOUSE` (tui-alt-screen.ts:49): adds all-motion tracking
/// (`?1003h`) on top of [`ENABLE_BUTTON_MOTION_MOUSE`] for direct terminals.
pub const ENABLE_ALL_MOTION_MOUSE: &str = "\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1004h\x1b[?1006h";

/// `DISABLE_MOUSE` (tui-alt-screen.ts:50): disables mouse modes in reverse
/// enable order.
pub const DISABLE_MOUSE: &str = "\x1b[?1006l\x1b[?1004l\x1b[?1003l\x1b[?1002l\x1b[?1000l";

/// `FOCUS_IN` (tui-alt-screen.ts:51): terminal gained focus.
pub const FOCUS_IN: &str = "\x1b[I";

/// `FOCUS_OUT` (tui-alt-screen.ts:52): terminal lost focus.
pub const FOCUS_OUT: &str = "\x1b[O";

/// `WheelEvent` (tui-alt-screen.ts:101-105).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WheelEvent {
    /// `-1` scroll up, `1` scroll down (upstream `-1 | 1`).
    pub direction: i32,
    /// 0-based column.
    pub x: u32,
    /// 0-based row.
    pub y: u32,
}

/// `SgrMouseEvent` (tui-alt-screen.ts:94-99).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SgrMouseEvent {
    /// SGR button code. Bit layout: bits 0-1 button (0 left, 1 middle, 2
    /// right, 3 release/no button), bit 2 (4) shift, bit 3 (8) meta, bit 4
    /// (16) ctrl, bit 5 (32) motion, bit 6 (64) wheel.
    pub button: u32,
    /// 0-based column.
    pub x: u32,
    /// 0-based row.
    pub y: u32,
    /// `true` for the release form (final `m`); `false` for press (`M`).
    pub release: bool,
}

/// `parseWheelEvent` (tui-alt-screen.ts:462-487).
///
/// Parses a mouse-wheel event from either encoding:
/// - SGR (`ESC[<b;x;yM` / `...m`): accepted only when the wheel bit (64) is
///   set and the direction bits (`button & 3`) are 0 (up, `-1`) or 1
///   (down, `+1`); horizontal wheel directions 2/3 are rejected.
/// - Legacy X10 (`ESC[M` + 3 bytes, 6 bytes total): button = `byte[3] - 32`,
///   x = `byte[4] - 33`, y = `byte[5] - 33`, with the same wheel-bit and
///   direction constraints.
///
/// Returns `None` for anything else.
pub fn parse_wheel_event(data: &str) -> Option<WheelEvent> {
    if let Some(caps) = SGR_MOUSE_RE.captures(data) {
        let button: u32 = caps.get(1)?.as_str().parse().ok()?;
        if button & 64 == 0 {
            return None;
        }
        let direction = button & 3;
        if direction != 0 && direction != 1 {
            return None;
        }
        return Some(WheelEvent {
            direction: if direction == 0 { -1 } else { 1 },
            x: caps.get(2)?.as_str().parse::<u32>().ok()?.saturating_sub(1),
            y: caps.get(3)?.as_str().parse::<u32>().ok()?.saturating_sub(1),
        });
    }
    if data.len() == 6 && data.starts_with("\x1b[M") {
        let bytes = data.as_bytes();
        // Upstream charCodeAt() arithmetic; i32 keeps JS 32-bit bitwise
        // semantics for bytes below 32 (malformed but never sent).
        let button = i32::from(bytes[3]) - 32;
        if button & 64 == 0 {
            return None;
        }
        let direction = button & 3;
        if direction != 0 && direction != 1 {
            return None;
        }
        return Some(WheelEvent {
            direction: if direction == 0 { -1 } else { 1 },
            x: u32::from(bytes[4]).saturating_sub(33),
            y: u32::from(bytes[5]).saturating_sub(33),
        });
    }
    None
}

/// `parseSgrMouseEvent` (tui-alt-screen.ts:503-512).
///
/// Parses a full SGR mouse event `ESC[<b;x;yM` / `ESC[<b;x;ym`: `x`/`y` are
/// converted from 1-based to 0-based, and `release` is `true` exactly when
/// the sequence ends in `m`. The raw `button` code is returned as-is (see
/// [`SgrMouseEvent::button`] for the bit layout).
pub fn parse_sgr_mouse_event(data: &str) -> Option<SgrMouseEvent> {
    let caps = SGR_MOUSE_RE.captures(data)?;
    Some(SgrMouseEvent {
        button: caps.get(1)?.as_str().parse().ok()?,
        x: caps.get(2)?.as_str().parse::<u32>().ok()?.saturating_sub(1),
        y: caps.get(3)?.as_str().parse::<u32>().ok()?.saturating_sub(1),
        release: caps.get(4)?.as_str() == "m",
    })
}

/// `isMouseSequence` (tui-alt-screen.ts:965-967).
///
/// Recognizes any SGR mouse sequence shape or a 6-byte X10 `ESC[M` sequence.
/// The caller uses this as a catch-all to swallow mouse input that neither
/// parser consumed (upstream tui-alt-screen.ts:419), e.g. sequences with an
/// unknown button code.
pub fn is_mouse_sequence(data: &str) -> bool {
    SGR_MOUSE_RE.is_match(data) || (data.len() == 6 && data.starts_with("\x1b[M"))
}

/// Multiplexer detection (tui-alt-screen.ts:236-246).
///
/// Upstream checks `process.env.TMUX` / `ZELLIJ` / `STY` presence (values are
/// ignored) or a lowercase `TERM` starting with `tmux` / `screen`. The
/// environment is read through the injected `get_env` closure so tests never
/// touch the real environment (coding-standards §12.4).
pub fn is_multiplexer(get_env: impl Fn(&str) -> Option<String>) -> bool {
    get_env("TMUX").is_some()
        || get_env("ZELLIJ").is_some()
        || get_env("STY").is_some()
        || get_env("TERM").is_some_and(|term| {
            let term = term.to_lowercase();
            term.starts_with("tmux") || term.starts_with("screen")
        })
}

/// [`is_multiplexer`] over the real process environment.
pub fn is_multiplexer_env() -> bool {
    is_multiplexer(|name| std::env::var(name).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Env stub answering from a fixed table; tests never read the real
    /// environment (coding-standards §12.4).
    fn env_of<'a>(entries: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            entries
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_string())
        }
    }

    #[test]
    fn test_parse_sgr_mouse_event_press() {
        // ESC[<0;5;10M: left button press at 1-based (5, 10).
        let event = parse_sgr_mouse_event("\x1b[<0;5;10M").unwrap();
        assert_eq!(event.button, 0);
        assert_eq!(event.x, 4);
        assert_eq!(event.y, 9);
        assert!(!event.release);
    }

    #[test]
    fn test_parse_sgr_mouse_event_release() {
        // Final `m` marks the release form.
        let event = parse_sgr_mouse_event("\x1b[<0;5;10m").unwrap();
        assert_eq!(event.button, 0);
        assert_eq!(event.x, 4);
        assert_eq!(event.y, 9);
        assert!(event.release);
    }

    #[test]
    fn test_parse_sgr_mouse_event_motion_flag() {
        // Button 32 = motion bit (bit 5) set, press form.
        let event = parse_sgr_mouse_event("\x1b[<32;5;10M").unwrap();
        assert_eq!(event.button, 32);
        assert_eq!(event.x, 4);
        assert_eq!(event.y, 9);
        assert!(!event.release);
    }

    #[test]
    fn test_parse_wheel_event_sgr_up() {
        // Button 64: wheel bit set, direction bits 0 = up.
        let event = parse_wheel_event("\x1b[<64;15;20M").unwrap();
        assert_eq!(event.direction, -1);
        assert_eq!(event.x, 14);
        assert_eq!(event.y, 19);
    }

    #[test]
    fn test_parse_wheel_event_sgr_down() {
        // Button 65: wheel bit set, direction bits 1 = down.
        let event = parse_wheel_event("\x1b[<65;15;20M").unwrap();
        assert_eq!(event.direction, 1);
        assert_eq!(event.x, 14);
        assert_eq!(event.y, 19);
    }

    #[test]
    fn test_parse_wheel_event_rejects_horizontal_sgr() {
        // Buttons 66/67: wheel bit set, direction bits 2/3 = horizontal.
        assert_eq!(parse_wheel_event("\x1b[<66;15;20M"), None);
        assert_eq!(parse_wheel_event("\x1b[<67;15;20M"), None);
    }

    #[test]
    fn test_parse_wheel_event_rejects_non_wheel_sgr() {
        // Wheel bit (64) unset: plain press and motion are not wheel events.
        assert_eq!(parse_wheel_event("\x1b[<0;15;20M"), None);
        assert_eq!(parse_wheel_event("\x1b[<32;15;20M"), None);
    }

    #[test]
    fn test_parse_wheel_event_x10_up_and_down() {
        // ESC[M + bytes: button 32+64='`' / 32+65='a', coords 33+14='/' and
        // 33+19='4'.
        let up = parse_wheel_event("\x1b[M`/4").unwrap();
        assert_eq!(up.direction, -1);
        assert_eq!(up.x, 14);
        assert_eq!(up.y, 19);

        let down = parse_wheel_event("\x1b[Ma/4").unwrap();
        assert_eq!(down.direction, 1);
        assert_eq!(down.x, 14);
        assert_eq!(down.y, 19);
    }

    #[test]
    fn test_parse_wheel_event_rejects_horizontal_x10() {
        // Button bytes 32+66='b', 32+67='c': horizontal wheel directions.
        assert_eq!(parse_wheel_event("\x1b[Mb/4"), None);
        assert_eq!(parse_wheel_event("\x1b[Mc/4"), None);
    }

    #[test]
    fn test_parse_wheel_event_rejects_non_wheel_x10() {
        // Button byte 32+0=' ': wheel bit unset.
        assert_eq!(parse_wheel_event("\x1b[M /4"), None);
    }

    #[test]
    fn test_is_mouse_sequence_sgr_shape() {
        assert!(is_mouse_sequence("\x1b[<12;3;4M"));
        assert!(is_mouse_sequence("\x1b[<12;3;4m"));
    }

    #[test]
    fn test_is_mouse_sequence_x10_shape() {
        assert!(is_mouse_sequence("\x1b[Mxyz"));
        assert!(is_mouse_sequence("\x1b[M`/4"));
    }

    #[test]
    fn test_is_mouse_sequence_swallows_complete_unknown_button() {
        // Fully-formed SGR sequence with an unknown button code: parsers
        // reject it, is_mouse_sequence still consumes it (upstream :419).
        assert_eq!(parse_wheel_event("\x1b[<12345;3;4M"), None);
        assert_eq!(
            parse_sgr_mouse_event("\x1b[<12345;3;4M").map(|e| e.button),
            Some(12345)
        );
        assert!(is_mouse_sequence("\x1b[<12345;3;4M"));
    }

    #[test]
    fn test_is_mouse_sequence_rejects_incomplete_fragments() {
        // Malformed/incomplete fragments must NOT be swallowed.
        assert!(!is_mouse_sequence("\x1b[<12;3"));
        assert!(!is_mouse_sequence("\x1b[<12;3;4"));
        assert!(!is_mouse_sequence("\x1b[<12;3;4X"));
        assert!(!is_mouse_sequence("\x1b[Mxy")); // X10 needs 6 bytes total
        assert!(!is_mouse_sequence("abc"));
    }

    /// Intent port of `uses button-motion tracking inside terminal
    /// multiplexers` (tui-alt-screen.test.ts:165-210), reduced to the pure
    /// env predicate.
    #[test]
    fn test_is_multiplexer_scenario_matrix() {
        type Case = (&'static str, &'static [(&'static str, &'static str)], bool);
        let cases: &[Case] = &[
            ("tmux env", &[("TMUX", "/tmp/tmux/default,1,0")], true),
            ("tmux env value ignored", &[("TMUX", "")], true),
            ("tmux term", &[("TERM", "tmux-256color")], true),
            (
                "tmux term value case-insensitive",
                &[("TERM", "TMUX-256COLOR")],
                true,
            ),
            ("zellij env", &[("ZELLIJ", "0")], true),
            ("screen env", &[("STY", "123.session")], true),
            ("screen term", &[("TERM", "screen-256color")], true),
            ("direct terminal", &[("TERM", "xterm-256color")], false),
            ("no environment", &[], false),
        ];
        for (name, env, expected) in cases {
            assert_eq!(is_multiplexer(env_of(env)), *expected, "{name}");
        }
    }

    #[test]
    fn test_mouse_sequences_match_upstream_bytes() {
        // Byte-for-byte against tui-alt-screen.ts:48-52.
        assert_eq!(
            ENABLE_BUTTON_MOTION_MOUSE,
            "\x1b[?1000h\x1b[?1002h\x1b[?1004h\x1b[?1006h"
        );
        assert_eq!(
            ENABLE_ALL_MOTION_MOUSE,
            "\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1004h\x1b[?1006h"
        );
        assert_eq!(
            DISABLE_MOUSE,
            "\x1b[?1006l\x1b[?1004l\x1b[?1003l\x1b[?1002l\x1b[?1000l"
        );
        assert_eq!(FOCUS_IN, "\x1b[I");
        assert_eq!(FOCUS_OUT, "\x1b[O");
    }
}
