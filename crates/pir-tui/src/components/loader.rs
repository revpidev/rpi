//! Port of `packages/tui/src/components/loader.ts` @ pi 0.82.1 (2efa728).
//!
//! Intentional differences:
//! - The upstream constructor takes the whole `TUI` instance and the interval
//!   tick calls `ui.requestRender()` (loader.ts:88-89); here the component
//!   holds only a [`RenderHandle`], the capability it needs.
//! - The upstream `setInterval` timer is implemented as a dedicated thread
//!   waiting on `recv_timeout` of a stop channel. `stop()`/`Drop` send the
//!   stop signal and join the thread, so no background task leaks
//!   (coding-standards §6.4); upstream would keep the interval running until
//!   an explicit `stop()`.
//! - Frame state lives in an `Arc<AtomicUsize>`; `render(&self)` computes the
//!   display text on demand from the current frame (upstream mutates
//!   `currentFrame` + `setText` in the interval callback and stores the text
//!   on the component). The rendered bytes are identical.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::tui::{Component, RenderHandle};

use super::text::{ColorFn, Text};

/// Animation frames for the default spinner (loader.ts:11).
pub const DEFAULT_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// Default frame interval in milliseconds (loader.ts:12).
pub const DEFAULT_INTERVAL_MS: u64 = 80;

/// Options for a loader indicator (`LoaderIndicatorOptions`, loader.ts:4-10).
#[derive(Debug, Clone, Default)]
pub struct LoaderIndicatorOptions {
    /// Animation frames. Use an empty array to hide the indicator.
    pub frames: Option<Vec<String>>,
    /// Frame interval in milliseconds for animated indicators.
    pub interval_ms: Option<u64>,
}

/// Loader component that updates with an optional spinning animation
/// (upstream `Loader`, loader.ts:17).
pub struct Loader {
    text: Text,
    frames: Vec<String>,
    interval_ms: u64,
    current_frame: Arc<AtomicUsize>,
    render_indicator_verbatim: bool,
    spinner_color_fn: ColorFn,
    message_color_fn: ColorFn,
    message: String,
    render_handle: RenderHandle,
    // Animation thread lifecycle; `None` while stopped.
    stop_tx: Option<mpsc::Sender<()>>,
    thread_handle: Option<JoinHandle<()>>,
}

impl Loader {
    /// `indicator` defaults to `None` (default spinner frames, colored).
    pub fn new<F1, F2>(
        render_handle: RenderHandle,
        spinner_color_fn: F1,
        message_color_fn: F2,
        message: impl Into<String>,
        indicator: Option<LoaderIndicatorOptions>,
    ) -> Self
    where
        F1: Fn(&str) -> String + Send + Sync + 'static,
        F2: Fn(&str) -> String + Send + Sync + 'static,
    {
        let mut loader =
            Self::new_default(render_handle, spinner_color_fn, message_color_fn, message);
        loader.set_indicator(indicator);
        loader
    }

    /// Like [`Loader::new`] with `indicator` left `None`, but overrides the
    /// frame interval (milliseconds); `0` falls back to
    /// [`DEFAULT_INTERVAL_MS`] (same as [`LoaderIndicatorOptions`]). A long
    /// interval pins the first frame deterministically for callers that
    /// render right after construction.
    pub fn new_with_interval<F1, F2>(
        render_handle: RenderHandle,
        spinner_color_fn: F1,
        message_color_fn: F2,
        message: impl Into<String>,
        interval_ms: u64,
    ) -> Self
    where
        F1: Fn(&str) -> String + Send + Sync + 'static,
        F2: Fn(&str) -> String + Send + Sync + 'static,
    {
        let mut loader =
            Self::new_default(render_handle, spinner_color_fn, message_color_fn, message);
        if interval_ms > 0 {
            loader.interval_ms = interval_ms;
        }
        loader.start();
        loader
    }

    /// Shared default state for the constructors: default spinner frames,
    /// colored, default interval, not yet started.
    fn new_default<F1, F2>(
        render_handle: RenderHandle,
        spinner_color_fn: F1,
        message_color_fn: F2,
        message: impl Into<String>,
    ) -> Self
    where
        F1: Fn(&str) -> String + Send + Sync + 'static,
        F2: Fn(&str) -> String + Send + Sync + 'static,
    {
        Self {
            text: Text::new("", 1, 0, None),
            frames: DEFAULT_FRAMES.iter().map(|s| s.to_string()).collect(),
            interval_ms: DEFAULT_INTERVAL_MS,
            current_frame: Arc::new(AtomicUsize::new(0)),
            render_indicator_verbatim: false,
            spinner_color_fn: Box::new(spinner_color_fn),
            message_color_fn: Box::new(message_color_fn),
            message: message.into(),
            render_handle,
            stop_tx: None,
            thread_handle: None,
        }
    }

    pub fn start(&mut self) {
        self.update_display();
        self.restart_animation();
    }

