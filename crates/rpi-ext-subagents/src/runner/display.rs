//! Display-text primitives shared by the streaming progress snapshots and
//! the notify formatting (TE09).
//!
//! Port of pi-subagents `src/shared/display-text.ts` (`sanitizeDisplayText`
//! :38-82, `truncateDisplayText` :14-24, `previewDisplayText` :26-33) and
//! the formatter helpers it feeds: `formatDuration` / `shortenPath` /
//! `formatToolCall` (`src/shared/formatters.ts:50-54, 125-131, 100-120`),
//! `extractToolArgsPreview` (`src/shared/utils.ts:546-589`),
//! `resolveCurrentPath` (`src/runs/shared/long-running-guard.ts:55-69`) and
//! `extractTextFromContent` (`src/shared/utils.ts:594-619`).
//!
//! Length arithmetic is by UTF-16 code units (JS `String.length` /
//! `char.length`) so previews and truncations match upstream byte-for-byte
//! on the wire; the scan itself walks Unicode scalar values (JS
//! `codePointAt`).

/// `truncateDisplayText` (display-text.ts:14-24): first `max_length` UTF-16
/// units, no suffix.
pub fn truncate_display_text(value: &str, max_length: usize) -> String {
    if max_length == 0 {
        return String::new();
    }
    if value.encode_utf16().count() <= max_length {
        return value.to_string();
    }
    let mut units = 0usize;
    let mut out = String::new();
    for ch in value.chars() {
        let width = ch.len_utf16();
        if units + width > max_length {
            break;
        }
        units += width;
        out.push(ch);
    }
    out
}

/// `previewDisplayText` (display-text.ts:26-33): sanitized, then either
/// verbatim, a hard truncation (budget ≤ 3), or `...`-suffixed truncation.
pub fn preview_display_text(value: &str, max_length: usize) -> String {
    let normalized = sanitize_display_text(value);
    if normalized.encode_utf16().count() <= max_length {
        return normalized;
    }
    if max_length <= 3 {
        return truncate_display_text(&normalized, max_length);
    }
    format!("{}...", truncate_display_text(&normalized, max_length - 3))
}

/// `sanitizeDisplayText` (display-text.ts:38-82): strip ANSI escape
/// sequences (CSI / OSC / DCS / SOS / PM / APC and single-char escapes),
/// drop control and whitespace code points, collapse runs to one space and
/// trim. A dropped run leaves a single pending space, never a leading one.
pub fn sanitize_display_text(value: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    let mut output: Vec<String> = Vec::new();
    let mut pending_space = false;

    fn is_whitespace_or_control(code_point: u32) -> bool {
        if code_point <= 0x20 || (0x7f..=0x9f).contains(&code_point) {
            return true;
        }
        // JS surrogates (0xd800-0xdfff) never surface as Rust chars. The
        // `String.fromCodePoint(cp).trim()` tail is the Unicode whitespace
        // set (incl. NBSP); U+FEFF is trimmed by JS but is not
        // `is_whitespace` in Rust — special-cased to keep parity.
        code_point == 0xfeff
            || char::from_u32(code_point)
                .map(char::is_whitespace)
                .unwrap_or(false)
    }

    // consumeControlString (display-text.ts:9-20): scan to ST (0x9c or
    // ESC \), or BEL when `osc`.
    fn consume_control_string(chars: &[char], mut index: usize, osc: bool) -> usize {
        while index < chars.len() {
            let code_point = chars[index] as u32;
            if osc && code_point == 0x07 {
                return index + 1;
            }
            if code_point == 0x9c {
                return index + 1;
            }
            if code_point == 0x1b && chars.get(index + 1) == Some(&'\\') {
                return index + 2;
            }
            index += 1;
        }
        chars.len()
    }

    // consumeCsi (display-text.ts:22-30): scan to the final byte 0x40-0x7e.
    fn consume_csi(chars: &[char], mut index: usize) -> usize {
        while index < chars.len() {
            let code_point = chars[index] as u32;
            if (0x40..=0x7e).contains(&code_point) {
                return index + 1;
            }
            index += 1;
        }
        chars.len()
    }

    let mut index = 0usize;
    while index < chars.len() {
        let code_point = chars[index] as u32;
        // appendSpace(): a dropped run only *marks* a space when something
        // precedes it — leading escapes never create a leading space.
        let mark_space = |output: &mut Vec<String>, pending: &mut bool| {
            if !output.is_empty() {
                *pending = true;
            }
        };
        if code_point == 0x1b {
            let next = chars.get(index + 1).copied();
            mark_space(&mut output, &mut pending_space);
            match next {
                Some('[') => {
                    index = consume_csi(&chars, index + 2);
                }
                Some(']') | Some('P') | Some('X') | Some('^') | Some('_') => {
                    index = consume_control_string(&chars, index + 2, next == Some(']'));
                }
                Some(_) => {
                    index += 2;
                }
                None => {
                    index += 1;
                }
            }
            continue;
        }
        if code_point == 0x9b {
            mark_space(&mut output, &mut pending_space);
            index = consume_csi(&chars, index + 1);
            continue;
        }
        if matches!(code_point, 0x90 | 0x98 | 0x9d | 0x9e | 0x9f) {
            mark_space(&mut output, &mut pending_space);
            index = consume_control_string(&chars, index + 1, code_point == 0x9d);
            continue;
        }
        if is_whitespace_or_control(code_point) {
            if !output.is_empty() {
                pending_space = true;
            }
        } else {
            if pending_space {
                output.push(" ".to_string());
            }
            output.push(chars[index].to_string());
            pending_space = false;
        }
        index += 1;
    }
    output.join("")
}

