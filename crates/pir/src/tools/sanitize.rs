//! Output sanitization utilities.
//!
//! Port of `packages/coding-agent/src/utils/ansi.ts` (`stripAnsi`) and
//! `packages/coding-agent/src/utils/shell.ts` (`sanitizeBinaryOutput`)
//! @ pi 0.82.1 (2efa728).
//!
//! Intentional differences: none.

use std::sync::OnceLock;

use regex::Regex;

/// Regex matching ANSI OSC and CSI escape sequences.
///
/// Derived from `ansi-regex` (MIT, Sindre Sorhus), used by upstream `stripAnsi`.
/// - OSC: `ESC ] ... ST` where ST = BEL | ESC\ | 0x9C
/// - CSI: `ESC` or `0x9B`, optional intermediates/params, then a final byte
const ANSI_PATTERN: &str = concat!(
    // OSC sequences only: ESC ] ... ST (non-greedy until first ST)
    r"(?:\x1B\][\s\S]*?(?:\x07|\x1B\x5C|\x9C))",
    "|",
    // CSI and related: ESC/C1, optional intermediates, optional params, final byte.
    // Note: `[` inside the character class must be escaped for the Rust regex crate.
    r"[\x1B\x9B][\[\]\\()#;?]*(?:\d{1,4}(?:[;:]\d{0,4})*)?[\dA-PR-TZcf-nq-uy=><~]",
);

static ANSI_REGEX: OnceLock<Regex> = OnceLock::new();

/// Strip ANSI escape sequences from a string.
///
/// Port of `stripAnsi` (ansi.ts:46-60). Fast path: if the input contains
/// neither ESC (`U+001B`) nor CSI (`U+009B`), it is returned unchanged.
pub fn strip_ansi(value: &str) -> String {
    // Fast path: ANSI codes require ESC (7-bit) or CSI (8-bit) introducer.
    if !value.contains('\u{001B}') && !value.contains('\u{009B}') {
        return value.to_string();
    }

    let regex = ANSI_REGEX.get_or_init(|| {
        // Invariant: ANSI_PATTERN is a verified-valid regex literal.
        Regex::new(ANSI_PATTERN).expect("ANSI_PATTERN is verified valid at development time")
    });

    regex.replace_all(value, "").into_owned()
}

/// Remove control characters and Unicode format characters from output.
///
/// Port of `sanitizeBinaryOutput` (shell.ts:144-174). Removes:
/// - Control characters `U+0000`–`U+001F` (preserves `\t`, `\n`, `\r`)
/// - Unicode format characters `U+FFF9`–`U+FFFB`
pub fn sanitize_binary_output(value: &str) -> String {
    value
        .chars()
        .filter(|&c| {
            let code = c as u32;

            // Allow tab, newline, carriage return.
            if code == 0x09 || code == 0x0A || code == 0x0D {
                return true;
            }

            // Filter out control characters 0x00–0x1F.
            if code <= 0x1F {
                return false;
            }

            // Filter out Unicode format characters U+FFF9–U+FFFB.
            if (0xFFF9..=0xFFFB).contains(&code) {
                return false;
            }

            true
        })
        .collect()
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_ansi_fast_path_no_escape() {
        assert_eq!(strip_ansi("hello world"), "hello world");
    }

    #[test]
    fn test_strip_ansi_csi_color() {
        assert_eq!(strip_ansi("\u{001B}[31mred\u{001B}[0m"), "red");
    }

    #[test]
    fn test_strip_ansi_osc_title() {
        // OSC sequence terminated by BEL
        assert_eq!(strip_ansi("\u{001B}]0;title\u{0007}text"), "text");
        // OSC sequence terminated by ESC\
        assert_eq!(strip_ansi("\u{001B}]0;title\u{001B}\\text"), "text");
    }

    #[test]
    fn test_strip_ansi_csi_cursor() {
        assert_eq!(strip_ansi("a\u{001B}[2Kb"), "ab");
    }

    #[test]
    fn test_strip_ansi_c1_introducer() {
        // 0x9B is the 8-bit CSI introducer
        assert_eq!(strip_ansi("\u{009B}31mred\u{009B}0m"), "red");
    }

    #[test]
    fn test_strip_ansi_preserves_newlines() {
        assert_eq!(strip_ansi("\u{001B}[32m\nline\n\u{001B}[0m"), "\nline\n");
    }

    #[test]
    fn test_sanitize_preserves_printable() {
        assert_eq!(sanitize_binary_output("hello\tworld\n"), "hello\tworld\n");
    }

    #[test]
    fn test_sanitize_removes_control_chars() {
        // Bell, backspace, vertical tab, form feed → removed
        assert_eq!(sanitize_binary_output("a\u{0007}b\u{0008}c"), "abc");
    }

    #[test]
    fn test_sanitize_preserves_tab_newline_cr() {
        assert_eq!(sanitize_binary_output("\t\n\r"), "\t\n\r");
    }

    #[test]
    fn test_sanitize_removes_format_chars() {
        // U+FFF9 (INTERLINEAR ANNOTATION ANCHOR), U+FFFA, U+FFFB → removed
        assert_eq!(sanitize_binary_output("a\u{FFF9}b\u{FFFA}c\u{FFFB}"), "abc");
    }

    #[test]
    fn test_sanitize_keeps_unicode() {
        assert_eq!(
            sanitize_binary_output("héllo 世界 \u{1F642}"),
            "héllo 世界 \u{1F642}"
        );
    }
}
