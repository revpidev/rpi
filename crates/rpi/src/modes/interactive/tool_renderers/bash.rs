//! Bash tool renderer — port of the `renderCall`/`renderResult` hooks in
//! `packages/coding-agent/src/core/tools/bash.ts` (formatBashCall :231-237,
//! rebuildBashResultRenderComponent :239-319, hooks :462-496)
//! @ pi 0.82.1 (2efa728) (T17); the v1.1.0 pin moved these hooks to
//! `packages/coding-agent/src/core/tools/renderers/bash.ts` (the V17-05
//! `Took`/`durationMs` references use that path).
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
//!   matches the upstream renderer (toFixed(1) under a minute, minutes /
//!   seconds / hours above, `fe219d7f8`).

use std::cell::RefCell;
use std::sync::{Arc, Mutex};

use rpi_tui::components::text::Text;
use rpi_tui::tui::Component;
use rpi_tui::utils::{truncate_to_width, visible_width, wrap_text_with_ansi};
use serde_json::Value;

use super::render_utils::{ToolTimerState, invalid_arg_text, str_value};
use crate::core::themes::Theme;
use crate::modes::interactive::components::keybinding_hints::key_hint;
use crate::modes::interactive::components::tool_execution::{
    RenderShell, ResultRenderOptions, ToolDefinition, ToolRenderContext, ToolResultState,
    get_text_output, lock_recover,
};
use crate::modes::interactive::components::visual_truncate::truncate_to_visual_lines;
use crate::tools::truncate::{DEFAULT_MAX_BYTES, format_size};

/// `BASH_PREVIEW_LINES` (bash.ts:204).
const BASH_PREVIEW_LINES: usize = 5;

/// `BashRenderState` (bash.ts:207-211 `BashRenderState`): per tool call,
/// carried by the [`RendererStateSlot`]. The timing/ticker logic lives in
/// the shared [`ToolTimerState`] (render-utils.ts:130-170).
#[derive(Default)]
pub struct BashRenderState {
    timer: ToolTimerState,
    /// rpi#78 A4/0.3: incremental styling for the 100 ms partial snapshots.
    output: Mutex<BashOutputCache>,
}

/// Per-call output styling + visual-line cache.
///
/// `bash` emits a full accumulated-output snapshot every 100 ms
/// (`tools/bash.rs` `BASH_UPDATE_THROTTLE_MS`), and every update used to
/// re-style the whole text line by line and re-wrap it to compute the collapsed
/// preview. Both are incremental here: only the grown tail is re-styled, and
/// the preview's tail lines are wrapped instead of the whole output.
#[derive(Default)]
struct BashOutputCache {
    /// The raw (trimmed, footer-stripped) output the cache covers.
    raw: String,
    /// The fully styled text (`fg("toolOutput", line)` joined by `\n`).
    styled: Arc<String>,
    /// Visual-line bookkeeping for the collapsed preview; `None` disables the
    /// incremental preview path (a CR in the output, or no counts yet).
    visual: Option<VisualLines>,
}

#[derive(Default, Clone)]
struct VisualLines {
    width: usize,
    /// Visual line count per logical line of `styled`.
    counts: Vec<usize>,
    total: usize,
}

/// One logical line as the render path sees it (`Text::render` replaces tabs)
/// wrapped and right-padded at `width` to exactly what `Text::new(text, 0, 0,
/// None).render(width)` produces per line (rpi#78 A4/0.3).
fn wrap_output_line(line: &str, width: usize) -> Vec<String> {
    let content_width = width.max(1);
    wrap_text_with_ansi(&line.replace('\t', "   "), content_width)
        .into_iter()
        .map(|visual| {
            let padding = content_width.saturating_sub(visible_width(&visual));
            format!("{visual}{}", " ".repeat(padding))
        })
        .collect()
}

