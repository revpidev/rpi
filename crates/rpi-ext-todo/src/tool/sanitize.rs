//! Terminal-control sanitization for model-controlled task text.
//!
//! Port of upstream `packages/rpiv-todo/tool/sanitize.ts` @ `0fdf4f8`:
//! complete CSI/OSC escape sequences are dropped whole (no printable
//! remnants like `[31m`), newlines and tabs become spaces so task fields
//! cannot change the layout, and bidi controls are removed so a field
//! cannot reorder how neighbouring output reads.

/// Remove terminal control characters (upstream `sanitizeTerminalText`).
pub fn sanitize_terminal_text(value: &str) -> String {
    // Stage 1: escape sequences — CSI (ESC-[ / C1 0x9b) and OSC
    // (ESC-] / C1 0x9d) with their payload, then any remaining
    // two-character ESC sequence.
    let without_escape = strip_escape_sequences(value);
    // Stage 2: Unicode line/paragraph separators join lines like \n does;
    // control chars — \n/\r/\t map to space, the rest of C0 + DEL + C1
    // drop; bidi embedding/override/isolate controls and LRM/RLM marks
    // are removed.
    let mut out = String::with_capacity(without_escape.len());
    for character in without_escape.chars() {
        match character {
            '\u{2028}' | '\u{2029}' | '\n' | '\r' | '\t' => out.push(' '),
            c if is_c0_control(c) => {}
            c if is_c1_or_del(c) => {}
            c if is_bidi_control(c) => {}
            c => out.push(c),
        }
    }
    out
}

fn is_c0_control(character: char) -> bool {
    // \u0000-\u001f minus the ones mapped to space above.
    matches!(character, '\u{0000}'..='\u{001f}') && !matches!(character, '\n' | '\r' | '\t')
}

fn is_c1_or_del(character: char) -> bool {
    // \u0080-\u009f (C1, incl. the 0x9b/0x9d introducers) + \u007f (DEL).
    matches!(character, '\u{0080}'..='\u{009f}') || character == '\u{007f}'
}