    pub fn stop(&mut self) {
        if let Some(stop_tx) = self.stop_tx.take() {
            let _ = stop_tx.send(());
        }
        if let Some(thread_handle) = self.thread_handle.take() {
            // The thread wakes from `recv_timeout` on the signal above, so the
            // join is bounded by the scheduler latency, not the frame interval.
            let _ = thread_handle.join();
        }
    }

    pub fn set_message(&mut self, message: impl Into<String>) {
        self.message = message.into();
        self.update_display();
    }

    pub fn set_indicator(&mut self, indicator: Option<LoaderIndicatorOptions>) {
        let (verbatim, frames, interval_ms) = match indicator {
            Some(options) => (
                true,
                options
                    .frames
                    .unwrap_or_else(|| DEFAULT_FRAMES.iter().map(|s| s.to_string()).collect()),
                options
                    .interval_ms
                    .filter(|ms| *ms > 0)
                    .unwrap_or(DEFAULT_INTERVAL_MS),
            ),
            None => (
                false,
                DEFAULT_FRAMES.iter().map(|s| s.to_string()).collect(),
                DEFAULT_INTERVAL_MS,
            ),
        };
        self.render_indicator_verbatim = verbatim;
        self.frames = frames;
        self.interval_ms = interval_ms;
        // Stop and join the old animation thread before resetting the frame
        // counter: a tick landing after `store(0)` would advance past 0, and
        // it would still use the old `frames_len`.
        self.stop();
        self.current_frame.store(0, Ordering::SeqCst);
        self.update_display();
        self.restart_animation();
    }

    fn restart_animation(&mut self) {
        self.stop();
        if self.frames.len() <= 1 {
            return;
        }
        let frames_len = self.frames.len();
        let interval = Duration::from_millis(self.interval_ms);
        let current_frame = Arc::clone(&self.current_frame);
        let render_handle = self.render_handle.clone();
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        self.thread_handle = Some(thread::spawn(move || loop {
            match stop_rx.recv_timeout(interval) {
                Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {
                    current_frame.store(
                        (current_frame.load(Ordering::SeqCst) + 1) % frames_len,
                        Ordering::SeqCst,
                    );
                    render_handle.request_render();
                }
            }
        }));
        self.stop_tx = Some(stop_tx);
    }

    /// Upstream `updateDisplay` (loader.ts:83-91) also calls `setText` with the
    /// frame/message text; here the display text is computed in `render`, so
    /// this only mirrors the `ui.requestRender()` side effect.
    fn update_display(&self) {
        self.render_handle.request_render();
    }

    /// The display text for the current frame, computed on demand
    /// (upstream `updateDisplay`, loader.ts:84-87).
    fn display_text(&self) -> String {
        let frame = self
            .frames
            .get(self.current_frame.load(Ordering::SeqCst))
            .map(String::as_str)
            .unwrap_or("");
        let rendered_frame = if self.render_indicator_verbatim {
            frame.to_string()
        } else {
            (self.spinner_color_fn)(frame)
        };
        let indicator = if frame.is_empty() {
            String::new()
        } else {
            format!("{rendered_frame} ")
        };
        format!("{indicator}{}", (self.message_color_fn)(&self.message))
    }
}

impl Drop for Loader {
    fn drop(&mut self) {
        // Never leak the animation thread (coding-standards §6.4).
        self.stop();
    }
}

impl Component for Loader {
    fn render(&self, width: usize) -> Vec<String> {
        let mut lines = vec![String::new()];
        lines.extend(self.text.render_text(&self.display_text(), width));
        lines
    }