/// `formatDuration` (formatters.ts:50-54): ms → `250ms` / `1.2s` /
/// `3m05s`-style `minutes m seconds s` (no hours unit, unbounded minutes).
pub fn format_duration(ms: u64) -> String {
    if ms < 1000 {
        return format!("{ms}ms");
    }
    if ms < 60_000 {
        return format!("{:.1}s", ms as f64 / 1000.0);
    }
    format!("{}m{}s", ms / 60_000, (ms % 60_000) / 1000)
}

/// `shortenPath` (formatters.ts:125-131): `$HOME` prefix → `~`.
pub fn shorten_path(path: &str) -> String {
    if let Some(home) = std::env::var_os("HOME") {
        let home = home.to_string_lossy();
        if !home.is_empty() {
            if let Some(rest) = path.strip_prefix(home.as_ref()) {
                return format!("~{rest}");
            }
        }
    }
    path.to_string()
}

/// `extractTextFromContent` (utils.ts:594-619): join the text of `text`
/// blocks (and nested `tool_result` content) with newlines.
pub fn extract_text_from_content(content: &serde_json::Value) -> String {
    let Some(parts) = content.as_array() else {
        return String::new();
    };
    let mut texts: Vec<String> = Vec::new();
    for part in parts {
        let Some(part) = part.as_object() else {
            continue;
        };
        if part.get("type").and_then(|v| v.as_str()) == Some("text") {
            if let Some(text) = part.get("text") {
                texts.push(text.as_str().unwrap_or_default().to_string());
            }
        } else if part.get("type").and_then(|v| v.as_str()) == Some("tool_result") {
            if let Some(inner) = part.get("content") {
                let inner = extract_text_from_content(inner);
                if !inner.is_empty() {
                    texts.push(inner);
                }
            }
        } else if let Some(text) = part.get("text") {
            texts.push(text.as_str().unwrap_or_default().to_string());
        }
    }
    texts.join("\n")
}

/// `resolveCurrentPath` (long-running-guard.ts:55-69): first non-empty
/// string among `path/file/filename/target/cwd`; `bash` additionally
/// extracts the redirect target (`>`, `>>` or `tee `) from the command.
pub fn resolve_current_path(
    tool_name: Option<&str>,
    args: Option<&serde_json::Value>,
) -> Option<String> {
    let tool_name = tool_name?;
    let args = args?;
    let direct = ["path", "file", "filename", "target", "cwd"];
    for key in direct {
        if let Some(value) = args.get(key).and_then(|v| v.as_str()) {
            if !value.trim().is_empty() {
                return Some(value.trim().to_string());
            }
        }
    }
    if tool_name == "bash" {
        let command = args.get("command").and_then(|v| v.as_str())?;
        // `/(?:>|>>|tee\s+)(\S+)/` — first match by scan position. The
        // single-char `>` alternative always shadows `>>` (regex alternation
        // order), so a `>` captures the non-space run immediately after it
        // and a `> ` (trailing space) fails the whole position; `tee\s+`
        // only participates at a `tee` position. Bug-for-bug with upstream.
        let chars: Vec<char> = command.chars().collect();
        let mut index = 0usize;
        while index < chars.len() {
            let captured = if chars[index] == '>' {
                Some(index + 1)
            } else if chars[index..].starts_with(&['t', 'e', 'e']) {
                // `tee` must be followed by at least one whitespace, which
                // is consumed before the capture.
                let mut end = index + 3;
                while end < chars.len() && chars[end].is_whitespace() {
                    end += 1;
                }
                (end > index + 3).then_some(end)
            } else {
                None
            };
            if let Some(start) = captured {
                let target: String = chars[start..]
                    .iter()
                    .take_while(|c| !c.is_whitespace())
                    .collect();
                if !target.is_empty() {
                    return Some(target);
                }
            }
            index += 1;
        }
    }
    None
}

