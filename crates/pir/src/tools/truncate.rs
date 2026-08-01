//! Shared truncation utilities for tool outputs.
//!
//! Port of `packages/coding-agent/src/core/tools/truncate.ts` @ pi 0.82.1 (2efa728).
//!
//! Truncation is based on two independent limits — whichever is hit first wins:
//! - Line limit (default: 2000 lines)
//! - Byte limit (default: 50 KB)
//!
//! Never returns partial lines (except bash tail truncation edge case).

use serde::{Deserialize, Serialize};

/// Default maximum number of lines in tool output (truncate.ts:11).
pub const DEFAULT_MAX_LINES: usize = 2000;

/// Default maximum byte count in tool output — 50 KB (truncate.ts:12).
pub const DEFAULT_MAX_BYTES: usize = 50 * 1024; // 51200

/// Maximum characters per grep match line (truncate.ts:13).
pub const GREP_MAX_LINE_LENGTH: usize = 500;

/// Which limit triggered truncation. Serialised as `"lines"` / `"bytes"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TruncatedBy {
    #[serde(rename = "lines")]
    Lines,
    #[serde(rename = "bytes")]
    Bytes,
}

/// Result of a truncation operation.
///
/// Serialised as camelCase JSON because it is embedded in tool-result `details`
/// that must byte-align with upstream (coding-standards §4.4).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TruncationResult {
    /// The truncated content.
    pub content: String,
    /// Whether truncation occurred.
    pub truncated: bool,
    /// Which limit was hit (`None` if not truncated).
    pub truncated_by: Option<TruncatedBy>,
    /// Total number of lines in the original content.
    pub total_lines: usize,
    /// Total number of bytes (UTF-8) in the original content.
    pub total_bytes: usize,
    /// Number of complete lines in the truncated output.
    pub output_lines: usize,
    /// Number of bytes (UTF-8) in the truncated output.
    pub output_bytes: usize,
    /// Whether the last line was partially truncated (tail-only edge case).
    pub last_line_partial: bool,
    /// Whether the first line exceeded the byte limit (head-only edge case).
    pub first_line_exceeds_limit: bool,
    /// The max-lines limit that was applied.
    pub max_lines: usize,
    /// The max-bytes limit that was applied.
    pub max_bytes: usize,
}

/// Options for truncation functions (truncate.ts:40-45).
#[derive(Debug, Clone, Copy)]
pub struct TruncateOptions {
    pub max_lines: usize,
    pub max_bytes: usize,
}

impl Default for TruncateOptions {
    fn default() -> Self {
        Self {
            max_lines: DEFAULT_MAX_LINES,
            max_bytes: DEFAULT_MAX_BYTES,
        }
    }
}

/// Result of truncating a single line (truncate.ts:268-275).
#[derive(Debug, Clone)]
pub struct TruncatedLine {
    pub text: String,
    pub was_truncated: bool,
}

// -----------------------------------------------------------------------
// splitLinesForCounting (truncate.ts:47-56)
// -----------------------------------------------------------------------

/// Split content into lines for counting purposes.
///
/// Uses `split('\n')` semantics: if the content ends with `\n`, the trailing
/// empty element is popped (a trailing newline does not count as an extra
/// line). Empty content returns zero lines.
fn split_lines_for_counting(content: &str) -> Vec<&str> {
    if content.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&str> = content.split('\n').collect();
    if content.ends_with('\n') {
        lines.pop();
    }
    lines
}

// -----------------------------------------------------------------------
// formatSize (truncate.ts:61-69)
// -----------------------------------------------------------------------

/// Format a byte count as a human-readable size string.
///
/// `< 1 KB` → `"NB"`; `< 1 MB` → `"N.NKB"`; else `"N.NMB"`.
/// Mirrors JS `toFixed(1)` (one decimal place).
pub fn format_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

// -----------------------------------------------------------------------
// truncateHead (truncate.ts:78-160)
// -----------------------------------------------------------------------

