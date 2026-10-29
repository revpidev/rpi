//! Shared render helpers for the built-in tool renderers — port of
//! `packages/coding-agent/src/core/tools/render-utils.ts` @ pi 0.82.1
//! (2efa728) (T17).
//!
//! `getTextOutput` and the sanitize/strip-ANSI pipeline live in
//! `components/tool_execution.rs`; this module carries the remaining helpers
//! the built-in renderers need.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use rpi_tui::terminal_image::{get_capabilities, hyperlink};
use rpi_tui::tui::RenderHandle;
use serde_json::Value;

use crate::core::themes::Theme;
use crate::modes::interactive::components::tool_execution::lock_recover;

/// `formatToolDuration` (render-utils.ts:117-128 @ 4a080252f): `4.2s` below a
/// minute, then `1m 30s` and `2h 3m 4s`. Under a minute keeps one decimal and
/// rounds half away from zero like JS `toFixed(1)` (1250ms → `1.3s`); the
/// duration is truncated to whole milliseconds first like `Date.now()`
/// deltas.
pub fn format_tool_duration(duration: Duration) -> String {
    let ms = duration.as_millis() as f64;
    let seconds = ms / 1000.0;
    if seconds < 60.0 {
        let tenths = (ms / 100.0).round();
        return format!("{:.1}s", tenths / 10.0);
    }

    let total_seconds = seconds.floor() as u64;
    let minutes = total_seconds / 60;
    let remainder = total_seconds % 60;
    if minutes < 60 {
        return format!("{minutes}m {remainder}s");
    }

    format!("{}h {}m {remainder}s", minutes / 60, minutes % 60)
}

#[derive(Default)]
struct ToolTiming {
    started_at: Option<Instant>,
    ended_at: Option<Instant>,
}

/// The 1s Elapsed ticker (`setInterval`, render-utils.ts:153-155): ticks
/// `request_render` until stopped; `dispose`/`Drop` joins the thread so no
/// background task leaks (coding-standards §6.4).
struct TickerGuard {
    stop_tx: Option<mpsc::Sender<()>>,
    thread_handle: Option<JoinHandle<()>>,
}

impl TickerGuard {
    fn start(render_handle: RenderHandle) -> Self {
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        let thread_handle = thread::spawn(move || {
            loop {
                match stop_rx.recv_timeout(Duration::from_secs(1)) {
                    Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
                    Err(RecvTimeoutError::Timeout) => render_handle.request_render(),
                }
            }
        });
        Self {
            stop_tx: Some(stop_tx),
            thread_handle: Some(thread_handle),
        }
    }

    fn dispose(&mut self) {
        if let Some(stop_tx) = self.stop_tx.take() {
            let _ = stop_tx.send(());
        }
        if let Some(thread_handle) = self.thread_handle.take() {
            let _ = thread_handle.join();
        }
    }
}

impl Drop for TickerGuard {
    fn drop(&mut self) {
        self.dispose();
    }
}

/// `ToolTimerState` (render-utils.ts:130-135): per-call wall-clock state,
/// carried by a renderer's [`crate::modes::interactive::components::tool_execution::RendererStateSlot`].
#[derive(Default)]
pub struct ToolTimerState {
    timing: Mutex<ToolTiming>,
    ticker: Mutex<Option<TickerGuard>>,
}

impl ToolTimerState {
    /// `startToolTimer` (render-utils.ts:137-144): start the row's clock once
    /// execution starts. Call from `renderCall`.
    pub fn start(&self, execution_started: bool) {
        if !execution_started {
            return;
        }
        let mut timing = lock_recover(&self.timing);
        if timing.started_at.is_none() {
            timing.started_at = Some(Instant::now());
            timing.ended_at = None;
        }
    }

    /// Start or stop the 1s ticker and settle the end time
    /// (render-utils.ts:156-162). Call from `renderResult`.
    pub fn tick(&self, render_handle: &RenderHandle, is_partial: bool, is_error: bool) {
        {
            let timing = lock_recover(&self.timing);
            let mut ticker = lock_recover(&self.ticker);
            if timing.started_at.is_some() && is_partial && ticker.is_none() {
                *ticker = Some(TickerGuard::start(render_handle.clone()));
            }
        }
        if !is_partial || is_error {
            {
                let mut timing = lock_recover(&self.timing);
                if timing.ended_at.is_none() {
                    timing.ended_at = Some(Instant::now());
                }
            }
            if let Some(mut guard) = lock_recover(&self.ticker).take() {
                guard.dispose();
            }
        }
    }