impl BashOutputCache {
    /// Style `raw` (incrementally when it grew by appending) and return the
    /// shared styled text. Byte-identical to
    /// `raw.split('\n').map(|l| theme.fg("toolOutput", l)).join("\n")`.
    fn styled(&mut self, raw: &str, theme: &Theme) -> Arc<String> {
        if !raw.starts_with(&self.raw) {
            self.raw.clear();
            self.styled = Arc::new(String::new());
            self.visual = None;
        }
        if raw.len() > self.raw.len() {
            let raw_tail_start = self.raw.rfind('\n').map_or(0, |index| index + 1);
            let styled_tail_start = self.styled.rfind('\n').map_or(0, |index| index + 1);
            let styled = Arc::make_mut(&mut self.styled);
            styled.truncate(styled_tail_start);
            for (index, line) in raw[raw_tail_start..].split('\n').enumerate() {
                if index > 0 {
                    styled.push('\n');
                }
                styled.push_str(&theme.fg("toolOutput", line));
            }
            self.raw.clear();
            self.raw.push_str(raw);
            if let Some(visual) = self.visual.as_mut() {
                // The line starting at `styled_tail_start` grew; everything
                // before it is unchanged.
                let keep = visual.counts.len().saturating_sub(1);
                let styled = Arc::clone(&self.styled);
                visual.resync_from(&styled, styled_tail_start, keep);
            }
        }
        Arc::clone(&self.styled)
    }

    /// The collapsed preview: the last `max_visual_lines` visual lines plus the
    /// number of hidden ones. `None` = use the whole-text path.
    fn preview(&mut self, width: usize, max_visual_lines: usize) -> Option<(Vec<String>, usize)> {
        // CR normalization happens on the whole text in `wrap_text_with_ansi`;
        // a per-line fast path cannot reproduce it, so fall back.
        if self.styled.contains('\r') {
            self.visual = None;
            return None;
        }
        if self
            .visual
            .as_ref()
            .is_none_or(|visual| visual.width != width)
        {
            self.visual = Some(VisualLines::build(&self.styled, width));
        }
        let visual = self.visual.as_ref()?;
        let skipped = visual.total.saturating_sub(max_visual_lines);
        if skipped == 0 {
            return None;
        }
        // Walk the logical lines backwards until enough visual lines are
        // covered, then wrap only those (the preview keeps the tail).
        let mut needed = max_visual_lines;
        let mut start = visual.counts.len();
        while start > 0 && needed > 0 {
            start -= 1;
            needed = needed.saturating_sub(visual.counts[start]);
        }
        let mut lines: Vec<String> = Vec::new();
        for line in self.styled.split('\n').skip(start) {
            lines.extend(wrap_output_line(line, width));
        }
        let keep_from = lines.len().saturating_sub(max_visual_lines);
        lines.drain(..keep_from);
        Some((lines, skipped))
    }
}

impl VisualLines {
    fn build(styled: &str, width: usize) -> Self {
        let mut visual = VisualLines {
            width,
            counts: Vec::new(),
            total: 0,
        };
        visual.resync_from(styled, 0, 0);
        visual
    }

