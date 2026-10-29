//! Host timers for the Session and Harness (port of
//! `packages/durable/src/timers.ts` @ `4a080252f`).
//!
//! A foreground timer paces or resumes work and must fire, so one the host
//! declines or throws on falls back to the runtime's timer, reported once. A
//! background timer only frees memory or resumes a sleep and is declined when
//! the host cannot unreference it.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::harness::types::{ReportFn, SetTimer, TimerDisarm, TimerOptions};

/// Longest delay one runtime timer supports; a longer wait is armed in steps.
const MAX_TIMER_DELAY: u64 = 2_147_483_647;

/// The default host timer: the async runtime's sleep. Declined when no runtime
/// is available to arm it.
pub fn native_timer() -> SetTimer {
    Arc::new(|callback, delay_ms, _options| {
        let handle = tokio::runtime::Handle::try_current().ok()?;
        let join = handle.spawn(async move {
            let mut remaining = delay_ms;
            loop {
                let step = remaining.min(MAX_TIMER_DELAY);
                tokio::time::sleep(std::time::Duration::from_millis(step)).await;
                remaining -= step;
                if remaining == 0 {
                    break;
                }
            }
            callback();
        });
        Some(Arc::new(move || join.abort()))
    })
}

/// The timers the Session and Harness arm through the host's `setTimer`.
pub struct Timers {
    arm: SetTimer,
    report: ReportFn,
    reported_fallback: AtomicBool,
}

impl Timers {
    /// Wrap a host timer; a foreground one the host declines or throws on falls
    /// back to the runtime timer, reported once.
    pub fn new(set_timer: Option<SetTimer>, report: ReportFn) -> Arc<Self> {
        Arc::new(Self {
            arm: set_timer.unwrap_or_else(native_timer),
            report,
            reported_fallback: AtomicBool::new(false),
        })
    }

    fn fall_back(&self, message: &str) {
        if !self.reported_fallback.swap(true, Ordering::SeqCst) {
            (self.report)(message.to_string());
        }
    }

    /// A timer that paces or resumes work; it must fire.
    pub fn foreground(&self, callback: Arc<dyn Fn() + Send + Sync>, delay_ms: f64) -> TimerDisarm {
        let delay = delay_ms.max(0.0) as u64;
        let armed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            (self.arm)(
                Arc::clone(&callback),
                delay,
                TimerOptions { background: false },
            )
        }));
        match armed {
            Ok(Some(disarm)) => return disarm,
            Ok(None) => self.fall_back(
                "HarnessOptions.setTimer declined a timer that paces work; using the runtime timer",
            ),
            Err(_) => self.fall_back(
                "HarnessOptions.setTimer threw for a timer that paces work; using the runtime timer",
            ),
        }
        native_timer()(callback, delay, TimerOptions { background: false })
            .unwrap_or_else(|| Arc::new(|| {}))
    }

    /// A timer that only frees memory or resumes a sleep; `None` when the host
    /// declines it.
    pub fn background(
        &self,
        callback: Arc<dyn Fn() + Send + Sync>,
        delay_ms: f64,
    ) -> Option<TimerDisarm> {
        let delay = delay_ms.max(0.0) as u64;
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            (self.arm)(callback, delay, TimerOptions { background: true })
        })) {
            Ok(disarm) => disarm,
            Err(_) => {
                (self.report)("HarnessOptions.setTimer threw for a background timer".to_string());
                None
            }
        }
    }
}
