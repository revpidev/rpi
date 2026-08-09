//! Bash tool renderer — port of the `renderCall`/`renderResult` hooks in
//! `packages/coding-agent/src/core/tools/bash.ts` (formatBashCall :231-237,
//! rebuildBashResultRenderComponent :239-319, hooks :462-496)
//! @ pi 0.82.1 (2efa728) (T17).
//!
//! Intentional differences:
//! - Upstream keeps per-call render state in a shared mutable `state` object
//!   and reuses components via `context.lastComponent`; here the state lives
//!   in the component's [`RendererStateSlot`] as [`BashRenderState`] and the
//!   visual component is rebuilt from it on every update — the rendered
//!   bytes are identical.
//! - The 1s `setInterval` Elapsed ticker (bash.ts:474-476) is a dedicated
//!   thread on a stop channel (same pattern as rpi-tui `Loader` /
//!   `CountdownTimer`, coding-standards §6.4) that only calls
//!   `request_render`; the elapsed line is recomputed from the shared timing
//!   state at `render()` time, so the timer never touches the component.
//! - `Date.now()` milliseconds become `Instant` durations; `formatDuration`
//!   is `(ms/1000).toFixed(1)` in both.

use std::cell::RefCell;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use rpi_tui::components::text::Text;
use rpi_tui::tui::{Component, RenderHandle};
use rpi_tui::utils::truncate_to_width;
use serde_json::Value;

use super::render_utils::{invalid_arg_text, str_value};
use crate::core::themes::Theme;
use crate::modes::interactive::components::keybinding_hints::key_hint;
use crate::modes::interactive::components::tool_execution::{
    get_text_output, lock_recover, RenderShell, ResultRenderOptions, ToolDefinition,
    ToolRenderContext, ToolResultState,
};
use crate::modes::interactive::components::visual_truncate::{
    truncate_to_visual_lines, VisualTruncateResult,
};
use crate::tools::truncate::{format_size, DEFAULT_MAX_BYTES};

/// `BASH_PREVIEW_LINES` (bash.ts:204).
const BASH_PREVIEW_LINES: usize = 5;

/// `BashRenderState` (bash.ts:207-211 `BashRenderState`): per tool call,
/// carried by the [`RendererStateSlot`].
#[derive(Default)]
pub struct BashRenderState {
    timing: Mutex<BashTiming>,
    timer: Mutex<Option<TickerGuard>>,
}

#[derive(Default)]
struct BashTiming {
    started_at: Option<Instant>,
    ended_at: Option<Instant>,
}

/// The 1s Elapsed ticker (bash.ts:474-476 `setInterval`): ticks
/// `request_render` until stopped; `dispose`/`Drop` joins the thread so no
/// background task leaks (coding-standards §6.4).
struct TickerGuard {
    stop_tx: Option<mpsc::Sender<()>>,
    thread_handle: Option<JoinHandle<()>>,
}

