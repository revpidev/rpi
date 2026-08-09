//! Port of `packages/tui/src/components/cancellable-loader.ts` @ pi 0.82.1
//! (2efa728).
//!
//! Intentional differences:
//! - The DOM `AbortController`/`AbortSignal` is replaced by a small Rust
//!   `AbortSignal` (shared `Arc` with an atomic flag + listener list). The
//!   observable semantics match: `aborted()` query, `add_listener()` for the
//!   `'abort'` event, abort is idempotent, and listeners fire synchronously in
//!   registration order.
//! - Escape handling goes through [`get_keybindings`] +
//!   `Keybinding::SelectCancel` exactly like upstream
//!   `kb.matches(data, "tui.select.cancel")` — no hardcoded keys.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::keybindings::{get_keybindings, Keybinding};
use crate::tui::{Component, RenderHandle};

use super::loader::{Loader, LoaderIndicatorOptions};

/// Shared abort state, mirroring the observable surface of the DOM
/// `AbortSignal` used upstream (cancellable-loader.ts:14-27).
pub struct AbortSignal {
    aborted: AtomicBool,
    // Drained on abort; registered listeners fire once, in order.
    listeners: Mutex<Vec<Box<dyn Fn() + Send>>>,
}

impl AbortSignal {
    fn new() -> Self {
        Self {
            aborted: AtomicBool::new(false),
            listeners: Mutex::new(Vec::new()),
        }
    }

    /// Whether the signal was aborted (`AbortSignal.aborted`).
    pub fn aborted(&self) -> bool {
        self.aborted.load(Ordering::SeqCst)
    }

    /// Register a listener invoked when the signal aborts
    /// (`AbortSignal.addEventListener("abort", ...)`).
    pub fn add_listener(&self, listener: impl Fn() + Send + 'static) {
        if let Ok(mut listeners) = self.listeners.lock() {
            listeners.push(Box::new(listener));
        }
    }

    /// Fire the abort, once (DOM `AbortController.abort()` is idempotent).
    fn abort(&self) {
        if self.aborted.swap(true, Ordering::SeqCst) {
            return;
        }
        let listeners = self
            .listeners
            .lock()
            .map(|mut listeners| std::mem::take(&mut *listeners))
            .unwrap_or_default();
        for listener in listeners {
            listener();
        }
    }
}

/// Loader that can be cancelled with Escape (upstream `CancellableLoader`,
/// cancellable-loader.ts:13).
pub struct CancellableLoader {
    loader: Loader,
    signal: Arc<AbortSignal>,

    /// Called when user presses Escape (upstream `onAbort`).
    pub on_abort: Option<Box<dyn FnMut() + Send>>,
}

impl CancellableLoader {
    /// Same constructor surface as [`Loader`] (upstream extends it).
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
        Self {
            loader: Loader::new(
                render_handle,
                spinner_color_fn,
                message_color_fn,
                message,
                indicator,
            ),
            signal: Arc::new(AbortSignal::new()),
            on_abort: None,
        }
    }

    /// The abort signal, aborted when the user presses Escape.
    pub fn signal(&self) -> Arc<AbortSignal> {
        Arc::clone(&self.signal)
    }

    /// Whether the loader was aborted.
    pub fn aborted(&self) -> bool {
        self.signal.aborted()
    }

    /// Upstream `dispose` (cancellable-loader.ts:37-39): stop the animation.
    pub fn dispose(&mut self) {
        self.loader.stop();
    }
}

impl Component for CancellableLoader {
    fn render(&self, width: usize) -> Vec<String> {
        self.loader.render(width)
    }

    fn invalidate(&mut self) {
        self.loader.invalidate();
    }

