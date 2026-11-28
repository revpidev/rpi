//! A one-shot timeout whose countdown pauses while a child waits on its
//! supervisor (V19-03 S38; upstream `supervisor-wait-timer.ts` @ v0.88.1).
//!
//! The timer measures active time with [`std::time::Instant`] (monotonic), so
//! a wall-clock change neither shortens nor extends a run budget. Each
//! blocking `contact_supervisor`/`intercom` call holds the timer paused by its
//! tool-call id; nested waits use a set so one early resume cannot restart the
//! countdown while another wait is still open.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::Notify;
use tokio::task::JoinHandle;

struct TimerState {
    remaining: Duration,
    running_since: Option<Instant>,
    paused_by: HashSet<String>,
    done: bool,
}

/// A pausable one-shot timer. Dropping the last handle or [`Self::clear`]
/// stops it.
#[derive(Clone)]
pub struct PausableTimeout {
    inner: Arc<PausableInner>,
}

struct PausableInner {
    state: Arc<Mutex<TimerState>>,
    notify: Arc<Notify>,
    task: Mutex<Option<JoinHandle<()>>>,
}

impl std::fmt::Debug for PausableTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PausableTimeout")
            .field("remaining", &self.remaining())
            .finish()
    }
}

impl PausableTimeout {
    /// Start a timer that calls `on_fire` once after `delay` of active time.
    pub fn start(delay: Duration, on_fire: impl FnOnce() + Send + 'static) -> Self {
        let state = Arc::new(Mutex::new(TimerState {
            remaining: delay,
            running_since: Some(Instant::now()),
            paused_by: HashSet::new(),
            done: false,
        }));
        let notify = Arc::new(Notify::new());
        let task_state = Arc::clone(&state);
        let task_notify = Arc::clone(&notify);
        let task = tokio::spawn(async move {
            let mut on_fire = Some(on_fire);
            loop {
                let deadline = {
                    let state = task_state.lock().unwrap_or_else(|e| e.into_inner());
                    if state.done {
                        return;
                    }
                    if state.paused_by.is_empty() {
                        Some(Instant::now() + state.remaining)
                    } else {
                        None
                    }
                };
                match deadline {
                    Some(deadline) => {
                        tokio::select! {
                            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                                let fire = {
                                    let mut state = task_state.lock().unwrap_or_else(|e| e.into_inner());
                                    if state.done || !state.paused_by.is_empty() {
                                        false
                                    } else {
                                        state.done = true;
                                        state.remaining = Duration::ZERO;
                                        state.running_since = None;
                                        true
                                    }
                                };
                                if fire {
                                    if let Some(on_fire) = on_fire.take() {
                                        on_fire();
                                    }
                                    return;
                                }
                            }
                            _ = task_notify.notified() => {}
                        }
                    }
                    None => task_notify.notified().await,
                }
            }
        });
        Self {
            inner: Arc::new(PausableInner {
                state,
                notify,
                task: Mutex::new(Some(task)),
            }),
        }
    }

    /// Pause the countdown for `key`. Repeating the same key is idempotent.
    pub fn pause(&self, key: &str) {
        {
            let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.done || !state.paused_by.insert(key.to_string()) {
                return;
            }
            if state.paused_by.len() == 1
                && let Some(running_since) = state.running_since.take()
            {
                let elapsed = running_since.elapsed();
                state.remaining = state.remaining.saturating_sub(elapsed);
            }
        }
        self.inner.notify.notify_one();
    }

    /// Resume the countdown for `key`; the timer restarts only when no other
    /// wait is open.
    pub fn resume(&self, key: &str) {
        {
            let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.done || !state.paused_by.remove(key) || !state.paused_by.is_empty() {
                return;
            }
            state.running_since = Some(Instant::now());
        }
        self.inner.notify.notify_one();
    }

    /// Active time left (paused time excluded).
    pub fn remaining(&self) -> Duration {
        let state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        match state.running_since {
            Some(running_since) => state.remaining.saturating_sub(running_since.elapsed()),
            None => state.remaining,
        }
    }

    /// Stop the timer without firing.
    pub fn clear(&self) {
        {
            let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
            state.done = true;
            state.running_since = None;
            state.remaining = Duration::ZERO;
        }
        self.inner.notify.notify_one();
        if let Some(task) = self
            .inner
            .task
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            task.abort();
        }
    }
}

impl Drop for PausableInner {
    fn drop(&mut self) {
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.done = true;
            state.running_since = None;
        }
        self.notify.notify_one();
        if let Some(task) = self.task.lock().unwrap_or_else(|e| e.into_inner()).take() {
            task.abort();
        }
    }
}

/// A child blocked in one of these calls is waiting on its supervisor, not
/// working (`isBlockingSupervisorTool`, supervisor-wait-timer.ts:2-8).
pub fn is_blocking_supervisor_tool(tool_name: &str, args: &serde_json::Value) -> bool {
    match tool_name {
        "contact_supervisor" => matches!(
            args.get("reason").and_then(serde_json::Value::as_str),
            Some("need_decision") | Some("interview_request")
        ),
        "intercom" => args.get("action").and_then(serde_json::Value::as_str) == Some("ask"),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn paused_timer_keeps_the_remaining_budget() {
        let fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = Arc::clone(&fired);
        let timer = PausableTimeout::start(Duration::from_millis(100), move || {
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        tokio::time::sleep(Duration::from_millis(40)).await;
        timer.pause("call-1");
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(!fired.load(std::sync::atomic::Ordering::SeqCst));
        timer.resume("call-1");
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert!(fired.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn nested_pauses_do_not_resume_early() {
        let fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = Arc::clone(&fired);
        let timer = PausableTimeout::start(Duration::from_millis(50), move || {
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        timer.pause("a");
        timer.pause("b");
        timer.resume("a");
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert!(!fired.load(std::sync::atomic::Ordering::SeqCst));
        timer.resume("b");
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert!(fired.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn blocking_supervisor_calls_are_classified() {
        assert!(is_blocking_supervisor_tool(
            "contact_supervisor",
            &serde_json::json!({"reason": "need_decision"})
        ));
        assert!(is_blocking_supervisor_tool(
            "contact_supervisor",
            &serde_json::json!({"reason": "interview_request"})
        ));
        assert!(!is_blocking_supervisor_tool(
            "contact_supervisor",
            &serde_json::json!({"reason": "progress_update"})
        ));
        assert!(is_blocking_supervisor_tool(
            "intercom",
            &serde_json::json!({"action": "ask"})
        ));
        assert!(!is_blocking_supervisor_tool(
            "intercom",
            &serde_json::json!({"action": "send"})
        ));
    }
}