    fn invalidate(&mut self) {
        self.text.invalidate();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::visible_width;
    use std::sync::atomic::AtomicUsize;
    use std::thread;
    use std::time::Instant;

    fn noop_render_handle() -> RenderHandle {
        RenderHandle::new(|| {})
    }

    fn strip_ansi(input: &str) -> String {
        let mut out = String::with_capacity(input.len());
        let mut chars = input.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' && chars.peek() == Some(&'[') {
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

    fn frames(list: &[&str]) -> Option<LoaderIndicatorOptions> {
        Some(LoaderIndicatorOptions {
            frames: Some(list.iter().map(|s| s.to_string()).collect()),
            interval_ms: None,
        })
    }

    #[test]
    fn renders_blank_line_then_text_padded_to_width() {
        // Custom frames render verbatim (no spinner color, loader.ts:84-85).
        let loader = Loader::new(
            noop_render_handle(),
            |s| format!("\x1b[36m{s}\x1b[0m"),
            |s| format!("\x1b[2m{s}\x1b[0m"),
            "Loading...",
            frames(&["a"]),
        );
        let lines = loader.render(20);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "");
        assert_eq!(
            lines[1],
            format!(" a \x1b[2mLoading...\x1b[0m{}", " ".repeat(7))
        );
        assert_eq!(visible_width(&lines[1]), 20);
    }

    #[test]
    fn colors_default_frames_with_spinner_color_fn() {
        let loader = Loader::new(
            noop_render_handle(),
            |s| format!("\x1b[36m{s}\x1b[0m"),
            |s| format!("\x1b[2m{s}\x1b[0m"),
            "Loading...",
            None,
        );
        let lines = loader.render(20);
        let stripped = strip_ansi(&lines[1]);
        // Default frames are rendered through the spinner color fn (verbatim
        // only for custom indicators, loader.ts:84-85).
        assert!(lines[1].contains("\x1b[36m"));
        assert!(DEFAULT_FRAMES.iter().any(|frame| stripped.contains(frame)));
        assert!(stripped.contains("Loading..."));
    }

    #[test]
    fn renders_custom_frames_verbatim() {
        let loader = Loader::new(
            noop_render_handle(),
            |s| format!("[spinner:{s}]"),
            |s| s.to_string(),
            "Working",
            frames(&["a"]),
        );
        let stripped = strip_ansi(&loader.render(20)[1]);
        assert_eq!(stripped.trim_end(), " a Working");
        assert!(!stripped.contains("[spinner:"));
    }

    #[test]
    fn empty_frames_hide_indicator() {
        let loader = Loader::new(
            noop_render_handle(),
            |s| format!("[spinner:{s}]"),
            |s| s.to_string(),
            "Working",
            frames(&[]),
        );
        let stripped = strip_ansi(&loader.render(20)[1]);
        assert_eq!(stripped.trim_end(), " Working");
    }

    #[test]
    fn single_frame_does_not_animate() {
        let mut loader = Loader::new(
            noop_render_handle(),
            |s| s.to_string(),
            |s| s.to_string(),
            "Working",
            frames(&["a"]),
        );
        let before = loader.render(20);
        thread::sleep(Duration::from_millis(50));
        assert_eq!(before, loader.render(20));
        loader.stop();
    }

    #[test]
    fn animates_frames_and_stop_freezes() {
        let mut loader = Loader::new(
            noop_render_handle(),
            |s| s.to_string(),
            |s| s.to_string(),
            "Working",
            Some(LoaderIndicatorOptions {
                frames: Some(vec!["a".into(), "b".into(), "c".into()]),
                interval_ms: Some(10),
            }),
        );

        // Poll until the frame advanced past the initial one.
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut advanced = false;
        while Instant::now() < deadline {
            let stripped = strip_ansi(&loader.render(30)[1]);
            if stripped.contains("b Working") || stripped.contains("c Working") {
                advanced = true;
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert!(advanced, "animation should advance frames");

        loader.stop();
        let frozen = loader.render(30);
        thread::sleep(Duration::from_millis(60));
        assert_eq!(frozen, loader.render(30), "stopped loader must not animate");
    }

    #[test]
    fn set_message_updates_display() {
        let mut loader = Loader::new(
            noop_render_handle(),
            |s| s.to_string(),
            |s| s.to_string(),
            "Loading...",
            frames(&["a"]),
        );
        assert!(strip_ansi(&loader.render(20)[1]).contains("Loading..."));
        loader.set_message("Working");
        assert!(strip_ansi(&loader.render(20)[1]).contains("Working"));
    }

    #[test]
    fn set_indicator_restarts_animation() {
        let mut loader = Loader::new(
            noop_render_handle(),
            |s| s.to_string(),
            |s| s.to_string(),
            "Working",
            frames(&["a"]),
        );
        loader.set_indicator(Some(LoaderIndicatorOptions {
            frames: Some(vec!["x".into(), "y".into()]),
            interval_ms: Some(10),
        }));
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut advanced = false;
        while Instant::now() < deadline {
            let stripped = strip_ansi(&loader.render(30)[1]);
            if stripped.contains("y Working") {
                advanced = true;
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert!(advanced, "set_indicator should restart the animation");
        loader.stop();
    }

    #[test]
    fn ticks_request_renders_through_render_handle() {
        let renders = Arc::new(AtomicUsize::new(0));
        let renders_thread = Arc::clone(&renders);
        let handle = RenderHandle::new(move || {
            renders_thread.fetch_add(1, Ordering::Relaxed);
        });
        let mut loader = Loader::new(
            handle,
            |s| s.to_string(),
            |s| s.to_string(),
            "Working",
            Some(LoaderIndicatorOptions {
                frames: Some(vec!["a".into(), "b".into()]),
                interval_ms: Some(10),
            }),
        );
        // start() calls updateDisplay once (loader.ts:48, 88-89).
        let initial = renders.load(Ordering::Relaxed);
        assert!(initial >= 1, "start() must request a render");

        let deadline = Instant::now() + Duration::from_secs(2);
        while renders.load(Ordering::Relaxed) <= initial && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(
            renders.load(Ordering::Relaxed) > initial,
            "animation ticks must request renders"
        );
        loader.stop();
    }

    #[test]
    fn drop_stops_animation_thread_without_panicking() {
        // Dropping a running loader must join the animation thread (no leak);
        // a panic here would surface as a test failure.
        let loader = Loader::new(
            noop_render_handle(),
            |s| s.to_string(),
            |s| s.to_string(),
            "Working",
            Some(LoaderIndicatorOptions {
                frames: Some(vec!["a".into(), "b".into()]),
                interval_ms: Some(10),
            }),
        );
        drop(loader);
    }
}