    /// Whether the 1s ticker thread is running (renderer tests).
    #[cfg(test)]
    pub fn ticker_running(&self) -> bool {
        lock_recover(&self.ticker).is_some()
    }

    /// `toolTimerText` (render-utils.ts:146-170): `Elapsed 4.2s` while the
    /// result is partial and `Took 4.2s` once it is final. A final result's
    /// recorded duration wins; the renderer's own clock is the fallback for
    /// live progress and for results stored without one.
    pub fn text(&self, is_partial: bool, duration_ms: Option<u64>) -> Option<String> {
        if !is_partial && let Some(duration_ms) = duration_ms {
            return Some(format!(
                "Took {}",
                format_tool_duration(Duration::from_millis(duration_ms))
            ));
        }
        let timing = lock_recover(&self.timing);
        let started_at = timing.started_at?;
        let end = timing.ended_at.unwrap_or_else(Instant::now);
        let label = if is_partial { "Elapsed" } else { "Took" };
        Some(format!(
            "{label} {}",
            format_tool_duration(end.saturating_duration_since(started_at))
        ))
    }
}

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

/// `COLLAPSED_ARGS_CHARS` (render-utils.ts:71).
const COLLAPSED_ARGS_CHARS: usize = 100;

/// `formatToolCallWithArgs` (render-utils.ts:74-96 @ 5257d0d5f): the title
/// followed by the arguments. Collapsed, they are `key=value` pairs on the
/// title line, cut to [`COLLAPSED_ARGS_CHARS`]; expanded, each is a
/// `key: value` line below the title, with strings shown raw and
/// continuation lines indented. Non-object arguments render as `args=…`.
pub fn format_tool_call_with_args(
    title: &str,
    args: &Value,
    theme: &Theme,
    expanded: bool,
) -> String {
    let header = theme.fg("toolTitle", &Theme::bold(title));
    if args.is_null() {
        return header;
    }
    let entries: Vec<(String, &Value)> = match args {
        Value::Object(object) => object
            .iter()
            .map(|(key, value)| (key.clone(), value))
            .collect(),
        other => vec![("args".to_string(), other)],
    };
    if entries.is_empty() {
        return header;
    }
    if expanded {
        let lines: Vec<String> = entries
            .iter()
            .map(|(key, value)| {
                let text = match value {
                    Value::String(text) => text.clone(),
                    other => {
                        serde_json::to_string_pretty(other).unwrap_or_else(|_| js_value_text(other))
                    }
                };
                // `replaceTabs(text).replace(/\r/g, "").split("\n").join("\n    ")`.
                let text = normalize_display_text(&replace_tabs(&text))
                    .split('\n')
                    .collect::<Vec<_>>()
                    .join("\n    ");
                format!("  {key}: {text}")
            })
            .collect();
        return format!("{header}\n{}", theme.fg("muted", &lines.join("\n")));
    }
    // `JSON.stringify(value) ?? String(value)` (render-utils.ts:93).
    let pairs = entries
        .iter()
        .map(|(key, value)| {
            let rendered = serde_json::to_string(value).unwrap_or_else(|_| js_value_text(value));
            format!("{key}={rendered}")
        })
        .collect::<Vec<_>>()
        .join(" ");
    let preview = if pairs.chars().count() > COLLAPSED_ARGS_CHARS {
        let truncated: String = pairs.chars().take(COLLAPSED_ARGS_CHARS - 3).collect();
        format!("{truncated}...")
    } else {
        pairs
    };
    format!("{header} {}", theme.fg("muted", &preview))
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

/// `parseLineNumber` (render-utils.ts:31-34): finite numbers and non-empty
/// numeric strings; everything else (null, bool, malformed text, Infinity) is
/// not a line number.
fn parse_line_number(value: &Value) -> Option<f64> {
    match value {
        Value::String(text) if !text.trim().is_empty() => text
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|value| value.is_finite()),
        Value::Number(number) => number.as_f64().filter(|value| value.is_finite()),
        _ => None,
    }
}