    /// Keep the counts for the first `keep` logical lines and recount
    /// everything from byte `from_byte` on (the start of logical line `keep`).
    fn resync_from(&mut self, styled: &str, from_byte: usize, keep: usize) {
        self.counts.truncate(keep);
        self.total = self.counts.iter().sum();
        for line in styled[from_byte..].split('\n') {
            let count = wrap_output_line(line, self.width).len();
            self.counts.push(count);
            self.total += count;
        }
    }
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
/// truncation warnings, and the Elapsed/Took timing line. The complete
/// collapsed output — including the expand hint — is cached per width
/// (bash.ts:213-217, :272-294 @ b485fa312): the renderer runs on every
/// frame for every bash result in the transcript. The timing line is
/// computed at `render()` time from the shared state so the 1s ticker only
/// needs to request a re-render.
struct BashResultRenderComponent {
    styled_output: Arc<String>,
    /// Precomputed collapsed preview at a known width: `(width, visual lines,
    /// skipped count)` (rpi#78 A4/0.3). `None`/width mismatch falls back to the
    /// whole-text wrap.
    preview: Option<(usize, Vec<String>, usize)>,
    warnings: Option<String>,
    expanded: bool,
    is_partial: bool,
    /// `durationMs` (bash.ts:104-105 @ 18336987a, #10557): a final result's
    /// recorded duration wins over the renderer's own clock — it is
    /// monotonic and survives session reloads.
    duration_ms: Option<u64>,
    state: Arc<BashRenderState>,
    theme: Theme,
    cache: RefCell<Option<(usize, Vec<String>)>>,
}

impl BashResultRenderComponent {
    /// The collapsed preview lines (leading spacer, optional hint, kept
    /// visual lines), cached per width.
    fn preview(&self, width: usize) -> Vec<String> {
        if let Some((cached_width, cached)) = &*self.cache.borrow()
            && *cached_width == width
        {
            return cached.clone();
        }
        // padding_x = 0: the component sits inside the component's `Box`
        // (visual-truncate.rs doc).
        let (visual_lines, skipped_count) = match &self.preview {
            Some((preview_width, lines, skipped)) if *preview_width == width => {
                (lines.clone(), *skipped)
            }
            _ => {
                let preview =
                    truncate_to_visual_lines(&self.styled_output, BASH_PREVIEW_LINES, width, 0);
                (preview.visual_lines, preview.skipped_count)
            }
        };
        let mut lines: Vec<String> = vec![String::new()];
        if skipped_count > 0 {
            // bash.ts:280-284.
            let hint = format!(
                "{} {}{}",
                self.theme
                    .fg("muted", &format!("... ({skipped_count} earlier lines,")),
                key_hint(&self.theme, "app.tools.expand", "to expand"),
                self.theme.fg("muted", ")")
            );
            lines.push(truncate_to_width(&hint, width, "...", false));
        }
        lines.extend(visual_lines);
        *self.cache.borrow_mut() = Some((width, lines.clone()));
        lines
    }
}

impl Component for BashResultRenderComponent {
    fn render(&self, width: usize) -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();

        if !self.styled_output.is_empty() {
            if self.expanded {
                // `new Text(`\n${styledOutput}`)` (bash.ts:270): the upstream
                // `Text` component wraps at width, so long tool-output lines
                // wrap instead of overflowing the terminal.
                lines.push(String::new());
                lines.extend(wrap_text_with_ansi(&self.styled_output, width));
            } else {
                lines.extend(self.preview(width));
            }
        }

        if let Some(warnings) = &self.warnings {
            // `new Text(`\n${warning}`)` (bash.ts:311): wrap at width — a
            // long full-output path would otherwise exceed the terminal
            // width and trip the renderer's width guard (tui.rs).
            lines.push(String::new());
            for warning in warnings.split('\n') {
                lines.extend(wrap_text_with_ansi(warning, width));
            }
        }

