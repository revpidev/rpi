//! stdout → ComponentTree / setStatus text (TE12 FR-G).
//!
//! Render structure: a `column` of one `text` node per output line with
//! `truncate: true`. Rationale (TE12 "宽度" verification): the rpi
//! `TruncatedText` keeps each node single-line (clips to the render width
//! with an ANSI-safe `...`), so a wide statusline degrades like CC's
//! per-line clipping instead of word-wrapping into extra rows; the node
//! sets NO `fg`/`dim`, so the script's own ANSI passes through verbatim
//! (`component_tree.rs:110-142` only wraps when styling props are set).

use serde_json::{json, Value};

/// Rendering lines cap (defensive; CC documents no cap).
pub const MAX_LINES: usize = 8;

/// `\x1b[0m` — SGR reset.
const RESET: &str = "\x1b[0m";

/// Build the `ui.setFooter` component tree from raw script stdout
/// (`{"component": <tree>}`). Empty stdout renders an empty column (zero
/// footer rows).
pub fn footer_tree(stdout: &str, padding: usize) -> Value {
    let trimmed = stdout.trim();
    let children: Vec<Value> = if trimmed.is_empty() {
        Vec::new()
    } else {
        let prefix = " ".repeat(padding);
        trimmed
            .split('\n')
            .take(MAX_LINES)
            .map(|line| {
                json!({
                    "type": "text",
                    "props": {
                        "text": format!("{prefix}{}", normalize_line(line)),
                        "truncate": true,
                    },
                })
            })
            .collect()
    };
    json!({"type": "column", "props": {}, "children": children})
}

/// Build the `ui.setStatus` text (`status` placement): the whole stdout,
/// trimmed. The host folds multi-line whitespace into single spaces
/// (footer.rs `sanitize_status_text`) — the documented degradation of the
/// `status` placement (user decision 2026-08-19). `None` for empty output
/// (nothing to publish this round).
pub fn status_text(stdout: &str) -> Option<String> {
    let text = stdout.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

/// Per-line normalization: strip a trailing `\r` (CRLF scripts) and append
/// an SGR reset when the line ends with SGR styling still active — an
/// unterminated escape at the node boundary would leak the color into
/// whatever the host renders after the column.
fn normalize_line(line: &str) -> String {
    let line = line.strip_suffix('\r').unwrap_or(line);
    if sgr_still_active(line) {
        format!("{line}{RESET}")
    } else {
        line.to_owned()
    }
}

/// Whether SGR styling is still active at the end of `line`: scan the CSI
/// `m` sequences; a sequence resets when ALL of its parameters are 0/empty
/// (`0m`, `m`, `0;0m`); anything else (`31m`, `0;32m`, `39m`) leaves or
/// sets styling. Approximation on purpose: over-appending a reset is
/// harmless, and tracking per-attribute state is not worth it for a
/// statusline renderer.
fn sgr_still_active(line: &str) -> bool {
    let bytes = line.as_bytes();
    let mut styled = false;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\x1b' && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            let mut j = i + 2;
            while j < bytes.len() && !(0x40..=0x7e).contains(&bytes[j]) {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'm' {
                let resets = line[i + 2..j]
                    .split(';')
                    .all(|param| param.is_empty() || param == "0");
                styled = !resets;
            }
            i = j + 1;
        } else {
            i += 1;
        }
    }
    styled
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_color_lines_become_truncated_text_nodes() {
        let stdout =
            "\x1b[36m\x1b[1m◆ GLM-5.1\x1b[0m · \x1b[90m❖ xhigh\x1b[0m\n\x1b[32m██████░░░░\x1b[0m 43%"
                .to_owned();
        let tree = footer_tree(&stdout, 0);
        assert_eq!(tree["type"], "column");
        let children = tree["children"].as_array().expect("children");
        assert_eq!(children.len(), 2);
        for child in children {
            assert_eq!(child["type"], "text");
            assert_eq!(child["props"]["truncate"], true);
            // No fg/dim: the script's ANSI must pass through untouched.
            assert!(child["props"].get("fg").is_none());
            assert!(child["props"].get("dim").is_none());
        }
        assert_eq!(
            children[0]["props"]["text"].as_str().unwrap(),
            "\x1b[36m\x1b[1m◆ GLM-5.1\x1b[0m · \x1b[90m❖ xhigh\x1b[0m"
        );
        assert_eq!(
            children[1]["props"]["text"].as_str().unwrap(),
            "\x1b[32m██████░░░░\x1b[0m 43%"
        );
    }

    #[test]
    fn unterminated_sgr_gets_a_reset_appended() {
        let tree = footer_tree("\x1b[31mred line\nplain", 0);
        let children = tree["children"].as_array().expect("children");
        assert_eq!(
            children[0]["props"]["text"].as_str().unwrap(),
            "\x1b[31mred line\x1b[0m"
        );
        assert_eq!(children[1]["props"]["text"].as_str().unwrap(), "plain");
    }

    #[test]
    fn already_reset_lines_are_left_alone() {
        // Style opened then fully reset mid-line: no extra reset appended.
        let tree = footer_tree("\x1b[32mbar\x1b[0m 43%\n\x1b[0m", 0);
        let children = tree["children"].as_array().expect("children");
        assert_eq!(
            children[0]["props"]["text"].as_str().unwrap(),
            "\x1b[32mbar\x1b[0m 43%"
        );
        assert_eq!(children[1]["props"]["text"].as_str().unwrap(), "\x1b[0m");
    }

    #[test]
    fn crlf_and_trailing_newlines_are_normalized() {
        // Trailing blank lines are trimmed away; a blank line in the
        // MIDDLE survives as an empty text node.
        let tree = footer_tree("line one\r\n\r\nline two\r\n\n", 0);
        let children = tree["children"].as_array().expect("children");
        assert_eq!(children.len(), 3);
        assert_eq!(children[0]["props"]["text"].as_str().unwrap(), "line one");
        assert_eq!(children[1]["props"]["text"].as_str().unwrap(), "");
        assert_eq!(children[2]["props"]["text"].as_str().unwrap(), "line two");
    }

    #[test]
    fn empty_stdout_renders_an_empty_column() {
        let tree = footer_tree("", 0);
        assert_eq!(tree["type"], "column");
        assert_eq!(tree["children"].as_array().map(Vec::len), Some(0));
        assert_eq!(status_text(""), None);
        assert_eq!(status_text("  \n  "), None);
    }

    #[test]
    fn more_than_max_lines_are_dropped() {
        let stdout = (1..=20)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let tree = footer_tree(&stdout, 0);
        assert_eq!(tree["children"].as_array().map(Vec::len), Some(MAX_LINES));
    }

    #[test]
    fn padding_prefixes_every_line() {
        let tree = footer_tree("a\nb", 2);
        let children = tree["children"].as_array().expect("children");
        assert_eq!(children[0]["props"]["text"].as_str().unwrap(), "  a");
        assert_eq!(children[1]["props"]["text"].as_str().unwrap(), "  b");
    }

    #[test]
    fn status_text_returns_trimmed_whole_output() {
        assert_eq!(status_text(" one \n two \n"), Some("one \n two".to_owned()));
    }
}