/// `formatToolCall` (formatters.ts:100-120): the `text`/`expandedText`
/// summaries used by streamed tool-call lists.
pub fn format_tool_call(name: &str, args: &serde_json::Value, expanded: bool) -> String {
    let arg_str = |key: &str| args.get(key).and_then(|v| v.as_str());
    match name {
        "bash" => {
            let command = arg_str("command").unwrap_or_default();
            format!(
                "$ {}",
                preview_display_text(command, if expanded { 240 } else { 60 })
            )
        }
        "read" | "write" | "edit" => {
            let target = arg_str("path")
                .or_else(|| arg_str("file_path"))
                .unwrap_or_default();
            format!("{name} {}", sanitize_display_text(&shorten_path(target)))
        }
        _ => format!(
            "{name} {}",
            preview_display_text(&args.to_string(), if expanded { 160 } else { 40 })
        ),
    }
}

/// `extractToolArgsPreview` (utils.ts:546-589): the one-line argument
/// preview for `progress.currentToolArgs`, in upstream branch order.
pub fn extract_tool_args_preview(args: &serde_json::Value) -> String {
    let preview_value = |value: &serde_json::Value| -> Option<String> {
        match value {
            serde_json::Value::String(raw) if !raw.trim().is_empty() => {
                Some(sanitize_display_text(raw))
            }
            serde_json::Value::Number(number) => Some(number.to_string()),
            serde_json::Value::Bool(flag) => Some(flag.to_string()),
            _ => None,
        }
    };
    let preview_array = |value: &serde_json::Value| -> Option<String> {
        let items = value.as_array()?;
        if items.is_empty() {
            return None;
        }
        let first = preview_value(items.first()?)?;
        let suffix = if items.len() > 1 {
            format!(" (+{} more)", items.len() - 1)
        } else {
            String::new()
        };
        Some(format!("{first}{suffix}"))
    };

    if let Some(tool) = args.get("tool").and_then(|v| v.as_str()) {
        let server = match args.get("server").and_then(|v| v.as_str()) {
            Some(server) => format!("{}/", sanitize_display_text(server)),
            None => String::new(),
        };
        let tool_args = match args.get("args").and_then(|v| v.as_str()) {
            Some(raw) => format!(
                " {}",
                truncate_display_text(&sanitize_display_text(raw), 40)
            ),
            None => String::new(),
        };
        return sanitize_display_text(&format!("{server}{tool}{tool_args}"));
    }

    if let Some(queries) = preview_array(args.get("queries").unwrap_or(&serde_json::Value::Null)) {
        return preview_display_text(&queries, 60);
    }
    let nonempty_str = |key: &str| -> Option<&str> {
        args.get(key)
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
    };
    if let Some(query) = nonempty_str("query") {
        return preview_display_text(query, 60);
    }
    if let Some(workflow) = nonempty_str("workflow") {
        return format!("workflow={}", preview_display_text(workflow, 48));
    }
    if let Some(url) = nonempty_str("url") {
        return preview_display_text(url, 60);
    }
    if let Some(urls) = preview_array(args.get("urls").unwrap_or(&serde_json::Value::Null)) {
        return preview_display_text(&urls, 60);
    }
    if let Some(prompt) = nonempty_str("prompt") {
        return preview_display_text(prompt, 60);
    }
    let preview_keys = [
        "command",
        "path",
        "file_path",
        "pattern",
        "query",
        "url",
        "task",
        "describe",
        "search",
    ];
    for key in preview_keys {
        if let Some(value) = nonempty_str(key) {
            return preview_display_text(value, 60);
        }
    }
    if let Some(map) = args.as_object() {
        for (key, value) in map {
            let display_key = sanitize_display_text(key);
            if let Some(array) = preview_array(value) {
                return format!("{display_key}={}", preview_display_text(&array, 50));
            }
            if let serde_json::Value::String(raw) = value {
                if !raw.is_empty() {
                    return format!("{display_key}={}", preview_display_text(raw, 50));
                }
            }
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sanitize_strips_ansi_and_collapses_whitespace() {
        assert_eq!(sanitize_display_text("plain text"), "plain text");
        assert_eq!(
            sanitize_display_text("\u{1b}[31mred\u{1b}[0m text"),
            "red text"
        );
        // Leading control run never produces a leading space.
        assert_eq!(sanitize_display_text("  \n\t leading"), "leading");
        assert_eq!(sanitize_display_text("a\u{1b}]0;title\u{7}b"), "a b");
    }

    #[test]
    fn preview_truncates_with_dots() {
        assert_eq!(preview_display_text("short", 10), "short");
        // The truncation keeps the trailing space before "..." (upstream
        // does not re-trim).
        assert_eq!(preview_display_text("a long value here", 10), "a long ...");
        assert_eq!(preview_display_text("abcdef", 3), "abc");
        assert_eq!(truncate_display_text("abcdef", 0), "");
    }

    #[test]
    fn duration_and_path_formats() {
        assert_eq!(format_duration(250), "250ms");
        assert_eq!(format_duration(1500), "1.5s");
        assert_eq!(format_duration(59_900), "59.9s");
        assert_eq!(format_duration(60_000), "1m0s");
        assert_eq!(format_duration(185_000), "3m5s");
        assert_eq!(format_duration(3_700_000), "61m40s");
    }

    #[test]
    fn current_path_extraction() {
        let args = json!({ "path": " /tmp/x.rs ", "command": "ls" });
        assert_eq!(
            resolve_current_path(Some("read"), Some(&args)),
            Some("/tmp/x.rs".to_string())
        );
        // `(?:>|>>|tee\s+)(\S+)` bug-for-bug: a space after `>` fails the
        // whole position (verified against upstream regex semantics), the
        // single-char `>` alternative shadows `>>` and captures the second
        // `>` itself.
        let args = json!({ "command": "echo hi > /tmp/out.txt" });
        assert_eq!(resolve_current_path(Some("bash"), Some(&args)), None);
        let args = json!({ "command": "echo hi >/tmp/out.txt" });
        assert_eq!(
            resolve_current_path(Some("bash"), Some(&args)),
            Some("/tmp/out.txt".to_string())
        );
        let args = json!({ "command": "echo a >> log.txt" });
        assert_eq!(
            resolve_current_path(Some("bash"), Some(&args)),
            Some(">".to_string())
        );
        let args = json!({ "command": "tee log.txt" });
        assert_eq!(
            resolve_current_path(Some("bash"), Some(&args)),
            Some("log.txt".to_string())
        );
        assert_eq!(resolve_current_path(Some("bash"), Some(&json!({}))), None);
    }

    #[test]
    fn args_preview_branch_order() {
        assert_eq!(
            extract_tool_args_preview(
                &json!({ "tool": "echo", "server": "s", "args": "{\"a\":1}" })
            ),
            "s/echo {\"a\":1}"
        );
        assert_eq!(
            extract_tool_args_preview(&json!({ "queries": ["first", "second", "third"] })),
            "first (+2 more)"
        );
        assert_eq!(
            extract_tool_args_preview(&json!({ "query": "  find me  " })),
            "find me"
        );
        assert_eq!(
            extract_tool_args_preview(&json!({ "unknown_key": "value" })),
            "unknown_key=value"
        );
        assert_eq!(extract_tool_args_preview(&json!({})), "");
    }

    #[test]
    fn tool_call_summary_shapes() {
        assert_eq!(
            format_tool_call("bash", &json!({ "command": "cargo build" }), false),
            "$ cargo build"
        );
        assert!(
            format_tool_call("read", &json!({ "path": "/home/user/proj/a.rs" }), false)
                .starts_with("read ")
        );
        assert_eq!(
            format_tool_call("other", &json!({ "k": "v" }), false),
            "other {\"k\":\"v\"}"
        );
    }

    #[test]
    fn text_extraction_from_content() {
        let content = json!([
            { "type": "text", "text": "one" },
            { "type": "tool_result", "content": [{ "type": "text", "text": "inner" }] },
            { "text": "bare" },
            { "type": "image", "mimeType": "image/png" },
        ]);
        assert_eq!(extract_text_from_content(&content), "one\ninner\nbare");
        assert_eq!(extract_text_from_content(&json!("str")), "");
    }
}
