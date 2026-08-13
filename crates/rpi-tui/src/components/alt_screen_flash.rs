//! Port of `packages/tui/src/components/alt-screen-flash.ts` @ pi 4181f66.
//!
//! `AltScreenFlashContainer`: a stack of transient messages composited by the
//! alternate-screen renderer. Each entry renders as one reversed-video line
//! and expires after a fixed duration.
//!
//! Intentional differences:
//! - The per-entry `setTimeout` is NOT a thread/timer: each entry stores an
//!   expiry `deadline: Instant`; the host loop polls
//!   [`AltScreenFlashContainer::next_deadline`] and drives
//!   [`AltScreenFlashContainer::tick`], which removes every entry whose
//!   deadline has passed and fires the stored render callback once (upstream
//!   fires the callback once per timer; the observable render result is the
//!   same — all expired entries disappear in the same frame). Same
//!   explicit-deadline convention as T28 / `components/scroll_view.rs`
//!   (deviation D-082).
//! - `flash` takes the duration as an explicit `u64` milliseconds argument
//!   (Rust has no default parameters; upstream `durationMs = 1000`). Pass
//!   [`DEFAULT_DURATION_MS`] for the default. Upstream's
//!   `Math.max(0, durationMs)` clamp cannot apply to `u64` and is dropped.
//! - `dispose` only clears the entries — there are no timers to cancel
//!   (upstream `clearTimeout`).
//! - Upstream `invalidate(): void {}` matches the [`Component`] default and
//!   is not redefined.
//! - State lives in a `RefCell` so every method takes `&self` (the
//!   `Component::render` contract); `Send` is preserved, matching the
//!   single-threaded render loop (same precedent as `components/text.rs`).

use std::cell::RefCell;
use std::time::{Duration, Instant};

use crate::tui::{Component, RenderHandle};
use crate::utils::truncate_to_width;

/// Default flash duration in milliseconds (`DEFAULT_DURATION_MS`,
/// alt-screen-flash.ts:4).
pub const DEFAULT_DURATION_MS: u64 = 1000;

/// `FlashEntry` (alt-screen-flash.ts:6-10) with the `NodeJS.Timeout` replaced
/// by an explicit expiry deadline (see the header note).
struct FlashEntry {
    /// Monotonic entry id (upstream `FlashEntry.id`). Upstream's timer
    /// callback uses it to locate the entry in `entries`; the deadline model
    /// removes entries by expiry instead, so the field is allocation-only —
    /// kept to mirror the upstream entry shape.
    #[allow(dead_code)]
    id: u64,
    message: String,
    deadline: Instant,
}

/// Transient messages composited by the alternate-screen renderer
/// (`AltScreenFlashContainer`, alt-screen-flash.ts:13-51).
pub struct AltScreenFlashContainer {
    entries: RefCell<Vec<FlashEntry>>,
    next_id: RefCell<u64>,
    request_render: RenderHandle,
}

impl AltScreenFlashContainer {
    /// Upstream constructor (alt-screen-flash.ts:18-20).
    pub fn new(request_render: RenderHandle) -> Self {
        Self {
            entries: RefCell::new(Vec::new()),
            next_id: RefCell::new(0),
            request_render,
        }
    }

    /// `flash` (alt-screen-flash.ts:22-36): show `message` as a transient
    /// reversed-video line for `duration_ms`. Upstream's `durationMs`
    /// parameter defaults to [`DEFAULT_DURATION_MS`]; pass it explicitly.
    pub fn flash(&self, message: impl Into<String>, duration_ms: u64) {
        let id = *self.next_id.borrow();
        *self.next_id.borrow_mut() += 1;
        self.entries.borrow_mut().push(FlashEntry {
            id,
            message: message.into(),
            deadline: Instant::now() + Duration::from_millis(duration_ms),
        });
        self.request_render.request_render();
    }

    /// Deadline of the earliest expiring entry (explicit-deadline replacement
    /// for the per-entry `setTimeout`s). The host loop should call
    /// [`AltScreenFlashContainer::tick`] at or after this instant.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.entries
            .borrow()
            .iter()
            .map(|entry| entry.deadline)
            .min()
    }

    /// Drive the per-entry timers: remove every entry whose deadline has
    /// passed and fire the stored render callback once (upstream's
    /// `setTimeout` callbacks, alt-screen-flash.ts:25-31; upstream fires one
    /// callback per expired timer, the rendered result is identical).
    pub fn tick(&self, now: Instant) {
        let removed = {
            let mut entries = self.entries.borrow_mut();
            let before = entries.len();
            entries.retain(|entry| now < entry.deadline);
            entries.len() != before
        };
        if removed {
            self.request_render.request_render();
        }
    }

    /// `dispose` (alt-screen-flash.ts:38-41): drop all pending entries.
    pub fn dispose(&self) {
        self.entries.borrow_mut().clear();
    }
}

