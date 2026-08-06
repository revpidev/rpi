//! Status indicators — port of
//! `packages/coding-agent/src/modes/interactive/components/status-indicator.ts`
//! @ pi 0.82.1 (2efa728).
//!
//! Intentional differences:
//! - The theme is passed explicitly (`Arc<Theme>`) instead of read from the
//!   global `theme` getter (theme.ts:799-816).
//! - Upstream classes extend `Loader`; the port composes a [`Loader`]
//!   (same render contract). [`RetryStatusIndicator`] shares the loader
//!   through `Arc<Mutex<Loader>>` because the countdown thread calls
//!   `set_message` while the render loop calls `render`.
//! - [`CountdownTimer`] is the thread-based port (see its module doc).
//! - `WorkingIndicatorOptions` (the extension-provided spinner options,
//!   status-indicator.ts:18) maps to `LoaderIndicatorOptions`; the
//!   constructor here keeps upstream's default (default frames).

use std::sync::{Arc, Mutex};

use pir_tui::components::loader::{Loader, LoaderIndicatorOptions};
use pir_tui::tui::{Component, RenderHandle};

use crate::core::themes::Theme;

use super::countdown_timer::CountdownTimer;
use super::keybinding_hints::key_text;

/// `StatusIndicatorKind` (status-indicator.ts:7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusIndicatorKind {
    Working,
    Retry,
    Compaction,
    BranchSummary,
}

/// `StatusIndicator` (status-indicator.ts:9-27).
pub struct StatusIndicator {
    pub kind: StatusIndicatorKind,
    loader: Loader,
}

impl StatusIndicator {
    pub fn new(
        kind: StatusIndicatorKind,
        render_handle: RenderHandle,
        spinner_color_fn: Box<dyn Fn(&str) -> String + Send + Sync>,
        message_color_fn: Box<dyn Fn(&str) -> String + Send + Sync>,
        message: impl Into<String>,
        indicator: Option<LoaderIndicatorOptions>,
    ) -> Self {
        Self {
            kind,
            loader: Loader::new(
                render_handle,
                spinner_color_fn,
                message_color_fn,
                message,
                indicator,
            ),
        }
    }

    /// `Loader.setMessage` (upstream inheritance) — used by the retry
    /// countdown through [`RetryStatusIndicator`].
    pub fn set_message(&mut self, message: impl Into<String>) {
        self.loader.set_message(message);
    }

    /// `dispose` (status-indicator.ts:24-26).
    pub fn dispose(&mut self) {
        self.loader.stop();
    }
}

impl Component for StatusIndicator {
    fn render(&self, width: usize) -> Vec<String> {
        self.loader.render(width)
    }

    fn invalidate(&mut self) {
        self.loader.invalidate();
    }
}

/// `WorkingStatusIndicator` (status-indicator.ts:29-40): accent spinner,
/// muted message.
pub struct WorkingStatusIndicator {
    inner: StatusIndicator,
}

impl WorkingStatusIndicator {
    pub fn new(render_handle: RenderHandle, message: impl Into<String>, theme: Arc<Theme>) -> Self {
        Self {
            inner: StatusIndicator::new(
                StatusIndicatorKind::Working,
                render_handle,
                Box::new({
                    let theme = Arc::clone(&theme);
                    move |spinner: &str| theme.fg("accent", spinner)
                }),
                Box::new({
                    let theme = Arc::clone(&theme);
                    move |text: &str| theme.fg("muted", text)
                }),
                message,
                None,
            ),
        }
    }

    pub fn dispose(&mut self) {
        self.inner.dispose();
    }
}

impl Component for WorkingStatusIndicator {
    fn render(&self, width: usize) -> Vec<String> {
        self.inner.render(width)
    }

    fn invalidate(&mut self) {
        self.inner.invalidate();
    }
}

/// `RetryStatusIndicator` (status-indicator.ts:42-72): warning spinner with
/// a live countdown message.
pub struct RetryStatusIndicator {
    loader: Arc<Mutex<Loader>>,
    countdown: Option<CountdownTimer>,
}

