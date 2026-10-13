//! Output sanitization utilities.
//!
//! Port of `packages/coding-agent/src/utils/ansi.ts` (`stripAnsi`,
//! `splitIncompleteAnsiSuffix`) and
//! `packages/coding-agent/src/utils/shell.ts` (`sanitizeBinaryOutput`)
//! @ `abe508e1b` (v1.1.0, `27c7b6ff4`).
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

/// Regex matching the characters removed by `sanitizeBinaryOutput`
/// (shell.ts:155-157 @ b485fa312): control characters except tab, newline,
/// and carriage return, plus the interlinear annotation characters
/// U+FFF9–U+FFFB. All removed characters are single UTF-16 code units, so
/// surrogate pairs are never split.
const BINARY_OUTPUT_PATTERN: &str = r"[\x00-\x08\x0B\x0C\x0E-\x1F\x{FFF9}-\x{FFFB}]";

static BINARY_OUTPUT_REGEX: OnceLock<Regex> = OnceLock::new();

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

/// Longest unfinished sequence held back while streaming (ansi.ts:44).
const MAX_PENDING_ANSI_LENGTH: usize = 256;

/// Whether the rest of the text is an OSC sequence that never reaches its
/// string terminator (BEL, `ESC\`, or `0x9C`).
fn osc_unfinished(rest: &str) -> bool {
    let Some(rest) = rest.strip_prefix("\u{001B}]") else {
        return false;
    };
    let mut chars = rest.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\u{0007}' | '\u{009C}' => return false,
            '\u{001B}' => match chars.peek() {
                // A trailing ESC may start `ESC\` in the next chunk, and any
                // other ESC starts a sequence that itself must reach the end.
                None => return true,
                Some('\\') => return false,
                Some(_) => {}
            },
            _ => {}
        }
    }
    true
}

/// Whether the rest of the text is a CSI introducer (ESC/C1, optional
/// intermediates and params) without its final byte.
///
/// This mirrors the `CSI_START` suffix of the upstream
/// `unfinishedAnsiAtEndRegex` (ansi.ts:36-38); the regex crate has no
/// negative lookahead for the OSC branch, so both branches are hand-rolled.
fn csi_unfinished(rest: &str) -> bool {
    let mut chars = rest.chars().peekable();
    match chars.next() {
        Some('\u{001B}') | Some('\u{009B}') => {}
        _ => return false,
    }
    while let Some(&ch) = chars.peek() {
        if "[\\]()#;?".contains(ch) {
            chars.next();
        } else {
            break;
        }
    }
    // Params: `(?:\d{1,4}(?:[;:]\d{0,4})*)?`.
    let mut digits = 0;
    while let Some(&ch) = chars.peek() {
        if ch.is_ascii_digit() && digits < 4 {
            chars.next();
            digits += 1;
        } else {
            break;
        }
    }
    if digits > 0 {
        loop {
            match chars.peek() {
                Some(&ch) if ch == ';' || ch == ':' => {
                    chars.next();
                    let mut group_digits = 0;
                    while let Some(&digit) = chars.peek() {
                        if digit.is_ascii_digit() && group_digits < 4 {
                            chars.next();
                            group_digits += 1;
                        } else {
                            break;
                        }
                    }
                }
                _ => break,
            }
        }
    }
    // Missing final byte: the sequence extends to the end of the text.
    chars.peek().is_none()
}

/// The start offset of an unfinished escape sequence ending the text, if any.
fn unfinished_ansi_start(window: &str) -> Option<usize> {
    for (index, ch) in window.char_indices() {
        if ch != '\u{001B}' && ch != '\u{009B}' {
            continue;
        }
        let rest = &window[index..];
        if osc_unfinished(rest) || csi_unfinished(rest) {
            return Some(index);
        }
    }
    None
}

/// Split streamed text into the part that is safe to strip now and a
/// trailing unfinished escape sequence that should be prepended to the next
/// chunk. Port of `splitIncompleteAnsiSuffix` (ansi.ts:52-63, #10504).
///
/// The window bounds the backward scan; a sequence longer than
/// [`MAX_PENDING_ANSI_LENGTH`] is processed as-is (upstream behavior).
/// Counts Unicode scalar values where upstream counts UTF-16 code units;
/// only the scan bound differs, never the returned split.
pub fn split_incomplete_ansi_suffix(value: &str) -> (String, String) {
    if !value.contains('\u{001B}') && !value.contains('\u{009B}') {
        return (value.to_owned(), String::new());
    }
    let window_start = value
        .char_indices()
        .nth_back(MAX_PENDING_ANSI_LENGTH - 1)
        .map_or(0, |(index, _)| index);
    let window = &value[window_start..];
    let Some(split_at) = unfinished_ansi_start(window) else {
        return (value.to_owned(), String::new());
    };
    let split_at = window_start + split_at;
    (value[..split_at].to_owned(), value[split_at..].to_owned())
}