/// Truncate content from the head (keep first N lines/bytes).
///
/// Suitable for file reads where you want to see the beginning. Never returns
/// partial lines. If the first line alone exceeds the byte limit, returns empty
/// content with `first_line_exceeds_limit = true`.
pub fn truncate_head(content: &str, options: Option<TruncateOptions>) -> TruncationResult {
    let opts = options.unwrap_or_default();
    let max_lines = opts.max_lines;
    let max_bytes = opts.max_bytes;

    let total_bytes = content.len(); // UTF-8 byte length
    let lines = split_lines_for_counting(content);
    let total_lines = lines.len();

    // No truncation needed (truncate.ts:87-101)
    if total_lines <= max_lines && total_bytes <= max_bytes {
        return TruncationResult {
            content: content.to_string(),
            truncated: false,
            truncated_by: None,
            total_lines,
            total_bytes,
            output_lines: total_lines,
            output_bytes: total_bytes,
            last_line_partial: false,
            first_line_exceeds_limit: false,
            max_lines,
            max_bytes,
        };
    }

    // First-line exceeds byte limit (truncate.ts:104-119)
    let first_line_bytes = lines[0].len();
    if first_line_bytes > max_bytes {
        return TruncationResult {
            content: String::new(),
            truncated: true,
            truncated_by: Some(TruncatedBy::Bytes),
            total_lines,
            total_bytes,
            output_lines: 0,
            output_bytes: 0,
            last_line_partial: false,
            first_line_exceeds_limit: true,
            max_lines,
            max_bytes,
        };
    }

    // Collect complete lines that fit (truncate.ts:122-160)
    let mut output_bytes_count = 0usize;
    let mut truncated_by = TruncatedBy::Lines;
    let mut collected_count = 0usize;

    for (i, line) in lines.iter().enumerate().take(lines.len().min(max_lines)) {
        let line_bytes = line.len() + if i > 0 { 1 } else { 0 }; // +1 for newline

        if output_bytes_count + line_bytes > max_bytes {
            truncated_by = TruncatedBy::Bytes;
            break;
        }

        output_bytes_count += line_bytes;
        collected_count += 1;
    }

    // If we stopped because of the line limit (truncate.ts:140-142)
    if collected_count >= max_lines && output_bytes_count <= max_bytes {
        truncated_by = TruncatedBy::Lines;
    }

    let output_content: String = lines[..collected_count].join("\n");
    let final_output_bytes = output_content.len();

    TruncationResult {
        content: output_content,
        truncated: true,
        truncated_by: Some(truncated_by),
        total_lines,
        total_bytes,
        output_lines: collected_count,
        output_bytes: final_output_bytes,
        last_line_partial: false,
        first_line_exceeds_limit: false,
        max_lines,
        max_bytes,
    }
}

// -----------------------------------------------------------------------
// truncateTail (truncate.ts:168-241)
// -----------------------------------------------------------------------

/// Truncate content from the tail (keep last N lines/bytes).
///
/// Suitable for bash output where you want to see the end (errors, final
/// results). May return a partial first line if the last line of the original
/// content exceeds the byte limit.
pub fn truncate_tail(content: &str, options: Option<TruncateOptions>) -> TruncationResult {
    let opts = options.unwrap_or_default();
    let max_lines = opts.max_lines;
    let max_bytes = opts.max_bytes;

    let total_bytes = content.len();
    let lines = split_lines_for_counting(content);
    let total_lines = lines.len();

    // No truncation needed (truncate.ts:177-191)
    if total_lines <= max_lines && total_bytes <= max_bytes {
        return TruncationResult {
            content: content.to_string(),
            truncated: false,
            truncated_by: None,
            total_lines,
            total_bytes,
            output_lines: total_lines,
            output_bytes: total_bytes,
            last_line_partial: false,
            first_line_exceeds_limit: false,
            max_lines,
            max_bytes,
        };
    }

    // Work backwards from the end (truncate.ts:194-241)
    let mut output_bytes_count = 0usize;
    let mut truncated_by = TruncatedBy::Lines;
    let mut last_line_partial = false;
    let mut collected: Vec<&str> = Vec::new();
    // Stores the partial-line text when the edge case fires (owned, because
    // truncateStringToBytesFromEnd returns an allocated String).
    let mut partial_line_text: Option<String> = None;

    let mut i = lines.len() as isize - 1;
    while i >= 0 && collected.len() < max_lines {
        let line = lines[i as usize];
        let line_bytes = line.len() + if !collected.is_empty() { 1 } else { 0 };

        if output_bytes_count + line_bytes > max_bytes {
            truncated_by = TruncatedBy::Bytes;
            // Edge case: no lines collected yet and this line exceeds maxBytes
            // → take the end of the line (partial) (truncate.ts:207-213)
            if collected.is_empty() {
                let truncated_line = truncate_string_to_bytes_from_end(line, max_bytes);
                output_bytes_count = truncated_line.len();
                partial_line_text = Some(truncated_line);
                last_line_partial = true;
            }
            break;
        }

        collected.insert(0, line);
        output_bytes_count += line_bytes;
        i -= 1;
    }

    // Assemble the output content.
    let (output_content, output_line_count, final_output_bytes) =
        if let Some(pl) = partial_line_text {
            // Partial-line edge case: single partial line.
            let bytes = pl.len();
            (pl, 1, bytes)
        } else {
            // If stopped due to line limit (truncate.ts:221-223)
            if collected.len() >= max_lines && output_bytes_count <= max_bytes {
                truncated_by = TruncatedBy::Lines;
            }
            let joined = collected.join("\n");
            let bytes = joined.len();
            (joined, collected.len(), bytes)
        };

    TruncationResult {
        content: output_content,
        truncated: true,
        truncated_by: Some(truncated_by),
        total_lines,
        total_bytes,
        output_lines: output_line_count,
        output_bytes: final_output_bytes,
        last_line_partial,
        first_line_exceeds_limit: false,
        max_lines,
        max_bytes,
    }
}

