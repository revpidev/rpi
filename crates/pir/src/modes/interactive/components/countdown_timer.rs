//! Reusable countdown timer — port of
//! `packages/coding-agent/src/modes/interactive/components/countdown-timer.ts`
//! @ pi 0.82.1 (2efa728).
//!
//! Intentional differences:
//! - Upstream uses `setInterval` and clears it on expiry/dispose; the port
//!   uses a dedicated thread waiting on `recv_timeout` of a stop channel
//!   (same pattern as pir-tui `Loader`, coding-standards §6.4), so `dispose`
//!   joins the thread and no background task leaks.
//! - Upstream `tui?: TUI` is `Option<RenderHandle>` (the capability the timer
//!   needs, `tui.requestRender()` on each tick).

use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use pir_tui::tui::RenderHandle;

/// `CountdownTimer` (countdown-timer.ts:7-38): calls `on_tick` with the
/// remaining seconds immediately on construction, then once per second until
/// the timeout elapses; calls `on_expire` on the final tick.
pub struct CountdownTimer {
    stop_tx: Option<mpsc::Sender<()>>,
    thread_handle: Option<JoinHandle<()>>,
}

impl CountdownTimer {
    pub fn new(
        timeout_ms: u64,
        render_handle: Option<RenderHandle>,
        on_tick: Box<dyn Fn(u64) + Send>,
        on_expire: Box<dyn FnOnce() + Send>,
    ) -> Self {
        let mut remaining_seconds = timeout_ms.div_ceil(1000);
        on_tick(remaining_seconds);

        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        let thread_handle = thread::spawn(move || loop {
            match stop_rx.recv_timeout(Duration::from_secs(1)) {
                Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {
                    remaining_seconds = remaining_seconds.saturating_sub(1);
                    on_tick(remaining_seconds);
                    if let Some(handle) = &render_handle {
                        handle.request_render();
                    }
                    if remaining_seconds == 0 {
                        on_expire();
                        break;
                    }
                }
            }
        });

        Self {
            stop_tx: Some(stop_tx),
            thread_handle: Some(thread_handle),
        }
    }

    /// `dispose` (countdown-timer.ts:33-38): stop ticking. Safe to call after
    /// natural expiry (the thread has already ended; join is a no-op).
    pub fn dispose(&mut self) {
        if let Some(stop_tx) = self.stop_tx.take() {
            let _ = stop_tx.send(());
        }
        if let Some(thread_handle) = self.thread_handle.take() {
            let _ = thread_handle.join();
        }
    }
}

impl Drop for CountdownTimer {
    fn drop(&mut self) {
        // Never leak the tick thread (coding-standards §6.4).
        self.dispose();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    #[test]
    fn ticks_once_per_second_and_expires() {
        let ticks = Arc::new(Mutex::new(Vec::new()));
        let ticks_thread = Arc::clone(&ticks);
        let expired = Arc::new(AtomicU64::new(0));
        let expired_thread = Arc::clone(&expired);
        let _timer = CountdownTimer::new(
            2500,
            None,
            Box::new(move |seconds| ticks_thread.lock().unwrap().push(seconds)),
            Box::new(move || {
                expired_thread.store(1, Ordering::SeqCst);
            }),
        );

        // Construction fires the initial tick (ceil(2500/1000) = 3).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(6);
        while expired.load(Ordering::SeqCst) == 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(expired.load(Ordering::SeqCst), 1, "timer must expire");
        let ticks = ticks.lock().unwrap();
        // 3, 2, 1, 0 (the 0-tick fires before expire, countdown-timer.ts:22-29).
        assert_eq!(*ticks, vec![3, 2, 1, 0]);
    }

    #[test]
    fn dispose_stops_ticking() {
        let count = Arc::new(AtomicU64::new(0));
        let count_thread = Arc::clone(&count);
        let mut timer = CountdownTimer::new(
            100_000,
            None,
            Box::new(move |_| {
                count_thread.fetch_add(1, Ordering::SeqCst);
            }),
            Box::new(|| {}),
        );
        // Initial tick only.
        assert_eq!(count.load(Ordering::SeqCst), 1);
        timer.dispose();
        std::thread::sleep(Duration::from_millis(1200));
        assert_eq!(count.load(Ordering::SeqCst), 1, "no ticks after dispose");
    }

    #[test]
    fn drop_joins_thread_without_panicking() {
        let timer = CountdownTimer::new(100_000, None, Box::new(|_| {}), Box::new(|| {}));
        drop(timer);
    }
}