impl Component for AltScreenFlashContainer {
    /// `render` (alt-screen-flash.ts:45-50): one reversed-video line per
    /// entry, oldest first. The message is padded with a leading/trailing
    /// space and truncated to the width with no ellipsis (`""`).
    fn render(&self, width: usize) -> Vec<String> {
        self.entries
            .borrow()
            .iter()
            .map(|entry| {
                let message = truncate_to_width(&format!(" {} ", entry.message), width, "", false);
                format!("\x1b[7m{message}\x1b[27m")
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    //! Flash stacking and expiry for `AltScreenFlashContainer`
    //! (tui-alt-screen.test.ts "stacks flash messages and collapses them as
    //! they expire"): stacked reversed-video lines, width truncation, and
    //! per-entry expiry driven deterministically via `tick`.

    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;

    /// RenderHandle counting invocations (upstream test spies on the
    /// `requestRender` callback passed to the constructor).
    fn counting_handle(counter: &Arc<AtomicU64>) -> RenderHandle {
        let counter = counter.clone();
        RenderHandle::new(move || {
            counter.fetch_add(1, Ordering::Relaxed);
        })
    }

    fn noop_handle() -> RenderHandle {
        RenderHandle::new(|| {})
    }

    #[test]
    fn renders_each_flash_as_its_own_reversed_line() {
        let flashes = AltScreenFlashContainer::new(noop_handle());
        flashes.flash("First", 80);
        flashes.flash("Second", 500);

        assert_eq!(
            flashes.render(20),
            vec![
                "\x1b[7m First \x1b[27m".to_string(),
                "\x1b[7m Second \x1b[27m".to_string(),
            ]
        );
    }

    #[test]
    fn truncates_long_messages_to_the_width_without_ellipsis() {
        let flashes = AltScreenFlashContainer::new(noop_handle());
        flashes.flash("this message is much too long", 100);

        let lines = flashes.render(10);
        assert_eq!(lines.len(), 1);
        // The trailing `\x1b[0m` is `truncateToWidth`'s truncation reset
        // (upstream utils.ts:150-162 appends it on every truncated result,
        // even with an empty ellipsis).
        assert_eq!(lines[0], "\x1b[7m this mess\x1b[0m\x1b[27m");
    }

    #[test]
    fn flash_requests_a_render() {
        let counter = Arc::new(AtomicU64::new(0));
        let flashes = AltScreenFlashContainer::new(counting_handle(&counter));

        flashes.flash("hello", 100);
        assert_eq!(counter.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn entries_collapse_one_by_one_as_their_deadlines_pass() {
        let counter = Arc::new(AtomicU64::new(0));
        let flashes = AltScreenFlashContainer::new(counting_handle(&counter));
        flashes.flash("First", 80);
        flashes.flash("Second", 500);
        assert_eq!(counter.load(Ordering::Relaxed), 2);
        assert_eq!(flashes.render(20).len(), 2);

        // The earliest deadline is "First" (80ms vs 500ms).
        let first_deadline = flashes.next_deadline().unwrap_or_else(|| unreachable!());
        // Before the deadline: both entries still render, no extra request.
        flashes.tick(first_deadline - Duration::from_millis(1));
        assert_eq!(flashes.render(20).len(), 2);
        assert_eq!(counter.load(Ordering::Relaxed), 2);
        // At the deadline: only "First" expires, render requested.
        flashes.tick(first_deadline);
        assert_eq!(counter.load(Ordering::Relaxed), 3);
        assert_eq!(
            flashes.render(20),
            vec!["\x1b[7m Second \x1b[27m".to_string()]
        );

        // Later: "Second" expires too, leaving nothing.
        let second_deadline = flashes.next_deadline().unwrap_or_else(|| unreachable!());
        flashes.tick(second_deadline);
        assert_eq!(counter.load(Ordering::Relaxed), 4);
        assert!(flashes.render(20).is_empty());
        assert!(flashes.next_deadline().is_none());
    }

    #[test]
    fn tick_removes_all_entries_due_at_the_same_instant_with_one_render() {
        let counter = Arc::new(AtomicU64::new(0));
        let flashes = AltScreenFlashContainer::new(counting_handle(&counter));
        flashes.flash("a", 50);
        flashes.flash("b", 60);
        assert_eq!(counter.load(Ordering::Relaxed), 2);

        let deadline = flashes.next_deadline().unwrap_or_else(|| unreachable!());
        // Jump past both deadlines at once: both entries disappear, one
        // render request (the rendered frame contains no flash either way).
        flashes.tick(deadline + Duration::from_millis(100));
        assert_eq!(counter.load(Ordering::Relaxed), 3);
        assert!(flashes.render(20).is_empty());
        assert!(flashes.next_deadline().is_none());
    }

    #[test]
    fn dispose_drops_all_pending_entries() {
        let counter = Arc::new(AtomicU64::new(0));
        let flashes = AltScreenFlashContainer::new(counting_handle(&counter));
        flashes.flash("a", 1000);
        flashes.flash("b", 1000);
        assert!(flashes.next_deadline().is_some());

        flashes.dispose();
        assert!(flashes.render(20).is_empty());
        assert!(flashes.next_deadline().is_none());
        // dispose does not request a render (upstream clears timers only).
        assert_eq!(counter.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn flash_uses_the_default_duration_constant() {
        let flashes = AltScreenFlashContainer::new(noop_handle());
        flashes.flash("default", DEFAULT_DURATION_MS);
        let deadline = flashes.next_deadline().unwrap_or_else(|| unreachable!());
        // ~1000ms from now; the deadline must be in the near future.
        let upper = std::time::Instant::now() + Duration::from_millis(DEFAULT_DURATION_MS + 1);
        assert!(deadline <= upper);
    }
}