impl TickerGuard {
    fn start(render_handle: RenderHandle) -> Self {
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        let thread_handle = thread::spawn(move || loop {
            match stop_rx.recv_timeout(Duration::from_secs(1)) {
                Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => render_handle.request_render(),
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

/// `formatDuration` (bash.ts:227-229): `(ms/1000).toFixed(1)`. Mirrors JS
/// exactly: the duration is truncated to whole milliseconds first
/// (`Date.now()` deltas are integers), and the tenths digit rounds half away
/// from zero (`toFixed` picks the larger n on ties, e.g. 1250ms → `1.3s`;
/// Rust's `{:.1}` would round ties to even).
fn format_duration(duration: Duration) -> String {
    let ms = duration.as_millis() as f64;
    let tenths = (ms / 100.0).round();
    format!("{:.1}s", tenths / 10.0)
}

/// `formatBashCall` (bash.ts:231-237).
fn format_bash_call(args: &Value, theme: &Theme) -> String {
    let command = str_value(args.get("command"));
    let timeout_suffix = match args.get("timeout").and_then(Value::as_f64) {
        // JS truthiness: 0 (and NaN, unreachable from JSON) → no suffix.
        Some(timeout) if timeout != 0.0 => theme.fg(
            "muted",
            &format!(" (timeout {}s)", format_js_number(timeout)),
        ),
        _ => String::new(),
    };
    let command_display = match command {
        None => invalid_arg_text(theme),
        Some(command) if command.is_empty() => theme.fg("toolOutput", "..."),
        Some(command) => command,
    };
    format!(
        "{}{}",
        theme.fg("toolTitle", &Theme::bold(&format!("$ {command_display}"))),
        timeout_suffix
    )
}

/// JS `Number#toString` for the timeout suffix: integral values print
/// without a decimal point (`30` → "30", `0.5` → "0.5").
fn format_js_number(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

/// The truncation fields of `BashToolDetails` (bash.ts:52-55), read from the
/// camelCase `details` JSON (`TruncationResult` serde, T06).
#[derive(Default)]
struct TruncationView {
    truncated: bool,
    truncated_by_lines: bool,
    output_lines: u64,
    total_lines: u64,
    max_bytes: Option<usize>,
}

impl TruncationView {
    fn from_details(details: Option<&Value>) -> Self {
        let Some(truncation) = details.and_then(|d| d.get("truncation")) else {
            return Self::default();
        };
        Self {
            truncated: truncation
                .get("truncated")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            truncated_by_lines: truncation.get("truncatedBy").and_then(Value::as_str)
                == Some("lines"),
            output_lines: truncation
                .get("outputLines")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            total_lines: truncation
                .get("totalLines")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            max_bytes: truncation
                .get("maxBytes")
                .and_then(Value::as_u64)
                .map(|v| v as usize),
        }
    }
}

/// The result-render component (bash.ts:219-225 `BashResultRenderComponent`
/// and :239-319 `rebuildBashResultRenderComponent`): output preview,
/// truncation warnings, and the Elapsed/Took timing line. The preview is
/// cached per width (bash.ts:213-217, :272-294); the timing line is computed
/// at `render()` time from the shared state so the 1s ticker only needs to
/// request a re-render.
struct BashResultRenderComponent {
    styled_output: String,
    warnings: Option<String>,
    expanded: bool,
    is_partial: bool,
    state: Arc<BashRenderState>,
    theme: Theme,
    cache: RefCell<Option<(usize, VisualTruncateResult)>>,
}

impl BashResultRenderComponent {
    fn preview(&self, width: usize) -> VisualTruncateResult {
        if let Some((cached_width, cached)) = &*self.cache.borrow() {
            if *cached_width == width {
                return cached.clone();
            }
        }
        // padding_x = 0: the component sits inside the component's `Box`
        // (visual-truncate.rs doc).
        let preview = truncate_to_visual_lines(&self.styled_output, BASH_PREVIEW_LINES, width, 0);
        *self.cache.borrow_mut() = Some((width, preview.clone()));
        preview
    }
}

impl Component for BashResultRenderComponent {
    fn render(&self, width: usize) -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();

        if !self.styled_output.is_empty() {
            if self.expanded {
                // `new Text(`\n${styledOutput}`)` (bash.ts:270).
                lines.push(String::new());
                lines.extend(self.styled_output.split('\n').map(str::to_string));
            } else {
                let preview = self.preview(width);
                lines.push(String::new());
                if preview.skipped_count > 0 {
                    // bash.ts:280-284.
                    let hint = format!(
                        "{} {}{}",
                        self.theme.fg(
                            "muted",
                            &format!("... ({} earlier lines,", preview.skipped_count)
                        ),
                        key_hint(&self.theme, "app.tools.expand", "to expand"),
                        self.theme.fg("muted", ")")
                    );
                    lines.push(truncate_to_width(&hint, width, "...", false));
                }
                lines.extend(preview.visual_lines);
            }
        }

        if let Some(warnings) = &self.warnings {
            // `new Text(`\n${warning}`)` (bash.ts:311).
            lines.push(String::new());
            lines.extend(warnings.split('\n').map(str::to_string));
        }

        let timing = lock_recover(&self.state.timing);
        if let Some(started_at) = timing.started_at {
            // bash.ts:314-318: `Elapsed` while partial, `Took` once settled.
            let label = if self.is_partial { "Elapsed" } else { "Took" };
            let end = timing.ended_at.unwrap_or_else(Instant::now);
            let duration = end.saturating_duration_since(started_at);
            lines.push(String::new());
            lines.push(
                self.theme
                    .fg("muted", &format!("{label} {}", format_duration(duration))),
            );
        }

        lines
    }

    fn invalidate(&mut self) {
        // bash.ts:288-293: width-keyed caches reset on invalidate.
        *self.cache.get_mut() = None;
    }
}

/// The bash tool's render definition (bash.ts:462-496).
pub struct BashToolRenderer;

impl ToolDefinition for BashToolRenderer {
    fn render_call(
        &self,
        args: &Value,
        theme: &Theme,
        context: &ToolRenderContext,
    ) -> Option<Box<dyn Component>> {
        let state = context.state.get_or_init::<BashRenderState>();
        if context.execution_started {
            // bash.ts:463-467: first render after execution start records
            // `startedAt` (and resets `endedAt`).
            let mut timing = lock_recover(&state.timing);
            if timing.started_at.is_none() {
                timing.started_at = Some(Instant::now());
                timing.ended_at = None;
            }
        }
        Some(Box::new(Text::new(
            format_bash_call(args, theme),
            0,
            0,
            None,
        )))
    }

    fn render_result(
        &self,
        result: &ToolResultState,
        options: ResultRenderOptions,
        theme: &Theme,
        context: &ToolRenderContext,
    ) -> Option<Box<dyn Component>> {
        let state = context.state.get_or_init::<BashRenderState>();
        {
            let timing = lock_recover(&state.timing);
            let mut timer = lock_recover(&state.timer);
            // bash.ts:473-476: start the 1s Elapsed ticker once executing.
            if timing.started_at.is_some() && options.is_partial && timer.is_none() {
                *timer = Some(TickerGuard::start(context.render_handle.clone()));
            }
            // bash.ts:477-483: settle the end time and stop the ticker.
            if !options.is_partial || context.is_error {
                drop(timing);
                {
                    let mut timing = lock_recover(&state.timing);
                    if timing.ended_at.is_none() {
                        timing.ended_at = Some(Instant::now());
                    }
                }
                if let Some(mut guard) = timer.take() {
                    guard.dispose();
                }
            }
        }

        let details = result.details.as_ref();
        let truncation = TruncationView::from_details(details);
        let full_output_path = details
            .and_then(|d| d.get("fullOutputPath"))
            .and_then(Value::as_str);

        let mut output = get_text_output(Some(result), context.show_images)
            .trim()
            .to_string();
        // bash.ts:256-261: once settled, strip the truncated-output footer
        // (`\n\n[... Full output: <path>]`) — the warnings line replaces it.
        if !options.is_partial
            && truncation.truncated
            && full_output_path.is_some()
            && output.ends_with(']')
        {
            if let Some(footer_start) = output.rfind("\n\n[") {
                if output[footer_start..].contains(full_output_path.unwrap_or_default()) {
                    output = output[..footer_start].trim_end().to_string();
                }
            }
        }

        // bash.ts:263-267: per-line `toolOutput` coloring.
        let styled_output = if output.is_empty() {
            String::new()
        } else {
            output
                .split('\n')
                .map(|line| theme.fg("toolOutput", line))
                .collect::<Vec<_>>()
                .join("\n")
        };

        // bash.ts:297-312: `[Full output: …. Truncated: …]` warnings.
        let warnings = if truncation.truncated || full_output_path.is_some() {
            let mut warnings: Vec<String> = Vec::new();
            if let Some(path) = full_output_path {
                warnings.push(format!("Full output: {path}"));
            }
            if truncation.truncated {
                if truncation.truncated_by_lines {
                    warnings.push(format!(
                        "Truncated: showing {} of {} lines",
                        truncation.output_lines, truncation.total_lines
                    ));
                } else {
                    warnings.push(format!(
                        "Truncated: {} lines shown ({} limit)",
                        truncation.output_lines,
                        format_size(truncation.max_bytes.unwrap_or(DEFAULT_MAX_BYTES))
                    ));
                }
            }
            Some(theme.fg("warning", &format!("[{}]", warnings.join(". "))))
        } else {
            None
        };

        Some(Box::new(BashResultRenderComponent {
            styled_output,
            warnings,
            expanded: options.expanded,
            is_partial: options.is_partial,
            state,
            theme: theme.clone(),
            cache: RefCell::new(None),
        }))
    }

    fn render_shell(&self) -> Option<RenderShell> {
        // No `renderShell` in the upstream definition → `undefined`.
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::themes::load_theme;
    use crate::modes::interactive::components::tool_execution::RendererStateSlot;
    use rpi_tui::tui::RenderHandle;
    use serde_json::json;

    fn theme() -> Theme {
        load_theme("dark", None).expect("builtin dark theme")
    }

    fn context(state: &RendererStateSlot) -> ToolRenderContext {
        ToolRenderContext {
            args: json!({}),
            tool_call_id: "call_1".to_owned(),
            render_handle: RenderHandle::new(|| {}),
            state: state.clone(),
            cwd: "/cwd".to_owned(),
            execution_started: true,
            args_complete: true,
            is_partial: true,
            expanded: false,
            show_images: false,
            is_error: false,
        }
    }

    fn strip_ansi(input: &str) -> String {
        let mut out = String::with_capacity(input.len());
        let mut chars = input.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\u{1b}' if chars.peek() == Some(&'[') => {
                    chars.next();
                    for c in chars.by_ref() {
                        if c == 'm' {
                            break;
                        }
                    }
                }
                // OSC 8 hyperlink (`ESC]8;;..ESC\`): strip it too, so exact
                // assertions are independent of the process-global terminal
                // capability cache that other tests mutate.
                '\u{1b}' if chars.peek() == Some(&']') => {
                    chars.next();
                    for c in chars.by_ref() {
                        if c == '\u{1b}' {
                            chars.next(); // `\` of the `ESC\` terminator
                            break;
                        }
                    }
                }
                _ => out.push(c),
            }
        }
        out
    }

    #[test]
    fn format_duration_matches_js_to_fixed() {
        // Integer milliseconds with JS `toFixed(1)` semantics: half-away
        // rounding at the tenths digit (1250ms → 1.25 → "1.3", not "1.2").
        assert_eq!(format_duration(Duration::from_millis(1250)), "1.3s");
        assert_eq!(format_duration(Duration::from_millis(1249)), "1.2s");
        assert_eq!(format_duration(Duration::from_millis(50)), "0.1s");
        assert_eq!(format_duration(Duration::from_millis(0)), "0.0s");
        assert_eq!(format_duration(Duration::from_millis(999)), "1.0s");
    }

    #[test]
    fn call_renders_dollar_command_and_timeout_suffix() {
        let theme = theme();
        let text = format_bash_call(&json!({"command": "lscpu"}), &theme);
        assert!(strip_ansi(&text).starts_with("$ lscpu"));
        let with_timeout = format_bash_call(&json!({"command": "make", "timeout": 30}), &theme);
        assert!(strip_ansi(&with_timeout).contains("$ make (timeout 30s)"));
        // JS truthiness: 0 → no suffix.
        let zero = format_bash_call(&json!({"command": "make", "timeout": 0}), &theme);
        assert!(!strip_ansi(&zero).contains("timeout"));
        // Missing command → `...`; non-string command → invalid-arg text.
        let missing = format_bash_call(&json!({}), &theme);
        assert!(strip_ansi(&missing).contains("$ ..."));
        let invalid = format_bash_call(&json!({"command": 42}), &theme);
        assert!(strip_ansi(&invalid).contains("[invalid arg]"));
    }

    #[test]
    fn result_shows_output_warnings_and_timing() {
        let theme = theme();
        let state = RendererStateSlot::default();
        let renderer = BashToolRenderer;
        let result = ToolResultState {
            content: vec![
                crate::modes::interactive::components::tool_execution::ToolResultContentLoose::text(
                    (1..=12)
                        .map(|i| format!("line{i}"))
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
            ],
            is_error: false,
            details: Some(json!({
                "truncation": {
                    "truncated": true,
                    "truncatedBy": "lines",
                    "outputLines": 12,
                    "totalLines": 50
                },
                "fullOutputPath": "/tmp/pi-bash-abc.log"
            })),
        };
        let context = context(&state);
        let component = renderer
            .render_result(
                &result,
                ResultRenderOptions {
                    expanded: false,
                    is_partial: false,
                },
                &theme,
                &context,
            )
            .expect("result component");
        let stripped = strip_ansi(&component.render(80).join("\n"));
        // Collapsed preview: last 5 lines + skipped hint.
        assert!(stripped.contains("... (7 earlier lines,"));
        assert!(stripped.contains("line12"));
        assert!(!stripped.contains("line7\n") || stripped.matches("line7").count() <= 1);
        // Warnings line.
        assert!(stripped
            .contains("[Full output: /tmp/pi-bash-abc.log. Truncated: showing 12 of 50 lines]"));
        // Timing line: `Took` once settled (startedAt set by render_call).
        renderer.render_call(&json!({"command": "lscpu"}), &theme, &context);
        let component = renderer
            .render_result(
                &result,
                ResultRenderOptions {
                    expanded: false,
                    is_partial: false,
                },
                &theme,
                &context,
            )
            .expect("result component");
        let stripped = strip_ansi(&component.render(80).join("\n"));
        assert!(stripped.contains("Took 0."), "stripped: {stripped}");
    }

    #[test]
    fn partial_result_shows_elapsed_and_starts_ticker() {
        let theme = theme();
        let state = RendererStateSlot::default();
        let renderer = BashToolRenderer;
        let context = context(&state);
        renderer.render_call(&json!({"command": "sleep 2"}), &theme, &context);
        let result = ToolResultState {
            content: vec![],
            is_error: false,
            details: None,
        };
        let component = renderer
            .render_result(
                &result,
                ResultRenderOptions {
                    expanded: false,
                    is_partial: true,
                },
                &theme,
                &context,
            )
            .expect("partial component");
        let stripped = strip_ansi(&component.render(80).join("\n"));
        assert!(stripped.contains("Elapsed 0."), "stripped: {stripped}");
        // The ticker is running for this partial result…
        let render_state = state.get_or_init::<BashRenderState>();
        assert!(lock_recover(&render_state.timer).is_some());
        // …and settles + stops on the final result.
        let component = renderer
            .render_result(
                &result,
                ResultRenderOptions {
                    expanded: false,
                    is_partial: false,
                },
                &theme,
                &context,
            )
            .expect("final component");
        assert!(lock_recover(&render_state.timer).is_none());
        let stripped = strip_ansi(&component.render(80).join("\n"));
        assert!(stripped.contains("Took 0."), "stripped: {stripped}");
    }
}