impl RetryStatusIndicator {
    pub fn new(
        render_handle: RenderHandle,
        attempt: u32,
        max_attempts: u32,
        delay_ms: u64,
        theme: Arc<Theme>,
    ) -> Self {
        let retry_message = move |seconds: u64| {
            format!(
                "Retrying ({attempt}/{max_attempts}) in {seconds}s... ({} to cancel)",
                key_text("app.interrupt")
            )
        };
        let loader = Arc::new(Mutex::new(Loader::new(
            render_handle.clone(),
            {
                let theme = Arc::clone(&theme);
                move |spinner: &str| theme.fg("warning", spinner)
            },
            {
                let theme = Arc::clone(&theme);
                move |text: &str| theme.fg("muted", text)
            },
            retry_message(delay_ms.div_ceil(1000)),
            None,
        )));

        // Countdown updates the loader message each second
        // (status-indicator.ts:55-64). The `onExpire` callback nulls the
        // upstream reference; here the timer thread ends itself, so the
        // callback is a no-op (dispose after expiry joins an ended thread).
        let countdown_loader = Arc::clone(&loader);
        let countdown = CountdownTimer::new(
            delay_ms,
            Some(render_handle),
            Box::new(move |seconds| {
                if let Ok(mut loader) = countdown_loader.lock() {
                    loader.set_message(retry_message(seconds));
                }
            }),
            Box::new(|| {}),
        );

        Self {
            loader,
            countdown: Some(countdown),
        }
    }

    /// `dispose` (status-indicator.ts:67-71).
    pub fn dispose(&mut self) {
        if let Some(mut countdown) = self.countdown.take() {
            countdown.dispose();
        }
        if let Ok(mut loader) = self.loader.lock() {
            loader.stop();
        }
    }
}

impl Component for RetryStatusIndicator {
    fn render(&self, width: usize) -> Vec<String> {
        let Ok(loader) = self.loader.lock() else {
            return Vec::new();
        };
        loader.render(width)
    }

    fn invalidate(&mut self) {
        if let Ok(mut loader) = self.loader.lock() {
            loader.invalidate();
        }
    }
}

/// `CompactionStatusReason` (status-indicator.ts:74).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionStatusReason {
    Manual,
    Threshold,
    Overflow,
}

/// `CompactionStatusIndicator` (status-indicator.ts:76-91).
pub struct CompactionStatusIndicator {
    inner: StatusIndicator,
}

impl CompactionStatusIndicator {
    pub fn new(
        render_handle: RenderHandle,
        reason: CompactionStatusReason,
        theme: Arc<Theme>,
    ) -> Self {
        let cancel_hint = format!("({} to cancel)", key_text("app.interrupt"));
        let label = match reason {
            CompactionStatusReason::Manual => format!("Compacting context... {cancel_hint}"),
            CompactionStatusReason::Overflow => {
                format!("Context overflow detected, Auto-compacting... {cancel_hint}")
            }
            CompactionStatusReason::Threshold => format!("Auto-compacting... {cancel_hint}"),
        };
        Self {
            inner: StatusIndicator::new(
                StatusIndicatorKind::Compaction,
                render_handle,
                Box::new({
                    let theme = Arc::clone(&theme);
                    move |spinner: &str| theme.fg("accent", spinner)
                }),
                Box::new({
                    let theme = Arc::clone(&theme);
                    move |text: &str| theme.fg("muted", text)
                }),
                label,
                None,
            ),
        }
    }

    pub fn dispose(&mut self) {
        self.inner.dispose();
    }
}

impl Component for CompactionStatusIndicator {
    fn render(&self, width: usize) -> Vec<String> {
        self.inner.render(width)
    }

    fn invalidate(&mut self) {
        self.inner.invalidate();
    }
}

/// `BranchSummaryStatusIndicator` (status-indicator.ts:93-103).
pub struct BranchSummaryStatusIndicator {
    inner: StatusIndicator,
}