        if let Some(timer_text) = self.state.timer.text(self.is_partial, self.duration_ms) {
            lines.push(String::new());
            lines.push(self.theme.fg("muted", &timer_text));
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
        // bash.ts:463-467: first render after execution start records
        // `startedAt` (and resets `endedAt`).
        state.timer.start(context.execution_started);
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
        // bash.ts:473-483: start the 1s Elapsed ticker once executing and
        // settle the end time/stop it once the result is final.
        state
            .timer
            .tick(&context.render_handle, options.is_partial, context.is_error);

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
            && let Some(footer_start) = output.rfind("\n\n[")
            && output[footer_start..].contains(full_output_path.unwrap_or_default())
        {
            output = output[..footer_start].trim_end().to_string();
        }

        // bash.ts:263-267: per-line `toolOutput` coloring — incremental
        // (rpi#78 A4/0.3): a 100 ms partial snapshot restyles only the grown
        // tail instead of the whole accumulated output.
        let styled_output = if output.is_empty() {
            Arc::new(String::new())
        } else {
            let mut cache = lock_recover(&state.output);
            cache.styled(&output, theme)
        };
        // The collapsed preview only needs the tail; compute it here (where the
        // per-call cache is reachable) rather than wrapping the whole output on
        // every update.
        let preview = {
            let width = context.terminal_width;
            if width == 0 {
                None
            } else {
                let mut cache = lock_recover(&state.output);
                cache
                    .preview(width, BASH_PREVIEW_LINES)
                    .map(|(lines, skipped)| (width, lines, skipped))
            }
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
            preview,
            warnings,
            expanded: options.expanded,
            is_partial: options.is_partial,
            duration_ms: context.duration_ms,
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
    use crate::modes::interactive::tool_renderers::render_utils::format_tool_duration;
    use rpi_tui::tui::RenderHandle;
    use serde_json::json;
    use std::time::Duration;

    fn theme() -> Theme {
        load_theme("dark", None).expect("builtin dark theme")
    }

    fn context(state: &RendererStateSlot) -> ToolRenderContext {
        ToolRenderContext {
            args: json!({}),
            tool_call_id: "call_1".to_owned(),
            render_handle: RenderHandle::new(|| {}),
            invalidate: RenderHandle::new(|| {}),
            state: state.clone(),
            cwd: "/cwd".to_owned(),
            execution_started: true,
            args_complete: true,
            is_partial: true,
            expanded: false,
            show_images: false,
            is_error: false,
            duration_ms: None,
            output_pad: 0,
            terminal_width: 0,
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
    fn format_tool_duration_matches_js_to_fixed() {
        // Integer milliseconds with JS `toFixed(1)` semantics: half-away
        // rounding at the tenths digit (1250ms → 1.25 → "1.3", not "1.2").
        assert_eq!(format_tool_duration(Duration::from_millis(1250)), "1.3s");
        assert_eq!(format_tool_duration(Duration::from_millis(1249)), "1.2s");
        assert_eq!(format_tool_duration(Duration::from_millis(50)), "0.1s");
        assert_eq!(format_tool_duration(Duration::from_millis(0)), "0.0s");
        assert_eq!(format_tool_duration(Duration::from_millis(999)), "1.0s");
    }

    /// #9628 (`fe219d7f8`, tool-execution-component.test.ts `test.each`):
    /// keep short durations precise and make long shell durations readable —
    /// minutes+seconds at ≥1min (note 59_999ms still renders the <60s
    /// branch, where toFixed(1) lands on "60.0s"), hours past 59m.
    #[test]
    fn format_tool_duration_supports_hours_minutes_seconds() {
        let cases: &[(u128, &str)] = &[
            (0, "0.0s"),
            (4_200, "4.2s"),
            (59_900, "59.9s"),
            (59_999, "60.0s"),
            (60_000, "1m 0s"),
            (90_900, "1m 30s"),
            (1_592_200, "26m 32s"),
            (3_599_999, "59m 59s"),
            (3_600_000, "1h 0m 0s"),
            (7_384_900, "2h 3m 4s"),
        ];
        for (ms, formatted) in cases {
            assert_eq!(
                format_tool_duration(Duration::from_millis(*ms as u64)),
                *formatted,
                "{ms}ms"
            );
        }
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
                "fullOutputPath": "/tmp/rpi-bash-abc.log"
            })),
            duration_ms: None,
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
        assert!(
            stripped.contains(
                "[Full output: /tmp/rpi-bash-abc.log. Truncated: showing 12 of 50 lines]"
            )
        );
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
    fn warnings_wrap_at_terminal_width_with_long_full_output_path() {
        // Regression: a long `fullOutputPath` used to be pushed as a raw
        // line, exceeding the terminal width and panicking the renderer
        // (tui.rs width guard: `Rendered line … exceeds terminal width`).
        let theme = theme();
        let state = RendererStateSlot::default();
        let renderer = BashToolRenderer;
        let long_path = format!("/tmp/rpi-bash-{}.log", "76d578ae3f748c37".repeat(6));
        let result = ToolResultState {
            content: vec![],
            is_error: false,
            details: Some(json!({
                "truncation": {
                    "truncated": true,
                    "truncatedBy": "bytes",
                    "outputLines": 24,
                    "maxBytes": 51200
                },
                "fullOutputPath": long_path
            })),
            duration_ms: None,
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
        // The warnings line must wrap so every rendered line fits the
        // width (the un-truncated line was 91 > 79 in the reported crash).
        for (index, line) in component.render(79).iter().enumerate() {
            assert!(
                rpi_tui::utils::visible_width(line) <= 79,
                "line {index} exceeds width: w={} {line:?}",
                rpi_tui::utils::visible_width(line)
            );
        }
        let stripped = strip_ansi(&component.render(79).join("\n"));
        assert!(stripped.contains("Full output:"), "stripped: {stripped}");
        assert!(stripped.contains("/tmp/rpi-bash-"), "stripped: {stripped}");
        // The full path survives the wrap (not truncated away).
        assert!(
            stripped.contains("76d578ae3f748c37"),
            "stripped: {stripped}"
        );
        assert!(stripped.contains("24 lines shown"), "stripped: {stripped}");
    }

    #[test]
    fn expanded_output_wraps_long_lines_at_terminal_width() {
        // Regression: expanded mode pushed output lines raw; a long line
        // would trip the renderer's width guard. Upstream wraps via `Text`.
        let theme = theme();
        let state = RendererStateSlot::default();
        let renderer = BashToolRenderer;
        let long_line = "x".repeat(200);
        let result = ToolResultState {
            content: vec![
                crate::modes::interactive::components::tool_execution::ToolResultContentLoose::text(
                    format!("{long_line}\nshort"),
                ),
            ],
            is_error: false,
            details: None,
            duration_ms: None,
        };
        let context = context(&state);
        let component = renderer
            .render_result(
                &result,
                ResultRenderOptions {
                    expanded: true,
                    is_partial: false,
                },
                &theme,
                &context,
            )
            .expect("result component");
        for (index, line) in component.render(79).iter().enumerate() {
            assert!(
                rpi_tui::utils::visible_width(line) <= 79,
                "line {index} exceeds width: w={} {line:?}",
                rpi_tui::utils::visible_width(line)
            );
        }
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
            duration_ms: None,
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
        assert!(render_state.timer.ticker_running());
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
        assert!(!render_state.timer.ticker_running());
        let stripped = strip_ansi(&component.render(80).join("\n"));
        assert!(stripped.contains("Took 0."), "stripped: {stripped}");
    }

    // V17-05 FR-C R3 (#10557): a final result's recorded `durationMs` wins
    // over the renderer's own clock (it survives reloads).
    #[test]
    fn recorded_duration_wins_over_the_internal_clock() {
        let theme = theme();
        let state = RendererStateSlot::default();
        let renderer = BashToolRenderer;
        let result = ToolResultState {
            content: vec![],
            is_error: false,
            details: None,
            duration_ms: Some(65_400),
        };
        let mut ctx = context(&state);
        renderer.render_call(&json!({"command": "sleep 65"}), &theme, &ctx);
        ctx.duration_ms = Some(65_400);
        let component = renderer
            .render_result(
                &result,
                ResultRenderOptions {
                    expanded: false,
                    is_partial: false,
                },
                &theme,
                &ctx,
            )
            .expect("final component");
        let stripped = strip_ansi(&component.render(80).join("\n"));
        // `formatDuration(65400)` = `1m 5s`; the internal clock (started
        // during `render_call`) would print `Took 0.`.
        assert!(stripped.contains("Took 1m 5s"), "stripped: {stripped}");
        assert!(!stripped.contains("Took 0."), "stripped: {stripped}");
    }

    fn naive_styled(raw: &str, theme: &Theme) -> String {
        raw.split('\n')
            .map(|line| theme.fg("toolOutput", line))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// rpi#78 A4/0.3: the incremental styling must reproduce the naive
    /// per-line join byte for byte, including tabs, blank lines and multi-byte
    /// payloads.
    #[test]
    fn incremental_styling_matches_the_naive_join() {
        let theme = theme();
        let mut cache = BashOutputCache::default();
        let full = "a\tb\n\n中文 🚀 // 注\n\tindented\nlast";
        let mut raw = String::new();
        for ch in full.chars() {
            raw.push(ch);
            let styled = cache.styled(&raw, &theme);
            assert_eq!(*styled, naive_styled(&raw, &theme), "at {raw:?}");
        }
        // A non-prefix change rebuilds from scratch.
        let replaced = cache.styled("other\ncontent", &theme);
        assert_eq!(*replaced, naive_styled("other\ncontent", &theme));
    }

    /// The cached collapsed preview must equal the whole-text visual truncation
    /// the renderer used before the cache existed.
    #[test]
    fn incremental_preview_matches_the_whole_text_truncation() {
        let theme = theme();
        let raw: String = (1..=40)
            .map(|i| format!("line{i} {}\n", "x".repeat(i * 7)))
            .collect();
        let raw = raw.trim_end().to_string();
        for width in [20usize, 40, 80] {
            let mut cache = BashOutputCache::default();
            let mut growing = String::new();
            for line in raw.split('\n') {
                if !growing.is_empty() {
                    growing.push('\n');
                }
                growing.push_str(line);
                let styled = cache.styled(&growing, &theme);
                let expected = truncate_to_visual_lines(&styled, BASH_PREVIEW_LINES, width, 0);
                match cache.preview(width, BASH_PREVIEW_LINES) {
                    Some((lines, skipped)) => {
                        assert_eq!(lines, expected.visual_lines, "width {width}");
                        assert_eq!(skipped, expected.skipped_count, "width {width}");
                    }
                    None => assert_eq!(expected.skipped_count, 0, "width {width}"),
                }
            }
        }
    }

    /// A CR in the output cannot be reproduced by the per-line fast path, so
    /// the cache must defuse and let the renderer use the whole-text wrap.
    #[test]
    fn cr_output_falls_back_to_the_whole_text_path() {
        let theme = theme();
        let mut cache = BashOutputCache::default();
        let _ = cache.styled("a\r\nb\r\nc", &theme);
        assert!(cache.preview(80, BASH_PREVIEW_LINES).is_none());
    }

    /// Rendering with the precomputed preview must be byte-identical to the
    /// whole-text fallback the renderer used before.
    #[test]
    fn cached_preview_renders_identically_to_the_fallback() {
        let theme = theme();
        let state = RendererStateSlot::default();
        let renderer = BashToolRenderer;
        let mut output: String = (1..=60)
            .map(|i| format!("line{i} {}\n", "y".repeat(i * 3)))
            .collect();
        output.push_str("tail");
        let result = ToolResultState {
            content: vec![
                crate::modes::interactive::components::tool_execution::ToolResultContentLoose::text(
                    output,
                ),
            ],
            is_error: false,
            details: None,
            duration_ms: None,
        };

        let render = |terminal_width: usize| {
            let state = RendererStateSlot::default();
            let mut ctx = context(&state);
            ctx.terminal_width = terminal_width;
            let component = renderer
                .render_result(
                    &result,
                    ResultRenderOptions {
                        expanded: false,
                        is_partial: false,
                    },
                    &theme,
                    &ctx,
                )
                .expect("result component");
            component.render(80)
        };
        // 0 = unknown width at context-build time (whole-text fallback); 80 =
        // the cache path.
        let _ = state;
        assert_eq!(render(80), render(0));
    }
}