fn is_bidi_control(character: char) -> bool {
    // LRM/RLM (\u200e/\u200f), embedding/override (\u202a-\u202e),
    // isolates (\u2066-\u2069).
    matches!(character, '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

/// Consume one OSC payload starting AFTER the introducer. Returns the
/// index just past the match: payload runs while it is not BEL (0x07),
/// C1-ST (0x9c) or ESC; `ESC \` terminates and is consumed, a bare ESC
/// ends the match before itself (the upstream optional terminator group
/// fails, and the ESC is left for the bare-ESC rule), and a payload that
/// runs off the end swallows the rest of the string (unterminated OSC —
/// upstream comment).
fn consume_osc_payload(chars: &[char], mut cursor: usize) -> usize {
    while cursor < chars.len() {
        match chars[cursor] {
            '\u{0007}' | '\u{009c}' => return cursor + 1,
            '\u{001b}' => {
                return if chars.get(cursor + 1) == Some(&'\\') {
                    cursor + 2
                } else {
                    cursor
                };
            }
            _ => cursor += 1,
        }
    }
    cursor
}

/// Single pass implementing the net effect of the upstream three ordered
/// regex replaces (CSI, OSC, bare `ESC .`): a complete CSI/OSC sequence is
/// consumed whole; an incomplete CSI falls back to the two-character
/// ESC-rule consumption; leftover ESCs pair with the next char.
fn strip_escape_sequences(value: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    let mut out = String::with_capacity(value.len());
    let mut index = 0;
    while index < chars.len() {
        let character = chars[index];
        match character {
            '\u{001b}' => match chars.get(index + 1) {
                // ESC-[ … CSI sequence: parameter bytes [0-?]*, then
                // intermediate bytes [ -/]*, then final byte [@-~].
                Some('[') => {
                    let mut cursor = index + 2;
                    while cursor < chars.len() && matches!(chars[cursor], '0'..='?') {
                        cursor += 1;
                    }
                    while cursor < chars.len() && matches!(chars[cursor], ' '..='/') {
                        cursor += 1;
                    }
                    if cursor < chars.len() && matches!(chars[cursor], '@'..='~') {
                        index = cursor + 1;
                    } else {
                        // Incomplete CSI: the CSI regex fails and the bare
                        // ESC rule consumes ESC + '['.
                        index += 2;
                    }
                }
                // ESC-] … OSC sequence.
                Some(']') => {
                    index = consume_osc_payload(&chars, index + 2);
                }
                // Bare two-character ESC sequence (`\u001b.`) — the JS
                // regex `.` never matches a line terminator (\n, \r,
                // U+2028, U+2029), so an ESC before one does NOT pair: the
                // ESC stays for the control-character pass (dropped there)
                // and the terminator survives to become a space (F4).
                _ => {
                    let next_is_terminator = index + 1 < chars.len()
                        && matches!(chars[index + 1], '\n' | '\r' | '\u{2028}' | '\u{2029}');
                    if index + 1 < chars.len() && !next_is_terminator {
                        index += 2;
                    } else {
                        out.push(character);
                        index += 1;
                    }
                }
            },
            '\u{009b}' => {
                // C1 CSI single-byte introducer.
                let mut cursor = index + 1;
                while cursor < chars.len() && matches!(chars[cursor], '0'..='?') {
                    cursor += 1;
                }
                while cursor < chars.len() && matches!(chars[cursor], ' '..='/') {
                    cursor += 1;
                }
                if cursor < chars.len() && matches!(chars[cursor], '@'..='~') {
                    index = cursor + 1;
                } else {
                    // Incomplete: the introducer is itself a C1 control —
                    // dropped here; parameter bytes stay (upstream leaves
                    // them to the control-character pass too).
                    index += 1;
                }
            }
            '\u{009d}' => {
                // C1 OSC introducer.
                index = consume_osc_payload(&chars, index + 1);
            }
            _ => {
                out.push(character);
                index += 1;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    //! Port of upstream `tool/sanitize.test.ts` @ `0fdf4f8`.

    use super::*;

    #[test]
    fn drops_complete_ansi_c1_escape_sequences() {
        assert_eq!(
            sanitize_terminal_text("safe\u{001b}[31mred\u{001b}[0m\u{009b}2J"),
            "safered"
        );
    }

    #[test]
    fn drops_osc_sequences_including_payload() {
        assert_eq!(
            sanitize_terminal_text(
                "a\u{001b}]0;evil title\u{0007}b\u{001b}]8;;http://x\u{001b}\\c"
            ),
            "abc"
        );
    }

    #[test]
    fn keeps_task_fields_on_one_terminal_line() {
        assert_eq!(
            sanitize_terminal_text("one\ntwo\tthree\r"),
            "one two three "
        );
        assert_eq!(sanitize_terminal_text("a\u{2028}b\u{2029}c"), "a b c");
    }

    #[test]
    fn removes_bare_control_characters_and_bidi_overrides() {
        assert_eq!(
            sanitize_terminal_text("a\u{0007}b\u{007f}c\u{202e}gfedcba\u{202c}"),
            "abcgfedcba"
        );
    }

    // Ordered-regex net-effect edges the upstream suite exercises only
    // implicitly (the three replaces run sequentially on shared ESC runs).
    #[test]
    fn unterminated_osc_swallows_the_rest_of_the_string() {
        assert_eq!(sanitize_terminal_text("a\u{001b}]0;title"), "a");
    }

    #[test]
    fn osc_terminated_by_bare_esc_leaves_the_esc_for_the_bare_rule() {
        // OSC payload stops before the ESC; `ESC X` is then consumed by the
        // two-char ESC rule; `b` survives.
        assert_eq!(sanitize_terminal_text("a\u{001b}]0;t\u{001b}Xb"), "ab");
    }

    #[test]
    fn bare_esc_before_a_line_terminator_does_not_pair() {
        // JS `\u001b.` — `.` never matches a line terminator, so the ESC
        // is dropped by the control pass and the \n becomes a space (F4).
        assert_eq!(sanitize_terminal_text("a\u{001b}\nb"), "a b");
        assert_eq!(sanitize_terminal_text("a\u{001b}\rb"), "a b");
    }

    #[test]
    fn incomplete_csi_falls_back_to_the_bare_esc_rule() {
        // No final byte: the CSI regex fails; `\u001b[` is consumed by the
        // bare rule; `31` survives.
        assert_eq!(sanitize_terminal_text("a\u{001b}[31"), "a31");
    }
}