impl BranchSummaryStatusIndicator {
    pub fn new(render_handle: RenderHandle, theme: Arc<Theme>) -> Self {
        let message = format!(
            "Summarizing branch... ({} to cancel)",
            key_text("app.interrupt")
        );
        Self {
            inner: StatusIndicator::new(
                StatusIndicatorKind::BranchSummary,
                render_handle,
                Box::new({
                    let theme = Arc::clone(&theme);
                    move |spinner: &str| theme.fg("accent", spinner)
                }),
                Box::new({
                    let theme = Arc::clone(&theme);
                    move |text: &str| theme.fg("muted", text)
                }),
                message,
                None,
            ),
        }
    }

    pub fn dispose(&mut self) {
        self.inner.dispose();
    }
}

impl Component for BranchSummaryStatusIndicator {
    fn render(&self, width: usize) -> Vec<String> {
        self.inner.render(width)
    }

    fn invalidate(&mut self) {
        self.inner.invalidate();
    }
}

/// `IdleStatus` (status-indicator.ts:105-113): two blank full-width lines.
pub struct IdleStatus;

impl Component for IdleStatus {
    fn render(&self, width: usize) -> Vec<String> {
        let empty_line = " ".repeat(width);
        vec![empty_line.clone(), empty_line]
    }

    fn invalidate(&mut self) {
        // No cached state to invalidate.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::themes::load_theme;
    use pir_tui::tui::RenderHandle;

    fn theme() -> Arc<Theme> {
        Arc::new(load_theme("dark", None).expect("builtin dark theme"))
    }

    fn strip_ansi(input: &str) -> String {
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
    fn working_indicator_renders_message() {
        let indicator =
            WorkingStatusIndicator::new(RenderHandle::new(|| {}), "Working...", theme());
        let stripped = strip_ansi(&indicator.render(40).join("\n"));
        assert!(stripped.contains("Working..."));
    }

    #[test]
    fn compaction_labels() {
        let manual = CompactionStatusIndicator::new(
            RenderHandle::new(|| {}),
            CompactionStatusReason::Manual,
            theme(),
        );
        let stripped = strip_ansi(&manual.render(60).join("\n"));
        assert!(stripped.contains("Compacting context... (escape to cancel)"));

        let overflow = CompactionStatusIndicator::new(
            RenderHandle::new(|| {}),
            CompactionStatusReason::Overflow,
            theme(),
        );
        let stripped = strip_ansi(&overflow.render(60).join("\n"));
        assert!(stripped.contains("Context overflow detected, Auto-compacting..."));

        let threshold = CompactionStatusIndicator::new(
            RenderHandle::new(|| {}),
            CompactionStatusReason::Threshold,
            theme(),
        );
        let stripped = strip_ansi(&threshold.render(60).join("\n"));
        assert!(stripped.contains("Auto-compacting..."));
        assert!(!stripped.contains("Context overflow"));
    }

    #[test]
    fn branch_summary_label() {
        let indicator = BranchSummaryStatusIndicator::new(RenderHandle::new(|| {}), theme());
        let stripped = strip_ansi(&indicator.render(40).join("\n"));
        assert!(stripped.contains("Summarizing branch..."));
    }

    #[test]
    fn idle_status_renders_two_blank_lines() {
        let idle = IdleStatus;
        let lines = idle.render(10);
        assert_eq!(
            lines,
            vec!["          ".to_string(), "          ".to_string()]
        );
    }

    #[test]
    fn retry_indicator_counts_down() {
        let mut indicator =
            RetryStatusIndicator::new(RenderHandle::new(|| {}), 1, 3, 1200, theme());
        let initial = strip_ansi(&indicator.render(60).join("\n"));
        assert!(
            initial.contains("Retrying (1/3) in 2s..."),
            "initial: {initial}"
        );

        // Wait for the countdown to tick past the initial value.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut saw_tick = false;
        while std::time::Instant::now() < deadline {
            let stripped = strip_ansi(&indicator.render(60).join("\n"));
            if stripped.contains("in 1s...") || stripped.contains("in 0s...") {
                saw_tick = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(saw_tick, "countdown message must update");
        indicator.dispose();
    }

    #[test]
    fn retry_indicator_dispose_stops_countdown() {
        let mut indicator =
            RetryStatusIndicator::new(RenderHandle::new(|| {}), 1, 3, 10_000, theme());
        indicator.dispose();
        // No panic, thread joined.
        let _ = indicator.render(60);
    }
}