/// Remove control characters and Unicode interlinear annotation characters
/// from output.
///
/// Port of `sanitizeBinaryOutput` (shell.ts:153-162 @ b485fa312): one regex
/// replace instead of a per-character array pass. Removes:
/// - Control characters `U+0000`–`U+001F` (preserves `\t`, `\n`, `\r`)
/// - Unicode interlinear annotation characters `U+FFF9`–`U+FFFB`
pub fn sanitize_binary_output(value: &str) -> String {
    let regex = BINARY_OUTPUT_REGEX.get_or_init(|| {
        // Invariant: BINARY_OUTPUT_PATTERN is a verified-valid regex literal.
        Regex::new(BINARY_OUTPUT_PATTERN)
            .expect("BINARY_OUTPUT_PATTERN is verified valid at development time")
    });
    regex.replace_all(value, "").into_owned()
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

    // #10504 (27c7b6ff4): escape sequences split across streamed chunks.

    #[test]
    fn test_split_holds_back_csi_without_final_byte() {
        let (complete, pending) = split_incomplete_ansi_suffix("\u{001B}[31mERROR\u{001B}[0");
        assert_eq!(complete, "\u{001B}[31mERROR");
        assert_eq!(pending, "\u{001B}[0");

        let (complete, pending) = split_incomplete_ansi_suffix("before\u{001B}");
        assert_eq!(complete, "before");
        assert_eq!(pending, "\u{001B}");
    }

    #[test]
    fn test_split_holds_back_unterminated_osc() {
        let (complete, pending) = split_incomplete_ansi_suffix("a\u{001B}]0;window ");
        assert_eq!(complete, "a");
        assert_eq!(pending, "\u{001B}]0;window ");

        let (complete, pending) = split_incomplete_ansi_suffix("\u{001B}]0;title\u{001B}");
        assert_eq!(complete, "");
        assert_eq!(pending, "\u{001B}]0;title\u{001B}");
    }

    #[test]
    fn test_split_keeps_terminated_sequences() {
        let (complete, pending) = split_incomplete_ansi_suffix("\u{001B}[31mred\u{001B}[0m");
        assert_eq!(complete, "\u{001B}[31mred\u{001B}[0m");
        assert!(pending.is_empty());

        let (complete, pending) = split_incomplete_ansi_suffix("\u{001B}]0;t\u{0007}text");
        assert_eq!(complete, "\u{001B}]0;t\u{0007}text");
        assert!(pending.is_empty());
    }

    #[test]
    fn test_split_skips_earlier_terminated_sequence() {
        let (complete, pending) =
            split_incomplete_ansi_suffix("\u{001B}]0;t\u{0007}after\u{001B}[3");
        assert_eq!(complete, "\u{001B}]0;t\u{0007}after");
        assert_eq!(pending, "\u{001B}[3");
    }

    #[test]
    fn test_split_fast_path_and_window() {
        let (complete, pending) = split_incomplete_ansi_suffix("plain text");
        assert_eq!(complete, "plain text");
        assert!(pending.is_empty());

        // An introducer farther back than the window is not held back.
        let long = format!("\u{001B}]{}", "x".repeat(300));
        let (complete, pending) = split_incomplete_ansi_suffix(&long);
        assert_eq!(complete, long);
        assert!(pending.is_empty());
        // The ESC is removed by sanitizeBinaryOutput, leaving the payload.
        assert_eq!(
            sanitize_binary_output(&strip_ansi(&long)),
            format!("]{}", "x".repeat(300))
        );
    }

    #[test]
    fn test_split_multi_byte_window_boundary() {
        // The window must not split a multi-byte character (would panic on
        // the string slice).
        let value = format!("{}{}", "é".repeat(300), "\u{001B}[31");
        let (complete, pending) = split_incomplete_ansi_suffix(&value);
        assert_eq!(complete, "é".repeat(300));
        assert_eq!(pending, "\u{001B}[31");
    }
}