/// `formatReadLineRange` (render-utils.ts:37-49): the `:start-end` suffix for
/// a read call. Omitted optional arguments may arrive as `null` (strict
/// schemas); numeric strings are accepted. A malformed offset or limit omits
/// the whole range instead of rendering `NaN` (#9887).
pub fn format_read_line_range(offset: Option<&Value>, limit: Option<&Value>) -> String {
    let offset = offset.filter(|value| !value.is_null());
    let limit = limit.filter(|value| !value.is_null());
    if offset.is_none() && limit.is_none() {
        return String::new();
    }
    let start = match offset {
        None => 1.0,
        Some(value) => match parse_line_number(value) {
            Some(start) => start,
            None => return String::new(),
        },
    };
    let Some(limit) = limit else {
        return format!(":{}", format_js_number(start));
    };
    let Some(count) = parse_line_number(limit) else {
        return String::new();
    };
    let end = start + count - 1.0;
    let range = format!(":{}", format_js_number(start));
    if end != 0.0 {
        format!("{range}-{}", format_js_number(end))
    } else {
        range
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

#[cfg(test)]
mod tests {
    //! `formatToolCallWithArgs` (render-utils.ts:71-96 @ 5257d0d5f, V16-13
    //! FR-F R1): collapsed `key=value` truncation and expanded per-line
    //! rendering.

    use super::*;
    use crate::core::themes::load_theme;
    use serde_json::json;

    fn theme() -> Theme {
        load_theme("dark", None).expect("builtin dark theme")
    }

    /// Strip CSI SGR sequences so assertions are independent of colors.
    fn plain(input: &str) -> String {
        let mut out = String::with_capacity(input.len());
        let mut chars = input.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\u{1b}' && chars.peek() == Some(&'[') {
                chars.next();
                for c in chars.by_ref() {
                    if c == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    #[test]
    fn collapsed_renders_key_value_pairs_on_the_title_line() {
        let text = format_tool_call_with_args(
            "custom-tool",
            &json!({"path": "src/main.rs", "count": 2}),
            &theme(),
            false,
        );
        assert_eq!(plain(&text), "custom-tool path=\"src/main.rs\" count=2");
    }

    #[test]
    fn collapsed_truncates_pairs_at_100_chars() {
        let long = "x".repeat(200);
        let text =
            format_tool_call_with_args("custom-tool", &json!({"path": long}), &theme(), false);
        let plain = plain(&text);
        let pairs = plain.strip_prefix("custom-tool ").expect("header");
        assert_eq!(pairs.chars().count(), 100);
        assert!(pairs.ends_with("..."), "{pairs}");
    }

    #[test]
    fn expanded_renders_one_line_per_argument() {
        let text = format_tool_call_with_args(
            "custom-tool",
            &json!({"path": "src/main.rs", "count": 2, "options": {"a": true}}),
            &theme(),
            true,
        );
        assert_eq!(
            plain(&text),
            "custom-tool\n  path: src/main.rs\n  count: 2\n  options: {\n      \"a\": true\n    }"
        );
    }

    #[test]
    fn expanded_replaces_tabs_strips_cr_and_indents_continuations() {
        let text = format_tool_call_with_args(
            "custom-tool",
            &json!({"text": "a\tb\r\nc"}),
            &theme(),
            true,
        );
        assert_eq!(plain(&text), "custom-tool\n  text: a   b\n    c");
    }

    #[test]
    fn non_object_args_render_under_args() {
        let collapsed = format_tool_call_with_args("custom-tool", &json!([1, 2]), &theme(), false);
        assert_eq!(plain(&collapsed), "custom-tool args=[1,2]");
        let expanded = format_tool_call_with_args("custom-tool", &json!("raw"), &theme(), true);
        assert_eq!(plain(&expanded), "custom-tool\n  args: raw");
    }

    #[test]
    fn null_args_and_empty_objects_show_the_title_only() {
        assert_eq!(
            plain(&format_tool_call_with_args(
                "custom-tool",
                &Value::Null,
                &theme(),
                false
            )),
            "custom-tool"
        );
        assert_eq!(
            plain(&format_tool_call_with_args(
                "custom-tool",
                &json!({}),
                &theme(),
                true
            )),
            "custom-tool"
        );
    }
}
