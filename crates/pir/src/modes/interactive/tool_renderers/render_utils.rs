//! Shared render helpers for the built-in tool renderers — port of
//! `packages/coding-agent/src/core/tools/render-utils.ts` @ pi 0.82.1
//! (2efa728) (T17).
//!
//! `getTextOutput` and the sanitize/strip-ANSI pipeline live in
//! `components/tool_execution.rs`; this module carries the remaining helpers
//! the built-in renderers need.

use std::path::{Path, PathBuf};

use pir_tui::terminal_image::{get_capabilities, hyperlink};
use serde_json::Value;

use crate::core::themes::Theme;

/// `str` (render-utils.ts:25-29): a string arg → itself; `undefined`/`null`
/// (missing key / `Value::Null`) → `""`; anything else → `None` (invalid
/// arg).
pub fn str_value(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Null) | None => Some(String::new()),
        Some(_) => None,
    }
}

/// `replaceTabs` (render-utils.ts:31-33): tab → three spaces.
pub fn replace_tabs(text: &str) -> String {
    text.replace('\t', "   ")
}

/// `normalizeDisplayText` (render-utils.ts:35-37): strip carriage returns.
pub fn normalize_display_text(text: &str) -> String {
    text.replace('\r', "")
}

/// `invalidArgText` (render-utils.ts:71-73).
pub fn invalid_arg_text(theme: &Theme) -> String {
    theme.fg("error", "[invalid arg]")
}

/// JS template-literal coercion of a JSON value, used for the `limit`
/// suffixes (grep.ts:89, find.ts:85-86, ls.ts:62-63) where upstream tests
/// `limit !== undefined` and interpolates whatever value is present.
pub fn js_value_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Number(n) => format_js_number(n.as_f64().unwrap_or(0.0)),
        Value::Bool(b) => b.to_string(),
        Value::Null => "null".to_string(),
        // `Array#toString` recurses and joins with "," (`"" + []` → `""`).
        Value::Array(items) => items
            .iter()
            .map(js_value_text)
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".to_string(),
    }
}

/// JS `Number#toString` for interpolated numbers: integral values print
/// without a decimal point (same helper as the bash renderer's).
fn format_js_number(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

/// `shortenPath` (render-utils.ts:10-17): replace the home-dir prefix with
/// `~` (unix `HOME`, same convention as `tools/path_utils.rs`).
pub fn shorten_path(path: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    if !home.is_empty() && path.starts_with(&home) {
        return format!("~{}", &path[home.len()..]);
    }
    path.to_string()
}

/// `linkPath` (render-utils.ts:19-23): wrap the styled text in an OSC 8
/// hyperlink when the terminal advertises hyperlink support.
pub fn link_path(styled_text: &str, raw_path: &str, cwd: &str) -> String {
    if !get_capabilities().hyperlinks {
        return styled_text.to_string();
    }
    let absolute = crate::tools::path_utils::resolve_to_cwd(raw_path, Path::new(cwd));
    hyperlink(styled_text, &path_to_file_url(&absolute))
}

/// `pathToFileURL` (node:url): minimal percent-encoder — the inverse of the
/// hand-rolled decoder in `tools/path_utils.rs`. Keeps the URL-path
/// unreserved/sub-delim set plus `:@/`; everything else is UTF-8
/// percent-encoded (uppercase hex).
fn path_to_file_url(path: &Path) -> String {
    const KEEP: &[u8] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~!$&'()*+,;=:@/";
    let mut url = String::from("file://");
    for &byte in path.as_os_str().to_string_lossy().as_bytes() {
        if KEEP.contains(&byte) {
            url.push(byte as char);
        } else {
            url.push_str(&format!("%{byte:02X}"));
        }
    }
    url
}

/// `renderToolPath` (render-utils.ts:75-85): `None` (non-string arg) →
/// invalid-arg text; empty string with no fallback → muted `...`; otherwise
/// accent-colored shortened path, hyperlinked when supported.
pub fn render_tool_path(
    raw_path: Option<String>,
    theme: &Theme,
    cwd: &str,
    empty_fallback: Option<&str>,
) -> String {
    let Some(raw_path) = raw_path else {
        return invalid_arg_text(theme);
    };
    let value = if raw_path.is_empty() {
        match empty_fallback {
            Some(fallback) if !fallback.is_empty() => fallback.to_string(),
            _ => return theme.fg("toolOutput", "..."),
        }
    } else {
        raw_path
    };
    link_path(&theme.fg("accent", &shorten_path(&value)), &value, cwd)
}

/// Resolve `path` against `cwd` (thin re-export to keep renderer call sites
/// free of `tools` internals).
pub fn resolve_to_cwd(path: &str, cwd: &str) -> PathBuf {
    crate::tools::path_utils::resolve_to_cwd(path, Path::new(cwd))
}