/// Truncate a string to fit within a byte limit, keeping the **end**.
///
/// Handles multi-byte UTF-8 characters correctly by skipping continuation
/// bytes (`0b10xxxxxx`) at the cut boundary.
///
/// Port of `truncateStringToBytesFromEnd` (truncate.ts:247-262).
fn truncate_string_to_bytes_from_end(s: &str, max_bytes: usize) -> String {
    let bytes = s.as_bytes();
    if bytes.len() <= max_bytes {
        return s.to_string();
    }

    let mut start = bytes.len() - max_bytes;

    // Skip UTF-8 continuation bytes to find a valid character boundary.
    while start < bytes.len() && (bytes[start] & 0xc0) == 0x80 {
        start += 1;
    }

    // The bytes[start..] are valid UTF-8 (start is a char boundary).
    match std::str::from_utf8(&bytes[start..]) {
        Ok(s) => s.to_string(),
        Err(_) => String::from_utf8_lossy(&bytes[start..]).into_owned(),
    }
}

// -----------------------------------------------------------------------
// truncateLine (truncate.ts:268-275)
// -----------------------------------------------------------------------

/// Truncate a single line to `max_chars` UTF-16 code units, adding a suffix.
///
/// **Important**: upstream uses JS `string.length` / `slice(0, max)` which
/// operate on **UTF-16 code units**. We replicate this with `encode_utf16`.
/// When the cut falls inside a surrogate pair, the trailing lone high
/// surrogate is dropped (Rust strings cannot hold lone surrogates).
pub fn truncate_line(line: &str, max_chars: Option<usize>) -> TruncatedLine {
    let max_chars = max_chars.unwrap_or(GREP_MAX_LINE_LENGTH);

    let utf16: Vec<u16> = line.encode_utf16().collect();
    if utf16.len() <= max_chars {
        return TruncatedLine {
            text: line.to_string(),
            was_truncated: false,
        };
    }

    let mut truncated: Vec<u16> = utf16.iter().take(max_chars).copied().collect();

    // If we sliced in the middle of a surrogate pair (last element is a high
    // surrogate 0xD800-0xDBFF), drop it to avoid a lone surrogate.
    if let Some(&last) = truncated.last() {
        if (0xD800..=0xDBFF).contains(&last) {
            truncated.pop();
        }
    }

    let text = String::from_utf16_lossy(&truncated);
    TruncatedLine {
        text: format!("{text}... [truncated]"),
        was_truncated: true,
    }
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn byte_len(s: &str) -> usize {
        s.len()
    }

    // Port of "counts UTF-8 bytes without Node Buffer"
    #[test]
    fn test_counts_utf8_bytes() {
        let content = "a\u{00E9}\u{1F642}\nb"; // a é 🙂 \n b
        let result = truncate_head(
            content,
            Some(TruncateOptions {
                max_bytes: 100,
                max_lines: 10,
            }),
        );

        assert!(!result.truncated);
        assert_eq!(result.total_bytes, byte_len(content));
        assert_eq!(result.output_bytes, byte_len(content));
        assert_eq!(result.total_bytes, 9); // 1+2+4+1+1
    }

    // Port of "does not count a trailing newline as an extra line"
    #[test]
    fn test_trailing_newline_not_extra_line() {
        let content = "line\nline\nline\n"; // 3 lines + trailing newline
        let head = truncate_head(
            content,
            Some(TruncateOptions {
                max_bytes: 100,
                max_lines: 3,
            }),
        );
        let tail = truncate_tail(
            content,
            Some(TruncateOptions {
                max_bytes: 100,
                max_lines: 3,
            }),
        );

        assert!(!head.truncated);
        assert_eq!(head.total_lines, 3);
        assert_eq!(head.output_lines, 3);
        assert!(!tail.truncated);
        assert_eq!(tail.total_lines, 3);
        assert_eq!(tail.output_lines, 3);
    }

    // Port of "truncates head on UTF-8 byte limits without partial lines"
    #[test]
    fn test_head_truncate_no_partial_lines() {
        let content = "\u{00E9}\u{00E9}\nabc"; // éé\nabc (4 bytes + \n + 3 bytes)
        let result = truncate_head(
            content,
            Some(TruncateOptions {
                max_bytes: 4,
                max_lines: 10,
            }),
        );

        assert_eq!(result.content, "\u{00E9}\u{00E9}");
        assert!(result.truncated);
        assert_eq!(result.truncated_by, Some(TruncatedBy::Bytes));
        assert_eq!(result.output_bytes, 4);
        assert!(!result.first_line_exceeds_limit);
    }

    // Port of "reports head truncation when the first line exceeds the byte limit"
    #[test]
    fn test_head_first_line_exceeds_limit() {
        let result = truncate_head(
            "\u{00E9}\u{00E9}\nabc",
            Some(TruncateOptions {
                max_bytes: 3,
                max_lines: 10,
            }),
        );

        assert_eq!(result.content, "");
        assert!(result.truncated);
        assert_eq!(result.truncated_by, Some(TruncatedBy::Bytes));
        assert!(result.first_line_exceeds_limit);
    }

    // Port of "truncates tail on UTF-8 boundaries when only a partial last line fits"
    #[test]
    fn test_tail_partial_line_utf8_boundary() {
        // a(1) é(2) 🙂(4) b(1) = 8 bytes total; maxBytes=5
        let result = truncate_tail(
            "a\u{00E9}\u{1F642}b",
            Some(TruncateOptions {
                max_bytes: 5,
                max_lines: 10,
            }),
        );

        assert_eq!(result.content, "\u{1F642}b"); // 🙂(4) + b(1) = 5 bytes
        assert!(result.truncated);
        assert_eq!(result.truncated_by, Some(TruncatedBy::Bytes));
        assert!(result.last_line_partial);
        assert_eq!(result.output_bytes, 5);
    }

    // Port of "truncates an oversized single line with a trailing newline"
    #[test]
    fn test_tail_oversized_single_line_trailing_newline() {
        let input = format!("{}\n", "X".repeat(300_000));
        let result = truncate_tail(
            &input,
            Some(TruncateOptions {
                max_bytes: 1024,
                max_lines: 100,
            }),
        );

        assert_eq!(result.content, "X".repeat(1024));
        assert_eq!(result.output_bytes, 1024);
        assert_eq!(result.output_lines, 1);
        assert!(result.last_line_partial);
        assert_eq!(result.truncated_by, Some(TruncatedBy::Bytes));
    }

    // Port of "drops an oversized trailing character when it cannot fit in tail byte limit"
    #[test]
    fn test_tail_drops_oversized_char() {
        // abc(3) + 🙂(4) = 7 bytes; maxBytes=3 → skip past 🙂, result empty
        let result = truncate_tail(
            "abc\u{1F642}",
            Some(TruncateOptions {
                max_bytes: 3,
                max_lines: 10,
            }),
        );

        assert_eq!(result.content, "");
        assert!(result.truncated);
        assert_eq!(result.truncated_by, Some(TruncatedBy::Bytes));
        assert!(result.last_line_partial);
        assert_eq!(result.output_bytes, 0);
    }

    // Port of "matches Buffer tail truncation semantics for surrogate edge cases"
    #[test]
    fn test_tail_buffer_semantics_surrogate_edge_cases() {
        // Rust strings are valid UTF-8, so lone surrogates cannot appear.
        // We verify the multi-byte boundary logic with valid Unicode chars.
        let cases: &[(&str, usize, &str)] = &[
            // input, max_bytes, expected_tail
            ("\u{1F469}\u{200D}\u{1F4BB}", 4, "\u{1F4BB}"), // 👩‍💻 → last 4 bytes = 💻
        ];
        for &(input, max_bytes, expected) in cases {
            let result = truncate_tail(
                input,
                Some(TruncateOptions {
                    max_bytes,
                    max_lines: 10,
                }),
            );
            assert_eq!(
                result.content, expected,
                "input={input:?} max_bytes={max_bytes}"
            );
            assert!(result.output_bytes <= max_bytes);
        }
    }

    // Deterministic fuzz-like test (simplified port)
    #[test]
    fn test_tail_fuzz_ascii_and_multibyte() {
        let alphabet = [
            "a",
            "\u{007F}",
            "\u{0080}",
            "\u{00E9}",
            "\u{07FF}",
            "\u{0800}",
            "\u{4E2D}",
            "\u{1F642}",
        ];

        // Check that truncate_tail output never exceeds max_bytes and ends on
        // a valid UTF-8 boundary.
        for &a in &alphabet {
            for &b in &alphabet {
                for &c in &alphabet {
                    let input = format!("{a}{b}{c}");
                    let total = input.len();
                    for max_bytes in 0..=total + 2 {
                        let result = truncate_tail(
                            &input,
                            Some(TruncateOptions {
                                max_bytes,
                                max_lines: 10,
                            }),
                        );
                        assert!(
                            result.output_bytes <= max_bytes,
                            "output_bytes {} > max_bytes {} for input {:?}",
                            result.output_bytes,
                            max_bytes,
                            input
                        );
                        // Content must be valid UTF-8 (already guaranteed by String).
                        assert!(std::str::from_utf8(result.content.as_bytes()).is_ok());
                    }
                }
            }
        }
    }

    #[test]
    fn test_truncate_line_short() {
        let result = truncate_line("hello", None);
        assert!(!result.was_truncated);
        assert_eq!(result.text, "hello");
    }

    #[test]
    fn test_truncate_line_exact_limit() {
        let s = "a".repeat(500);
        let result = truncate_line(&s, None);
        assert!(!result.was_truncated);
        assert_eq!(result.text, s);
    }

    #[test]
    fn test_truncate_line_exceeds_limit() {
        let s = "a".repeat(600);
        let result = truncate_line(&s, None);
        assert!(result.was_truncated);
        assert!(result.text.starts_with(&"a".repeat(500)));
        assert!(result.text.ends_with("... [truncated]"));
    }

    #[test]
    fn test_truncate_line_surrogate_pair_boundary() {
        // 🙂 = 1 UTF-16 code unit pair (2 units). If we cut at 1, the high
        // surrogate is dropped.
        let s: String = "a".repeat(500) + "\u{1F642}"; // 500 a's + 🙂
        let result = truncate_line(&s, None);
        assert!(result.was_truncated);
        // Should be 500 a's + suffix (the surrogate pair is at position 500/501,
        // but 500 a's are 500 UTF-16 units, so the cut is exactly at the boundary).
        assert!(result.text.starts_with(&"a".repeat(500)));
    }

    #[test]
    fn test_format_size() {
        assert_eq!(format_size(0), "0B");
        assert_eq!(format_size(512), "512B");
        assert_eq!(format_size(1023), "1023B");
        assert_eq!(format_size(1024), "1.0KB");
        assert_eq!(format_size(51200), "50.0KB");
        assert_eq!(format_size(1024 * 1024), "1.0MB");
        assert_eq!(format_size(1024 * 1024 * 5), "5.0MB");
    }

    #[test]
    fn test_truncation_result_serde_camel_case() {
        let result = TruncationResult {
            content: "test".to_string(),
            truncated: true,
            truncated_by: Some(TruncatedBy::Bytes),
            total_lines: 100,
            total_bytes: 200,
            output_lines: 50,
            output_bytes: 100,
            last_line_partial: false,
            first_line_exceeds_limit: true,
            max_lines: 2000,
            max_bytes: 51200,
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("\"truncatedBy\""));
        assert!(json.contains("\"totalLines\""));
        assert!(json.contains("\"outputBytes\""));
        assert!(json.contains("\"firstLineExceedsLimit\""));
        assert!(json.contains("\"lastLinePartial\""));
        assert!(json.contains("\"maxLines\""));
        assert!(json.contains("\"maxBytes\""));
        assert!(json.contains("\"bytes\""));
    }
}