    fn handle_input(&mut self, data: &str) {
        let keybindings = get_keybindings();
        let matched = keybindings
            .read()
            .map(|manager| manager.matches(data, Keybinding::SelectCancel))
            .unwrap_or(false);
        if matched {
            self.signal.abort();
            if let Some(on_abort) = self.on_abort.as_mut() {
                on_abort();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::thread;
    use std::time::{Duration, Instant};

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

    fn cancellable_loader() -> CancellableLoader {
        CancellableLoader::new(
            noop_render_handle(),
            |s| s.to_string(),
            |s| s.to_string(),
            "Working",
            Some(LoaderIndicatorOptions {
                frames: Some(vec!["a".to_string()]),
                interval_ms: None,
            }),
        )
    }

    #[test]
    fn renders_like_loader() {
        let loader = cancellable_loader();
        let lines = loader.render(20);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "");
        assert_eq!(strip_ansi(&lines[1]).trim_end(), " a Working");
    }

    #[test]
    fn cancel_key_aborts_and_fires_on_abort() {
        let mut loader = cancellable_loader();
        let fired = Arc::new(AtomicUsize::new(0));
        let fired_thread = Arc::clone(&fired);
        loader.on_abort = Some(Box::new(move || {
            fired_thread.fetch_add(1, Ordering::Relaxed);
        }));

        loader.handle_input("\x1b");

        assert!(loader.aborted(), "escape must abort via tui.select.cancel");
        assert_eq!(fired.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn ctrl_c_is_also_a_cancel_key() {
        let mut loader = cancellable_loader();
        loader.handle_input("\x03");
        assert!(loader.aborted());
    }

    #[test]
    fn non_cancel_keys_do_not_abort() {
        let mut loader = cancellable_loader();
        let fired = Arc::new(AtomicUsize::new(0));
        let fired_thread = Arc::clone(&fired);
        loader.on_abort = Some(Box::new(move || {
            fired_thread.fetch_add(1, Ordering::Relaxed);
        }));

        loader.handle_input("\r"); // enter
        loader.handle_input("\x18"); // ctrl+x
        loader.handle_input("\x04"); // ctrl+d

        assert!(!loader.aborted());
        assert_eq!(fired.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn abort_signal_is_idempotent() {
        let mut loader = cancellable_loader();
        let signal_fired = Arc::new(AtomicUsize::new(0));
        let signal_thread = Arc::clone(&signal_fired);
        loader.signal().add_listener(move || {
            signal_thread.fetch_add(1, Ordering::Relaxed);
        });
        let on_abort_fired = Arc::new(AtomicUsize::new(0));
        let on_abort_thread = Arc::clone(&on_abort_fired);
        loader.on_abort = Some(Box::new(move || {
            on_abort_thread.fetch_add(1, Ordering::Relaxed);
        }));

        loader.handle_input("\x1b");
        loader.handle_input("\x1b");
        loader.handle_input("\x03");

        assert!(loader.aborted());
        // `AbortController.abort()` is idempotent, so signal listeners fire once.
        assert_eq!(signal_fired.load(Ordering::Relaxed), 1);
        // `onAbort` runs on every matching keypress (upstream calls it
        // unconditionally after `abort()`, cancellable-loader.ts:29-35).
        assert_eq!(on_abort_fired.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn signal_listeners_fire_on_abort() {
        let mut loader = cancellable_loader();
        let fired = Arc::new(AtomicUsize::new(0));
        let fired_thread = Arc::clone(&fired);
        loader.signal().add_listener(move || {
            fired_thread.fetch_add(1, Ordering::Relaxed);
        });

        assert!(!loader.aborted());
        loader.handle_input("\x1b");

        assert!(loader.aborted());
        assert_eq!(fired.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn dispose_stops_animation_without_aborting() {
        let mut loader = CancellableLoader::new(
            noop_render_handle(),
            |s| s.to_string(),
            |s| s.to_string(),
            "Working",
            Some(LoaderIndicatorOptions {
                frames: Some(vec!["a".into(), "b".into()]),
                interval_ms: Some(10),
            }),
        );

        // Wait for the animation to actually advance once.
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut advanced = false;
        while Instant::now() < deadline {
            if strip_ansi(&loader.render(30)[1]).contains("b Working") {
                advanced = true;
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert!(advanced);

        loader.dispose();
        assert!(
            !loader.aborted(),
            "dispose must not abort (cancellable-loader.ts:37-39)"
        );

        let frozen = loader.render(30);
        thread::sleep(Duration::from_millis(60));
        assert_eq!(
            frozen,
            loader.render(30),
            "disposed loader must not animate"
        );
    }
}
